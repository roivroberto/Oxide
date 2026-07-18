#[cfg(unix)]
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use super::actor::{
    ActorEffect, ActorEvent, CleanupCause, CleanupMode, LaunchOutcome, MAX_STDERR_BYTES,
    MAX_STDERR_LINES, READER_INBOX_BODY_BYTES,
};
use super::framing::encode_message;
#[cfg(unix)]
use super::process_adapter::{
    AdapterControlError, TestIoLanePanic, TestLaunchGate, cleanup_partial_owner_at,
};
use super::process_adapter::{
    AdapterFatal, BoundedStderrBuffer, CleanupLaneOps, CleanupOpError, CompletionCell,
    FatalSideband, InboxBudget, IoStop, LanguageProcessAdapter, LanguageProcessConfig,
    ReaderThreadOutcome, SiblingResolutionError, WriterCompletion, WriterHandoffError,
    WriterOutcomeSlot, WriterThreadOutcome, after_owner_drop, join_if_completed, reader_lane,
    reader_lane_with_wake, resolve_cleanup_owner, resolve_sibling_lsp_from, run_lane_cleanup,
    run_reader, run_writer, writer_input,
};
#[cfg(not(windows))]
use super::process_adapter::{CleanupProcessOps, run_process_cleanup};
use super::snapshot::{ProcessGeneration, WriteSequence};

#[test]
fn sibling_resolver_uses_only_the_exact_absolute_peer() {
    let executable = if cfg!(windows) {
        PathBuf::from(r"C:\oxide\oxide-ide.exe")
    } else {
        PathBuf::from("/opt/oxide/oxide-ide")
    };
    let expected = executable.parent().unwrap().join(if cfg!(windows) {
        "rlox-lsp.exe"
    } else {
        "rlox-lsp"
    });

    assert_eq!(
        resolve_sibling_lsp_from(&executable, |_| Ok(true)),
        Ok(expected)
    );
    assert_eq!(
        resolve_sibling_lsp_from(&executable, |_| Ok(false)),
        Err(SiblingResolutionError::Missing)
    );
    assert_eq!(
        resolve_sibling_lsp_from(PathBuf::from("relative/oxide-ide").as_path(), |_| Ok(true)),
        Err(SiblingResolutionError::InvalidCurrentExecutable)
    );
}

#[test]
fn stderr_buffer_retains_the_newest_bounded_bytes_and_lines() {
    let mut buffer = BoundedStderrBuffer::new();
    let prefix = vec![b'x'; MAX_STDERR_BYTES];
    buffer.push(&prefix);
    for line in 0..=MAX_STDERR_LINES {
        buffer.push(format!("\nline-{line}").as_bytes());
    }

    let tail = buffer.finish();

    assert!(tail.text.len() <= MAX_STDERR_BYTES);
    assert!(tail.line_count <= MAX_STDERR_LINES);
    assert!(tail.text.ends_with(&format!("line-{MAX_STDERR_LINES}")));
    assert!(tail.truncated);
}

#[test]
fn stderr_projection_is_utf8_safe_after_front_truncation() {
    let mut buffer = BoundedStderrBuffer::new();
    buffer.push(&vec![b'a'; MAX_STDERR_BYTES - 1]);
    buffer.push("🙂tail".as_bytes());

    let tail = buffer.finish();

    assert!(tail.text.len() <= MAX_STDERR_BYTES);
    assert!(tail.text.ends_with("🙂tail"));
    assert!(tail.truncated);
}

#[test]
fn completion_cell_readiness_is_not_treated_as_thread_completion() {
    let completion = Arc::new(CompletionCell::new());
    let published = Arc::clone(&completion);
    let (cell_ready, observe_cell) = mpsc::sync_channel(0);
    let (release, gate) = mpsc::sync_channel(0);
    let (returned, observe_return) = mpsc::sync_channel(0);
    let mut handle = Some(thread::spawn(move || {
        published.publish(17_u8);
        cell_ready.send(()).unwrap();
        gate.recv().unwrap();
        returned.send(()).unwrap();
        17_u8
    }));
    observe_cell.recv().unwrap();

    assert_eq!(join_if_completed(&completion, &mut handle).unwrap(), None);
    assert!(handle.is_some());

    release.send(()).unwrap();
    observe_return.recv().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while handle.as_ref().is_some_and(|join| !join.is_finished()) {
        assert!(Instant::now() < deadline, "thread did not finish");
        thread::yield_now();
    }
    assert_eq!(
        join_if_completed(&completion, &mut handle).unwrap(),
        Some(17)
    );
    assert!(handle.is_none());
}

#[test]
fn reader_byte_budget_is_exact_and_released_with_each_item() {
    let budget = InboxBudget::new(READER_INBOX_BODY_BYTES);
    let full = budget
        .try_reserve(READER_INBOX_BODY_BYTES)
        .expect("exact byte capacity");
    assert!(budget.try_reserve(1).is_none());
    assert_eq!(budget.retained_bytes(), READER_INBOX_BODY_BYTES);

    drop(full);

    assert_eq!(budget.retained_bytes(), 0);
    assert!(budget.try_reserve(READER_INBOX_BODY_BYTES).is_some());
}

#[test]
fn writer_port_has_one_nonblocking_slot_and_reports_disconnect() {
    let generation = ProcessGeneration::from_raw(1).unwrap();
    let first = WriteSequence::from_raw(1).unwrap();
    let second = WriteSequence::from_raw(2).unwrap();
    let (port, receiver) = writer_input();

    port.try_send(generation, first, Arc::from(&b"one"[..]))
        .unwrap();
    assert_eq!(
        port.try_send(generation, second, Arc::from(&b"two"[..])),
        Err(WriterHandoffError::Full)
    );
    let received = receiver.try_recv().unwrap();
    assert_eq!(received.generation, generation);
    assert_eq!(received.sequence, first);
    assert_eq!(received.bytes.as_ref(), b"one");
    drop(receiver);
    assert_eq!(
        port.try_send(generation, second, Arc::from(&b"two"[..])),
        Err(WriterHandoffError::Disconnected)
    );
}

struct OneByteReader {
    bytes: Vec<u8>,
    offset: usize,
}

impl std::io::Read for OneByteReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.offset == self.bytes.len() {
            return Ok(0);
        }
        output[0] = self.bytes[self.offset];
        self.offset += 1;
        Ok(1)
    }
}

#[test]
fn reader_decodes_fragmented_concatenated_frames_without_retaining_budget() {
    let generation = ProcessGeneration::from_raw(1).unwrap();
    let mut bytes = encode_message(&json!({"jsonrpc":"2.0","method":"one"})).unwrap();
    bytes.extend(encode_message(&json!({"jsonrpc":"2.0","method":"two"})).unwrap());
    let budget = InboxBudget::new(READER_INBOX_BODY_BYTES);
    let (sender, inbox) = reader_lane(budget.clone());
    let fatal = FatalSideband::new();

    let outcome = run_reader(
        OneByteReader { bytes, offset: 0 },
        generation,
        sender,
        &fatal,
    );

    assert_eq!(outcome, ReaderThreadOutcome::CleanEof);
    assert!(inbox.try_recv().is_some());
    assert!(inbox.try_recv().is_some());
    assert!(inbox.try_recv().is_none());
    assert_eq!(budget.retained_bytes(), 0);
    assert_eq!(fatal.take(), None);
}

#[test]
fn reader_finishes_decoder_and_classifies_partial_eof_as_framing() {
    let generation = ProcessGeneration::from_raw(7).unwrap();
    let budget = InboxBudget::new(READER_INBOX_BODY_BYTES);
    let (sender, _inbox) = reader_lane(budget);
    let fatal = FatalSideband::new();

    let outcome = run_reader(
        std::io::Cursor::new(b"Content-Length: 9\r\n\r\n{".to_vec()),
        generation,
        sender,
        &fatal,
    );

    assert_eq!(outcome, ReaderThreadOutcome::FramingFailed);
    assert_eq!(
        fatal.take(),
        Some(AdapterFatal::Reader {
            generation,
            cause: super::actor::ReaderFatalCause::Framing,
        })
    );
}

#[derive(Clone, Default)]
struct RecordingWriter {
    state: Arc<std::sync::Mutex<(Vec<u8>, usize)>>,
}

impl std::io::Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.state.lock().unwrap().0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.state.lock().unwrap().1 += 1;
        Ok(())
    }
}

#[test]
fn writer_reports_only_after_exact_write_and_flush() {
    let generation = ProcessGeneration::from_raw(3).unwrap();
    let sequence = WriteSequence::from_raw(9).unwrap();
    let (port, receiver) = writer_input();
    port.try_send(generation, sequence, Arc::from(&b"frame"[..]))
        .unwrap();
    drop(port);
    let writer = RecordingWriter::default();
    let observed = writer.clone();
    let outcomes = WriterOutcomeSlot::new();
    let fatal = FatalSideband::new();

    let result = run_writer(
        writer,
        generation,
        receiver,
        IoStop::new(),
        &outcomes,
        &fatal,
    );

    assert_eq!(result, WriterThreadOutcome::InputClosed);
    assert_eq!(*observed.state.lock().unwrap(), (b"frame".to_vec(), 1));
    assert_eq!(
        outcomes.take(),
        Some(WriterCompletion {
            generation,
            sequence,
            outcome: super::actor::WriterOutcome::Flushed,
        })
    );
    assert_eq!(fatal.take(), None);
}

#[test]
fn occupied_writer_outcome_slot_is_fatal_and_never_blocks() {
    let generation = ProcessGeneration::from_raw(4).unwrap();
    let first = WriteSequence::from_raw(1).unwrap();
    let second = WriteSequence::from_raw(2).unwrap();
    let outcomes = WriterOutcomeSlot::new();
    assert!(outcomes.publish(WriterCompletion {
        generation,
        sequence: first,
        outcome: super::actor::WriterOutcome::Flushed,
    }));
    let (port, receiver) = writer_input();
    port.try_send(generation, second, Arc::from(&b"second"[..]))
        .unwrap();
    drop(port);
    let fatal = FatalSideband::new();

    let result = run_writer(
        RecordingWriter::default(),
        generation,
        receiver,
        IoStop::new(),
        &outcomes,
        &fatal,
    );

    assert_eq!(result, WriterThreadOutcome::ResultOverflow);
    assert_eq!(
        fatal.take(),
        Some(AdapterFatal::Writer {
            generation,
            cause: super::actor::WriterFatalCause::ResultOverflow,
        })
    );
}

#[test]
fn every_background_producer_signals_the_shared_advisory_wake() {
    let generation = ProcessGeneration::from_raw(8).unwrap();

    let (fatal_wake, fatal_rx) = mpsc::sync_channel(1);
    let fatal = FatalSideband::with_wake(fatal_wake);
    assert!(fatal.store_first(AdapterFatal::Reader {
        generation,
        cause: super::actor::ReaderFatalCause::Io,
    }));
    assert_eq!(fatal_rx.try_recv(), Ok(()));

    let (writer_wake, writer_rx) = mpsc::sync_channel(1);
    let outcomes = WriterOutcomeSlot::with_wake(writer_wake);
    assert!(outcomes.publish(WriterCompletion {
        generation,
        sequence: WriteSequence::from_raw(1).unwrap(),
        outcome: super::actor::WriterOutcome::Flushed,
    }));
    assert_eq!(writer_rx.try_recv(), Ok(()));

    let (reader_wake, reader_rx) = mpsc::sync_channel(1);
    let (sender, _inbox) =
        reader_lane_with_wake(InboxBudget::new(READER_INBOX_BODY_BYTES), reader_wake);
    let mut bytes = encode_message(&json!({"jsonrpc":"2.0","method":"wake"})).unwrap();
    let reader_fatal = FatalSideband::new();
    assert_eq!(
        run_reader(
            std::io::Cursor::new(std::mem::take(&mut bytes)),
            generation,
            sender,
            &reader_fatal
        ),
        ReaderThreadOutcome::CleanEof
    );
    assert_eq!(reader_rx.try_recv(), Ok(()));
}

#[test]
fn poll_event_uses_the_pinned_epoch_priority_when_every_slot_is_ready() {
    let generation = ProcessGeneration::from_raw(10).unwrap();
    let mut adapter = LanguageProcessAdapter::with_all_pending_events_for_test(generation);
    let mut observed = Vec::new();
    for _ in 0..6 {
        observed.push(match adapter.poll_event().expect("pending adapter event") {
            ActorEvent::ReaderFatal { .. } | ActorEvent::WriterFatal { .. } => "fatal",
            ActorEvent::ChildExited { .. } => "child",
            ActorEvent::LaunchFinished { .. } => "launch",
            ActorEvent::WriterFinished { .. } => "writer",
            ActorEvent::FinalizedGeneration { .. } | ActorEvent::CleanupFailed { .. } => {
                "finalization"
            }
            ActorEvent::ReaderMessage { .. } => "reader",
            event => panic!("unexpected pending event: {event:?}"),
        });
    }

    assert_eq!(
        observed,
        [
            "fatal",
            "child",
            "launch",
            "writer",
            "finalization",
            "reader"
        ]
    );
    assert!(adapter.poll_event().is_none());
}

#[test]
fn cleanup_closes_client_lanes_still_owned_by_a_pending_ready_report() {
    let generation = ProcessGeneration::from_raw(12).unwrap();
    let mut adapter = LanguageProcessAdapter::with_all_pending_events_for_test(generation);
    adapter.move_test_lanes_into_pending_launch();
    assert!(adapter.has_client_lanes_for_test());

    assert!(matches!(
        adapter.poll_event(),
        Some(ActorEvent::ReaderFatal {
            generation: event_generation,
            ..
        }) if event_generation == generation
    ));
    assert!(matches!(
        adapter.poll_event(),
        Some(ActorEvent::ChildExited {
            generation: event_generation,
            ..
        }) if event_generation == generation
    ));
    adapter
        .execute_effect(ActorEffect::BeginCleanup {
            generation,
            mode: CleanupMode::ForceTerminate,
            cause: CleanupCause::Shutdown,
        })
        .unwrap();

    assert!(!adapter.has_client_lanes_for_test());
    assert!(matches!(
        adapter.poll_event(),
        Some(ActorEvent::LaunchFinished {
            generation: event_generation,
            outcome: LaunchOutcome::Ready,
        }) if event_generation == generation
    ));
    assert!(!adapter.has_client_lanes_for_test());
}

#[test]
fn drop_is_signal_only_even_while_the_worker_control_lock_is_held() {
    let generation = ProcessGeneration::from_raw(11).unwrap();
    let adapter = LanguageProcessAdapter::with_all_pending_events_for_test(generation);
    let (entered, observe_entered) = mpsc::sync_channel(0);
    let (release, await_release) = mpsc::sync_channel(0);
    let holder = adapter.hold_control_lock_for_test(entered, await_release);
    observe_entered.recv().unwrap();
    let (dropped, observe_dropped) = mpsc::sync_channel(0);
    let dropper = thread::spawn(move || {
        drop(adapter);
        dropped.send(()).unwrap();
    });

    let result = observe_dropped.recv_timeout(Duration::from_secs(1));

    release.send(()).unwrap();
    holder.join().unwrap();
    dropper.join().unwrap();
    assert_eq!(result, Ok(()));
}

#[test]
fn a_prefilled_advisory_wake_coalesces_without_hiding_the_durable_event() {
    let generation = ProcessGeneration::from_raw(9).unwrap();
    let (wake, receiver) = mpsc::sync_channel(1);
    wake.try_send(()).unwrap();
    let fatal = FatalSideband::with_wake(wake);

    assert!(fatal.store_first(AdapterFatal::Writer {
        generation,
        cause: super::actor::WriterFatalCause::Io,
    }));

    assert_eq!(receiver.try_recv(), Ok(()));
    assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(
        fatal.take(),
        Some(AdapterFatal::Writer {
            generation,
            cause: super::actor::WriterFatalCause::Io,
        })
    );
}

fn next_adapter_event(adapter: &mut LanguageProcessAdapter) -> ActorEvent {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(event) = adapter.poll_event() {
            return event;
        }
        let now = std::time::Instant::now();
        assert!(now < deadline, "adapter event timed out");
        if !adapter.wait_for_wake(deadline.saturating_duration_since(now)) {
            thread::yield_now();
        }
    }
}

#[test]
fn failed_no_owner_launch_remains_cancelable_until_its_report_is_claimed() {
    let generation = ProcessGeneration::from_raw(40).unwrap();
    let missing = if cfg!(windows) {
        PathBuf::from(r"Z:\oxide-missing\rlox-lsp.exe")
    } else {
        PathBuf::from("/oxide-missing/rlox-lsp")
    };
    let config = LanguageProcessConfig::for_command(missing, Vec::new());
    let mut adapter = LanguageProcessAdapter::start(config).unwrap();
    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation })
        .unwrap();
    assert!(adapter.wait_for_wake(Duration::from_secs(5)));

    assert_eq!(
        adapter.execute_effect(ActorEffect::CancelLaunch { generation }),
        Ok(None)
    );
    assert!(matches!(
        adapter.poll_event(),
        Some(ActorEvent::LaunchFinished {
            generation: reported,
            outcome: super::actor::LaunchOutcome::FailedBeforeOwnership { .. },
        }) if reported == generation
    ));
    adapter.request_shutdown();
}

#[test]
fn external_prefilled_wake_is_nonblocking_and_the_launch_report_remains_durable() {
    let generation = ProcessGeneration::from_raw(39).unwrap();
    let missing = if cfg!(windows) {
        PathBuf::from(r"Z:\oxide-missing\rlox-lsp.exe")
    } else {
        PathBuf::from("/oxide-missing/rlox-lsp")
    };
    let (wake, receiver) = mpsc::sync_channel(1);
    wake.try_send(()).unwrap();
    let config = LanguageProcessConfig::for_command(missing, Vec::new());
    let mut adapter = LanguageProcessAdapter::start_with_wake(config, wake).unwrap();

    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation })
        .unwrap();

    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation: reported,
            outcome: LaunchOutcome::FailedBeforeOwnership { .. },
        } if reported == generation
    ));
    assert_eq!(receiver.try_recv(), Ok(()));
    assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    adapter.request_shutdown();
}

#[cfg(unix)]
#[test]
fn live_root_stdout_eof_is_reported_as_fatal_and_the_generation_remains_cleanupable() {
    let generation = ProcessGeneration::from_raw(49).unwrap();
    let config = LanguageProcessConfig::for_command(
        PathBuf::from("/bin/sh"),
        vec![
            OsString::from("-c"),
            OsString::from("exec 1>&-; exec sleep 30"),
        ],
    );
    let mut adapter = LanguageProcessAdapter::start(config).unwrap();
    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation })
        .unwrap();

    let mut launched = false;
    loop {
        match next_adapter_event(&mut adapter) {
            ActorEvent::LaunchFinished {
                generation: observed,
                outcome: LaunchOutcome::Ready,
            } if observed == generation => launched = true,
            ActorEvent::ReaderFatal {
                generation: observed,
                cause: super::actor::ReaderFatalCause::Io,
            } if observed == generation => break,
            ActorEvent::ChildExited { status, .. } => {
                panic!("live fixture exited before stdout EOF was classified: {status:?}")
            }
            _ => {}
        }
    }

    adapter
        .execute_effect(ActorEffect::BeginCleanup {
            generation,
            mode: CleanupMode::ForceTerminate,
            cause: CleanupCause::Reader,
        })
        .unwrap();
    loop {
        match next_adapter_event(&mut adapter) {
            ActorEvent::LaunchFinished {
                generation: observed,
                outcome: LaunchOutcome::Ready,
            } if observed == generation => launched = true,
            ActorEvent::FinalizedGeneration {
                generation: observed,
                ..
            } if observed == generation => break,
            ActorEvent::CleanupFailed { cause, .. } => {
                panic!("stdout EOF cleanup failed: {cause:?}")
            }
            _ => {}
        }
    }
    assert!(launched);
    adapter.acknowledge_finalization(generation).unwrap();
    adapter.request_shutdown();
}

#[cfg(unix)]
#[test]
fn caught_reader_and_writer_panics_fail_a_live_generation_once() {
    for (raw_generation, lane, cleanup_cause) in [
        (47, TestIoLanePanic::Reader, CleanupCause::Reader),
        (48, TestIoLanePanic::Writer, CleanupCause::Writer),
    ] {
        let generation = ProcessGeneration::from_raw(raw_generation).unwrap();
        let config = LanguageProcessConfig::for_command(
            PathBuf::from("/bin/sh"),
            vec![OsString::from("-c"), OsString::from("exec sleep 30")],
        )
        .with_io_lane_panic(lane);
        let mut adapter = LanguageProcessAdapter::start(config).unwrap();
        adapter
            .execute_effect(ActorEffect::LaunchGeneration { generation })
            .unwrap();

        let mut launched = false;
        loop {
            match next_adapter_event(&mut adapter) {
                ActorEvent::LaunchFinished {
                    generation: observed,
                    outcome: LaunchOutcome::Ready,
                } if observed == generation => launched = true,
                ActorEvent::ReaderFatal {
                    generation: observed,
                    cause: super::actor::ReaderFatalCause::AdapterInvariant,
                } if observed == generation && lane == TestIoLanePanic::Reader => break,
                ActorEvent::WriterFatal {
                    generation: observed,
                    cause: super::actor::WriterFatalCause::AdapterInvariant,
                } if observed == generation && lane == TestIoLanePanic::Writer => break,
                ActorEvent::ChildExited { status, .. } => {
                    panic!("live panic fixture exited before lane failure: {status:?}")
                }
                event => panic!("unexpected lane-panic event: {event:?}"),
            }
        }

        adapter
            .execute_effect(ActorEffect::BeginCleanup {
                generation,
                mode: CleanupMode::ForceTerminate,
                cause: cleanup_cause,
            })
            .unwrap();
        loop {
            match next_adapter_event(&mut adapter) {
                ActorEvent::LaunchFinished {
                    generation: observed,
                    outcome: LaunchOutcome::Ready,
                } if observed == generation => launched = true,
                ActorEvent::CleanupFailed {
                    generation: observed,
                    cause: super::actor::CleanupFailure::Join,
                    ..
                } if observed == generation => break,
                ActorEvent::FinalizedGeneration { .. } => {
                    panic!("a panicked lane was incorrectly treated as proven clean")
                }
                ActorEvent::ReaderFatal { .. } | ActorEvent::WriterFatal { .. } => {
                    panic!("lane panic was published more than once")
                }
                _ => {}
            }
        }
        assert!(launched);
        adapter.request_shutdown();
    }
}

#[cfg(unix)]
#[test]
fn io_thread_factory_covers_fail_after_zero_one_two_and_success_after_three() {
    for fail_after in 0..=3 {
        let generation = ProcessGeneration::from_raw(50 + fail_after as u64).unwrap();
        let config = LanguageProcessConfig::for_command(
            PathBuf::from("/bin/sh"),
            vec![OsString::from("-c"), OsString::from("cat >/dev/null")],
        )
        .with_io_thread_fail_after(fail_after);
        let mut adapter = LanguageProcessAdapter::start(config).unwrap();
        adapter
            .execute_effect(ActorEffect::LaunchGeneration { generation })
            .unwrap();

        let launch = next_adapter_event(&mut adapter);
        if fail_after < 3 {
            assert!(matches!(
                launch,
                ActorEvent::LaunchFinished {
                    generation: reported,
                    outcome: LaunchOutcome::FailedWithOwnedResources {
                        cause: super::actor::LaunchFailure::Thread,
                    },
                } if reported == generation
            ));
        } else {
            assert!(matches!(
                launch,
                ActorEvent::LaunchFinished {
                    generation: reported,
                    outcome: LaunchOutcome::Ready,
                } if reported == generation
            ));
        }

        adapter
            .execute_effect(ActorEffect::BeginCleanup {
                generation,
                mode: CleanupMode::ForceTerminate,
                cause: CleanupCause::Shutdown,
            })
            .unwrap();
        loop {
            match next_adapter_event(&mut adapter) {
                ActorEvent::FinalizedGeneration {
                    generation: finalized,
                    ..
                } if finalized == generation => break,
                ActorEvent::CleanupFailed { cause, .. } => {
                    panic!("cleanup failed after {fail_after} starts: {cause:?}")
                }
                _ => {}
            }
        }
        adapter.acknowledge_finalization(generation).unwrap();
        adapter.request_shutdown();
    }
}

#[cfg(unix)]
#[test]
fn final_stderr_tail_is_carried_by_the_finalization_event() {
    let generation = ProcessGeneration::from_raw(60).unwrap();
    let config = LanguageProcessConfig::for_command(
        PathBuf::from("/bin/sh"),
        vec![
            OsString::from("-c"),
            OsString::from(
                "printf 'final-tail' >&2; printf 'Content-Length: 34\\r\\n\\r\\n{\"jsonrpc\":\"2.0\",\"method\":\"ready\"}'; cat >/dev/null",
            ),
        ],
    );
    let mut adapter = LanguageProcessAdapter::start(config).unwrap();
    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation })
        .unwrap();
    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation: launched,
            outcome: LaunchOutcome::Ready,
        } if launched == generation
    ));
    loop {
        if matches!(
            next_adapter_event(&mut adapter),
            ActorEvent::ReaderMessage {
                generation: observed,
                ..
            } if observed == generation
        ) {
            break;
        }
    }

    adapter
        .execute_effect(ActorEffect::BeginCleanup {
            generation,
            mode: CleanupMode::ForceTerminate,
            cause: CleanupCause::Shutdown,
        })
        .unwrap();
    loop {
        match next_adapter_event(&mut adapter) {
            ActorEvent::FinalizedGeneration {
                generation: finalized,
                stderr_tail: Some(tail),
            } if finalized == generation => {
                assert_eq!(tail.text.as_ref(), "final-tail");
                break;
            }
            ActorEvent::CleanupFailed { cause, .. } => {
                panic!("cleanup failed before final stderr delivery: {cause:?}")
            }
            _ => {}
        }
    }
    adapter.acknowledge_finalization(generation).unwrap();
    adapter.request_shutdown();
}

#[cfg(not(windows))]
struct ScriptedCleanupProcess {
    now: Instant,
    root_exited: Result<bool, CleanupOpError>,
    terminate: Result<(), CleanupOpError>,
    reap: Result<(), CleanupOpError>,
    observed_deadline: Option<Instant>,
    calls: Vec<&'static str>,
}

#[cfg(not(windows))]
impl CleanupProcessOps for ScriptedCleanupProcess {
    fn now(&mut self) -> Instant {
        self.calls.push("now");
        self.now
    }

    fn root_exited(&mut self) -> Result<bool, CleanupOpError> {
        self.calls.push("probe");
        self.root_exited
    }

    fn terminate_tree_once(&mut self) -> Result<(), CleanupOpError> {
        self.calls.push("terminate");
        self.terminate
    }

    fn reap_root_until(&mut self, deadline: Instant) -> Result<(), CleanupOpError> {
        self.calls.push("reap");
        self.observed_deadline = Some(deadline);
        self.reap
    }
}

#[cfg(not(windows))]
#[test]
fn cleanup_process_ops_use_one_injected_deadline_and_preserve_error_precedence() {
    let now = Instant::now();
    let timeout = Duration::from_millis(250);
    for (terminate, reap, expected) in [
        (
            Err(CleanupOpError),
            Err(CleanupOpError),
            Some(super::actor::CleanupFailure::Terminate),
        ),
        (
            Ok(()),
            Err(CleanupOpError),
            Some(super::actor::CleanupFailure::Reap),
        ),
        (Ok(()), Ok(()), None),
    ] {
        let mut process = ScriptedCleanupProcess {
            now,
            root_exited: Ok(false),
            terminate,
            reap,
            observed_deadline: None,
            calls: Vec::new(),
        };

        let (deadline, failure) = run_process_cleanup(&mut process, timeout);

        assert_eq!(deadline, now + timeout);
        assert_eq!(process.observed_deadline, Some(deadline));
        assert_eq!(failure, expected);
        assert_eq!(process.calls, ["now", "probe", "terminate", "reap"]);
    }

    let mut already_exited = ScriptedCleanupProcess {
        now,
        root_exited: Ok(true),
        terminate: Err(CleanupOpError),
        reap: Ok(()),
        observed_deadline: None,
        calls: Vec::new(),
    };
    let (_, failure) = run_process_cleanup(&mut already_exited, timeout);
    assert_eq!(failure, None);
    assert_eq!(already_exited.calls, ["now", "probe", "reap"]);
}

struct ScriptedCleanupLanes {
    writer: Result<bool, CleanupOpError>,
    reader: Result<bool, CleanupOpError>,
    stderr: Result<(bool, Option<super::actor::BoundedStderrTail>), CleanupOpError>,
    deadlines: Vec<Instant>,
}

impl CleanupLaneOps for ScriptedCleanupLanes {
    fn finish_writer(&mut self, deadline: Instant) -> Result<bool, CleanupOpError> {
        self.deadlines.push(deadline);
        self.writer
    }

    fn finish_reader(&mut self, deadline: Instant) -> Result<bool, CleanupOpError> {
        self.deadlines.push(deadline);
        self.reader
    }

    fn finish_stderr(
        &mut self,
        deadline: Instant,
    ) -> Result<(bool, Option<super::actor::BoundedStderrTail>), CleanupOpError> {
        self.deadlines.push(deadline);
        self.stderr.clone()
    }
}

#[test]
fn lane_cleanup_uses_the_exact_shared_deadline_and_returns_the_exact_failed_owner() {
    let deadline = Instant::now() + Duration::from_secs(1);
    let tail = super::actor::BoundedStderrTail {
        text: Arc::from("retained"),
        line_count: 1,
        truncated: false,
    };
    let mut lanes = ScriptedCleanupLanes {
        writer: Err(CleanupOpError),
        reader: Ok(false),
        stderr: Ok((false, Some(tail.clone()))),
        deadlines: Vec::new(),
    };

    let (failure, observed_tail) = run_lane_cleanup(&mut lanes, deadline);

    assert_eq!(failure, Some(super::actor::CleanupFailure::Join));
    assert_eq!(observed_tail, Some(tail));
    assert_eq!(lanes.deadlines, [deadline, deadline, deadline]);
    assert_eq!(
        Some(super::actor::CleanupFailure::Terminate).or(failure),
        Some(super::actor::CleanupFailure::Terminate)
    );

    let owner = Arc::new(97_u8);
    let identity = Arc::clone(&owner);
    let (cause, returned) = resolve_cleanup_owner(owner, failure).unwrap_err();
    assert_eq!(cause, super::actor::CleanupFailure::Join);
    assert!(Arc::ptr_eq(&returned, &identity));
}

struct DropProbe(Arc<std::sync::atomic::AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[test]
fn proven_clean_owner_is_dropped_before_finalization_publication() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let observed = after_owner_drop(DropProbe(Arc::clone(&dropped)), || {
        dropped.load(std::sync::atomic::Ordering::Acquire)
    });

    assert!(observed);
}

#[cfg(unix)]
#[test]
fn failed_strict_partial_cleanup_returns_the_exact_owner_for_quarantine() {
    let mut command = std::process::Command::new("sh");
    command.args(["-c", "exec sleep 30"]);
    let partial = crate::contained_child::PartialContainedChild::unassigned_for_test(command)
        .expect("spawn partial fixture");
    let root_id = partial.root_id();
    let started = Instant::now();
    let clock = Instant::now();
    let timeout = Duration::from_millis(17);

    let (cause, partial) = cleanup_partial_owner_at(partial, clock, timeout, |owner, deadline| {
        assert_eq!(owner.root_id(), root_id);
        assert_eq!(deadline, clock + timeout);
        Err(crate::contained_child::PartialCleanupFailure::Reap)
    })
    .expect_err("failed cleanup must quarantine");

    assert_eq!(cause, super::actor::CleanupFailure::Reap);
    assert_eq!(partial.root_id(), root_id);
    assert!(started.elapsed() < Duration::from_secs(1));
    let _ = (*partial).cleanup_for_debugger();
}

#[cfg(unix)]
#[test]
fn finalization_requires_exact_ack_before_next_generation_launch() {
    let first = ProcessGeneration::from_raw(41).unwrap();
    let second = ProcessGeneration::from_raw(42).unwrap();
    let config = LanguageProcessConfig::for_command(
        PathBuf::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from("cat >/dev/null")],
    );
    let mut adapter = LanguageProcessAdapter::start(config).unwrap();
    assert_eq!(
        adapter.execute_effect(ActorEffect::LaunchGeneration { generation: first }),
        Ok(None)
    );
    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation,
            outcome: LaunchOutcome::Ready,
        } if generation == first
    ));

    assert_eq!(
        adapter.execute_effect(ActorEffect::BeginCleanup {
            generation: first,
            mode: CleanupMode::ForceTerminate,
            cause: CleanupCause::Shutdown,
        }),
        Ok(None)
    );
    loop {
        if matches!(
            next_adapter_event(&mut adapter),
            ActorEvent::FinalizedGeneration { generation, .. } if generation == first
        ) {
            break;
        }
    }
    assert_eq!(
        adapter.execute_effect(ActorEffect::LaunchGeneration { generation: second }),
        Err(AdapterControlError::GenerationBusy)
    );

    adapter.acknowledge_finalization(first).unwrap();
    assert_eq!(
        adapter.execute_effect(ActorEffect::LaunchGeneration { generation: second }),
        Ok(None)
    );
    assert_eq!(
        adapter.execute_effect(ActorEffect::CancelLaunch { generation: second }),
        Ok(None)
    );
    adapter.request_shutdown();
}

#[cfg(unix)]
#[test]
fn queued_launch_is_atomically_marked_launching_before_spawn_and_remains_cancelable() {
    let first = ProcessGeneration::from_raw(71).unwrap();
    let second = ProcessGeneration::from_raw(72).unwrap();
    let third = ProcessGeneration::from_raw(73).unwrap();
    let gate = Arc::new(TestLaunchGate::new(second));
    let config = LanguageProcessConfig::for_command(
        PathBuf::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from("cat >/dev/null")],
    )
    .with_launch_gate(Arc::clone(&gate));
    let mut adapter = LanguageProcessAdapter::start(config).unwrap();
    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation: first })
        .unwrap();
    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation,
            outcome: LaunchOutcome::Ready,
        } if generation == first
    ));
    adapter
        .execute_effect(ActorEffect::BeginCleanup {
            generation: first,
            mode: CleanupMode::ForceTerminate,
            cause: CleanupCause::Shutdown,
        })
        .unwrap();
    loop {
        if matches!(
            next_adapter_event(&mut adapter),
            ActorEvent::FinalizedGeneration { generation, .. } if generation == first
        ) {
            break;
        }
    }
    adapter.acknowledge_finalization(first).unwrap();
    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation: second })
        .unwrap();
    assert!(gate.wait_until_entered(Instant::now() + Duration::from_secs(5)));

    assert_eq!(
        adapter.execute_effect(ActorEffect::CancelLaunch { generation: second }),
        Ok(None)
    );
    assert_eq!(
        adapter.execute_effect(ActorEffect::LaunchGeneration { generation: third }),
        Err(AdapterControlError::GenerationBusy)
    );
    gate.release();

    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation,
            outcome: LaunchOutcome::FailedWithOwnedResources {
                cause: super::actor::LaunchFailure::Cancelled,
            },
        } if generation == second
    ));
    loop {
        match next_adapter_event(&mut adapter) {
            ActorEvent::FinalizedGeneration { generation, .. } if generation == second => break,
            ActorEvent::CleanupFailed { cause, .. } => {
                panic!("cancelled queued launch cleanup failed: {cause:?}")
            }
            _ => {}
        }
    }
    adapter.acknowledge_finalization(second).unwrap();
    adapter.request_shutdown();
}

#[cfg(unix)]
#[test]
fn cancel_latched_before_ready_publication_never_exposes_ready() {
    let generation = ProcessGeneration::from_raw(74).unwrap();
    let gate = Arc::new(TestLaunchGate::new(generation));
    let config = LanguageProcessConfig::for_command(
        PathBuf::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from("cat >/dev/null")],
    )
    .with_ready_gate(Arc::clone(&gate));
    let mut adapter = LanguageProcessAdapter::start(config).unwrap();
    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation })
        .unwrap();
    assert!(gate.wait_until_entered(Instant::now() + Duration::from_secs(5)));

    assert_eq!(
        adapter.execute_effect(ActorEffect::CancelLaunch { generation }),
        Ok(None)
    );
    gate.release();
    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation: reported,
            outcome: LaunchOutcome::FailedWithOwnedResources {
                cause: super::actor::LaunchFailure::Cancelled,
            },
        } if reported == generation
    ));
    loop {
        match next_adapter_event(&mut adapter) {
            ActorEvent::FinalizedGeneration {
                generation: finalized,
                ..
            } if finalized == generation => break,
            ActorEvent::CleanupFailed { cause, .. } => {
                panic!("cancel-before-ready cleanup failed: {cause:?}")
            }
            ActorEvent::LaunchFinished {
                outcome: LaunchOutcome::Ready,
                ..
            } => panic!("cancel-before-publication exposed Ready"),
            _ => {}
        }
    }
    adapter.acknowledge_finalization(generation).unwrap();
    adapter.request_shutdown();
}

#[cfg(unix)]
#[test]
fn finalization_report_is_acknowledgeable_while_publisher_is_gated_and_generation_reuse_is_rejected()
 {
    let first = ProcessGeneration::from_raw(75).unwrap();
    let second = ProcessGeneration::from_raw(76).unwrap();
    let gate = Arc::new(TestLaunchGate::new(first));
    let config = LanguageProcessConfig::for_command(
        PathBuf::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from("cat >/dev/null")],
    )
    .with_finalization_gate(Arc::clone(&gate));
    let mut adapter = LanguageProcessAdapter::start(config).unwrap();
    adapter
        .execute_effect(ActorEffect::LaunchGeneration { generation: first })
        .unwrap();
    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation,
            outcome: LaunchOutcome::Ready,
        } if generation == first
    ));
    adapter
        .execute_effect(ActorEffect::BeginCleanup {
            generation: first,
            mode: CleanupMode::ForceTerminate,
            cause: CleanupCause::Shutdown,
        })
        .unwrap();
    assert!(gate.wait_until_entered(Instant::now() + Duration::from_secs(5)));

    assert!(matches!(
        adapter.poll_event(),
        Some(ActorEvent::FinalizedGeneration { generation, .. }) if generation == first
    ));
    assert_eq!(adapter.acknowledge_finalization(first), Ok(()));
    assert_eq!(
        adapter.execute_effect(ActorEffect::LaunchGeneration { generation: first }),
        Err(AdapterControlError::GenerationMismatch)
    );
    assert_eq!(
        adapter.execute_effect(ActorEffect::LaunchGeneration { generation: second }),
        Ok(None)
    );
    assert_eq!(
        adapter.execute_effect(ActorEffect::CancelLaunch { generation: second }),
        Ok(None)
    );
    gate.release();

    assert!(matches!(
        next_adapter_event(&mut adapter),
        ActorEvent::LaunchFinished {
            generation,
            outcome: LaunchOutcome::FailedWithOwnedResources {
                cause: super::actor::LaunchFailure::Cancelled,
            },
        } if generation == second
    ));
    loop {
        match next_adapter_event(&mut adapter) {
            ActorEvent::FinalizedGeneration { generation, .. } if generation == second => break,
            ActorEvent::CleanupFailed { cause, .. } => {
                panic!("queued launch cleanup failed: {cause:?}")
            }
            _ => {}
        }
    }
    adapter.acknowledge_finalization(second).unwrap();
    adapter.request_shutdown();
}

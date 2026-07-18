use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crate::{DocumentId, DocumentStamp, EditRevision, NavigationGeneration};
use serde_json::{Value, json};

use super::actor::{
    AccountedJsonRpcMessage, ActorEffect, ActorEvent, BoundedStderrTail, CleanupFailure, FrameKind,
    LaunchFailure, LaunchOutcome, ManualTick, ReaderFatalCause, RequestKind, WriterOutcome,
};
use super::coordinator::*;
use super::framing::FrameDecoder;
use super::snapshot::{
    CaretGeneration, ClientRequestId, DefinitionResultId, LanguageStatus, NoticeId,
    ProcessGeneration, SnapshotRevision, WriteSequence,
};

fn stamp(revision: u64) -> DocumentStamp {
    DocumentStamp {
        document_id: DocumentId::from_raw(1).expect("document"),
        edit_revision: EditRevision::from_raw(revision).expect("revision"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FakeAction {
    Effect(ActorEffect),
    Acknowledge(ProcessGeneration),
    Shutdown,
}

#[derive(Default)]
struct FakeShared {
    events: Mutex<VecDeque<ActorEvent>>,
    actions: Mutex<Vec<FakeAction>>,
    fail_execute: AtomicUsize,
}

#[derive(Clone, Default)]
struct FakeHandle(Arc<FakeShared>);

impl FakeHandle {
    fn push(&self, event: ActorEvent) {
        self.0.events.lock().expect("event queue").push_back(event);
    }

    fn actions(&self) -> Vec<FakeAction> {
        self.0.actions.lock().expect("action log").clone()
    }

    fn remaining_events(&self) -> usize {
        self.0.events.lock().expect("event queue").len()
    }

    fn fail_next_execute(&self) {
        self.0.fail_execute.store(1, Ordering::Release);
    }

    fn last_frame(
        &self,
        expected: RequestKind,
    ) -> (ProcessGeneration, WriteSequence, ClientRequestId) {
        self.actions()
            .into_iter()
            .rev()
            .find_map(|action| match action {
                FakeAction::Effect(ActorEffect::SendFrame {
                    generation,
                    sequence,
                    kind: FrameKind::ClientRequest { id, kind },
                    ..
                }) if kind == expected => Some((generation, sequence, id)),
                _ => None,
            })
            .expect("matching client request")
    }

    fn last_named_frame(
        &self,
        expected: fn(&FrameKind) -> bool,
    ) -> (ProcessGeneration, WriteSequence) {
        self.actions()
            .into_iter()
            .rev()
            .find_map(|action| match action {
                FakeAction::Effect(ActorEffect::SendFrame {
                    generation,
                    sequence,
                    kind,
                    ..
                }) if expected(&kind) => Some((generation, sequence)),
                _ => None,
            })
            .expect("matching outbound frame")
    }
}

struct FakeAdapter {
    shared: Arc<FakeShared>,
}

impl FakeAdapter {
    fn new() -> (Self, FakeHandle) {
        let handle = FakeHandle::default();
        (
            Self {
                shared: Arc::clone(&handle.0),
            },
            handle,
        )
    }
}

impl ProcessAdapterFacade for FakeAdapter {
    fn poll_event(&mut self) -> Option<ActorEvent> {
        self.shared.events.lock().expect("event queue").pop_front()
    }

    fn execute_effect(
        &mut self,
        effect: ActorEffect,
    ) -> Result<Option<ActorEvent>, AdapterFacadeError> {
        self.shared
            .actions
            .lock()
            .expect("action log")
            .push(FakeAction::Effect(effect));
        if self.shared.fail_execute.swap(0, Ordering::AcqRel) == 1 {
            Err(AdapterFacadeError)
        } else {
            Ok(None)
        }
    }

    fn acknowledge_finalization(
        &mut self,
        generation: ProcessGeneration,
    ) -> Result<(), AdapterFacadeError> {
        self.shared
            .actions
            .lock()
            .expect("action log")
            .push(FakeAction::Acknowledge(generation));
        Ok(())
    }

    fn request_shutdown(&mut self) {
        self.shared
            .actions
            .lock()
            .expect("action log")
            .push(FakeAction::Shutdown);
    }
}

fn core_fixture() -> (
    CoordinatorCore<FakeAdapter>,
    FakeHandle,
    Arc<CommandMailbox>,
    super::snapshot::SnapshotSubscriber,
) {
    let (adapter, handle) = FakeAdapter::new();
    let (publisher, subscriber) = initial_snapshot_channel();
    let (mailbox, _wake) = CommandMailbox::channel();
    let core = CoordinatorCore::new(adapter, publisher, Arc::new(|| {}));
    (core, handle, mailbox, subscriber)
}

fn rpc(value: Value) -> AccountedJsonRpcMessage {
    let body = serde_json::to_vec(&value).expect("serialize fixture");
    let mut wire = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    wire.extend(body);
    let message = FrameDecoder::new()
        .push(&wire)
        .expect("decode fixture")
        .into_iter()
        .next()
        .expect("one message");
    AccountedJsonRpcMessage::from_message(message)
}

fn valid_initialize(id: ClientRequestId) -> AccountedJsonRpcMessage {
    rpc(json!({
        "jsonrpc":"2.0",
        "id":id.get(),
        "result":{
            "capabilities":{
                "positionEncoding":"utf-16",
                "textDocumentSync":{"openClose":true,"change":1},
                "definitionProvider":true,
                "semanticTokensProvider":{
                    "legend":{"tokenTypes":["keyword","comment","string","number","variable","operator"],"tokenModifiers":[]},
                    "full":true
                }
            }
        }
    }))
}

fn make_written(
    core: &mut CoordinatorCore<FakeAdapter>,
    handle: &FakeHandle,
    mailbox: &CommandMailbox,
) -> ProcessGeneration {
    core.run_epoch(ManualTick::from_raw(0), mailbox);
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    finish_launched_generation(core, handle, mailbox, generation, ManualTick::from_raw(0));
    generation
}

fn finish_launched_generation(
    core: &mut CoordinatorCore<FakeAdapter>,
    handle: &FakeHandle,
    mailbox: &CommandMailbox,
    generation: ProcessGeneration,
    now: ManualTick,
) {
    handle.push(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    core.run_epoch(now, mailbox);
    let (_, initialize_sequence, initialize_id) = handle.last_frame(RequestKind::Initialize);
    handle.push(ActorEvent::WriterFinished {
        generation,
        sequence: initialize_sequence,
        outcome: WriterOutcome::Flushed,
    });
    handle.push(ActorEvent::ReaderMessage {
        generation,
        message: valid_initialize(initialize_id),
    });
    core.run_epoch(now, mailbox);
    let (_, initialized_sequence) =
        handle.last_named_frame(|kind| matches!(kind, FrameKind::Initialized));
    handle.push(ActorEvent::WriterFinished {
        generation,
        sequence: initialized_sequence,
        outcome: WriterOutcome::Flushed,
    });
    core.run_epoch(now, mailbox);
    let (_, open_sequence) =
        handle.last_named_frame(|kind| matches!(kind, FrameKind::DidOpen { .. }));
    handle.push(ActorEvent::WriterFinished {
        generation,
        sequence: open_sequence,
        outcome: WriterOutcome::Flushed,
    });
    core.run_epoch(now, mailbox);
}

fn complete_semantic_request(
    core: &mut CoordinatorCore<FakeAdapter>,
    handle: &FakeHandle,
    mailbox: &CommandMailbox,
    generation: ProcessGeneration,
    now: ManualTick,
) {
    let (_, sequence, id) = handle.last_frame(RequestKind::SemanticTokens);
    handle.push(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    handle.push(ActorEvent::ReaderMessage {
        generation,
        message: rpc(json!({
            "jsonrpc":"2.0",
            "id":id.get(),
            "result":{"data":[]}
        })),
    });
    core.run_epoch(now, mailbox);
}

#[test]
fn initial_snapshot_is_starting_at_revision_one() {
    let (_publisher, subscriber) = initial_snapshot_channel();
    let snapshot = subscriber.load_stable();
    assert_eq!(snapshot.revision().get(), 1);
    assert_eq!(snapshot.status(), LanguageStatus::Starting);
}

#[test]
fn documents_coalesce_without_copying_the_latest_source() {
    let (mailbox, _wake) = CommandMailbox::channel();
    let mut latest = Arc::<str>::from("");
    for revision in 1..=32 {
        latest = Arc::from(format!("var value = {revision};"));
        mailbox
            .submit_document(Some(DesiredDocument {
                stamp: stamp(revision),
                text: Arc::clone(&latest),
            }))
            .expect("open mailbox");
    }

    let command = mailbox.take_document().expect("latest document");
    let DocumentCommand::Set(document) = command.as_ref() else {
        panic!("expected desired document");
    };
    assert_eq!(document.stamp, stamp(32));
    assert!(Arc::ptr_eq(&document.text, &latest));
    assert!(mailbox.take_document().is_none());
}

#[test]
fn a_full_advisory_wake_never_loses_authoritative_slots() {
    let (mailbox, wake) = CommandMailbox::channel();
    mailbox
        .submit_document(Some(DesiredDocument {
            stamp: stamp(1),
            text: Arc::from("var value = 1;"),
        }))
        .expect("document");
    mailbox
        .submit_caret(CaretCommand::Current(CaretIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            stamp: stamp(1),
            generation: CaretGeneration::from_raw(1).expect("caret"),
            primary_character: Some(4),
        }))
        .expect("caret");
    mailbox
        .request_definition(DefinitionIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
            stamp: stamp(1),
            caret_generation: CaretGeneration::from_raw(1).expect("caret"),
            caret_character: 4,
        })
        .expect("definition");
    mailbox
        .acknowledge_items(
            AcknowledgementBatch::new(
                ProcessGeneration::from_raw(1).expect("process"),
                SnapshotRevision::from_raw(1).expect("revision"),
                DefinitionResultId::from_raw(1),
                Arc::from([NoticeId::from_raw(1).expect("notice")]),
            )
            .expect("bounded batch"),
        )
        .expect("acknowledgement");

    assert!(wake.try_recv().is_ok());
    assert!(wake.try_recv().is_err());
    assert!(mailbox.take_document().is_some());
    assert!(mailbox.take_caret().is_some());
    assert!(mailbox.take_definition().is_some());
    assert!(mailbox.take_acknowledgement().is_some());
}

#[test]
fn shutdown_is_monotonic_and_closes_command_submission() {
    let (mailbox, _wake) = CommandMailbox::channel();
    mailbox.request_shutdown();
    mailbox.request_shutdown();
    assert!(mailbox.shutdown_requested());
    assert_eq!(mailbox.submit_document(None), Err(LanguageClosed));
    assert_eq!(
        mailbox.submit_caret(CaretCommand::Exhausted {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            stamp: stamp(1),
        }),
        Err(LanguageClosed)
    );
}

#[test]
fn actor_publications_begin_at_revision_two_after_visible_revision_one() {
    let (mut core, handle, mailbox, subscriber) = core_fixture();
    let result = core.run_epoch(ManualTick::from_raw(0), &mailbox);
    assert!(!result.stopped);
    assert_eq!(subscriber.load_stable().revision().get(), 2);
    assert!(matches!(
        handle.actions().as_slice(),
        [FakeAction::Effect(ActorEffect::LaunchGeneration { .. })]
    ));
}

#[test]
fn shutdown_cancels_a_launch_already_accepted_by_the_adapter() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    mailbox.request_shutdown();
    core.run_epoch(ManualTick::from_raw(1), &mailbox);

    let actions = handle.actions();
    assert!(matches!(
        actions.as_slice(),
        [
            FakeAction::Effect(ActorEffect::LaunchGeneration { generation: first }),
            FakeAction::Effect(ActorEffect::CancelLaunch { generation: second })
        ] if first == second
    ));
}

#[test]
fn finalization_is_acknowledged_before_retry_launch_is_exposed() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    handle.push(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::FailedWithOwnedResources {
            cause: LaunchFailure::Containment,
        },
    });
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    handle.push(ActorEvent::FinalizedGeneration {
        generation,
        stderr_tail: None,
    });
    core.run_epoch(ManualTick::from_raw(250), &mailbox);

    let actions = handle.actions();
    let acknowledged = actions
        .iter()
        .position(|action| *action == FakeAction::Acknowledge(generation))
        .expect("finalization acknowledgement");
    let retry_launch = actions
        .iter()
        .position(|action| {
            matches!(
                action,
                FakeAction::Effect(ActorEffect::LaunchGeneration { generation })
                    if generation.get() == 2
            )
        })
        .expect("retry launch");
    assert!(acknowledged < retry_launch);
}

#[test]
fn adapter_control_failure_publishes_disabled_and_closes_without_blocking() {
    let (mut core, handle, mailbox, subscriber) = core_fixture();
    handle.fail_next_execute();
    let result = core.run_epoch(ManualTick::from_raw(0), &mailbox);

    assert!(result.stopped);
    let snapshot = subscriber.load_stable();
    assert_eq!(snapshot.revision(), SnapshotRevision::terminal());
    assert_eq!(snapshot.status(), LanguageStatus::Disabled);
    assert_eq!(mailbox.submit_document(None), Err(LanguageClosed));
    assert!(handle.actions().contains(&FakeAction::Shutdown));
}

#[test]
fn launch_deadline_is_driven_by_virtual_time_without_polling() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    let launched = core.run_epoch(ManualTick::from_raw(0), &mailbox);
    assert_eq!(launched.next_deadline, Some(ManualTick::from_raw(5_000)));

    core.run_epoch(ManualTick::from_raw(4_999), &mailbox);
    assert_eq!(
        handle
            .actions()
            .iter()
            .filter(|action| matches!(action, FakeAction::Effect(ActorEffect::CancelLaunch { .. })))
            .count(),
        0
    );
    core.run_epoch(ManualTick::from_raw(5_000), &mailbox);
    assert_eq!(
        handle
            .actions()
            .iter()
            .filter(|action| matches!(action, FakeAction::Effect(ActorEffect::CancelLaunch { .. })))
            .count(),
        1
    );
}

#[test]
fn bounded_adapter_drain_cannot_starve_shutdown_or_document_control() {
    let (mut core, handle, mailbox, subscriber) = core_fixture();
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    for _ in 0..100 {
        handle.push(ActorEvent::StderrTailChanged {
            generation,
            tail: BoundedStderrTail {
                text: Arc::from(""),
                line_count: 0,
                truncated: false,
            },
        });
    }
    mailbox
        .submit_document(Some(DesiredDocument {
            stamp: stamp(7),
            text: Arc::from("var current = 7;"),
        }))
        .expect("document");
    mailbox.request_shutdown();

    core.run_epoch(ManualTick::from_raw(1), &mailbox);
    assert!(
        handle.remaining_events() > 0,
        "adapter draining must be bounded"
    );
    assert!(
        handle.actions().iter().any(|action| {
            matches!(action, FakeAction::Effect(ActorEffect::CancelLaunch { .. }))
        })
    );
    assert_eq!(subscriber.load_stable().desired_stamp(), Some(stamp(7)));
}

#[test]
fn cleanup_failure_is_never_acknowledged() {
    let (mut core, handle, mailbox, subscriber) = core_fixture();
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    handle.push(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::FailedWithOwnedResources {
            cause: LaunchFailure::Containment,
        },
    });
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    handle.push(ActorEvent::CleanupFailed {
        generation,
        cause: CleanupFailure::VerifyTreeEmpty,
        stderr_tail: None,
    });
    let result = core.run_epoch(ManualTick::from_raw(1), &mailbox);

    assert!(result.stopped);
    assert!(
        !handle
            .actions()
            .contains(&FakeAction::Acknowledge(generation))
    );
    assert_eq!(subscriber.load_stable().status(), LanguageStatus::Disabled);
}

#[test]
fn disjoint_acknowledgements_merge_while_the_advisory_wake_is_full() {
    let (mailbox, _wake) = CommandMailbox::channel();
    let first = NoticeId::from_raw(1).expect("notice");
    let second = NoticeId::from_raw(2).expect("notice");
    mailbox
        .acknowledge_items(
            AcknowledgementBatch::new(
                ProcessGeneration::from_raw(1).expect("process"),
                SnapshotRevision::from_raw(4).expect("revision"),
                None,
                Arc::from([first]),
            )
            .expect("batch"),
        )
        .expect("first acknowledgement");
    mailbox
        .acknowledge_items(
            AcknowledgementBatch::new(
                ProcessGeneration::from_raw(1).expect("process"),
                SnapshotRevision::from_raw(5).expect("revision"),
                DefinitionResultId::from_raw(3),
                Arc::from([second]),
            )
            .expect("batch"),
        )
        .expect("second acknowledgement");

    let merged = mailbox.take_acknowledgement().expect("merged batch");
    assert_eq!(merged.observed_revision.get(), 5);
    assert_eq!(merged.definition, DefinitionResultId::from_raw(3));
    assert_eq!(merged.notices.as_ref(), &[second, first]);
}

#[test]
fn acknowledgement_merge_replaces_old_process_and_rejects_stale_process_replay() {
    let (mailbox, _wake) = CommandMailbox::channel();
    let first_generation = ProcessGeneration::from_raw(1).expect("first process");
    let second_generation = ProcessGeneration::from_raw(2).expect("second process");
    let current_notice = NoticeId::from_raw(7).expect("current notice");
    mailbox
        .acknowledge_items(
            AcknowledgementBatch::new(
                first_generation,
                SnapshotRevision::from_raw(40).expect("old revision"),
                DefinitionResultId::from_raw(9),
                Arc::from([NoticeId::limit()]),
            )
            .expect("old batch"),
        )
        .expect("old acknowledgement");
    mailbox
        .acknowledge_items(
            AcknowledgementBatch::new(
                second_generation,
                SnapshotRevision::from_raw(2).expect("current revision"),
                None,
                Arc::from([current_notice]),
            )
            .expect("current batch"),
        )
        .expect("current acknowledgement");
    mailbox
        .acknowledge_items(
            AcknowledgementBatch::new(
                first_generation,
                SnapshotRevision::from_raw(99).expect("stale replay revision"),
                DefinitionResultId::from_raw(11),
                Arc::from([NoticeId::limit()]),
            )
            .expect("stale replay batch"),
        )
        .expect("stale replay acknowledgement");

    let retained = mailbox
        .take_acknowledgement()
        .expect("current batch retained");
    assert_eq!(retained.process_generation, second_generation);
    assert_eq!(retained.observed_revision.get(), 2);
    assert_eq!(retained.definition, None);
    assert_eq!(retained.notices.as_ref(), &[current_notice]);
}

#[test]
fn early_caret_is_replayed_before_its_definition_request() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    let caret_generation = CaretGeneration::from_raw(1).expect("caret");
    mailbox
        .submit_document(Some(DesiredDocument {
            stamp: stamp(1),
            text: Arc::from("var value = 1;"),
        }))
        .expect("document");
    mailbox
        .submit_caret(CaretCommand::Current(CaretIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            stamp: stamp(1),
            generation: caret_generation,
            primary_character: Some(4),
        }))
        .expect("caret");
    mailbox
        .request_definition(DefinitionIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
            stamp: stamp(1),
            caret_generation,
            caret_character: 4,
        })
        .expect("definition");

    make_written(&mut core, &handle, &mailbox);
    let (_, _, definition_id) = handle.last_frame(RequestKind::Definition);
    assert_eq!(definition_id.get(), 2);
}

#[test]
fn equal_time_launch_completion_beats_the_launch_deadline() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    handle.push(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    core.run_epoch(ManualTick::from_raw(5_000), &mailbox);

    assert!(handle.actions().iter().any(|action| {
        matches!(
            action,
            FakeAction::Effect(ActorEffect::SendFrame {
                kind: FrameKind::ClientRequest {
                    kind: RequestKind::Initialize,
                    ..
                },
                ..
            })
        )
    }));
    assert!(
        !handle.actions().iter().any(|action| {
            matches!(action, FakeAction::Effect(ActorEffect::CancelLaunch { .. }))
        })
    );
}

#[test]
fn writer_and_response_deadlines_are_exact_virtual_ticks() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    handle.push(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::Ready,
    });
    let writing = core.run_epoch(ManualTick::from_raw(0), &mailbox);
    assert_eq!(writing.next_deadline, Some(ManualTick::from_raw(2_000)));
    let (_, sequence, _) = handle.last_frame(RequestKind::Initialize);
    handle.push(ActorEvent::WriterFinished {
        generation,
        sequence,
        outcome: WriterOutcome::Flushed,
    });
    let awaiting = core.run_epoch(ManualTick::from_raw(0), &mailbox);
    assert_eq!(awaiting.next_deadline, Some(ManualTick::from_raw(10_000)));
    core.run_epoch(ManualTick::from_raw(10_000), &mailbox);
    assert!(handle.actions().iter().any(|action| {
        matches!(
            action,
            FakeAction::Effect(ActorEffect::BeginCleanup { generation: active, .. })
                if *active == generation
        )
    }));
}

#[test]
fn no_owner_launch_failure_retries_at_the_exact_backoff_tick() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    let generation = ProcessGeneration::from_raw(1).expect("generation");
    handle.push(ActorEvent::LaunchFinished {
        generation,
        outcome: LaunchOutcome::FailedBeforeOwnership {
            cause: LaunchFailure::MissingSibling,
        },
    });
    let failed = core.run_epoch(ManualTick::from_raw(0), &mailbox);
    assert_eq!(failed.next_deadline, Some(ManualTick::from_raw(250)));
    core.run_epoch(ManualTick::from_raw(249), &mailbox);
    assert_eq!(
        handle
            .actions()
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    FakeAction::Effect(ActorEffect::LaunchGeneration { .. })
                )
            })
            .count(),
        1
    );
    core.run_epoch(ManualTick::from_raw(250), &mailbox);
    assert_eq!(
        handle
            .actions()
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    FakeAction::Effect(ActorEffect::LaunchGeneration { .. })
                )
            })
            .count(),
        2
    );
}

#[test]
fn drop_signals_and_detaches_without_joining_the_actor_thread() {
    let (_publisher, subscriber) = initial_snapshot_channel();
    let (mailbox, wake) = CommandMailbox::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (worker_done_sender, worker_done_receiver) = mpsc::channel();
    let actor_thread = thread::spawn(move || {
        let _ = release_receiver.recv();
        let _ = worker_done_sender.send(());
    });
    let coordinator =
        LanguageCoordinator::from_parts(Arc::clone(&mailbox), subscriber, Some(actor_thread));
    let (drop_done_sender, drop_done_receiver) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(coordinator);
        let _ = drop_done_sender.send(());
    });

    wake.recv().expect("shutdown wake");
    let mut detached = false;
    for _ in 0..10_000 {
        if drop_done_receiver.try_recv().is_ok() {
            detached = true;
            break;
        }
        thread::yield_now();
    }
    release_sender.send(()).expect("release detached thread");
    worker_done_receiver.recv().expect("worker exit");
    dropper.join().expect("dropper");
    assert!(detached, "Drop must not join the coordinator thread");
    assert!(mailbox.shutdown_requested());
}

#[test]
fn startup_failure_snapshot_is_unavailable_and_repaints_once() {
    let (publisher, subscriber) = initial_snapshot_channel();
    let repaints = Arc::new(AtomicUsize::new(0));
    let repaint_count = Arc::clone(&repaints);
    let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        repaint_count.fetch_add(1, Ordering::AcqRel);
    });
    publish_unavailable(&publisher, &repaint);

    let snapshot = subscriber.load_stable();
    assert_eq!(snapshot.revision().get(), 2);
    assert_eq!(snapshot.status(), LanguageStatus::Unavailable);
    assert_eq!(repaints.load(Ordering::Acquire), 1);
}

#[derive(Clone, Copy)]
enum SubmissionKind {
    Document,
    Caret,
    Definition,
    Acknowledgement,
}

fn race_submission_against_closure(kind: SubmissionKind, shutdown: bool) {
    let (mailbox, _wake) = CommandMailbox::channel();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let hook_entered = Arc::clone(&entered);
    let hook_release = Arc::clone(&release);
    mailbox.test_set_before_store_hook(Some(Arc::new(move || {
        hook_entered.wait();
        hook_release.wait();
    })));
    let producer_mailbox = Arc::clone(&mailbox);
    let producer = thread::spawn(move || match kind {
        SubmissionKind::Document => producer_mailbox.submit_document(Some(DesiredDocument {
            stamp: stamp(1),
            text: Arc::from("var value = 1;"),
        })),
        SubmissionKind::Caret => producer_mailbox.submit_caret(CaretCommand::Exhausted {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            stamp: stamp(1),
        }),
        SubmissionKind::Definition => producer_mailbox.request_definition(DefinitionIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
            stamp: stamp(1),
            caret_generation: CaretGeneration::from_raw(1).expect("caret"),
            caret_character: 1,
        }),
        SubmissionKind::Acknowledgement => producer_mailbox.acknowledge_items(
            AcknowledgementBatch::new(
                ProcessGeneration::from_raw(1).expect("process"),
                SnapshotRevision::from_raw(1).expect("revision"),
                None,
                Arc::from([]),
            )
            .expect("batch"),
        ),
    });

    entered.wait();
    if shutdown {
        mailbox.request_shutdown();
    } else {
        mailbox.close();
    }
    release.wait();
    assert_eq!(producer.join().expect("producer"), Err(LanguageClosed));
    assert!(mailbox.take_document().is_none());
    assert!(mailbox.take_caret().is_none());
    assert!(mailbox.take_definition().is_none());
    assert!(mailbox.take_acknowledgement().is_none());
}

#[test]
fn every_submission_lane_rejects_and_clears_a_shutdown_or_close_race() {
    for kind in [
        SubmissionKind::Document,
        SubmissionKind::Caret,
        SubmissionKind::Definition,
        SubmissionKind::Acknowledgement,
    ] {
        race_submission_against_closure(kind, true);
        race_submission_against_closure(kind, false);
    }
}

#[test]
fn deadline_wait_uses_the_post_epoch_time_sample() {
    let deadline = ManualTick::from_raw(5_000);
    assert_eq!(
        duration_until(deadline, ManualTick::from_raw(4_999)),
        Some(Duration::from_millis(1))
    );
    assert_eq!(duration_until(deadline, ManualTick::from_raw(5_000)), None);
    assert_eq!(duration_until(deadline, ManualTick::from_raw(5_001)), None);
}

#[test]
fn old_exhaustion_is_ignored_and_fresh_current_one_is_accepted_in_generation_two() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    let first = ProcessGeneration::from_raw(1).expect("first generation");
    let second = ProcessGeneration::from_raw(2).expect("second generation");
    mailbox
        .submit_document(Some(DesiredDocument {
            stamp: stamp(1),
            text: Arc::from("var value = 1;"),
        }))
        .expect("document");
    mailbox
        .submit_caret(CaretCommand::Exhausted {
            process_generation: first,
            stamp: stamp(1),
        })
        .expect("exhausted caret");
    assert_eq!(make_written(&mut core, &handle, &mailbox), first);
    handle.push(ActorEvent::ReaderFatal {
        generation: first,
        cause: ReaderFatalCause::Io,
    });
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    handle.push(ActorEvent::FinalizedGeneration {
        generation: first,
        stderr_tail: None,
    });
    core.run_epoch(ManualTick::from_raw(250), &mailbox);
    finish_launched_generation(
        &mut core,
        &handle,
        &mailbox,
        second,
        ManualTick::from_raw(250),
    );
    complete_semantic_request(
        &mut core,
        &handle,
        &mailbox,
        second,
        ManualTick::from_raw(250),
    );

    let current_caret = CaretGeneration::from_raw(1).expect("fresh caret");
    mailbox
        .submit_caret(CaretCommand::Current(CaretIntent {
            process_generation: second,
            stamp: stamp(1),
            generation: current_caret,
            primary_character: Some(4),
        }))
        .expect("current caret");
    mailbox
        .request_definition(DefinitionIntent {
            process_generation: second,
            navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
            stamp: stamp(1),
            caret_generation: current_caret,
            caret_character: 4,
        })
        .expect("definition");
    core.run_epoch(ManualTick::from_raw(250), &mailbox);
    let (definition_generation, _, _) = handle.last_frame(RequestKind::Definition);
    assert_eq!(definition_generation, second);
}

#[test]
fn old_high_current_does_not_poison_fresh_current_one_in_generation_two() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    let first = ProcessGeneration::from_raw(1).expect("first generation");
    let second = ProcessGeneration::from_raw(2).expect("second generation");
    mailbox
        .submit_document(Some(DesiredDocument {
            stamp: stamp(1),
            text: Arc::from("var value = 1;"),
        }))
        .expect("document");
    mailbox
        .submit_caret(CaretCommand::Current(CaretIntent {
            process_generation: first,
            stamp: stamp(1),
            generation: CaretGeneration::from_raw(100).expect("old high caret"),
            primary_character: Some(4),
        }))
        .expect("old caret");
    assert_eq!(make_written(&mut core, &handle, &mailbox), first);
    handle.push(ActorEvent::ReaderFatal {
        generation: first,
        cause: ReaderFatalCause::Io,
    });
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    handle.push(ActorEvent::FinalizedGeneration {
        generation: first,
        stderr_tail: None,
    });
    core.run_epoch(ManualTick::from_raw(250), &mailbox);
    finish_launched_generation(
        &mut core,
        &handle,
        &mailbox,
        second,
        ManualTick::from_raw(250),
    );
    complete_semantic_request(
        &mut core,
        &handle,
        &mailbox,
        second,
        ManualTick::from_raw(250),
    );

    let fresh_caret = CaretGeneration::from_raw(1).expect("fresh caret");
    mailbox
        .submit_caret(CaretCommand::Current(CaretIntent {
            process_generation: second,
            stamp: stamp(1),
            generation: fresh_caret,
            primary_character: Some(4),
        }))
        .expect("fresh caret");
    mailbox
        .request_definition(DefinitionIntent {
            process_generation: second,
            navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
            stamp: stamp(1),
            caret_generation: fresh_caret,
            caret_character: 4,
        })
        .expect("definition");
    core.run_epoch(ManualTick::from_raw(250), &mailbox);
    let (definition_generation, _, _) = handle.last_frame(RequestKind::Definition);
    assert_eq!(definition_generation, second);
}

#[test]
fn definition_ahead_of_the_retained_caret_waits_for_its_matching_caret() {
    let (mut core, handle, mailbox, _subscriber) = core_fixture();
    mailbox
        .submit_document(Some(DesiredDocument {
            stamp: stamp(1),
            text: Arc::from("var value = 1;"),
        }))
        .expect("document");
    mailbox
        .submit_caret(CaretCommand::Current(CaretIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            stamp: stamp(1),
            generation: CaretGeneration::from_raw(1).expect("old caret"),
            primary_character: Some(4),
        }))
        .expect("old caret");
    let generation = make_written(&mut core, &handle, &mailbox);
    let (_, semantic_sequence, semantic_id) = handle.last_frame(RequestKind::SemanticTokens);
    handle.push(ActorEvent::WriterFinished {
        generation,
        sequence: semantic_sequence,
        outcome: WriterOutcome::Flushed,
    });
    handle.push(ActorEvent::ReaderMessage {
        generation,
        message: rpc(json!({
            "jsonrpc":"2.0",
            "id":semantic_id.get(),
            "result":{"data":[]}
        })),
    });
    core.run_epoch(ManualTick::from_raw(0), &mailbox);

    let new_caret = CaretGeneration::from_raw(2).expect("new caret");
    mailbox
        .request_definition(DefinitionIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            navigation_generation: NavigationGeneration::from_raw(1).expect("navigation"),
            stamp: stamp(1),
            caret_generation: new_caret,
            caret_character: 8,
        })
        .expect("early definition");
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    assert!(!handle.actions().iter().any(|action| {
        matches!(
            action,
            FakeAction::Effect(ActorEffect::SendFrame {
                kind: FrameKind::ClientRequest {
                    kind: RequestKind::Definition,
                    ..
                },
                ..
            })
        )
    }));

    mailbox
        .submit_caret(CaretCommand::Current(CaretIntent {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            stamp: stamp(1),
            generation: new_caret,
            primary_character: Some(8),
        }))
        .expect("matching caret");
    core.run_epoch(ManualTick::from_raw(0), &mailbox);
    let (_, _, definition_id) = handle.last_frame(RequestKind::Definition);
    assert_eq!(definition_id.get(), 3);
}

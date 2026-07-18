use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use oxide_ide::{
    ClosureHealth, SubmitError, SupervisorCommand, SupervisorCommandKind, SupervisorConfig,
    SupervisorEvent, WireDocument, WorkerEvent, WorkerSupervisor,
};
use rlox::{RevisionId, SourceId};

fn config() -> SupervisorConfig {
    SupervisorConfig {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_oxide-ide")),
        handshake_timeout: Duration::from_secs(5),
        stop_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(5),
    }
}

fn next_event(supervisor: &WorkerSupervisor) -> SupervisorEvent {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = supervisor.try_recv().expect("poll supervisor") {
            return event;
        }
        assert!(Instant::now() < deadline, "supervisor event timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_empty_or_debug_trace(stderr: &oxide_ide::StderrSummary) {
    if stderr.total_bytes == 0 {
        return;
    }
    assert!(!stderr.truncated);
    let trace = std::str::from_utf8(&stderr.retained).expect("debug trace is UTF-8");
    assert!(trace.contains("OP_"), "unexpected worker stderr: {trace}");
}

#[test]
fn supervisor_runs_worker_without_blocking_the_caller() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    sender
        .try_send(SupervisorCommand::LoadAndRun(WireDocument {
            source_id: SourceId(31),
            revision: RevisionId(12),
            display_name: "supervisor.lox".to_string(),
            text: "print \"supervised\";\n".to_string(),
        }))
        .expect("queue run");

    let mut saw_hello = false;
    let mut saw_output = false;
    let mut saw_completed = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) => match envelope.payload {
                WorkerEvent::Hello => saw_hello = true,
                WorkerEvent::Output { text } => {
                    assert_eq!(text, "supervised\n");
                    saw_output = true;
                }
                WorkerEvent::Completed => saw_completed = true,
                other => panic!("unexpected worker event: {other:?}"),
            },
            SupervisorEvent::Closed {
                status,
                stderr,
                health,
            } => {
                assert_eq!(status, Some(0));
                assert_empty_or_debug_trace(&stderr);
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
            SupervisorEvent::SubmissionRejected { command, error } => {
                panic!("submission rejected unexpectedly: {command:?}: {error:?}")
            }
        }
    }
    assert!(saw_hello && saw_output && saw_completed);
}

#[test]
fn concurrently_admitted_second_load_is_explicitly_rejected() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    sender
        .try_send(SupervisorCommand::LoadAndRun(WireDocument {
            source_id: SourceId(41),
            revision: RevisionId(21),
            display_name: "first.lox".to_string(),
            text: "print \"first\";\nwhile (true) {}\n".to_string(),
        }))
        .expect("queue first run");
    let second = sender.try_send(SupervisorCommand::LoadAndRun(WireDocument {
        source_id: SourceId(42),
        revision: RevisionId(22),
        display_name: "second.lox".to_string(),
        text: "print \"second\";\n".to_string(),
    }));
    assert!(matches!(second, Ok(()) | Err(SubmitError::InvalidState)));

    let mut output = String::new();
    let mut rejected = false;
    let mut stopped = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) => {
                if let WorkerEvent::Output { text } = envelope.payload {
                    output.push_str(&text);
                    if !stopped {
                        sender
                            .try_send(SupervisorCommand::Stop)
                            .expect("stop first run");
                        stopped = true;
                    }
                }
            }
            SupervisorEvent::SubmissionRejected { command, error } => {
                assert_eq!(command, SupervisorCommandKind::LoadAndRun);
                assert_eq!(error, SubmitError::InvalidState);
                rejected = true;
            }
            SupervisorEvent::Closed { health, .. } => {
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
        }
    }
    assert_eq!(output, "first\n");
    assert_eq!(rejected, second.is_ok());
}

#[test]
fn supervisor_drives_debug_steps_and_stop() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    sender
        .try_send(SupervisorCommand::LoadAndDebug(WireDocument {
            source_id: SourceId(32),
            revision: RevisionId(13),
            display_name: "supervisor-debug.lox".to_string(),
            text: "var value = 1;\nprint value;\n".to_string(),
        }))
        .expect("queue debug run");

    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Paused { .. })
        {
            break;
        }
    }
    sender
        .try_send(SupervisorCommand::StepInto)
        .expect("queue step");
    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Paused { .. })
        {
            break;
        }
    }
    sender
        .try_send(SupervisorCommand::Stop)
        .expect("queue stop");

    let mut cancelled = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) => {
                cancelled |= matches!(envelope.payload, WorkerEvent::Cancelled { .. });
            }
            SupervisorEvent::Closed { health, .. } => {
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
            SupervisorEvent::SubmissionRejected { command, error } => {
                panic!("submission rejected unexpectedly: {command:?}: {error:?}")
            }
        }
    }
    assert!(cancelled);
}

#[test]
fn supervisor_stops_an_infinite_run_and_reaps_it() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    sender
        .try_send(SupervisorCommand::LoadAndRun(WireDocument {
            source_id: SourceId(33),
            revision: RevisionId(14),
            display_name: "supervisor-stop.lox".to_string(),
            text: "print \"started\";\nwhile (true) { var value = 1; }\n".to_string(),
        }))
        .expect("queue run");

    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Output { .. })
        {
            break;
        }
    }
    sender
        .try_send(SupervisorCommand::Stop)
        .expect("queue stop");

    let mut cancelled = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) => {
                cancelled |= matches!(envelope.payload, WorkerEvent::Cancelled { .. });
            }
            SupervisorEvent::Closed { health, .. } => {
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
            SupervisorEvent::SubmissionRejected { command, error } => {
                panic!("submission rejected unexpectedly: {command:?}: {error:?}")
            }
        }
    }
    assert!(cancelled);
}

#[test]
fn supervisor_can_close_before_loading_a_program() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && envelope.payload == WorkerEvent::Hello
        {
            break;
        }
    }
    supervisor.close().expect("request close");
    match next_event(&supervisor) {
        SupervisorEvent::Closed { health, stderr, .. } => {
            assert_eq!(health, ClosureHealth::Clean);
            assert_eq!(stderr.total_bytes, 0);
        }
        SupervisorEvent::WorkerTerminated { reason, .. } => {
            panic!("worker terminated unexpectedly: {reason:?}")
        }
        SupervisorEvent::Worker(envelope) => {
            panic!("unexpected event after pre-run close: {envelope:?}")
        }
        SupervisorEvent::SubmissionRejected { command, error } => {
            panic!("submission rejected unexpectedly: {command:?}: {error:?}")
        }
    }
}

#[test]
fn supervisor_can_close_during_worker_boot_without_running_queued_work() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    supervisor
        .command_sender()
        .try_send(SupervisorCommand::LoadAndRun(WireDocument {
            source_id: SourceId(35),
            revision: RevisionId(16),
            display_name: "must-not-run.lox".to_string(),
            text: "print \"must not run\";\n".to_string(),
        }))
        .expect("queue boot-time run");
    supervisor.close().expect("request close during boot");

    let mut saw_hello = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) if envelope.payload == WorkerEvent::Hello => {
                saw_hello = true;
            }
            SupervisorEvent::Closed { health, stderr, .. } => {
                assert!(saw_hello);
                assert_eq!(health, ClosureHealth::Clean);
                assert_eq!(stderr.total_bytes, 0);
                break;
            }
            SupervisorEvent::Worker(envelope) => {
                panic!("queued work ran after close: {envelope:?}")
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
            SupervisorEvent::SubmissionRejected { command, error } => {
                panic!("submission rejected unexpectedly: {command:?}: {error:?}")
            }
        }
    }
}

#[test]
fn close_is_nonblocking_while_the_actor_cancels_and_reaps() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    supervisor
        .command_sender()
        .try_send(SupervisorCommand::LoadAndRun(WireDocument {
            source_id: SourceId(34),
            revision: RevisionId(15),
            display_name: "supervisor-close.lox".to_string(),
            text: "print \"started\";\nwhile (true) { var value = 1; }\n".to_string(),
        }))
        .expect("queue run");
    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Output { .. })
        {
            break;
        }
    }

    let started = Instant::now();
    supervisor.close().expect("request close");
    assert!(started.elapsed() < Duration::from_millis(100));

    let mut cancelled = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) => {
                cancelled |= matches!(envelope.payload, WorkerEvent::Cancelled { .. });
            }
            SupervisorEvent::Closed { health, .. } => {
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
            SupervisorEvent::SubmissionRejected { command, error } => {
                panic!("submission rejected unexpectedly: {command:?}: {error:?}")
            }
        }
    }
    assert!(cancelled);
}

#[test]
fn launch_rejects_missing_executable_and_zero_deadline() {
    let mut missing = config();
    missing.executable = PathBuf::from("/definitely/missing/oxide-ide");
    assert!(WorkerSupervisor::launch(missing).is_err());

    let mut invalid = config();
    invalid.stop_timeout = Duration::ZERO;
    assert!(WorkerSupervisor::launch(invalid).is_err());
}

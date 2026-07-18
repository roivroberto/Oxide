use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use oxide_ide::{
    ClosureHealth, SubmitError, SupervisorCommand, SupervisorCommandKind, SupervisorConfig,
    SupervisorEvent, SupervisorRunMode, SupervisorSource, SupervisorSubmissionId,
    WorkerCommandSender, WorkerEvent, WorkerSupervisor,
};

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

fn start(mode: SupervisorRunMode, display_name: &str, text: &str) -> SupervisorCommand {
    SupervisorCommand::Start {
        mode,
        source: SupervisorSource {
            display_name: display_name.to_string(),
            text: text.to_string(),
        },
    }
}

fn submit(
    sender: &WorkerCommandSender,
    submission_id: u64,
    command: SupervisorCommand,
) -> Result<(), SubmitError> {
    sender.try_send(SupervisorSubmissionId(submission_id), command)
}

#[test]
fn supervisor_runs_worker_without_blocking_the_caller() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    submit(
        &sender,
        1,
        start(
            SupervisorRunMode::Run,
            "supervisor.lox",
            "print \"supervised\";\n",
        ),
    )
    .expect("queue run");

    let mut saw_hello = false;
    let mut saw_started = false;
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
                ..
            } => {
                assert_eq!(status, Some(0));
                assert_empty_or_debug_trace(&stderr);
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
            SupervisorEvent::Started { submission_id, .. } => {
                assert_eq!(submission_id, SupervisorSubmissionId(1));
                saw_started = true;
            }
            SupervisorEvent::SubmissionAccepted { command, .. } => {
                panic!("unexpected control admission: {command:?}")
            }
            SupervisorEvent::CloseAccepted { .. } => panic!("unexpected close admission"),
            SupervisorEvent::SubmissionRejected { command, error, .. } => {
                panic!("submission rejected unexpectedly: {command:?}: {error:?}")
            }
        }
    }
    assert!(saw_hello && saw_started && saw_output && saw_completed);
}

#[test]
fn concurrently_admitted_second_load_is_explicitly_rejected() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    submit(
        &sender,
        1,
        start(
            SupervisorRunMode::Run,
            "first.lox",
            "print \"first\";\nwhile (true) {}\n",
        ),
    )
    .expect("queue first run");
    let second = submit(
        &sender,
        2,
        start(SupervisorRunMode::Run, "second.lox", "print \"second\";\n"),
    );
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
                        submit(&sender, 3, SupervisorCommand::Stop).expect("stop first run");
                        stopped = true;
                    }
                }
            }
            SupervisorEvent::SubmissionRejected {
                submission_id,
                command,
                error,
            } => {
                assert_eq!(submission_id, SupervisorSubmissionId(2));
                assert_eq!(command, SupervisorCommandKind::Start);
                assert_eq!(error, SubmitError::InvalidState);
                rejected = true;
            }
            SupervisorEvent::Started { .. }
            | SupervisorEvent::SubmissionAccepted { .. }
            | SupervisorEvent::CloseAccepted { .. } => {}
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
    submit(
        &sender,
        1,
        start(
            SupervisorRunMode::Debug,
            "supervisor-debug.lox",
            "var value = 1;\nprint value;\n",
        ),
    )
    .expect("queue debug run");

    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Paused { .. })
        {
            break;
        }
    }
    submit(&sender, 2, SupervisorCommand::StepInto).expect("queue step");
    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Paused { .. })
        {
            break;
        }
    }
    submit(&sender, 3, SupervisorCommand::Stop).expect("queue stop");

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
            SupervisorEvent::Started { .. }
            | SupervisorEvent::SubmissionAccepted { .. }
            | SupervisorEvent::CloseAccepted { .. } => {}
            SupervisorEvent::SubmissionRejected { command, error, .. } => {
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
    submit(
        &sender,
        1,
        start(
            SupervisorRunMode::Run,
            "supervisor-stop.lox",
            "print \"started\";\nwhile (true) { var value = 1; }\n",
        ),
    )
    .expect("queue run");

    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Output { .. })
        {
            break;
        }
    }
    submit(&sender, 2, SupervisorCommand::Stop).expect("queue stop");

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
            SupervisorEvent::Started { .. }
            | SupervisorEvent::SubmissionAccepted { .. }
            | SupervisorEvent::CloseAccepted { .. } => {}
            SupervisorEvent::SubmissionRejected { command, error, .. } => {
                panic!("submission rejected unexpectedly: {command:?}: {error:?}")
            }
        }
    }
    assert!(cancelled);
}

#[test]
fn worker_waits_for_load_after_hello_and_can_close_cleanly() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && envelope.payload == WorkerEvent::Hello
        {
            break;
        }
    }
    let quiet_deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < quiet_deadline {
        assert!(
            supervisor
                .try_recv()
                .expect("poll idle worker after hello")
                .is_none(),
            "worker advanced before a load command was admitted"
        );
        thread::sleep(Duration::from_millis(5));
    }
    supervisor.close().expect("request close");
    match next_event(&supervisor) {
        SupervisorEvent::Closed {
            run,
            health,
            stderr,
            ..
        } => {
            assert_eq!(run, None);
            assert_eq!(health, ClosureHealth::Clean);
            assert_eq!(stderr.total_bytes, 0);
        }
        SupervisorEvent::WorkerTerminated { reason, .. } => {
            panic!("worker terminated unexpectedly: {reason:?}")
        }
        SupervisorEvent::Worker(envelope) => {
            panic!("unexpected event after pre-run close: {envelope:?}")
        }
        SupervisorEvent::Started { .. }
        | SupervisorEvent::SubmissionAccepted { .. }
        | SupervisorEvent::CloseAccepted { .. } => {
            panic!("unexpected admission after pre-run close")
        }
        SupervisorEvent::SubmissionRejected { command, error, .. } => {
            panic!("submission rejected unexpectedly: {command:?}: {error:?}")
        }
    }
}

#[test]
fn closed_event_is_a_finalized_replacement_barrier() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && envelope.payload == WorkerEvent::Hello
        {
            break;
        }
    }
    supervisor.close().expect("request close");
    assert!(matches!(
        next_event(&supervisor),
        SupervisorEvent::Closed {
            health: ClosureHealth::Clean,
            ..
        }
    ));
    assert_eq!(
        submit(
            &sender,
            1,
            start(SupervisorRunMode::Run, "late.lox", "print 1;\n")
        ),
        Err(SubmitError::Closed)
    );
    assert!(supervisor.try_recv().expect("poll after close").is_none());
}

#[test]
fn supervisor_can_close_during_worker_boot_without_running_queued_work() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    submit(
        &sender,
        1,
        start(
            SupervisorRunMode::Run,
            "must-not-run.lox",
            "print \"must not run\";\n",
        ),
    )
    .expect("queue boot-time run");
    supervisor.close().expect("request close during boot");

    let mut saw_hello = false;
    let mut saw_rejection = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) if envelope.payload == WorkerEvent::Hello => {
                saw_hello = true;
            }
            SupervisorEvent::Closed { health, stderr, .. } => {
                assert!(saw_hello);
                assert!(saw_rejection);
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
            SupervisorEvent::SubmissionRejected {
                submission_id,
                command,
                error,
            } => {
                assert_eq!(submission_id, SupervisorSubmissionId(1));
                assert_eq!(command, SupervisorCommandKind::Start);
                assert_eq!(error, SubmitError::Closed);
                saw_rejection = true;
            }
            SupervisorEvent::Started { .. }
            | SupervisorEvent::SubmissionAccepted { .. }
            | SupervisorEvent::CloseAccepted { .. } => {
                panic!("queued work was admitted after close")
            }
        }
    }
}

#[test]
fn close_is_nonblocking_while_the_actor_cancels_and_reaps() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    submit(
        &sender,
        1,
        start(
            SupervisorRunMode::Run,
            "supervisor-close.lox",
            "print \"started\";\nwhile (true) { var value = 1; }\n",
        ),
    )
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

    let mut close_admission: Option<(
        oxide_ide::SupervisorRun,
        oxide_ide::RequestId,
        oxide_ide::EventSequence,
    )> = None;
    let mut cancelled = false;
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) => {
                if matches!(envelope.payload, WorkerEvent::Cancelled { .. }) {
                    let Some((run, request_id, next_event_sequence)) = close_admission else {
                        panic!("cancellation preceded close admission");
                    };
                    assert_eq!(envelope.worker_session_id, run.worker_session_id);
                    assert_eq!(envelope.run_id, run.run_id);
                    assert_eq!(envelope.source_revision, run.source_revision);
                    assert_eq!(envelope.request_id, request_id);
                    assert_eq!(envelope.sequence, next_event_sequence);
                    cancelled = true;
                }
            }
            SupervisorEvent::CloseAccepted {
                run,
                request_id,
                next_event_sequence,
            } => close_admission = Some((run, request_id, next_event_sequence)),
            SupervisorEvent::Closed { health, .. } => {
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            SupervisorEvent::WorkerTerminated { reason, .. } => {
                panic!("worker terminated unexpectedly: {reason:?}")
            }
            SupervisorEvent::Started { .. } | SupervisorEvent::SubmissionAccepted { .. } => {}
            SupervisorEvent::SubmissionRejected { command, error, .. } => {
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

#[test]
fn correlated_start_precedes_program_events_and_survives_close() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    let submission_id = SupervisorSubmissionId(7);
    sender
        .try_send(
            submission_id,
            SupervisorCommand::Start {
                mode: SupervisorRunMode::Run,
                source: SupervisorSource {
                    display_name: "correlated.lox".to_string(),
                    text: "print \"correlated\";\n".to_string(),
                },
            },
        )
        .expect("queue run");

    let hello = next_event(&supervisor);
    assert!(matches!(
        hello,
        SupervisorEvent::Worker(envelope) if envelope.payload == WorkerEvent::Hello
    ));

    let (run, request_id, next_event_sequence) = match next_event(&supervisor) {
        SupervisorEvent::Started {
            submission_id: actual_submission_id,
            mode,
            run,
            request_id,
            next_event_sequence,
        } => {
            assert_eq!(actual_submission_id, submission_id);
            assert_eq!(mode, SupervisorRunMode::Run);
            (run, request_id, next_event_sequence)
        }
        other => panic!("expected correlated start, got {other:?}"),
    };
    assert_eq!(run.worker_session_id, supervisor.worker_session_id());
    assert_ne!(run.source_id.0, 0);
    assert_eq!(run.run_id.0, 1);
    assert_eq!(run.source_revision.0, 1);
    assert_ne!(request_id.0, 0);
    assert_eq!(next_event_sequence.0, 2);

    let output = next_event(&supervisor);
    assert!(matches!(
        output,
        SupervisorEvent::Worker(envelope)
            if envelope.sequence == next_event_sequence
                && envelope.request_id == request_id
                && envelope.worker_session_id == run.worker_session_id
                && envelope.run_id == run.run_id
                && envelope.source_revision == run.source_revision
                && envelope.payload == WorkerEvent::Output {
                    text: "correlated\n".to_string()
                }
    ));

    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope)
                if matches!(envelope.payload, WorkerEvent::Completed) => {}
            SupervisorEvent::Closed {
                run: closed_run,
                health,
                ..
            } => {
                assert_eq!(closed_run, Some(run));
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            other => panic!("unexpected event after output: {other:?}"),
        }
    }
}

#[test]
fn public_control_admission_is_correlated_before_its_worker_event() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    sender
        .try_send(
            SupervisorSubmissionId(1),
            SupervisorCommand::Start {
                mode: SupervisorRunMode::Debug,
                source: SupervisorSource {
                    display_name: "control.lox".to_string(),
                    text: "var value = 1;\nprint value;\n".to_string(),
                },
            },
        )
        .expect("queue debug run");

    let run = loop {
        if let SupervisorEvent::Started { run, .. } = next_event(&supervisor) {
            break run;
        }
    };
    loop {
        if let SupervisorEvent::Worker(envelope) = next_event(&supervisor)
            && matches!(envelope.payload, WorkerEvent::Paused { .. })
        {
            break;
        }
    }

    let step_submission = SupervisorSubmissionId(2);
    sender
        .try_send(step_submission, SupervisorCommand::StepInto)
        .expect("queue step");
    let (step_request, step_sequence) = match next_event(&supervisor) {
        SupervisorEvent::SubmissionAccepted {
            submission_id,
            command,
            run: accepted_run,
            request_id,
            next_event_sequence,
        } => {
            assert_eq!(submission_id, step_submission);
            assert_eq!(command, SupervisorCommandKind::StepInto);
            assert_eq!(accepted_run, run);
            (request_id, next_event_sequence)
        }
        other => panic!("expected step admission, got {other:?}"),
    };
    match next_event(&supervisor) {
        SupervisorEvent::Worker(envelope) => {
            assert_eq!(envelope.request_id, step_request);
            assert_eq!(envelope.sequence, step_sequence);
            assert!(matches!(envelope.payload, WorkerEvent::Paused { .. }));
        }
        other => panic!("expected step pause, got {other:?}"),
    }

    let stop_submission = SupervisorSubmissionId(3);
    sender
        .try_send(stop_submission, SupervisorCommand::Stop)
        .expect("queue stop");
    let (stop_request, stop_sequence) = match next_event(&supervisor) {
        SupervisorEvent::SubmissionAccepted {
            submission_id,
            command,
            run: accepted_run,
            request_id,
            next_event_sequence,
        } => {
            assert_eq!(submission_id, stop_submission);
            assert_eq!(command, SupervisorCommandKind::Stop);
            assert_eq!(accepted_run, run);
            (request_id, next_event_sequence)
        }
        other => panic!("expected stop admission, got {other:?}"),
    };
    loop {
        match next_event(&supervisor) {
            SupervisorEvent::Worker(envelope) => {
                assert_eq!(envelope.request_id, stop_request);
                assert_eq!(envelope.sequence, stop_sequence);
                assert!(matches!(envelope.payload, WorkerEvent::Cancelled { .. }));
            }
            SupervisorEvent::Closed {
                run: closed_run,
                health,
                ..
            } => {
                assert_eq!(closed_run, Some(run));
                assert_eq!(health, ClosureHealth::Clean);
                break;
            }
            other => panic!("unexpected event while stopping: {other:?}"),
        }
    }
}

#[test]
fn submission_ids_reject_zero_duplicate_and_stale_values() {
    let supervisor = WorkerSupervisor::launch(config()).expect("launch supervisor");
    let sender = supervisor.command_sender();
    let start = || SupervisorCommand::Start {
        mode: SupervisorRunMode::Run,
        source: SupervisorSource {
            display_name: "ids.lox".to_string(),
            text: "while (true) {}\n".to_string(),
        },
    };

    assert_eq!(
        sender.try_send(SupervisorSubmissionId(0), start()),
        Err(SubmitError::InvalidSubmissionId)
    );
    sender
        .try_send(SupervisorSubmissionId(5), start())
        .expect("accept first nonzero ID");
    assert_eq!(
        sender.try_send(SupervisorSubmissionId(5), start()),
        Err(SubmitError::InvalidSubmissionId)
    );
    assert_eq!(
        sender.try_send(SupervisorSubmissionId(4), start()),
        Err(SubmitError::InvalidSubmissionId)
    );

    supervisor.close().expect("close supervisor");
    loop {
        if matches!(
            next_event(&supervisor),
            SupervisorEvent::Closed { .. } | SupervisorEvent::WorkerTerminated { .. }
        ) {
            break;
        }
    }
}

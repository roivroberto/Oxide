use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command as ProcessCommand, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxide_ide::{
    Command, Envelope, EventSequence, LineCodec, PROTOCOL_VERSION, RequestId, RunId, WireDocument,
    WorkerEvent, WorkerEventStreamValidator, WorkerSessionId,
};
use rlox::{PauseReason, RevisionId, SnapshotReason, SourceId};

type EventRead = Result<Option<Envelope<WorkerEvent>>, oxide_ide::DecodeError>;

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child is present")
    }

    fn wait(mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child_mut().try_wait().expect("query child status") {
                self.0.take();
                return status;
            }
            if Instant::now() >= deadline {
                panic!("worker did not exit before the test deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn command_envelope(
    session: WorkerSessionId,
    run: RunId,
    revision: RevisionId,
    request: u64,
    sequence: u64,
    payload: Command,
) -> Envelope<Command> {
    Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: session,
        run_id: run,
        source_revision: revision,
        request_id: RequestId(request),
        sequence: EventSequence(sequence),
        payload,
    }
}

fn receive_event(events: &Receiver<EventRead>) -> Envelope<WorkerEvent> {
    events
        .recv_timeout(Duration::from_secs(5))
        .expect("worker event before deadline")
        .expect("decode worker event")
        .expect("worker emits an event")
}

struct WorkerProcess {
    child: Option<ChildGuard>,
    stdin: Option<ChildStdin>,
    events: Receiver<EventRead>,
    event_validator: WorkerEventStreamValidator,
    write_codec: LineCodec,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<Vec<u8>>>,
    session: WorkerSessionId,
    next_request: u64,
    next_sequence: u64,
}

impl WorkerProcess {
    fn spawn(session: WorkerSessionId) -> Self {
        let session_text = session.0.to_string();
        let mut child = ChildGuard(Some(
            ProcessCommand::new(env!("CARGO_BIN_EXE_oxide-ide"))
                .args(["--worker", "--worker-session", &session_text])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn worker"),
        ));
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .expect("worker stdout is piped");
        let (event_tx, event_rx) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let mut codec = LineCodec::new();
            loop {
                let result = codec.read_worker_event(&mut stdout);
                let done = !matches!(result, Ok(Some(_)));
                if event_tx.send(result).is_err() || done {
                    break;
                }
            }
        });
        let mut stderr = child
            .child_mut()
            .stderr
            .take()
            .expect("worker stderr is piped");
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).expect("drain worker stderr");
            bytes
        });
        let stdin = child
            .child_mut()
            .stdin
            .take()
            .expect("worker stdin is piped");
        let mut worker = Self {
            child: Some(child),
            stdin: Some(stdin),
            events: event_rx,
            event_validator: WorkerEventStreamValidator::new(session).unwrap(),
            write_codec: LineCodec::new(),
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            session,
            next_request: 1,
            next_sequence: 1,
        };
        let hello = worker.receive();
        assert_eq!(hello.payload, WorkerEvent::Hello);
        worker
    }

    fn receive(&mut self) -> Envelope<WorkerEvent> {
        let event = receive_event(&self.events);
        self.event_validator
            .validate(&event)
            .expect("validate worker event");
        event
    }

    fn send(&mut self, run: RunId, revision: RevisionId, payload: Command) -> RequestId {
        let request = self.next_request;
        let sequence = self.next_sequence;
        self.next_request += 1;
        self.next_sequence += 1;
        let envelope = command_envelope(self.session, run, revision, request, sequence, payload);
        self.write_codec
            .write_command(
                self.stdin.as_mut().expect("worker stdin is open"),
                &envelope,
            )
            .expect("send worker command");
        RequestId(request)
    }

    fn finish_status(mut self) -> (ExitStatus, Vec<u8>) {
        drop(self.stdin.take());
        let status = self
            .child
            .take()
            .expect("child guard is present")
            .wait(Duration::from_secs(5));
        self.stdout_reader
            .take()
            .expect("stdout reader is present")
            .join()
            .expect("join worker stdout reader");
        let stderr = self
            .stderr_reader
            .take()
            .expect("stderr reader is present")
            .join()
            .expect("join worker stderr reader");
        (status, stderr)
    }

    fn finish(self) {
        let (status, stderr) = self.finish_status();
        assert!(status.success(), "worker exited with {status}");
        if !stderr.is_empty() {
            let trace = std::str::from_utf8(&stderr).expect("debug trace is UTF-8");
            assert!(
                trace.contains("OP_") || trace.contains("-- gc begin"),
                "unexpected worker stderr: {trace}"
            );
        }
    }
}

#[test]
fn handshake_run_output_terminal_and_shutdown() {
    let session = WorkerSessionId(41);
    let run = RunId(7);
    let revision = RevisionId(3);
    let mut worker = WorkerProcess::spawn(session);

    let load_request = worker.send(
        run,
        revision,
        Command::LoadAndRun {
            document: WireDocument {
                source_id: SourceId(9),
                revision,
                display_name: "handshake.lox".to_string(),
                text: "print \"hello\";\n".to_string(),
            },
        },
    );

    let output = worker.receive();
    assert_eq!(output.request_id, load_request);
    assert_eq!(
        output.payload,
        WorkerEvent::Output {
            text: "hello\n".to_string()
        }
    );

    let completed = worker.receive();
    assert_eq!(completed.request_id, load_request);
    assert_eq!(completed.payload, WorkerEvent::Completed);

    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn debug_step_continue_preserves_causal_requests() {
    let run = RunId(8);
    let revision = RevisionId(4);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(42));
    let load_request = worker.send(
        run,
        revision,
        Command::LoadAndDebug {
            document: WireDocument {
                source_id: SourceId(10),
                revision,
                display_name: "debug.lox".to_string(),
                text: "var value = 1;\nprint value;\n".to_string(),
            },
        },
    );

    let initial = worker.receive();
    assert_eq!(initial.request_id, load_request);
    let WorkerEvent::Paused { snapshot, .. } = initial.payload else {
        panic!("expected initial pause")
    };
    assert_eq!(
        snapshot.reason,
        SnapshotReason::Paused(PauseReason::DebugPoint)
    );

    let step_request = worker.send(run, revision, Command::StepInto);
    let stepped = worker.receive();
    assert_eq!(stepped.request_id, step_request);
    let WorkerEvent::Paused { snapshot, .. } = stepped.payload else {
        panic!("expected step pause")
    };
    assert_eq!(snapshot.reason, SnapshotReason::Paused(PauseReason::Step));

    let continue_request = worker.send(run, revision, Command::Continue);
    let mut saw_output = false;
    loop {
        let event = worker.receive();
        assert_eq!(event.request_id, continue_request);
        match event.payload {
            WorkerEvent::Output { text } => {
                assert_eq!(text, "1\n");
                saw_output = true;
            }
            WorkerEvent::Completed => break,
            other => panic!("unexpected event after Continue: {other:?}"),
        }
    }
    assert!(saw_output);
    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn debug_step_into_function_preserves_worker_and_frames() {
    let run = RunId(15);
    let revision = RevisionId(11);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(49));
    let source = "fun add(a, b) {\n  var total = a + b;\n  print total;\n}\n\nadd(2, 3);\n";
    worker.send(
        run,
        revision,
        Command::LoadAndDebug {
            document: WireDocument {
                source_id: SourceId(18),
                revision,
                display_name: "step-function.ox".to_string(),
                text: source.to_string(),
            },
        },
    );
    assert!(matches!(
        worker.receive().payload,
        WorkerEvent::Paused { .. }
    ));

    let step_over = worker.send(run, revision, Command::StepOver);
    let event = worker.receive();
    assert_eq!(event.request_id, step_over);
    assert!(matches!(event.payload, WorkerEvent::Paused { .. }));

    let step_into = worker.send(run, revision, Command::StepInto);
    let event = worker.receive();
    assert_eq!(event.request_id, step_into);
    let WorkerEvent::Paused { snapshot, .. } = event.payload else {
        panic!("expected a pause inside add")
    };
    assert!(
        snapshot.frames.iter().any(|frame| frame.function == "add"),
        "step into must expose the called frame"
    );

    let stop = worker.send(run, revision, Command::Stop);
    let event = worker.receive();
    assert_eq!(event.request_id, stop);
    assert!(matches!(event.payload, WorkerEvent::Cancelled { .. }));
    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn explicit_pause_then_stop_cancels_without_executing_paused_opcode() {
    let run = RunId(9);
    let revision = RevisionId(5);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(43));
    worker.send(
        run,
        revision,
        Command::LoadAndRun {
            document: WireDocument {
                source_id: SourceId(11),
                revision,
                display_name: "pause-stop.lox".to_string(),
                text: "print \"started\";\nwhile (true) { var value = 1; }\n".to_string(),
            },
        },
    );
    assert_eq!(
        worker.receive().payload,
        WorkerEvent::Output {
            text: "started\n".to_string()
        }
    );

    let pause_request = worker.send(run, revision, Command::Pause);
    let paused = worker.receive();
    assert_eq!(paused.request_id, pause_request);
    let WorkerEvent::Paused { snapshot, .. } = paused.payload else {
        panic!("expected explicit pause")
    };
    assert_eq!(
        snapshot.reason,
        SnapshotReason::Paused(PauseReason::Explicit)
    );

    let stop_request = worker.send(run, revision, Command::Stop);
    let cancelled = worker.receive();
    assert_eq!(cancelled.request_id, stop_request);
    assert!(matches!(cancelled.payload, WorkerEvent::Cancelled { .. }));
    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn compile_diagnostics_precede_the_fault_terminal() {
    let run = RunId(10);
    let revision = RevisionId(6);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(44));
    let load_request = worker.send(
        run,
        revision,
        Command::LoadAndRun {
            document: WireDocument {
                source_id: SourceId(12),
                revision,
                display_name: "compile-error.lox".to_string(),
                text: "var = 1;\n".to_string(),
            },
        },
    );
    let diagnostic = worker.receive();
    assert_eq!(diagnostic.request_id, load_request);
    assert!(matches!(diagnostic.payload, WorkerEvent::Diagnostic { .. }));
    let terminal = worker.receive();
    assert_eq!(terminal.request_id, load_request);
    assert!(matches!(terminal.payload, WorkerEvent::Faulted { .. }));
    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn illegal_commands_are_rejected_without_replacing_the_active_run() {
    let run = RunId(11);
    let revision = RevisionId(7);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(45));
    let pre_run_request = worker.send(RunId(0), RevisionId(0), Command::Pause);
    let pre_run_rejection = worker.receive();
    assert_eq!(pre_run_rejection.request_id, pre_run_request);
    assert!(matches!(
        pre_run_rejection.payload,
        WorkerEvent::CommandRejected { .. }
    ));

    worker.send(
        run,
        revision,
        Command::LoadAndDebug {
            document: WireDocument {
                source_id: SourceId(13),
                revision,
                display_name: "state.lox".to_string(),
                text: "print 1;\n".to_string(),
            },
        },
    );
    assert!(matches!(
        worker.receive().payload,
        WorkerEvent::Paused { .. }
    ));

    let attempted_run = RunId(99);
    let attempted_revision = RevisionId(88);
    let second_load = worker.send(
        attempted_run,
        attempted_revision,
        Command::LoadAndRun {
            document: WireDocument {
                source_id: SourceId(14),
                revision: attempted_revision,
                display_name: "other.lox".to_string(),
                text: "print 2;\n".to_string(),
            },
        },
    );
    let second_load_rejection = worker.receive();
    assert_eq!(second_load_rejection.run_id, attempted_run);
    assert_eq!(second_load_rejection.source_revision, attempted_revision);
    assert_eq!(second_load_rejection.request_id, second_load);
    assert!(matches!(
        second_load_rejection.payload,
        WorkerEvent::CommandRejected { .. }
    ));

    let input_request = worker.send(
        run,
        revision,
        Command::ProvideInput {
            in_reply_to: RequestId(1),
            text: "ignored".to_string(),
        },
    );
    let input_rejection = worker.receive();
    assert_eq!(input_rejection.request_id, input_request);
    assert!(matches!(
        input_rejection.payload,
        WorkerEvent::CommandRejected { ref code, .. }
            if code == "command.input_unsupported"
    ));

    let stop_request = worker.send(run, revision, Command::Stop);
    let cancelled = worker.receive();
    assert_eq!(cancelled.request_id, stop_request);
    assert!(matches!(cancelled.payload, WorkerEvent::Cancelled { .. }));
    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn output_budget_emits_one_marker_then_preserves_the_terminal() {
    let run = RunId(12);
    let revision = RevisionId(8);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(46));
    let value = "x".repeat(60 * 1024);
    worker.send(
        run,
        revision,
        Command::LoadAndRun {
            document: WireDocument {
                source_id: SourceId(15),
                revision,
                display_name: "output-budget.lox".to_string(),
                text: format!("while (true) print \"{value}\";\n"),
            },
        },
    );

    let mut output_events = 0;
    loop {
        match worker.receive().payload {
            WorkerEvent::Output { text } => {
                assert!(text.len() <= oxide_ide::MAX_OUTPUT_CHUNK_TEXT_BYTES);
                output_events += 1;
            }
            WorkerEvent::OutputTruncated => break,
            other => panic!("unexpected event before output truncation: {other:?}"),
        }
    }
    assert!(output_events > 1);

    let stop_request = worker.send(run, revision, Command::Stop);
    let terminal = worker.receive();
    assert_eq!(terminal.request_id, stop_request);
    assert!(matches!(terminal.payload, WorkerEvent::Cancelled { .. }));
    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn admitted_post_terminal_control_is_silent_and_shutdown_stays_contiguous() {
    let run = RunId(13);
    let revision = RevisionId(9);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(47));
    worker.send(
        run,
        revision,
        Command::LoadAndRun {
            document: WireDocument {
                source_id: SourceId(16),
                revision,
                display_name: "terminal-barrier.lox".to_string(),
                text: "print 1;\n".to_string(),
            },
        },
    );
    assert!(matches!(
        worker.receive().payload,
        WorkerEvent::Output { .. }
    ));
    assert_eq!(worker.receive().payload, WorkerEvent::Completed);

    worker.send(run, revision, Command::Pause);
    worker.send(run, revision, Command::Shutdown);
    worker.finish();
}

#[test]
fn malformed_live_command_fatally_stops_an_infinite_run() {
    let run = RunId(14);
    let revision = RevisionId(10);
    let mut worker = WorkerProcess::spawn(WorkerSessionId(48));
    worker.send(
        run,
        revision,
        Command::LoadAndRun {
            document: WireDocument {
                source_id: SourceId(17),
                revision,
                display_name: "fatal-protocol.lox".to_string(),
                text: "print \"started\";\nwhile (true) { var value = 1; }\n".to_string(),
            },
        },
    );
    assert!(matches!(
        worker.receive().payload,
        WorkerEvent::Output { .. }
    ));
    worker
        .stdin
        .as_mut()
        .expect("worker stdin is open")
        .write_all(b"{}\n")
        .expect("write malformed command");

    let (status, _stderr) = worker.finish_status();
    assert!(
        !status.success(),
        "fatal protocol misuse exited successfully"
    );
}

#[test]
fn worker_bootstrap_rejects_noncanonical_or_incomplete_arguments() {
    let invalid: &[&[&str]] = &[
        &["--worker"],
        &["--worker", "--worker-session"],
        &["--worker", "--worker-session", "0"],
        &["--worker", "--worker-session", "01"],
        &["--worker", "--worker-session", "+1"],
        &["--worker", "--worker-session", "nope"],
        &["--worker", "--worker-session", "1", "extra"],
    ];
    for arguments in invalid {
        let output = ProcessCommand::new(env!("CARGO_BIN_EXE_oxide-ide"))
            .args(*arguments)
            .output()
            .expect("run invalid worker bootstrap");
        assert!(!output.status.success(), "accepted arguments {arguments:?}");
        assert!(
            output.stdout.is_empty(),
            "invalid worker emitted protocol bytes"
        );
    }
}

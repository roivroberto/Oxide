use std::path::PathBuf;

use oxide_ide::{
    AppModel, ClientStartId, ClientSubmission, ClosureHealth, ControlAvailability, Envelope,
    EventSequence, ExecutionViewState, FileFailureKind, FileModelEvent, MAX_OPEN_FILE_BYTES,
    MAX_SOURCE_LINES, MAX_VISIBLE_OUTPUT_BYTES, MAX_VISIBLE_OUTPUT_LINES,
    MAX_WIRE_DOCUMENT_JSON_BYTES, ModelEffect, ModelEvent, ModelStatus, OUTPUT_TRUNCATION_MARKER,
    RequestId, RunBinding, RunId, RunMode, SnapshotProvenance, SupervisorModelEvent, UiAction,
    UnsavedChoice, WireDiagnostic, WireRuntimeFrame, WorkerEvent, WorkerSessionId,
    WorkerTerminationReason, apply_event,
};
use rlox::{
    ActivationId, DebugPointId, DiagnosticPhase, DiagnosticSeverity, FrameSnapshot, PauseLocation,
    PauseReason, RevisionId, SnapshotReason, SourceId, SourceSpan, TextPosition, VmSnapshot,
};

const SOURCE: &str = "var value = 1;\nprint value;\n";

fn disabled() -> ControlAvailability {
    ControlAvailability {
        run: false,
        debug: false,
        pause: false,
        continue_execution: false,
        step_into: false,
        step_over: false,
        step_out: false,
        stop: false,
        editor: false,
        submit_input: false,
    }
}

fn ready() -> ControlAvailability {
    ControlAvailability {
        run: true,
        debug: true,
        editor: true,
        ..disabled()
    }
}

fn set_source(model: &mut AppModel, source: &str) {
    let stamp = model.document().expect("document").stamp();
    assert!(
        apply_event(
            model,
            ModelEvent::Ui(UiAction::Edit {
                document: stamp,
                text: source.to_string(),
            }),
        )
        .is_empty()
    );
}

fn begin_start(model: &mut AppModel, mode: RunMode) -> ClientStartId {
    let action = if mode == RunMode::Run {
        UiAction::Run
    } else {
        UiAction::Debug
    };
    let effects = apply_event(model, ModelEvent::Ui(action));
    let [ModelEffect::Start(intent)] = effects.as_slice() else {
        panic!("expected one start effect: {effects:?}");
    };
    assert_eq!(intent.mode, mode);
    assert_eq!(intent.normalized_source.as_ref(), SOURCE);
    intent.client_start_id
}

fn binding(session: u64) -> RunBinding {
    RunBinding {
        worker_session_id: WorkerSessionId(session),
        run_id: RunId(1),
        source_id: SourceId(session),
        source_revision: RevisionId(1),
    }
}

fn admit_start(
    model: &mut AppModel,
    client_start_id: ClientStartId,
    mode: RunMode,
    run: RunBinding,
) {
    assert!(
        apply_event(
            model,
            ModelEvent::Supervisor(SupervisorModelEvent::Started {
                client_start_id,
                mode,
                run,
                request_id: RequestId(1),
                next_event_sequence: EventSequence(2),
            }),
        )
        .is_empty()
    );
}

fn running_model(mode: RunMode) -> (AppModel, RunBinding) {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let start_id = begin_start(&mut model, mode);
    let run = binding(11);
    admit_start(&mut model, start_id, mode, run);
    (model, run)
}

fn position(source: &str, offset: usize) -> TextPosition {
    let mut line = 1;
    let mut column = 1;
    for character in source[..offset].chars() {
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    TextPosition {
        byte_offset: offset,
        line,
        column,
    }
}

fn span(run: RunBinding, start: usize, end: usize) -> SourceSpan {
    SourceSpan {
        source_id: run.source_id,
        revision: run.source_revision,
        start: position(SOURCE, start),
        end: position(SOURCE, end),
    }
}

fn snapshot(run: RunBinding, reason: SnapshotReason) -> VmSnapshot {
    let current_span = span(run, 15, 27);
    VmSnapshot {
        reason,
        current_span,
        frames: vec![FrameSnapshot {
            activation_id: ActivationId(7),
            function: "script".to_string(),
            function_truncated: false,
            current_span,
            call_site: None,
            parameters: Vec::new(),
            parameters_truncated: false,
            locals: Vec::new(),
            locals_truncated: false,
            upvalues: Vec::new(),
            upvalues_truncated: false,
        }],
        frames_truncated: false,
        globals: Vec::new(),
        globals_truncated: false,
    }
}

fn paused_event(run: RunBinding, sequence: u64, reason: PauseReason) -> WorkerEvent {
    let snapshot = snapshot(run, SnapshotReason::Paused(reason));
    WorkerEvent::Paused {
        location: PauseLocation {
            source_id: run.source_id,
            revision: run.source_revision,
            span: snapshot.current_span,
            debug_point_id: DebugPointId(1),
            activation_id: ActivationId(7),
            dynamic_event: sequence,
        },
        snapshot,
    }
}

fn worker(run: RunBinding, sequence: u64, request: u64, payload: WorkerEvent) -> ModelEvent {
    ModelEvent::Supervisor(SupervisorModelEvent::Worker(Box::new(Envelope {
        version: oxide_ide::PROTOCOL_VERSION,
        worker_session_id: run.worker_session_id,
        run_id: run.run_id,
        source_revision: run.source_revision,
        request_id: RequestId(request),
        sequence: EventSequence(sequence),
        payload,
    })))
}

fn diagnostic(run: RunBinding, message: &str) -> WireDiagnostic {
    WireDiagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "runtime.test".to_string(),
        code_truncated: false,
        message: message.to_string(),
        message_truncated: false,
        span: span(run, 15, 27),
        frames: vec![WireRuntimeFrame {
            function: "script".to_string(),
            function_truncated: false,
            span: span(run, 15, 27),
        }],
        frames_truncated: false,
    }
}

#[test]
fn control_table_covers_every_view_state_and_stopping_overlay() {
    let idle = AppModel::new();

    let mut starting = AppModel::new();
    set_source(&mut starting, SOURCE);
    let start_id = begin_start(&mut starting, RunMode::Run);

    let run = binding(21);
    let mut running = starting.clone();
    admit_start(&mut running, start_id, RunMode::Run, run);

    let mut waiting = running.clone();
    apply_event(
        &mut waiting,
        worker(
            run,
            2,
            1,
            WorkerEvent::InputRequested {
                prompt: "value: ".to_string(),
            },
        ),
    );

    let (mut paused, paused_run) = running_model(RunMode::Debug);
    apply_event(
        &mut paused,
        worker(
            paused_run,
            2,
            1,
            paused_event(paused_run, 2, PauseReason::DebugPoint),
        ),
    );

    let mut completed = running.clone();
    apply_event(&mut completed, worker(run, 2, 1, WorkerEvent::Completed));

    let mut cancelled = running.clone();
    let stop = apply_event(&mut cancelled, ModelEvent::Ui(UiAction::Stop));
    let [ModelEffect::SubmitCommand(intent)] = stop.as_slice() else {
        panic!("expected stop intent");
    };
    apply_event(
        &mut cancelled,
        ModelEvent::Supervisor(SupervisorModelEvent::CommandAdmitted {
            client_command_id: intent.client_command_id,
            command: oxide_ide::ExecutionCommandKind::Stop,
            run,
            request_id: RequestId(2),
            next_event_sequence: EventSequence(2),
        }),
    );
    apply_event(
        &mut cancelled,
        worker(
            run,
            2,
            2,
            WorkerEvent::Cancelled {
                snapshot: snapshot(run, SnapshotReason::Cancelled),
            },
        ),
    );

    let mut faulted = running.clone();
    let fault = diagnostic(run, "boom");
    apply_event(
        &mut faulted,
        worker(
            run,
            2,
            1,
            WorkerEvent::Diagnostic {
                diagnostic: fault.clone(),
            },
        ),
    );
    apply_event(
        &mut faulted,
        worker(
            run,
            3,
            1,
            WorkerEvent::Faulted {
                diagnostic: fault,
                snapshot: snapshot(run, SnapshotReason::Faulted),
            },
        ),
    );

    let mut crashed = running.clone();
    apply_event(
        &mut crashed,
        ModelEvent::Supervisor(SupervisorModelEvent::WorkerTerminated {
            client_start_id: None,
            worker_session_id: run.worker_session_id,
            run: Some(run),
            reason: WorkerTerminationReason::UnexpectedExit(Some(9)),
        }),
    );

    let running_controls = ControlAvailability {
        pause: true,
        stop: true,
        ..disabled()
    };
    let waiting_controls = ControlAvailability {
        stop: true,
        submit_input: true,
        ..disabled()
    };
    let paused_controls = ControlAvailability {
        continue_execution: true,
        step_into: true,
        step_over: true,
        step_out: true,
        stop: true,
        ..disabled()
    };
    let cases = [
        (ExecutionViewState::Idle, idle.controls(), ready()),
        (
            ExecutionViewState::Starting,
            starting.controls(),
            ControlAvailability {
                stop: true,
                ..disabled()
            },
        ),
        (
            ExecutionViewState::Running,
            running.controls(),
            running_controls,
        ),
        (
            ExecutionViewState::WaitingForInput,
            waiting.controls(),
            waiting_controls,
        ),
        (
            ExecutionViewState::Paused,
            paused.controls(),
            paused_controls,
        ),
        (ExecutionViewState::Completed, completed.controls(), ready()),
        (ExecutionViewState::Cancelled, cancelled.controls(), ready()),
        (ExecutionViewState::Faulted, faulted.controls(), ready()),
        (
            ExecutionViewState::WorkerCrashed,
            crashed.controls(),
            ready(),
        ),
    ];
    for (state, actual, expected) in cases {
        assert_eq!(actual, expected, "wrong controls for {state:?}");
    }

    let mut stopping = running;
    assert_eq!(
        apply_event(&mut stopping, ModelEvent::Ui(UiAction::Stop)).len(),
        1
    );
    assert_eq!(stopping.controls(), disabled());
    assert!(
        apply_event(&mut stopping, ModelEvent::Ui(UiAction::Stop)).is_empty(),
        "duplicate Stop must be inert"
    );
}

#[test]
fn start_binds_only_matching_admission_and_uses_immutable_unsaved_source() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let start_id = begin_start(&mut model, RunMode::Debug);

    let stale = ClientStartId::from_raw(start_id.get() + 1).unwrap();
    let before = model.clone();
    apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::Started {
            client_start_id: stale,
            mode: RunMode::Debug,
            run: binding(30),
            request_id: RequestId(1),
            next_event_sequence: EventSequence(2),
        }),
    );
    assert_eq!(model, before);

    admit_start(&mut model, start_id, RunMode::Debug, binding(31));
    assert_eq!(model.execution_state(), ExecutionViewState::Running);
    assert!(model.active_run().is_some());
    let stamp = model.document().unwrap().stamp();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Ui(UiAction::Edit {
                document: stamp,
                text: "print 999;".to_string(),
            }),
        )
        .is_empty()
    );
    assert_eq!(model.document().unwrap().text(), SOURCE);
}

#[test]
fn exact_sequence_accepts_terminal_once_and_gap_forces_reaped_crash() {
    let (mut model, run) = running_model(RunMode::Run);
    apply_event(
        &mut model,
        worker(
            run,
            2,
            1,
            WorkerEvent::Output {
                text: "one\n".to_string(),
            },
        ),
    );
    apply_event(&mut model, worker(run, 3, 1, WorkerEvent::Completed));
    assert_eq!(model.execution_state(), ExecutionViewState::Completed);
    assert_eq!(model.program_output(), "one\n");
    let terminal = model.clone();
    apply_event(
        &mut model,
        worker(
            run,
            4,
            1,
            WorkerEvent::Output {
                text: "late".to_string(),
            },
        ),
    );
    assert_eq!(model, terminal);

    let (mut gapped, gap_run) = running_model(RunMode::Run);
    let effects = apply_event(
        &mut gapped,
        worker(
            gap_run,
            3,
            1,
            WorkerEvent::Output {
                text: "missing predecessor".to_string(),
            },
        ),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert!(gapped.stop_requested());
    assert_eq!(gapped.program_output(), "");
    assert!(
        apply_event(
            &mut gapped,
            worker(
                gap_run,
                2,
                1,
                WorkerEvent::Output {
                    text: "too late".to_string(),
                },
            ),
        )
        .is_empty()
    );
    apply_event(
        &mut gapped,
        ModelEvent::Supervisor(SupervisorModelEvent::WorkerTerminated {
            client_start_id: None,
            worker_session_id: gap_run.worker_session_id,
            run: Some(gap_run),
            reason: WorkerTerminationReason::SupervisorClosed,
        }),
    );
    assert_eq!(gapped.execution_state(), ExecutionViewState::WorkerCrashed);
}

#[test]
fn identical_diagnostics_keep_identity_and_fault_copy_is_not_duplicated() {
    let (mut model, run) = running_model(RunMode::Run);
    let diagnostic = diagnostic(run, "same");
    for sequence in [2, 3] {
        apply_event(
            &mut model,
            worker(
                run,
                sequence,
                1,
                WorkerEvent::Diagnostic {
                    diagnostic: diagnostic.clone(),
                },
            ),
        );
    }
    assert_eq!(model.problems().len(), 2);
    assert_ne!(model.problems()[0].id(), model.problems()[1].id());
    apply_event(
        &mut model,
        worker(
            run,
            4,
            1,
            WorkerEvent::Faulted {
                diagnostic,
                snapshot: snapshot(run, SnapshotReason::Faulted),
            },
        ),
    );
    assert_eq!(model.problems().len(), 2);
    assert_eq!(model.execution_state(), ExecutionViewState::Faulted);
    assert!(model.fault_span().is_some());
    assert_eq!(
        model.retained_snapshot().unwrap().provenance(),
        SnapshotProvenance::Faulted
    );
}

#[test]
fn stop_is_sticky_but_natural_completion_wins_the_race() {
    let (mut model, run) = running_model(RunMode::Run);
    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::Stop));
    let [ModelEffect::SubmitCommand(intent)] = effects.as_slice() else {
        panic!("expected stop command");
    };
    apply_event(
        &mut model,
        worker(
            run,
            2,
            1,
            WorkerEvent::Output {
                text: "before terminal\n".to_string(),
            },
        ),
    );
    apply_event(&mut model, worker(run, 3, 1, WorkerEvent::Completed));
    assert_eq!(model.execution_state(), ExecutionViewState::Completed);
    assert_eq!(model.program_output(), "before terminal\n");
    assert!(!model.stop_requested());
    let completed = model.clone();
    apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::SubmissionRejected {
            submission: ClientSubmission::Command(intent.client_command_id),
            error: oxide_ide::SubmitError::Terminal,
        }),
    );
    assert_eq!(model, completed, "late submission result replaced terminal");
}

#[test]
fn frame_and_problem_navigation_do_not_overwrite_execution_or_fault_markers() {
    let (mut model, run) = running_model(RunMode::Debug);
    apply_event(
        &mut model,
        worker(run, 2, 1, paused_event(run, 2, PauseReason::DebugPoint)),
    );
    let current = model.current_span();
    let snapshot_key = model.retained_snapshot().unwrap().key();
    let effects = apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::SelectFrame {
            snapshot: snapshot_key,
            activation_id: ActivationId(7),
        }),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::Navigate {
            run: effect_run,
            ..
        }] if *effect_run == run
    ));
    assert_eq!(model.current_span(), current);
    assert_eq!(model.fault_span(), None);

    let mut faulted = running_model(RunMode::Run).0;
    let run = faulted.active_run().unwrap();
    let diagnostic = diagnostic(run, "navigate me");
    apply_event(
        &mut faulted,
        worker(
            run,
            2,
            1,
            WorkerEvent::Diagnostic {
                diagnostic: diagnostic.clone(),
            },
        ),
    );
    apply_event(
        &mut faulted,
        worker(
            run,
            3,
            1,
            WorkerEvent::Faulted {
                diagnostic,
                snapshot: snapshot(run, SnapshotReason::Faulted),
            },
        ),
    );
    let fault = faulted.fault_span();
    let problem = faulted.problems()[0].id();
    let effects = apply_event(
        &mut faulted,
        ModelEvent::Ui(UiAction::SelectProblem(problem)),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::Navigate {
            run: effect_run,
            ..
        }] if *effect_run == run
    ));
    assert_eq!(faulted.fault_span(), fault);
    assert!(faulted.navigation_span().is_some());
}

#[test]
fn save_as_is_transactional_and_preserves_native_unicode_path() {
    let mut model = AppModel::new();
    set_source(&mut model, "\u{feff}first\r\nsecond\rthird");
    assert_eq!(model.document().unwrap().text(), "first\nsecond\nthird");
    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::SaveAs));
    let [ModelEffect::PickSaveAs { operation_id, .. }] = effects.as_slice() else {
        panic!("expected save picker");
    };
    let operation_id = *operation_id;
    let path = PathBuf::from(r"C:\Users\Student\Mga Programa\wika-語.lox");
    let effects = apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::SavePicked {
            operation_id,
            path: Some(path.clone()),
        }),
    );
    let [
        ModelEffect::WriteFile {
            operation_id: write_id,
            path: written_path,
            contents,
        },
    ] = effects.as_slice()
    else {
        panic!("expected write");
    };
    assert_eq!(*write_id, operation_id);
    assert_eq!(written_path, &path);
    assert_eq!(contents.as_ref(), b"first\nsecond\nthird");
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::WriteFinished {
            operation_id,
            result: Ok(()),
        }),
    );
    assert_eq!(model.document().unwrap().path(), Some(path.as_path()));
    assert!(!model.document().unwrap().is_dirty());
}

#[test]
fn dirty_open_discard_keeps_old_buffer_when_picker_is_cancelled() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let original = model.document().unwrap().clone();
    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::Open));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected prompt");
    };
    let prompt_id = *operation_id;
    let picker = apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::ResolveUnsaved {
            operation_id: prompt_id,
            choice: UnsavedChoice::Discard,
        }),
    );
    let [ModelEffect::PickOpen { operation_id }] = picker.as_slice() else {
        panic!("expected open picker");
    };
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::OpenPicked {
            operation_id: *operation_id,
            path: None,
        }),
    );
    assert_eq!(model.document(), Some(&original));
}

#[test]
fn dirty_exit_cancel_is_inert_and_reaped_exit_authorizes_once() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::RequestExit));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected exit prompt");
    };
    let prompt_id = *operation_id;
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::ResolveUnsaved {
            operation_id: prompt_id,
            choice: UnsavedChoice::Cancel,
        }),
    );
    assert_eq!(model.controls(), ready());

    let start_id = begin_start(&mut model, RunMode::Run);
    let run = binding(44);
    admit_start(&mut model, start_id, RunMode::Run, run);
    let close = apply_event(&mut model, ModelEvent::Ui(UiAction::RequestExit));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = close.as_slice() else {
        panic!("dirty active exit still resolves the document first");
    };
    let effects = apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::ResolveUnsaved {
            operation_id: *operation_id,
            choice: UnsavedChoice::Discard,
        }),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    let effects = apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::WorkerTerminated {
            client_start_id: None,
            worker_session_id: run.worker_session_id,
            run: Some(run),
            reason: WorkerTerminationReason::SupervisorClosed,
        }),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::AuthorizeClose { .. }]
    ));
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: None,
                worker_session_id: run.worker_session_id,
                run: Some(run),
                health: ClosureHealth::Clean,
            }),
        )
        .is_empty(),
        "close authorization must be one-shot"
    );
}

#[test]
fn admitted_resume_reenables_pause_and_wire_rejection_desynchronizes() {
    let (mut model, run) = running_model(RunMode::Debug);
    apply_event(
        &mut model,
        worker(run, 2, 1, paused_event(run, 2, PauseReason::DebugPoint)),
    );
    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::Continue));
    let [ModelEffect::SubmitCommand(intent)] = effects.as_slice() else {
        panic!("expected Continue command");
    };
    assert!(!model.controls().pause);
    apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::CommandAdmitted {
            client_command_id: intent.client_command_id,
            command: oxide_ide::ExecutionCommandKind::Continue,
            run,
            request_id: RequestId(2),
            next_event_sequence: EventSequence(3),
        }),
    );
    assert_eq!(model.execution_state(), ExecutionViewState::Running);
    assert!(
        model.controls().pause,
        "silent continuation must remain pausable"
    );
    assert_eq!(
        model.retained_snapshot().unwrap().provenance(),
        SnapshotProvenance::LastSafePause
    );
    assert!(!model.retained_snapshot().unwrap().is_live());

    let effects = apply_event(
        &mut model,
        worker(
            run,
            3,
            2,
            WorkerEvent::CommandRejected {
                code: "worker.invalid_state".to_string(),
                message: "unexpected rejection".to_string(),
            },
        ),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert!(model.stop_requested());
    assert_eq!(model.controls(), disabled());
}

#[test]
fn start_submission_rejection_waits_for_child_close() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let start_id = begin_start(&mut model, RunMode::Run);
    let effects = apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::SubmissionRejected {
            submission: ClientSubmission::Start(start_id),
            error: oxide_ide::SubmitError::Full,
        }),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert_eq!(model.execution_state(), ExecutionViewState::Starting);
    assert!(model.stop_requested());

    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: Some(start_id),
                worker_session_id: WorkerSessionId(90),
                run: None,
                health: ClosureHealth::Clean,
            }),
        )
        .is_empty()
    );
    assert_eq!(model.execution_state(), ExecutionViewState::Idle);
    assert_eq!(model.controls(), ready());
}

#[test]
fn output_cap_is_utf8_safe_and_renders_one_marker_inside_cap() {
    let (mut model, run) = running_model(RunMode::Run);
    let chunk = "x".repeat(oxide_ide::MAX_OUTPUT_CHUNK_TEXT_BYTES);
    for sequence in 2..18 {
        apply_event(
            &mut model,
            worker(
                run,
                sequence,
                1,
                WorkerEvent::Output {
                    text: chunk.clone(),
                },
            ),
        );
    }
    assert_eq!(model.program_output().len(), MAX_VISIBLE_OUTPUT_BYTES);
    apply_event(
        &mut model,
        worker(
            run,
            18,
            1,
            WorkerEvent::Output {
                text: "🦀".to_string(),
            },
        ),
    );
    apply_event(&mut model, worker(run, 19, 1, WorkerEvent::OutputTruncated));
    let rendered = model.rendered_output();
    assert!(rendered.len() <= MAX_VISIBLE_OUTPUT_BYTES);
    assert!(rendered.is_char_boundary(rendered.len()));
    assert_eq!(rendered.matches(OUTPUT_TRUNCATION_MARKER).count(), 1);
    assert!(model.output_was_truncated());
}

#[test]
fn output_line_cap_prevents_pathological_layouts() {
    let (mut model, run) = running_model(RunMode::Run);
    let text = "\n".repeat(MAX_VISIBLE_OUTPUT_LINES + 1);
    apply_event(&mut model, worker(run, 2, 1, WorkerEvent::Output { text }));

    let rendered = model.rendered_output();
    assert_eq!(rendered.matches(OUTPUT_TRUNCATION_MARKER).count(), 1);
    assert_eq!(
        rendered.matches('\n').count(),
        MAX_VISIBLE_OUTPUT_LINES + OUTPUT_TRUNCATION_MARKER.matches('\n').count() - 1
    );
    assert!(model.output_was_truncated());
}

#[test]
fn malformed_nested_span_is_rejected_atomically() {
    let (mut model, run) = running_model(RunMode::Run);
    let mut invalid = diagnostic(run, "bad span");
    invalid.span.start.line = 99;
    let effects = apply_event(
        &mut model,
        worker(
            run,
            2,
            1,
            WorkerEvent::Diagnostic {
                diagnostic: invalid,
            },
        ),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert!(model.problems().is_empty());
    assert_eq!(model.program_output(), "");
    assert!(model.stop_requested());
}

#[test]
fn next_run_is_deferred_until_exact_cleanup_barrier() {
    let (mut model, first_run) = running_model(RunMode::Run);
    apply_event(&mut model, worker(first_run, 2, 1, WorkerEvent::Completed));
    assert!(model.cleanup_pending());
    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::Run));
    assert!(
        effects.is_empty(),
        "a second child must not launch before reap"
    );
    assert_eq!(model.execution_state(), ExecutionViewState::Starting);

    let stale = binding(777);
    let before = model.clone();
    apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::Closed {
            client_start_id: None,
            worker_session_id: stale.worker_session_id,
            run: Some(stale),
            health: ClosureHealth::Clean,
        }),
    );
    assert_eq!(model, before);

    let effects = apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::Closed {
            client_start_id: None,
            worker_session_id: first_run.worker_session_id,
            run: Some(first_run),
            health: ClosureHealth::Clean,
        }),
    );
    assert!(matches!(effects.as_slice(), [ModelEffect::Start(_)]));
}

#[test]
fn open_read_is_atomic_and_normalizes_only_after_success() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let old = model.document().unwrap().clone();
    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::Open));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected prompt");
    };
    let picker = apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::ResolveUnsaved {
            operation_id: *operation_id,
            choice: UnsavedChoice::Discard,
        }),
    );
    let [ModelEffect::PickOpen { operation_id }] = picker.as_slice() else {
        panic!("expected picker");
    };
    let operation_id = *operation_id;
    let path = PathBuf::from("C:/Mga Programa/bago-語.lox");
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::OpenPicked {
            operation_id,
            path: Some(path.clone()),
        }),
    );
    assert_eq!(model.document(), Some(&old));
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::ReadFinished {
            operation_id,
            result: Ok(b"\xef\xbb\xbfuna\r\ndalawa\rtatlo".to_vec()),
        }),
    );
    let opened = model.document().unwrap();
    assert_ne!(opened.id(), old.id());
    assert_eq!(opened.path(), Some(path.as_path()));
    assert_eq!(opened.text(), "una\ndalawa\ntatlo");
    assert!(!opened.is_dirty());
}

#[test]
fn exit_does_not_replace_an_in_flight_save() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let picker = apply_event(&mut model, ModelEvent::Ui(UiAction::Save));
    let [ModelEffect::PickSaveAs { operation_id, .. }] = picker.as_slice() else {
        panic!("expected save picker");
    };
    let operation_id = *operation_id;
    let effects = apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::SavePicked {
            operation_id,
            path: Some(PathBuf::from(r"C:\Users\Student\in-flight.lox")),
        }),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::WriteFile { .. }]
    ));
    let before = model.clone();
    assert!(
        apply_event(&mut model, ModelEvent::Ui(UiAction::RequestExit)).is_empty(),
        "the window adapter can retry close after the save finishes"
    );
    assert_eq!(model, before);
}

#[test]
fn saving_during_execution_keeps_stop_available() {
    let (mut model, _) = running_model(RunMode::Run);
    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::Save));
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::PickSaveAs { .. }]
    ));
    assert_eq!(
        model.controls(),
        ControlAvailability {
            stop: true,
            ..disabled()
        }
    );
}

#[test]
fn invalid_matching_start_admission_is_closed_without_binding() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let start_id = begin_start(&mut model, RunMode::Debug);
    let run = binding(91);
    let effects = apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::Started {
            client_start_id: start_id,
            mode: RunMode::Debug,
            run,
            request_id: RequestId(1),
            next_event_sequence: EventSequence(3),
        }),
    );
    assert_eq!(
        effects,
        vec![ModelEffect::CloseWorker {
            target: oxide_ide::WorkerTarget::Run(run),
        }]
    );
    assert_eq!(model.active_run(), None);
    assert!(model.stop_requested());
    assert_eq!(model.status(), Some(&ModelStatus::ProtocolDesynchronized));
}

#[test]
fn launch_failure_restores_the_previous_terminal_state_immediately() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let start_id = begin_start(&mut model, RunMode::Run);
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::StartFailed {
                client_start_id: start_id,
                kind: std::io::ErrorKind::NotFound,
            }),
        )
        .is_empty()
    );
    assert_eq!(model.execution_state(), ExecutionViewState::Idle);
    assert_eq!(model.controls(), ready());
    assert_eq!(
        model.status(),
        Some(&ModelStatus::StartFailed(std::io::ErrorKind::NotFound))
    );
}

#[test]
fn open_rejects_adapter_data_larger_than_the_declared_limit() {
    let mut model = AppModel::new();
    let picker = apply_event(&mut model, ModelEvent::Ui(UiAction::Open));
    let [ModelEffect::PickOpen { operation_id }] = picker.as_slice() else {
        panic!("expected open picker");
    };
    let operation_id = *operation_id;
    let path = PathBuf::from(r"C:\Users\Student\too-large.lox");
    let effects = apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::OpenPicked {
            operation_id,
            path: Some(path),
        }),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::ReadFile {
            max_bytes: MAX_OPEN_FILE_BYTES,
            ..
        }]
    ));
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::ReadFinished {
            operation_id,
            result: Ok(vec![b'x'; MAX_OPEN_FILE_BYTES + 1]),
        }),
    );
    assert_eq!(
        model.status(),
        Some(&ModelStatus::FileFailed(FileFailureKind::InvalidData))
    );
    assert_eq!(model.document().unwrap().text(), "");
}

#[test]
fn open_rejects_pathological_line_counts_without_replacing_the_document() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let original = model.document().unwrap().clone();
    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::Open));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected unsaved prompt");
    };
    let picker = apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::ResolveUnsaved {
            operation_id: *operation_id,
            choice: UnsavedChoice::Discard,
        }),
    );
    let [ModelEffect::PickOpen { operation_id }] = picker.as_slice() else {
        panic!("expected open picker");
    };
    let operation_id = *operation_id;
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::OpenPicked {
            operation_id,
            path: Some(PathBuf::from(r"C:\Users\Student\too-many-lines.lox")),
        }),
    );
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::ReadFinished {
            operation_id,
            result: Ok("\n".repeat(MAX_SOURCE_LINES).into_bytes()),
        }),
    );

    assert_eq!(model.document(), Some(&original));
    assert_eq!(model.status(), Some(&ModelStatus::SourceLimitReached));
    assert!(matches!(
        apply_event(&mut model, ModelEvent::Ui(UiAction::Open)).as_slice(),
        [ModelEffect::PromptUnsaved { .. }]
    ));
}

#[test]
fn unicode_source_positions_are_validated_by_scalar_column() {
    const UNICODE_SOURCE: &str = "print \"🦀\";\nprint \"語\";\n";
    let mut model = AppModel::new();
    set_source(&mut model, UNICODE_SOURCE);
    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::Run));
    let [ModelEffect::Start(intent)] = effects.as_slice() else {
        panic!("expected start");
    };
    let start_id = intent.client_start_id;
    let run = binding(92);
    admit_start(&mut model, start_id, RunMode::Run, run);
    let start = UNICODE_SOURCE.find('🦀').unwrap();
    let end = UNICODE_SOURCE.find('語').unwrap() + '語'.len_utf8();
    let unicode_span = SourceSpan {
        source_id: run.source_id,
        revision: run.source_revision,
        start: position(UNICODE_SOURCE, start),
        end: position(UNICODE_SOURCE, end),
    };
    let diagnostic = WireDiagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "runtime.unicode".to_string(),
        code_truncated: false,
        message: "unicode span".to_string(),
        message_truncated: false,
        span: unicode_span,
        frames: vec![WireRuntimeFrame {
            function: "script".to_string(),
            function_truncated: false,
            span: unicode_span,
        }],
        frames_truncated: false,
    };
    assert!(
        apply_event(
            &mut model,
            worker(run, 2, 1, WorkerEvent::Diagnostic { diagnostic }),
        )
        .is_empty()
    );
    assert_eq!(model.problems().len(), 1);
    assert!(!model.stop_requested());
}

#[test]
fn problem_count_is_bounded_even_for_tiny_diagnostics() {
    let (mut model, run) = running_model(RunMode::Run);
    let diagnostic = diagnostic(run, "x");
    for sequence in 2..=258 {
        apply_event(
            &mut model,
            worker(
                run,
                sequence,
                1,
                WorkerEvent::Diagnostic {
                    diagnostic: diagnostic.clone(),
                },
            ),
        );
    }
    assert_eq!(model.problems().len(), 256);
    assert!(model.problems_were_truncated());
    assert_eq!(model.status(), Some(&ModelStatus::ProblemLimitReached));
}

#[test]
fn save_is_inert_once_exit_resolution_has_started() {
    let mut authorized = AppModel::new();
    assert!(matches!(
        apply_event(&mut authorized, ModelEvent::Ui(UiAction::RequestExit)).as_slice(),
        [ModelEffect::AuthorizeClose { .. }]
    ));
    let before = authorized.clone();
    assert!(apply_event(&mut authorized, ModelEvent::Ui(UiAction::SaveAs)).is_empty());
    assert_eq!(authorized, before);

    let (mut waiting, run) = running_model(RunMode::Run);
    let prompt = apply_event(&mut waiting, ModelEvent::Ui(UiAction::RequestExit));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected dirty exit prompt");
    };
    assert_eq!(
        apply_event(
            &mut waiting,
            ModelEvent::Ui(UiAction::ResolveUnsaved {
                operation_id: *operation_id,
                choice: UnsavedChoice::Discard,
            }),
        ),
        vec![ModelEffect::CloseWorker {
            target: oxide_ide::WorkerTarget::Run(run),
        }]
    );
    let before = waiting.clone();
    assert!(apply_event(&mut waiting, ModelEvent::Ui(UiAction::Save)).is_empty());
    assert_eq!(waiting, before);
}

#[test]
fn wire_oversized_text_can_be_edited_down_but_cannot_run() {
    let mut model = AppModel::new();
    let escaped = "\0".repeat(MAX_WIRE_DOCUMENT_JSON_BYTES / 6 + 1);
    set_source(&mut model, &escaped);
    assert_eq!(
        model.controls(),
        ControlAvailability {
            editor: true,
            ..disabled()
        }
    );
    assert!(apply_event(&mut model, ModelEvent::Ui(UiAction::Run)).is_empty());

    let stamp = model.document().unwrap().stamp();
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: SOURCE.to_string(),
        }),
    );
    assert_eq!(model.document().unwrap().text(), SOURCE);
    assert_eq!(model.controls(), ready());
}

#[test]
fn interactively_oversized_text_is_rejected_atomically_and_a_valid_edit_recovers() {
    let mut model = AppModel::new();
    let original = model.document().unwrap().clone();
    let oversized = "x".repeat(MAX_OPEN_FILE_BYTES + 1);
    set_source(&mut model, &oversized);
    assert_eq!(model.document(), Some(&original));
    assert_eq!(model.status(), Some(&ModelStatus::SourceLimitReached));

    let stamp = model.document().unwrap().stamp();
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: SOURCE.to_string(),
        }),
    );
    assert_eq!(model.document().unwrap().text(), SOURCE);
    assert_eq!(model.status(), None);
    assert_eq!(model.controls(), ready());
}

#[test]
fn interactively_excessive_line_count_is_rejected_atomically() {
    let mut model = AppModel::new();
    let original = model.document().unwrap().clone();
    let stamp = original.stamp();
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: "\n".repeat(MAX_SOURCE_LINES),
        }),
    );

    assert_eq!(model.document(), Some(&original));
    assert_eq!(model.status(), Some(&ModelStatus::SourceLimitReached));
}

#[test]
fn save_on_a_clean_untitled_document_opens_save_as() {
    let mut model = AppModel::new();
    assert!(matches!(
        apply_event(&mut model, ModelEvent::Ui(UiAction::Save)).as_slice(),
        [ModelEffect::PickSaveAs { .. }]
    ));
}

#[test]
fn pause_racing_with_stop_is_retained_only_as_a_nonlive_snapshot() {
    let (mut model, run) = running_model(RunMode::Debug);
    assert!(matches!(
        apply_event(&mut model, ModelEvent::Ui(UiAction::Stop)).as_slice(),
        [ModelEffect::SubmitCommand(_)]
    ));
    assert!(
        apply_event(
            &mut model,
            worker(run, 2, 1, paused_event(run, 2, PauseReason::DebugPoint)),
        )
        .is_empty()
    );
    assert_eq!(model.execution_state(), ExecutionViewState::Running);
    assert_eq!(model.current_span(), None);
    let retained = model.retained_snapshot().unwrap();
    assert_eq!(retained.provenance(), SnapshotProvenance::LastSafePause);
    assert!(!retained.is_live());
    assert_eq!(model.controls(), disabled());
}

#[test]
fn cancellation_requires_the_admitted_stop_request() {
    let (mut model, run) = running_model(RunMode::Run);
    assert!(matches!(
        apply_event(&mut model, ModelEvent::Ui(UiAction::Stop)).as_slice(),
        [ModelEffect::SubmitCommand(_)]
    ));
    let effects = apply_event(
        &mut model,
        worker(
            run,
            2,
            999,
            WorkerEvent::Cancelled {
                snapshot: snapshot(run, SnapshotReason::Cancelled),
            },
        ),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert_ne!(model.execution_state(), ExecutionViewState::Cancelled);
    assert!(model.stop_requested());
}

#[test]
fn pause_and_fault_locations_must_match_their_snapshots() {
    let (mut paused, pause_run) = running_model(RunMode::Debug);
    let mut mismatched_pause = paused_event(pause_run, 2, PauseReason::DebugPoint);
    let WorkerEvent::Paused { location, .. } = &mut mismatched_pause else {
        unreachable!();
    };
    location.span = span(pause_run, 0, 3);
    assert!(matches!(
        apply_event(&mut paused, worker(pause_run, 2, 1, mismatched_pause)).as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert_eq!(paused.current_span(), None);
    assert!(paused.retained_snapshot().is_none());

    let (mut faulted, fault_run) = running_model(RunMode::Run);
    let mut mismatched_diagnostic = diagnostic(fault_run, "mismatch");
    mismatched_diagnostic.span = span(fault_run, 0, 3);
    assert!(matches!(
        apply_event(
            &mut faulted,
            worker(
                fault_run,
                2,
                1,
                WorkerEvent::Faulted {
                    diagnostic: mismatched_diagnostic,
                    snapshot: snapshot(fault_run, SnapshotReason::Faulted),
                },
            ),
        )
        .as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert_eq!(faulted.execution_state(), ExecutionViewState::Running);
    assert!(faulted.problems().is_empty());
    assert_eq!(faulted.fault_span(), None);
}

#[test]
fn close_generated_cancellation_requires_its_exact_admission() {
    let (mut model, run) = running_model(RunMode::Run);
    let stop = apply_event(&mut model, ModelEvent::Ui(UiAction::Stop));
    let [ModelEffect::SubmitCommand(intent)] = stop.as_slice() else {
        panic!("expected stop submission");
    };
    assert!(matches!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::SubmissionRejected {
                submission: ClientSubmission::Command(intent.client_command_id),
                error: oxide_ide::SubmitError::Full,
            }),
        )
        .as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::CloseAdmitted {
                run,
                request_id: RequestId(2),
                next_event_sequence: EventSequence(2),
            }),
        )
        .is_empty()
    );
    assert!(
        apply_event(
            &mut model,
            worker(
                run,
                2,
                2,
                WorkerEvent::Cancelled {
                    snapshot: snapshot(run, SnapshotReason::Cancelled),
                },
            ),
        )
        .is_empty()
    );
    assert_eq!(model.execution_state(), ExecutionViewState::Cancelled);
}

#[test]
fn deferred_start_termination_cannot_release_the_previous_cleanup_barrier() {
    let (mut model, first_run) = running_model(RunMode::Run);
    apply_event(&mut model, worker(first_run, 2, 1, WorkerEvent::Completed));
    assert!(apply_event(&mut model, ModelEvent::Ui(UiAction::Run)).is_empty());
    let deferred_start = ClientStartId::from_raw(2).unwrap();
    let before = model.clone();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::WorkerTerminated {
                client_start_id: Some(deferred_start),
                worker_session_id: WorkerSessionId(999),
                run: None,
                reason: WorkerTerminationReason::UnexpectedExit(Some(9)),
            }),
        )
        .is_empty()
    );
    assert_eq!(model, before);
    assert!(model.cleanup_pending());

    assert!(matches!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: None,
                worker_session_id: first_run.worker_session_id,
                run: Some(first_run),
                health: ClosureHealth::Clean,
            }),
        )
        .as_slice(),
        [ModelEffect::Start(_)]
    ));
}

#[test]
fn finalized_close_retires_a_desynchronized_run() {
    let (mut model, run) = running_model(RunMode::Run);
    assert!(matches!(
        apply_event(
            &mut model,
            worker(
                run,
                3,
                1,
                WorkerEvent::Output {
                    text: "gap".to_string(),
                },
            ),
        )
        .as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));
    assert!(model.stop_requested());
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: None,
                worker_session_id: run.worker_session_id,
                run: Some(run),
                health: ClosureHealth::Clean,
            }),
        )
        .is_empty()
    );
    assert_eq!(model.execution_state(), ExecutionViewState::WorkerCrashed);
    assert_eq!(model.active_run(), None);
    assert!(!model.cleanup_pending());
    assert!(!model.stop_requested());
    assert_eq!(model.status(), Some(&ModelStatus::ProtocolDesynchronized));
    assert_eq!(model.controls(), ready());
}

#[test]
fn stale_and_failed_open_completions_preserve_the_existing_document() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let original = model.document().unwrap().clone();
    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::Open));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected prompt");
    };
    let picker = apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::ResolveUnsaved {
            operation_id: *operation_id,
            choice: UnsavedChoice::Discard,
        }),
    );
    let [ModelEffect::PickOpen { operation_id }] = picker.as_slice() else {
        panic!("expected picker");
    };
    let operation_id = *operation_id;
    let stale = oxide_ide::FileOperationId::from_raw(operation_id.get() + 1).unwrap();
    let path = PathBuf::from(r"C:\Users\Student\lumang-語.lox");

    let before = model.clone();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::File(FileModelEvent::OpenPicked {
                operation_id: stale,
                path: Some(path.clone()),
            }),
        )
        .is_empty()
    );
    assert_eq!(model, before);

    assert!(matches!(
        apply_event(
            &mut model,
            ModelEvent::File(FileModelEvent::OpenPicked {
                operation_id,
                path: Some(path),
            }),
        )
        .as_slice(),
        [ModelEffect::ReadFile { .. }]
    ));
    let reading = model.clone();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::File(FileModelEvent::ReadFinished {
                operation_id: stale,
                result: Ok(b"stale".to_vec()),
            }),
        )
        .is_empty()
    );
    assert_eq!(model, reading);
    assert!(
        apply_event(
            &mut model,
            ModelEvent::File(FileModelEvent::ReadFinished {
                operation_id,
                result: Err(FileFailureKind::PermissionDenied),
            }),
        )
        .is_empty()
    );
    assert_eq!(model.document(), Some(&original));
    assert_eq!(
        model.status(),
        Some(&ModelStatus::FileFailed(FileFailureKind::PermissionDenied))
    );
    let failed = model.clone();
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::ReadFinished {
            operation_id,
            result: Ok(b"duplicate".to_vec()),
        }),
    );
    assert_eq!(model, failed);
}

#[test]
fn stale_and_duplicate_write_completions_are_inert() {
    let mut model = AppModel::new();
    set_source(&mut model, SOURCE);
    let picker = apply_event(&mut model, ModelEvent::Ui(UiAction::SaveAs));
    let [ModelEffect::PickSaveAs { operation_id, .. }] = picker.as_slice() else {
        panic!("expected save picker");
    };
    let operation_id = *operation_id;
    let stale = oxide_ide::FileOperationId::from_raw(operation_id.get() + 1).unwrap();
    let path = PathBuf::from(r"C:\Users\Student\save-語.lox");
    let before = model.clone();
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::SavePicked {
            operation_id: stale,
            path: Some(path.clone()),
        }),
    );
    assert_eq!(model, before);

    assert!(matches!(
        apply_event(
            &mut model,
            ModelEvent::File(FileModelEvent::SavePicked {
                operation_id,
                path: Some(path.clone()),
            }),
        )
        .as_slice(),
        [ModelEffect::WriteFile { .. }]
    ));
    let writing = model.clone();
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::WriteFinished {
            operation_id: stale,
            result: Ok(()),
        }),
    );
    assert_eq!(model, writing);
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::SavePicked {
            operation_id,
            path: Some(PathBuf::from(r"C:\Users\Student\wrong.lox")),
        }),
    );
    assert_eq!(model, writing);

    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::WriteFinished {
            operation_id,
            result: Ok(()),
        }),
    );
    assert_eq!(model.document().unwrap().path(), Some(path.as_path()));
    assert!(!model.document().unwrap().is_dirty());
    let completed = model.clone();
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::WriteFinished {
            operation_id,
            result: Err(FileFailureKind::PermissionDenied),
        }),
    );
    assert_eq!(model, completed);
}

#[test]
fn invalid_utf8_open_preserves_the_current_document() {
    let mut model = AppModel::new();
    let original = model.document().unwrap().clone();
    let picker = apply_event(&mut model, ModelEvent::Ui(UiAction::Open));
    let [ModelEffect::PickOpen { operation_id }] = picker.as_slice() else {
        panic!("expected picker");
    };
    let operation_id = *operation_id;
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::OpenPicked {
            operation_id,
            path: Some(PathBuf::from(r"C:\Users\Student\invalid-語.lox")),
        }),
    );
    apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::ReadFinished {
            operation_id,
            result: Ok(vec![0xff, 0xfe]),
        }),
    );
    assert_eq!(model.document(), Some(&original));
    assert_eq!(model.status(), Some(&ModelStatus::InvalidUtf8));
}

#[test]
fn cancelling_exit_dispatches_a_deferred_run_after_cleanup_finished() {
    let (mut model, first_run) = running_model(RunMode::Run);
    apply_event(&mut model, worker(first_run, 2, 1, WorkerEvent::Completed));
    assert!(apply_event(&mut model, ModelEvent::Ui(UiAction::Run)).is_empty());
    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::RequestExit));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected exit prompt");
    };
    let operation_id = *operation_id;

    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: None,
                worker_session_id: first_run.worker_session_id,
                run: Some(first_run),
                health: ClosureHealth::Clean,
            }),
        )
        .is_empty()
    );
    assert!(!model.cleanup_pending());

    let effects = apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::ResolveUnsaved {
            operation_id,
            choice: UnsavedChoice::Cancel,
        }),
    );
    let [ModelEffect::Start(intent)] = effects.as_slice() else {
        panic!("cancel must release the deferred start: {effects:?}");
    };
    assert_eq!(intent.client_start_id.get(), 2);
    assert_eq!(model.execution_state(), ExecutionViewState::Starting);
}

#[test]
fn stale_close_cannot_retire_an_undispatched_generation_during_exit() {
    let (mut model, first_run) = running_model(RunMode::Run);
    apply_event(&mut model, worker(first_run, 2, 1, WorkerEvent::Completed));
    assert!(apply_event(&mut model, ModelEvent::Ui(UiAction::Run)).is_empty());
    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::RequestExit));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected exit prompt");
    };
    assert!(matches!(
        apply_event(
            &mut model,
            ModelEvent::Ui(UiAction::ResolveUnsaved {
                operation_id: *operation_id,
                choice: UnsavedChoice::Discard,
            }),
        )
        .as_slice(),
        [ModelEffect::CloseWorker {
            target: oxide_ide::WorkerTarget::Run(target),
        }] if *target == first_run
    ));

    let before = model.clone();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: Some(ClientStartId::from_raw(2).unwrap()),
                worker_session_id: WorkerSessionId(999),
                run: None,
                health: ClosureHealth::Clean,
            }),
        )
        .is_empty()
    );
    assert_eq!(model, before);
    assert!(model.cleanup_pending());

    assert!(matches!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: None,
                worker_session_id: first_run.worker_session_id,
                run: Some(first_run),
                health: ClosureHealth::Clean,
            }),
        )
        .as_slice(),
        [ModelEffect::AuthorizeClose { .. }]
    ));
}

#[test]
fn runtime_disconnect_marks_an_idle_model_failed_and_is_idempotent() {
    let mut model = AppModel::new();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::RuntimeDisconnected),
        )
        .is_empty()
    );
    assert_eq!(model.execution_state(), ExecutionViewState::WorkerCrashed);
    assert_eq!(model.status(), Some(&ModelStatus::RuntimeDisconnected));
    assert_eq!(model.active_run(), None);
    assert!(!model.cleanup_pending());
    assert!(!model.stop_requested());

    let recovered = model.clone();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::RuntimeDisconnected),
        )
        .is_empty()
    );
    assert_eq!(model, recovered);
}

#[test]
fn runtime_disconnect_retires_deferred_start_and_cleanup_generation() {
    let (mut model, completed_run) = running_model(RunMode::Run);
    apply_event(
        &mut model,
        worker(completed_run, 2, 1, WorkerEvent::Completed),
    );
    assert!(model.cleanup_pending());
    assert!(apply_event(&mut model, ModelEvent::Ui(UiAction::Run)).is_empty());
    assert_eq!(model.execution_state(), ExecutionViewState::Starting);

    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::RuntimeDisconnected),
        )
        .is_empty()
    );
    assert_eq!(model.execution_state(), ExecutionViewState::WorkerCrashed);
    assert!(!model.cleanup_pending());

    let recovered = model.clone();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::Closed {
                client_start_id: None,
                worker_session_id: completed_run.worker_session_id,
                run: Some(completed_run),
                health: ClosureHealth::Clean,
            }),
        )
        .is_empty()
    );
    assert_eq!(model, recovered, "the deferred start must not be launched");
}

#[test]
fn runtime_disconnect_retires_commands_and_preserves_only_a_nonlive_snapshot() {
    let (mut model, run) = running_model(RunMode::Debug);
    apply_event(
        &mut model,
        worker(run, 2, 1, paused_event(run, 2, PauseReason::DebugPoint)),
    );
    assert!(model.retained_snapshot().unwrap().is_live());
    assert!(matches!(
        apply_event(&mut model, ModelEvent::Ui(UiAction::Continue)).as_slice(),
        [ModelEffect::SubmitCommand(_)]
    ));
    assert!(matches!(
        apply_event(&mut model, ModelEvent::Ui(UiAction::Stop)).as_slice(),
        [ModelEffect::SubmitCommand(_)]
    ));

    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::RuntimeDisconnected),
        )
        .is_empty()
    );
    assert_eq!(model.active_run(), None);
    assert!(!model.cleanup_pending());
    assert!(!model.stop_requested());
    assert_eq!(model.current_span(), None);
    let retained = model.retained_snapshot().unwrap();
    assert!(!retained.is_live());
    assert_eq!(retained.provenance(), SnapshotProvenance::LastSafePause);

    let start_id = begin_start(&mut model, RunMode::Run);
    let replacement = binding(401);
    admit_start(&mut model, start_id, RunMode::Run, replacement);
    assert!(
        model.controls().pause,
        "the pending command must be retired"
    );
    assert!(model.controls().stop, "the pending stop must be retired");
}

#[test]
fn runtime_disconnect_clears_input_and_authorizes_a_waiting_exit_once() {
    let (mut model, run) = running_model(RunMode::Run);
    apply_event(
        &mut model,
        worker(
            run,
            2,
            1,
            WorkerEvent::InputRequested {
                prompt: "value: ".to_string(),
            },
        ),
    );
    assert!(model.pending_input().is_some());

    let prompt = apply_event(&mut model, ModelEvent::Ui(UiAction::RequestExit));
    let [ModelEffect::PromptUnsaved { operation_id, .. }] = prompt.as_slice() else {
        panic!("expected exit prompt: {prompt:?}");
    };
    assert!(matches!(
        apply_event(
            &mut model,
            ModelEvent::Ui(UiAction::ResolveUnsaved {
                operation_id: *operation_id,
                choice: UnsavedChoice::Discard,
            }),
        )
        .as_slice(),
        [ModelEffect::CloseWorker { .. }]
    ));

    let effects = apply_event(
        &mut model,
        ModelEvent::Supervisor(SupervisorModelEvent::RuntimeDisconnected),
    );
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::AuthorizeClose { .. }]
    ));
    assert_eq!(model.pending_input(), None);
    assert_eq!(model.execution_state(), ExecutionViewState::WorkerCrashed);
    assert_eq!(model.status(), Some(&ModelStatus::RuntimeDisconnected));

    assert!(
        apply_event(
            &mut model,
            ModelEvent::Supervisor(SupervisorModelEvent::RuntimeDisconnected),
        )
        .is_empty(),
        "close authorization must be one-shot"
    );
}

use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use oxide_ide::{
    AppAction, AppModel, Envelope, EventSequence, ExecutionCommand, FileModelEvent, ModelEffect,
    ModelEvent, OxideApp, PROTOCOL_VERSION, RequestId, RunBinding, RunId, RunMode,
    SupervisorModelEvent, UiAction, WireDiagnostic, WireRuntimeFrame, WorkerEvent, WorkerSessionId,
    apply_event, frame_accessible_name,
};
use rlox::{
    ActivationId, BindingSnapshot, DebugPointId, DebugValue, DiagnosticPhase, DiagnosticSeverity,
    FrameSnapshot, PauseLocation, PauseReason, RevisionId, SnapshotReason, SourceId, SourceSpan,
    TextPosition, ValueKind, VmSnapshot,
};
use std::path::PathBuf;

const DEBUG_SOURCE: &str = "var café = 1;\nprint café;\n";

fn text_position(source: &str, byte_offset: usize) -> TextPosition {
    let mut line = 1;
    let mut column = 1;
    for character in source[..byte_offset].chars() {
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    TextPosition {
        byte_offset,
        line,
        column,
    }
}

fn debug_span(run: RunBinding) -> SourceSpan {
    let start = DEBUG_SOURCE.find("print").expect("fixture statement");
    let end = start + "print café;".len();
    SourceSpan {
        source_id: run.source_id,
        revision: run.source_revision,
        start: text_position(DEBUG_SOURCE, start),
        end: text_position(DEBUG_SOURCE, end),
    }
}

fn worker_event(run: RunBinding, sequence: u64, payload: WorkerEvent) -> ModelEvent {
    ModelEvent::Supervisor(SupervisorModelEvent::Worker(Box::new(Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: run.worker_session_id,
        run_id: run.run_id,
        source_revision: run.source_revision,
        request_id: RequestId(1),
        sequence: EventSequence(sequence),
        payload,
    })))
}

fn running_model(mode: RunMode) -> (AppModel, RunBinding) {
    let mut model = AppModel::new();
    let stamp = model.document().expect("document").stamp();
    assert!(
        apply_event(
            &mut model,
            ModelEvent::Ui(UiAction::Edit {
                document: stamp,
                text: DEBUG_SOURCE.to_owned(),
            }),
        )
        .is_empty()
    );
    let effects = apply_event(
        &mut model,
        ModelEvent::Ui(if mode == RunMode::Run {
            UiAction::Run
        } else {
            UiAction::Debug
        }),
    );
    let [ModelEffect::Start(intent)] = effects.as_slice() else {
        panic!("expected start intent");
    };
    let client_start_id = intent.client_start_id;
    let run = RunBinding {
        worker_session_id: WorkerSessionId(51),
        run_id: RunId(1),
        source_id: SourceId(51),
        source_revision: RevisionId(1),
    };
    assert!(
        apply_event(
            &mut model,
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
    (model, run)
}

fn debug_snapshot(run: RunBinding) -> VmSnapshot {
    let span = debug_span(run);
    VmSnapshot {
        reason: SnapshotReason::Faulted,
        current_span: span,
        frames: vec![
            FrameSnapshot {
                activation_id: ActivationId(7),
                function: "inner".to_owned(),
                function_truncated: false,
                current_span: span,
                call_site: Some(span),
                parameters: Vec::new(),
                parameters_truncated: false,
                locals: vec![BindingSnapshot {
                    binding_id: None,
                    name: "café".to_owned(),
                    name_truncated: false,
                    binding_kind: "local".to_owned(),
                    value_kind: ValueKind::Number,
                    value: DebugValue::Number("1".to_owned()),
                }],
                locals_truncated: false,
                upvalues: Vec::new(),
                upvalues_truncated: false,
            },
            FrameSnapshot {
                activation_id: ActivationId(8),
                function: "script".to_owned(),
                function_truncated: false,
                current_span: span,
                call_site: None,
                parameters: Vec::new(),
                parameters_truncated: false,
                locals: Vec::new(),
                locals_truncated: false,
                upvalues: Vec::new(),
                upvalues_truncated: false,
            },
        ],
        frames_truncated: false,
        globals: Vec::new(),
        globals_truncated: false,
    }
}

fn faulted_model() -> (AppModel, RunBinding) {
    let (mut model, run) = running_model(RunMode::Debug);
    let span = debug_span(run);
    let diagnostic = WireDiagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "runtime.test".to_owned(),
        code_truncated: false,
        message: "boom".to_owned(),
        message_truncated: false,
        span,
        frames: vec![WireRuntimeFrame {
            function: "inner".to_owned(),
            function_truncated: false,
            span,
        }],
        frames_truncated: false,
    };
    assert!(
        apply_event(
            &mut model,
            worker_event(
                run,
                2,
                WorkerEvent::Diagnostic {
                    diagnostic: diagnostic.clone(),
                },
            ),
        )
        .is_empty()
    );
    assert!(
        apply_event(
            &mut model,
            worker_event(
                run,
                3,
                WorkerEvent::Faulted {
                    diagnostic,
                    snapshot: debug_snapshot(run),
                },
            ),
        )
        .is_empty()
    );
    (model, run)
}

fn paused_model() -> (AppModel, RunBinding) {
    let (mut model, run) = running_model(RunMode::Debug);
    let span = debug_span(run);
    let mut snapshot = debug_snapshot(run);
    snapshot.reason = SnapshotReason::Paused(PauseReason::DebugPoint);
    assert!(
        apply_event(
            &mut model,
            worker_event(
                run,
                2,
                WorkerEvent::Paused {
                    location: PauseLocation {
                        source_id: run.source_id,
                        revision: run.source_revision,
                        span,
                        debug_point_id: DebugPointId(1),
                        activation_id: snapshot.frames[0].activation_id,
                        dynamic_event: 1,
                    },
                    snapshot,
                },
            ),
        )
        .is_empty()
    );
    (model, run)
}

fn named_model() -> AppModel {
    let mut model = AppModel::new();
    let effects = apply_event(&mut model, ModelEvent::Ui(UiAction::Save));
    let [ModelEffect::PickSaveAs { operation_id, .. }] = effects.as_slice() else {
        panic!("untitled save must request a path");
    };
    let operation_id = *operation_id;
    let [ModelEffect::WriteFile { .. }] = apply_event(
        &mut model,
        ModelEvent::File(FileModelEvent::SavePicked {
            operation_id,
            path: Some(PathBuf::from("named.ox")),
        }),
    )
    .as_slice() else {
        panic!("selected path must be written");
    };
    assert!(
        apply_event(
            &mut model,
            ModelEvent::File(FileModelEvent::WriteFinished {
                operation_id,
                result: Ok(()),
            }),
        )
        .is_empty()
    );
    model
}

#[test]
fn idle_app_exposes_one_complete_action_catalog() {
    let app = OxideApp::headless(AppModel::new());
    assert_eq!(
        app.model().document().unwrap().display_name(),
        "untitled.ox"
    );
    let actual = app
        .action_catalog()
        .iter()
        .map(|spec| (spec.action, spec.enabled))
        .collect::<Vec<_>>();

    assert_eq!(
        actual,
        vec![
            (AppAction::New, true),
            (AppAction::Open, true),
            (AppAction::Save, true),
            (AppAction::SaveAs, true),
            (AppAction::CloseDocument, true),
            (AppAction::Exit, true),
            (AppAction::Run, true),
            (AppAction::Debug, true),
            (AppAction::Pause, false),
            (AppAction::Continue, false),
            (AppAction::StepInto, false),
            (AppAction::StepOver, false),
            (AppAction::StepOut, false),
            (AppAction::Stop, false),
        ]
    );
}

#[test]
fn permanent_runtime_disconnect_keeps_execution_actions_disabled_after_file_actions() {
    let mut app = OxideApp::headless(AppModel::new());
    app.queue_event(ModelEvent::Supervisor(
        SupervisorModelEvent::RuntimeDisconnected,
    ));
    assert_eq!(app.pump_events(32), 1);

    for spec in app
        .action_catalog()
        .iter()
        .filter(|spec| spec.section == oxide_ide::ActionSection::Debug)
    {
        assert!(!spec.enabled, "{} remained enabled", spec.label);
    }

    app.queue_action(AppAction::New);
    assert_eq!(app.pump_events(32), 1);
    assert!(
        app.action_catalog()
            .iter()
            .filter(|spec| spec.section == oxide_ide::ActionSection::Debug)
            .all(|spec| !spec.enabled)
    );
}

#[test]
fn headless_app_renders_the_required_accessible_shell() {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(|_| OxideApp::headless(AppModel::new()));

    harness.get_by_label("Source editor");
    harness.get_by_label("Output");
    harness.get_by_label("Problems");
    harness.get_by_label("Symbols");
    harness.get_by_label("Call Stack");
    harness.get_by_label("Ready");

    let run = harness.get_by_label("Run all");
    assert!(!run.accesskit_node().is_disabled());
    harness.get_by_label("Debug").click();
    harness.run();
    let pause = harness.get_by_label("Pause");
    assert!(pause.accesskit_node().is_disabled());
}

#[test]
fn ctrl_f5_runs_once_while_the_editor_has_focus() {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(|_| OxideApp::headless(AppModel::new()));
    harness
        .get_by_role(eframe::egui::accesskit::Role::MultilineTextInput)
        .focus();
    harness.run();

    harness.key_press_modifiers(eframe::egui::Modifiers::CTRL, eframe::egui::Key::F5);
    harness.run();

    let effects = harness.state_mut().take_pending_effects();
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        &effects[0],
        ModelEffect::Start(intent) if intent.mode == RunMode::Run
    ));
}

#[test]
fn ctrl_shift_s_wins_over_ctrl_s() {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(|_| OxideApp::headless(named_model()));

    harness.key_press_modifiers(
        eframe::egui::Modifiers {
            ctrl: true,
            shift: true,
            ..eframe::egui::Modifiers::NONE
        },
        eframe::egui::Key::S,
    );
    harness.run();

    let effects = harness.state_mut().take_pending_effects();
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects[0], ModelEffect::PickSaveAs { .. }));
}

#[test]
fn disabled_shift_f5_does_not_fall_through_to_start_debugging() {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(|_| OxideApp::headless(AppModel::new()));

    harness.key_press_modifiers(eframe::egui::Modifiers::SHIFT, eframe::egui::Key::F5);
    harness.run();

    assert!(harness.state_mut().take_pending_effects().is_empty());
    assert_eq!(
        harness.state().model().execution_state(),
        oxide_ide::ExecutionViewState::Idle
    );
}

#[test]
fn f5_continues_when_debugging_is_paused() {
    let (model, run) = paused_model();
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(move |_| OxideApp::headless(model));

    harness.key_press(eframe::egui::Key::F5);
    harness.run();
    harness.run();

    assert!(matches!(
        harness.state_mut().take_pending_effects().as_slice(),
        [ModelEffect::SubmitCommand(intent)]
            if intent.run == run && matches!(intent.command, ExecutionCommand::Continue)
    ));
}

#[test]
fn same_pass_edit_is_reduced_before_run_snapshots_the_source() {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(|_| OxideApp::headless(AppModel::new()));
    let editor = harness.get_by_role(eframe::egui::accesskit::Role::MultilineTextInput);
    editor.focus();
    harness.run();

    harness
        .get_by_role(eframe::egui::accesskit::Role::MultilineTextInput)
        .type_text("print 42;");
    harness.key_press_modifiers(eframe::egui::Modifiers::CTRL, eframe::egui::Key::F5);
    harness.run();
    harness.run();

    let effects = harness.state_mut().take_pending_effects();
    let [ModelEffect::Start(intent)] = effects.as_slice() else {
        panic!("expected one start effect, got {effects:?}");
    };
    assert_eq!(intent.normalized_source.as_ref(), "print 42;");
}

#[test]
fn same_frame_edit_is_reduced_before_native_close_checks_dirty_state() {
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(|_| OxideApp::headless(AppModel::new()));
    harness
        .get_by_role(eframe::egui::accesskit::Role::MultilineTextInput)
        .focus();
    harness.run();

    harness
        .get_by_role(eframe::egui::accesskit::Role::MultilineTextInput)
        .type_text("print 42;");
    harness
        .input_mut()
        .viewports
        .get_mut(&eframe::egui::ViewportId::ROOT)
        .expect("root viewport")
        .events
        .push(eframe::egui::ViewportEvent::Close);
    harness.run();
    harness.run();

    assert_eq!(
        harness.state().model().document().unwrap().text(),
        "print 42;"
    );
    assert!(harness.state().model().document().unwrap().is_dirty());
    assert!(matches!(
        harness.state_mut().take_pending_effects().as_slice(),
        [ModelEffect::PromptUnsaved { .. }]
    ));
}

#[test]
fn model_event_pump_is_bounded_and_leaves_the_remainder_ordered() {
    let mut app = OxideApp::headless(AppModel::new());
    for _ in 0..33 {
        app.queue_action(AppAction::New);
    }

    assert_eq!(app.pump_events(32), 32);
    assert_eq!(app.pending_event_count(), 1);
    assert_eq!(app.model().document().unwrap().id().get(), 33);

    assert_eq!(app.pump_events(32), 1);
    assert_eq!(app.pending_event_count(), 0);
    assert_eq!(app.model().document().unwrap().id().get(), 34);
}

#[test]
fn essential_controls_remain_on_screen_at_scaled_classroom_sizes() {
    for size in [
        eframe::egui::vec2(911.0, 512.0),
        eframe::egui::vec2(683.0, 384.0),
        eframe::egui::vec2(640.0, 360.0),
    ] {
        let harness = Harness::builder()
            .with_size(size)
            .build_eframe(|_| OxideApp::headless(AppModel::new()));
        let screen = eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO, size);

        for label in [
            "File",
            "Debug",
            "Run all",
            "Stop",
            "Source editor",
            "Ready",
            "Hide console",
            "Hide inspector",
        ] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                screen.contains(rect.min) && screen.contains(rect.max),
                "{label} is outside {size:?}: {rect:?}"
            );
        }
    }
}

#[test]
fn line_gutter_exposes_concise_accessibility_metadata_at_the_line_limit() {
    let mut model = AppModel::new();
    let stamp = model.document().unwrap().stamp();
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: "\n".repeat(oxide_ide::MAX_SOURCE_LINES - 1),
        }),
    );
    let harness = Harness::builder()
        .with_size(eframe::egui::vec2(683.0, 384.0))
        .build_eframe(move |_| OxideApp::headless(model));

    let gutter = harness.get_by_label("Line number gutter");
    assert_eq!(gutter.value().as_deref(), Some("Line number gutter"));
}

#[test]
fn horizontal_editor_scroll_keeps_the_line_gutter_fixed() {
    let mut model = AppModel::new();
    let stamp = model.document().unwrap().stamp();
    let source = format!("print \"{}\";", "wide".repeat(200));
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: source.clone(),
        }),
    );
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(683.0, 384.0))
        .build_eframe(move |_| OxideApp::headless(model));
    let initial_gutter_x = harness.get_by_label("Line number gutter").rect().left();

    harness
        .get_by_role(eframe::egui::accesskit::Role::MultilineTextInput)
        .hover();
    harness.run();
    harness.event(eframe::egui::Event::MouseWheel {
        unit: eframe::egui::MouseWheelUnit::Point,
        delta: eframe::egui::vec2(-180.0, 0.0),
        modifiers: eframe::egui::Modifiers::NONE,
        phase: eframe::egui::TouchPhase::Move,
    });
    harness.run();

    assert_eq!(
        harness.get_by_label("Line number gutter").rect().left(),
        initial_gutter_x
    );
}

#[test]
fn moving_the_caret_to_the_last_line_scrolls_the_shared_vertical_editor() {
    let mut model = AppModel::new();
    let stamp = model.document().unwrap().stamp();
    let source = (1..=120)
        .map(|line| format!("print {line};"))
        .collect::<Vec<_>>()
        .join("\n");
    apply_event(
        &mut model,
        ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: source,
        }),
    );
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(640.0, 360.0))
        .build_eframe(move |_| OxideApp::headless(model));
    let initial_gutter_top = harness.get_by_label("Line number gutter").rect().top();
    harness
        .get_by_role(eframe::egui::accesskit::Role::MultilineTextInput)
        .focus();
    harness.run();

    harness.key_press_modifiers(eframe::egui::Modifiers::CTRL, eframe::egui::Key::End);
    harness.run();
    harness.run();

    assert!(harness.get_by_label("Line number gutter").rect().top() < initial_gutter_top - 50.0);
}

#[test]
fn problem_and_frame_buttons_emit_exact_provenance_and_symbols_are_visible() {
    let (model, run) = faulted_model();
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(move |_| OxideApp::headless(model));

    harness.get_by_label("Problems").click();
    harness.run();
    harness
        .get_by_label("Error runtime, line 2, column 1: boom")
        .click();
    harness.run();
    harness.run();
    let effects = harness.state_mut().take_pending_effects();
    assert!(matches!(
        effects.as_slice(),
        [ModelEffect::Navigate {
            document: _,
            run: actual,
            span: _
        }] if *actual == run
    ));

    harness.get_by_label("Call Stack").click();
    harness.run();
    let frame_label = {
        let frame = &harness
            .state()
            .model()
            .retained_snapshot()
            .expect("snapshot")
            .snapshot()
            .frames[1];
        frame_accessible_name(frame, 1, false)
    };
    harness.get_by_label(&frame_label).click();
    harness.run();
    harness.run();
    assert_eq!(
        harness.state().model().selected_activation(),
        Some(ActivationId(8))
    );
    assert!(matches!(
        harness.state_mut().take_pending_effects().as_slice(),
        [ModelEffect::Navigate { run: actual, .. }] if *actual == run
    ));

    harness.get_by_label("Symbols").click();
    harness.run();
    harness.get_by_label("No locals.");
    let first_frame_label = {
        let frame = &harness
            .state()
            .model()
            .retained_snapshot()
            .expect("snapshot")
            .snapshot()
            .frames[0];
        frame_accessible_name(frame, 0, false)
    };
    harness.get_by_label("Call Stack").click();
    harness.run();
    harness.get_by_label(&first_frame_label).click();
    harness.run();
    harness.run();
    harness.state_mut().take_pending_effects();
    harness.get_by_label("Symbols").click();
    harness.run();
    harness.get_by_label("café · local · number = 1");
}

#[test]
fn pending_input_submits_once_with_the_exact_request_identity() {
    let (mut model, run) = running_model(RunMode::Run);
    assert!(
        apply_event(
            &mut model,
            worker_event(
                run,
                2,
                WorkerEvent::InputRequested {
                    prompt: "value: ".to_owned(),
                },
            ),
        )
        .is_empty()
    );
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(move |_| OxideApp::headless(model));
    let input = harness.get_by_role(eframe::egui::accesskit::Role::TextInput);
    input.focus();
    harness.run();
    harness
        .get_by_role(eframe::egui::accesskit::Role::TextInput)
        .type_text("kamusta 🦀");
    harness.get_by_label("Send input").click();
    harness.run();
    harness.run();

    let effects = harness.state_mut().take_pending_effects();
    let [ModelEffect::SubmitCommand(intent)] = effects.as_slice() else {
        panic!("expected one input command, got {effects:?}");
    };
    assert!(matches!(
        &intent.command,
        ExecutionCommand::ProvideInput { in_reply_to, text }
            if *in_reply_to == RequestId(1) && text == "kamusta 🦀"
    ));

    harness.run();
    harness.run();
    assert!(harness.state_mut().take_pending_effects().is_empty());
}

#[test]
fn new_input_request_reveals_output_and_focuses_the_input_field() {
    let (model, run) = running_model(RunMode::Run);
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(683.0, 384.0))
        .build_eframe(move |_| OxideApp::headless(model));
    harness.get_by_label("Problems").click();
    harness.run();

    harness.state_mut().queue_event(worker_event(
        run,
        2,
        WorkerEvent::InputRequested {
            prompt: "value: ".to_owned(),
        },
    ));
    harness.run();
    harness.run();

    harness.get_by_label("Input requested: value: ");
    let input = harness.get_by_role(eframe::egui::accesskit::Role::TextInput);
    assert!(input.is_focused());
    let screen = eframe::egui::Rect::from_min_size(
        eframe::egui::Pos2::ZERO,
        eframe::egui::vec2(683.0, 384.0),
    );
    assert!(screen.contains(input.rect().min) && screen.contains(input.rect().max));
}

#[test]
fn diagnostics_and_output_reveal_their_console_tabs() {
    let (model, run) = running_model(RunMode::Run);
    let span = debug_span(run);
    let diagnostic = WireDiagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "runtime.test".to_owned(),
        code_truncated: false,
        message: "boom".to_owned(),
        message_truncated: false,
        span,
        frames: Vec::new(),
        frames_truncated: false,
    };
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1_100.0, 700.0))
        .build_eframe(move |_| OxideApp::headless(model));

    harness
        .state_mut()
        .queue_event(worker_event(run, 2, WorkerEvent::Diagnostic { diagnostic }));
    harness.run();
    harness.run();
    assert_eq!(
        harness.get_by_label("Problems").accesskit_node().toggled(),
        Some(eframe::egui::accesskit::Toggled::True)
    );

    harness.state_mut().queue_event(worker_event(
        run,
        3,
        WorkerEvent::Output {
            text: "recovered\n".to_owned(),
        },
    ));
    harness.run();
    harness.run();
    assert_eq!(
        harness.get_by_label("Output").accesskit_node().toggled(),
        Some(eframe::egui::accesskit::Toggled::True)
    );
}

use eframe::egui::{Key, KeyboardShortcut, Modifiers};
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
use rlox_protocol::{
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
            (AppAction::GoToDefinition, false),
        ]
    );
}

#[test]
fn go_to_definition_is_a_plain_f12_navigation_action_without_a_toolbar_button() {
    let app = OxideApp::headless(AppModel::new());
    let catalog = app.action_catalog();
    let definition = catalog
        .iter()
        .find(|spec| spec.action == AppAction::GoToDefinition)
        .expect("go to definition action");

    assert_eq!(definition.section, oxide_ide::ActionSection::Navigate);
    assert_eq!(definition.label, "Go to Definition");
    assert_eq!(
        definition.shortcut,
        Some(KeyboardShortcut::new(Modifiers::NONE, Key::F12))
    );
    assert!(!definition.show_in_toolbar);

    for (action, shortcut) in [
        (
            AppAction::Debug,
            KeyboardShortcut::new(Modifiers::NONE, Key::F5),
        ),
        (
            AppAction::StepOver,
            KeyboardShortcut::new(Modifiers::NONE, Key::F10),
        ),
        (
            AppAction::StepInto,
            KeyboardShortcut::new(Modifiers::NONE, Key::F11),
        ),
    ] {
        assert_eq!(
            catalog
                .iter()
                .find(|spec| spec.action == action)
                .and_then(|spec| spec.shortcut),
            Some(shortcut),
            "{action:?} shortcut changed"
        );
    }
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
    harness.get_by_label("Language: unavailable");
    harness.get_by_label("Navigate").click();
    harness.run();
    assert!(
        harness
            .get_by_label("Go to Definition F12")
            .accesskit_node()
            .is_disabled()
    );

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
    assert!(
        matches!(
            effects.as_slice(),
            [ModelEffect::Navigate {
                navigation_generation,
                document: _,
                run: actual,
                span: _
            }] if navigation_generation.get() == 1 && *actual == run
        ),
        "unexpected effects: {effects:#?}; selected={:?}; queued={}",
        harness.state().model().selected_problem(),
        harness.state().pending_event_count()
    );

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
        [ModelEffect::Navigate {
            navigation_generation,
            run: actual,
            ..
        }] if navigation_generation.get() == 2 && *actual == run
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

mod language_worker_process {
    use lsp_server::Message;
    use oxide_ide::LANGUAGE_WORKER_ARGUMENT;
    use std::{
        io::{self, BufReader, Cursor, Write},
        process::{Child, Command, Output, Stdio},
        thread,
        time::{Duration, Instant},
    };

    const MAIN_EXIT_DEADLINE: Duration = Duration::from_secs(5);
    const CLEANUP_DEADLINE: Duration = Duration::from_millis(500);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    struct ChildOwner {
        child: Option<Child>,
    }

    impl ChildOwner {
        fn new(child: Child) -> Self {
            Self { child: Some(child) }
        }

        fn child_mut(&mut self) -> &mut Child {
            self.child.as_mut().expect("child owner is populated")
        }

        fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            self.child_mut().try_wait()
        }

        fn kill(&mut self) -> io::Result<()> {
            self.child_mut().kill()
        }

        fn exited_output(mut self) -> io::Result<Output> {
            self.child
                .take()
                .expect("exited child is still owned")
                .wait_with_output()
        }
    }

    impl Drop for ChildOwner {
        fn drop(&mut self) {
            let Some(child) = self.child.as_mut() else {
                return;
            };
            if let Err(error) = bounded_child_cleanup(child) {
                eprintln!("language worker test cleanup failed: {error}");
            }
        }
    }

    fn bounded_child_cleanup(child: &mut Child) -> Result<(), String> {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait()
                    .map(|_| ())
                    .map_err(|error| format!("could not reap exited child: {error}"));
            }
            Ok(None) => {}
            Err(error) => eprintln!("language worker cleanup status check failed: {error}"),
        }

        let kill_error = child.kill().err();
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let reap_error = child.wait().err();
                    return match (kill_error, reap_error) {
                        (None, None) => Ok(()),
                        (Some(kill_error), None) => Err(format!(
                            "kill failed: {kill_error}; child exited during bounded cleanup"
                        )),
                        (None, Some(reap_error)) => {
                            Err(format!("could not reap killed child: {reap_error}"))
                        }
                        (Some(kill_error), Some(reap_error)) => Err(format!(
                            "kill failed: {kill_error}; could not reap exited child: {reap_error}"
                        )),
                    };
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    return Err(match kill_error {
                        Some(error) => format!(
                            "kill failed: {error}; child still running after bounded cleanup"
                        ),
                        None => {
                            "child still running after successful kill request and bounded cleanup"
                                .to_owned()
                        }
                    });
                }
                Err(status_error) => {
                    return Err(match kill_error {
                        Some(kill_error) => format!(
                            "kill failed: {kill_error}; cleanup status check failed: {status_error}"
                        ),
                        None => format!("cleanup status check failed: {status_error}"),
                    });
                }
            }
        }
    }

    fn frame(value: serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(&value).expect("request is serializable");
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(&body);
        framed
    }

    fn wait_for_child(mut child: ChildOwner) -> Output {
        let deadline = Instant::now() + MAIN_EXIT_DEADLINE;
        loop {
            match child.try_wait().expect("child status is readable") {
                Some(_) => {
                    return child
                        .exited_output()
                        .expect("exited child output is readable");
                }
                None if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                None => {
                    let kill_result = child.kill();
                    let kill_report = match &kill_result {
                        Ok(()) => "kill request succeeded".to_owned(),
                        Err(error) => format!("kill request failed: {error}"),
                    };
                    let cleanup_deadline = Instant::now() + CLEANUP_DEADLINE;
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => {
                                let output = child
                                    .exited_output()
                                    .expect("exited timed-out child output is readable");
                                panic!(
                                    "language worker did not exit within five seconds; {kill_report}; stdout: {}; stderr: {}",
                                    String::from_utf8_lossy(&output.stdout),
                                    String::from_utf8_lossy(&output.stderr)
                                );
                            }
                            Ok(None) if Instant::now() < cleanup_deadline => {
                                thread::sleep(POLL_INTERVAL);
                            }
                            Ok(None) => panic!(
                                "language worker did not exit within five seconds; {kill_report}; child remained running after bounded cleanup"
                            ),
                            Err(status_error) => panic!(
                                "language worker did not exit within five seconds; {kill_report}; cleanup status check failed: {status_error}"
                            ),
                        }
                    }
                }
            }
        }
    }

    fn parse_stdout(stdout: Vec<u8>) -> Vec<serde_json::Value> {
        let mut reader = BufReader::new(Cursor::new(stdout));
        let mut messages = Vec::new();
        while let Some(message) =
            Message::read(&mut reader).expect("stdout contains only framed LSP messages")
        {
            messages.push(serde_json::to_value(message).expect("LSP message is serializable"));
        }
        messages
    }

    fn published_diagnostics(messages: &[serde_json::Value], version: i64) -> &serde_json::Value {
        messages
            .iter()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["version"] == version
            })
            .unwrap_or_else(|| panic!("missing diagnostics for version {version}: {messages:#?}"))
    }

    fn assert_error(
        messages: &[serde_json::Value],
        version: i64,
        code: &str,
        phase: &str,
        start: (u64, u64),
        end: (u64, u64),
    ) {
        let published = published_diagnostics(messages, version);
        let diagnostic = published["params"]["diagnostics"]
            .as_array()
            .expect("diagnostics is an array")
            .iter()
            .find(|diagnostic| diagnostic["code"] == code)
            .unwrap_or_else(|| panic!("missing {code}: {published:#?}"));
        assert_eq!(diagnostic["severity"], 1);
        assert_eq!(diagnostic["source"], "rlox");
        assert_eq!(diagnostic["data"]["phase"], phase);
        assert_eq!(diagnostic["range"]["start"]["line"], start.0);
        assert_eq!(diagnostic["range"]["start"]["character"], start.1);
        assert_eq!(diagnostic["range"]["end"]["line"], end.0);
        assert_eq!(diagnostic["range"]["end"]["character"], end.1);
    }

    #[test]
    fn language_worker_publishes_all_diagnostic_phases_and_clears_them() {
        let uri = "file:///workspace/diagnostics.lox";
        let input = [
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "capabilities": {
                        "general": {"positionEncodings": ["utf-16"]},
                        "textDocument": {
                            "publishDiagnostics": {
                                "versionSupport": true,
                                "dataSupport": true
                            }
                        }
                    }
                }
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {}
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "lox",
                        "version": 1,
                        "text": "\u{feff}\"😀\" @\r\n"
                    }
                }
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": {"uri": uri, "version": 2},
                    "contentChanges": [{"text": "\u{feff}//😀\r\nvar = 1;"}]
                }
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": {"uri": uri, "version": 3},
                    "contentChanges": [{"text": "//😀\rreturn;"}]
                }
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": {"uri": uri, "version": 4},
                    "contentChanges": [{"text": "var greeting = \"😀\";\r\nprint greeting;"}]
                }
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "shutdown",
                "params": null
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "exit",
                "params": null
            }),
        ]
        .into_iter()
        .flat_map(frame)
        .collect::<Vec<_>>();

        let mut child = ChildOwner::new(
            Command::new(env!("CARGO_BIN_EXE_oxide-ide"))
                .arg(LANGUAGE_WORKER_ARGUMENT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("language worker starts"),
        );
        let mut stdin = child
            .child_mut()
            .stdin
            .take()
            .expect("language worker stdin");
        stdin.write_all(&input).expect("write language session");
        drop(stdin);

        let output = wait_for_child(child);
        assert!(
            output.status.success(),
            "language worker failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let messages = parse_stdout(output.stdout);
        assert_eq!(messages.len(), 6, "unexpected LSP output: {messages:#?}");
        let initialize = &messages[0];
        assert_eq!(initialize["id"], 1);
        assert_eq!(
            initialize["result"]["capabilities"]["positionEncoding"],
            "utf-16"
        );
        for (message, version) in messages[1..=4].iter().zip(1..=4) {
            assert_eq!(message["method"], "textDocument/publishDiagnostics");
            assert_eq!(message["params"]["version"], version);
        }
        assert_error(&messages, 1, "scanner.error", "scanner", (0, 6), (0, 7));
        assert_error(&messages, 2, "parser.error", "parser", (1, 4), (1, 5));
        assert_error(&messages, 3, "compiler.error", "compiler", (1, 0), (1, 6));
        assert!(
            published_diagnostics(&messages, 4)["params"]["diagnostics"]
                .as_array()
                .expect("diagnostics is an array")
                .is_empty()
        );
        let shutdown = &messages[5];
        assert_eq!(shutdown["id"], 9);
        assert_eq!(shutdown["result"], serde_json::Value::Null);
    }
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

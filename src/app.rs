use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io;
use std::time::Duration;

use eframe::egui::text::CCursorRange;
use eframe::egui::{
    self, Button, CentralPanel, Color32, Frame, Key, KeyboardShortcut, MenuBar, Modifiers, Panel,
    RichText, ScrollArea, TextEdit,
};

use crate::file_dialog::{
    FileEventReceiver, FileExecutor, FileJob, FileSubmitError, show_open_dialog, show_save_dialog,
    show_unsaved_dialog,
};
use crate::{
    AppModel, BindingScope, BoundedTextBuffer, DocumentStamp, EditorSourceKey, ExecutionViewState,
    FileFailureKind, MarkerInputs, MarkerKind, MarkerMask, MarkerPlan, MarkerSpan, ModelEffect,
    ModelEvent, ModelStatus, PresentedBinding, PresentedValue, PresentedValueState, RequestId,
    RuntimeCoordinator, RuntimeDispatchError, SourceGrowthLimit, SourceMapper, SupervisorConfig,
    SupervisorModelEvent, UiAction, ValuePath, apply_event, build_layout_job, escape_display_text,
    frame_accessible_name, gutter_digits, present_binding, snapshot_accessible_name,
    snapshot_provenance_label,
};

const MAX_PENDING_EVENTS_PER_PASS: usize = 32;
const MAX_ADAPTER_EVENTS_PER_PASS: usize = 32;
const MAX_EFFECTS_PER_PASS: usize = 32;
const ADAPTER_RETRY_DELAY: Duration = Duration::from_millis(4);
pub const APP_NAME: &str = "Oxide IDE";
const SHORTCUT_PRIORITY: [AppAction; 11] = [
    AppAction::SaveAs,
    AppAction::Save,
    AppAction::Run,
    AppAction::Stop,
    AppAction::StepOut,
    AppAction::Debug,
    AppAction::Continue,
    AppAction::StepInto,
    AppAction::New,
    AppAction::Open,
    AppAction::StepOver,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AppAction {
    New,
    Open,
    Save,
    SaveAs,
    CloseDocument,
    Exit,
    Run,
    Debug,
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionSection {
    File,
    Debug,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionSpec {
    pub action: AppAction,
    pub section: ActionSection,
    pub label: &'static str,
    pub shortcut: Option<KeyboardShortcut>,
    pub enabled: bool,
    pub show_in_toolbar: bool,
}

impl AppAction {
    pub fn ui_action(self) -> UiAction {
        match self {
            Self::New => UiAction::New,
            Self::Open => UiAction::Open,
            Self::Save => UiAction::Save,
            Self::SaveAs => UiAction::SaveAs,
            Self::CloseDocument => UiAction::CloseDocument,
            Self::Exit => UiAction::RequestExit,
            Self::Run => UiAction::Run,
            Self::Debug => UiAction::Debug,
            Self::Pause => UiAction::Pause,
            Self::Continue => UiAction::Continue,
            Self::StepInto => UiAction::StepInto,
            Self::StepOver => UiAction::StepOver,
            Self::StepOut => UiAction::StepOut,
            Self::Stop => UiAction::Stop,
        }
    }
}

pub struct OxideApp {
    model: AppModel,
    backend: AppBackend,
    queued_events: VecDeque<ModelEvent>,
    deferred_actions: VecDeque<AppAction>,
    deferred_ui_actions: VecDeque<UiAction>,
    pending_effects: VecDeque<ModelEffect>,
    editor_text: String,
    editor_stamp: Option<DocumentStamp>,
    editor_mapper: Option<SourceMapper>,
    editor_cursor_range: Option<CCursorRange>,
    pending_navigation: Option<MarkerSpan>,
    navigation_focus_pending: bool,
    editor_notice: Option<String>,
    input_text: String,
    input_request: Option<RequestId>,
    input_submitted_for: Option<RequestId>,
    input_focus_pending: bool,
    adapter_error: Option<String>,
    runtime_disconnect_reported: bool,
    file_disconnect_reported: bool,
    accepted_file_job: Option<FileJob>,
    console_tab: ConsoleTab,
    inspector_tab: InspectorTab,
    console_expanded: bool,
    inspector_expanded: bool,
    theme_configured: bool,
    allow_native_close: bool,
    native_close_pending: bool,
    native_close_request_enqueued: bool,
}

enum AppBackend {
    Headless,
    Native {
        runtime: RuntimeCoordinator,
        files: FileExecutor,
        file_events: FileEventReceiver,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsoleTab {
    Output,
    Problems,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InspectorTab {
    Symbols,
    CallStack,
}

pub fn run_visible() -> eframe::Result<()> {
    let icon = load_app_icon()?;
    let viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_app_id("oxide-ide")
        .with_icon(icon)
        .with_inner_size([1_100.0, 700.0])
        .with_min_inner_size([640.0, 360.0])
        .with_resizable(true)
        .with_clamp_size_to_monitor_size(true);
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| {
            let app = OxideApp::native(cc)
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}

fn load_app_icon() -> eframe::Result<egui::IconData> {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/oxide-ide.png"))
        .map_err(|error| eframe::Error::AppCreation(Box::new(error)))
}

impl OxideApp {
    pub fn headless(model: AppModel) -> Self {
        let (editor_stamp, editor_text) = model.document().map_or_else(
            || (None, String::new()),
            |document| (Some(document.stamp()), document.text().to_owned()),
        );
        Self {
            model,
            backend: AppBackend::Headless,
            queued_events: VecDeque::new(),
            deferred_actions: VecDeque::new(),
            deferred_ui_actions: VecDeque::new(),
            pending_effects: VecDeque::new(),
            editor_text,
            editor_stamp,
            editor_mapper: None,
            editor_cursor_range: None,
            pending_navigation: None,
            navigation_focus_pending: false,
            editor_notice: None,
            input_text: String::new(),
            input_request: None,
            input_submitted_for: None,
            input_focus_pending: false,
            adapter_error: None,
            runtime_disconnect_reported: false,
            file_disconnect_reported: false,
            accepted_file_job: None,
            console_tab: ConsoleTab::Output,
            inspector_tab: InspectorTab::Symbols,
            console_expanded: true,
            inspector_expanded: true,
            theme_configured: false,
            allow_native_close: false,
            native_close_pending: false,
            native_close_request_enqueued: false,
        }
    }

    pub fn native(cc: &eframe::CreationContext<'_>) -> io::Result<Self> {
        let supervisor = SupervisorConfig::current_executable().map_err(|error| {
            io::Error::new(
                error.kind,
                format!(
                    "could not locate the Oxide IDE executable ({:?})",
                    error.kind
                ),
            )
        })?;
        let runtime_context = cc.egui_ctx.clone();
        let runtime = RuntimeCoordinator::spawn(supervisor, move || {
            runtime_context.request_repaint();
        })
        .map_err(|error| {
            io::Error::new(
                error.kind,
                format!("could not start the runtime coordinator ({:?})", error.kind),
            )
        })?;
        let file_context = cc.egui_ctx.clone();
        let (files, file_events) = FileExecutor::spawn(move || {
            file_context.request_repaint();
        })?;
        let mut app = Self::headless(AppModel::new());
        app.backend = AppBackend::Native {
            runtime,
            files,
            file_events,
        };
        Ok(app)
    }

    pub fn model(&self) -> &AppModel {
        &self.model
    }

    pub fn action_catalog(&self) -> [ActionSpec; 14] {
        let execution = self.model.controls();
        let file = self.model.file_controls();
        let runtime_available = !self.runtime_disconnect_reported;
        [
            action(
                AppAction::New,
                ActionSection::File,
                "New",
                ctrl(Key::N),
                file.new,
                true,
            ),
            action(
                AppAction::Open,
                ActionSection::File,
                "Open…",
                ctrl(Key::O),
                file.open,
                true,
            ),
            action(
                AppAction::Save,
                ActionSection::File,
                "Save",
                ctrl(Key::S),
                file.save,
                true,
            ),
            action(
                AppAction::SaveAs,
                ActionSection::File,
                "Save As…",
                ctrl_shift(Key::S),
                file.save_as,
                false,
            ),
            action(
                AppAction::CloseDocument,
                ActionSection::File,
                "Close Document",
                None,
                file.close_document,
                false,
            ),
            action(
                AppAction::Exit,
                ActionSection::File,
                "Exit",
                None,
                file.exit,
                false,
            ),
            action(
                AppAction::Run,
                ActionSection::Debug,
                "Run all",
                ctrl(Key::F5),
                execution.run && runtime_available,
                true,
            ),
            action(
                AppAction::Debug,
                ActionSection::Debug,
                "Start Debugging",
                key(Key::F5),
                execution.debug && runtime_available,
                true,
            ),
            action(
                AppAction::Pause,
                ActionSection::Debug,
                "Pause",
                None,
                execution.pause && runtime_available,
                true,
            ),
            action(
                AppAction::Continue,
                ActionSection::Debug,
                "Continue",
                key(Key::F5),
                execution.continue_execution && runtime_available,
                true,
            ),
            action(
                AppAction::StepInto,
                ActionSection::Debug,
                "Step Into",
                key(Key::F11),
                execution.step_into && runtime_available,
                false,
            ),
            action(
                AppAction::StepOver,
                ActionSection::Debug,
                "Step Over",
                key(Key::F10),
                execution.step_over && runtime_available,
                true,
            ),
            action(
                AppAction::StepOut,
                ActionSection::Debug,
                "Step Out",
                shift(Key::F11),
                execution.step_out && runtime_available,
                false,
            ),
            action(
                AppAction::Stop,
                ActionSection::Debug,
                "Stop",
                shift(Key::F5),
                execution.stop && runtime_available,
                true,
            ),
        ]
    }

    pub fn queue_action(&mut self, action: AppAction) {
        self.queued_events
            .push_back(ModelEvent::Ui(action.ui_action()));
    }

    pub fn queue_event(&mut self, event: ModelEvent) {
        self.queued_events.push_back(event);
    }

    pub fn take_pending_effects(&mut self) -> Vec<ModelEffect> {
        self.pending_effects.drain(..).collect()
    }

    pub fn pending_event_count(&self) -> usize {
        self.queued_events.len()
    }

    pub fn pump_events(&mut self, limit: usize) -> usize {
        let mut reduced = 0;
        while reduced < limit {
            let Some(event) = self.queued_events.pop_front() else {
                break;
            };
            let previous_state = self.model.execution_state();
            let previous_output_bytes = self.model.program_output().len();
            let previous_output_truncated = self.model.output_was_truncated();
            let previous_problem_count = self.model.problems().len();
            let previous_problems_truncated = self.model.problems_were_truncated();
            if matches!(
                &event,
                ModelEvent::Supervisor(SupervisorModelEvent::RuntimeDisconnected)
            ) {
                self.runtime_disconnect_reported = true;
            }
            self.pending_effects
                .extend(apply_event(&mut self.model, event));
            let revealed_problem = self.model.problems().len() > previous_problem_count
                || self.model.problems_were_truncated() && !previous_problems_truncated
                || self.model.execution_state() == ExecutionViewState::Faulted
                    && previous_state != ExecutionViewState::Faulted;
            let revealed_output = self.model.program_output().len() != previous_output_bytes
                || self.model.output_was_truncated() && !previous_output_truncated
                || previous_state == ExecutionViewState::Starting
                    && matches!(
                        self.model.execution_state(),
                        ExecutionViewState::Running | ExecutionViewState::Paused
                    );
            if revealed_problem {
                self.console_tab = ConsoleTab::Problems;
                self.console_expanded = true;
            } else if revealed_output {
                self.console_tab = ConsoleTab::Output;
                self.console_expanded = true;
            }
            reduced += 1;
        }
        self.reconcile_editor_buffer();
        reduced
    }

    fn configure_theme(&mut self, ctx: &egui::Context) {
        if self.theme_configured {
            return;
        }
        ctx.set_theme(egui::Theme::Light);
        ctx.all_styles_mut(|style| {
            style.visuals.selection.bg_fill = Color32::from_rgb(42, 101, 172);
            style.visuals.selection.stroke.color = Color32::WHITE;
            style.spacing.button_padding = egui::vec2(8.0, 4.0);
        });
        self.theme_configured = true;
    }

    fn defer_action(&mut self, ctx: &egui::Context, action: AppAction) {
        self.deferred_actions.push_back(action);
        ctx.request_repaint();
    }

    fn defer_ui_action(&mut self, ctx: &egui::Context, action: UiAction) {
        self.deferred_ui_actions.push_back(action);
        ctx.request_repaint();
    }

    fn flush_deferred_actions(&mut self, ctx: &egui::Context) {
        if self.deferred_actions.is_empty() && self.deferred_ui_actions.is_empty() {
            return;
        }
        self.queued_events
            .extend(self.deferred_ui_actions.drain(..).map(ModelEvent::Ui));
        self.queued_events.extend(
            self.deferred_actions
                .drain(..)
                .map(|action| ModelEvent::Ui(action.ui_action())),
        );
        ctx.request_repaint();
    }

    fn consume_shortcut(&mut self, ctx: &egui::Context) {
        let catalog = self.action_catalog();
        let mut checked = Vec::new();
        for action in SHORTCUT_PRIORITY {
            let Some(spec) = catalog.iter().find(|spec| spec.action == action) else {
                continue;
            };
            let Some(shortcut) = spec.shortcut else {
                continue;
            };
            if checked.contains(&shortcut) {
                continue;
            }
            checked.push(shortcut);
            let enabled_action = SHORTCUT_PRIORITY.iter().find_map(|candidate| {
                catalog
                    .iter()
                    .find(|candidate_spec| candidate_spec.action == *candidate)
                    .filter(|candidate_spec| {
                        candidate_spec.shortcut == Some(shortcut) && candidate_spec.enabled
                    })
                    .map(|candidate_spec| candidate_spec.action)
            });
            if ctx.input_mut(|input| input.consume_shortcut(&shortcut)) {
                if let Some(enabled_action) = enabled_action {
                    self.defer_action(ctx, enabled_action);
                }
                break;
            }
        }
    }

    fn reduce_pending_events(&mut self, ctx: &egui::Context) {
        self.pump_events(MAX_PENDING_EVENTS_PER_PASS);
        self.reconcile_native_close_request(ctx);
        if !self.queued_events.is_empty() {
            ctx.request_repaint();
        }
    }

    fn reconcile_native_close_request(&mut self, ctx: &egui::Context) {
        if !self.native_close_pending || self.allow_native_close {
            return;
        }
        if self.native_close_request_enqueued && !self.model.exit_resolution_active() {
            self.native_close_pending = false;
            self.native_close_request_enqueued = false;
            ctx.request_repaint();
        }
    }

    fn stage_native_close_request(&mut self, ctx: &egui::Context) {
        if !self.native_close_pending
            || self.allow_native_close
            || self.native_close_request_enqueued
            || !self.model.file_controls().exit
        {
            return;
        }
        self.native_close_request_enqueued = true;
        self.queued_events
            .push_back(ModelEvent::Ui(UiAction::RequestExit));
        ctx.request_repaint();
    }

    fn reconcile_editor_buffer(&mut self) {
        let pending_request = self.model.pending_input().map(|pending| pending.request_id);
        if self.input_request != pending_request {
            self.input_request = pending_request;
            self.input_text.clear();
            self.input_submitted_for = None;
            self.input_focus_pending = pending_request.is_some();
            if pending_request.is_some() {
                self.console_tab = ConsoleTab::Output;
                self.console_expanded = true;
            }
        } else if self.model.controls().submit_input {
            self.input_submitted_for = None;
        }
        let Some(document) = self.model.document() else {
            self.editor_stamp = None;
            self.editor_mapper = None;
            self.editor_cursor_range = None;
            self.editor_text.clear();
            self.editor_notice = None;
            return;
        };
        if self.editor_stamp != Some(document.stamp()) {
            self.editor_stamp = Some(document.stamp());
            self.editor_mapper = None;
            self.editor_cursor_range = None;
            self.editor_text = document.text().to_owned();
            self.editor_notice = None;
        }
    }

    fn poll_adapters(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        let mut completed_file_events = Vec::new();
        let mut runtime_disconnected = false;
        let mut file_disconnected = false;
        if let AppBackend::Native {
            runtime,
            file_events,
            ..
        } = &self.backend
        {
            for _ in 0..MAX_ADAPTER_EVENTS_PER_PASS {
                match runtime.try_recv() {
                    Ok(Some(event)) => events.push(event),
                    Ok(None) => break,
                    Err(_) => {
                        runtime_disconnected = true;
                        break;
                    }
                }
            }
            for _ in 0..MAX_ADAPTER_EVENTS_PER_PASS {
                match file_events.try_recv() {
                    Ok(event) => completed_file_events.push(event),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        file_disconnected = true;
                        break;
                    }
                }
            }
        }
        for event in completed_file_events {
            if self
                .accepted_file_job
                .as_ref()
                .is_some_and(|job| job.matches_event(&event))
            {
                self.accepted_file_job = None;
            }
            events.push(ModelEvent::File(event));
        }
        if !events.is_empty() {
            self.queued_events.extend(events);
            ctx.request_repaint();
        }
        if runtime_disconnected && !self.runtime_disconnect_reported {
            self.handle_runtime_disconnect("The execution service stopped unexpectedly.");
            ctx.request_repaint();
        }
        if file_disconnected {
            self.handle_file_disconnect();
            ctx.request_repaint();
        }
    }

    fn dispatch_native_effects(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if !matches!(self.backend, AppBackend::Native { .. }) {
            return;
        }
        for _ in 0..MAX_EFFECTS_PER_PASS {
            let Some(effect) = self.pending_effects.front().cloned() else {
                break;
            };
            let mut committed = true;
            let mut stop_after_commit = false;
            match effect.clone() {
                ModelEffect::Start(_)
                | ModelEffect::SubmitCommand(_)
                | ModelEffect::CloseWorker { .. } => {
                    let AppBackend::Native { runtime, .. } = &self.backend else {
                        unreachable!("native effect dispatch requires native adapters");
                    };
                    match runtime.try_dispatch(effect.clone()) {
                        Ok(()) => {}
                        Err(RuntimeDispatchError::Full(_)) => {
                            committed = false;
                        }
                        Err(RuntimeDispatchError::Closed(_)) => {
                            self.handle_runtime_disconnect("The execution service is unavailable.");
                        }
                        Err(RuntimeDispatchError::Unsupported(_)) => {
                            unreachable!("only runtime effects reach the runtime coordinator");
                        }
                    }
                }
                ModelEffect::PromptUnsaved { operation_id, .. } => {
                    let display_name = self.model.document().map_or_else(
                        || "this document".to_owned(),
                        |document| document.display_name(),
                    );
                    let choice = show_unsaved_dialog(frame, &display_name);
                    self.queued_events
                        .push_back(ModelEvent::Ui(UiAction::ResolveUnsaved {
                            operation_id,
                            choice,
                        }));
                }
                ModelEffect::PickOpen { operation_id } => {
                    self.queued_events
                        .push_back(ModelEvent::File(show_open_dialog(frame, operation_id)));
                }
                ModelEffect::PickSaveAs {
                    operation_id,
                    suggested_path,
                } => {
                    self.queued_events
                        .push_back(ModelEvent::File(show_save_dialog(
                            frame,
                            operation_id,
                            suggested_path.as_deref(),
                        )));
                }
                ModelEffect::ReadFile {
                    operation_id,
                    path,
                    max_bytes,
                } => {
                    committed = self.dispatch_file_job(FileJob::Read {
                        operation_id,
                        path,
                        max_bytes,
                    });
                }
                ModelEffect::WriteFile {
                    operation_id,
                    path,
                    contents,
                } => {
                    committed = self.dispatch_file_job(FileJob::Write {
                        operation_id,
                        path,
                        contents,
                    });
                }
                ModelEffect::Navigate {
                    document,
                    run,
                    span,
                } => {
                    self.pending_navigation = Some(MarkerSpan::new(
                        crate::EditorSourceKey::new(document, run),
                        span,
                    ));
                    self.navigation_focus_pending = true;
                }
                ModelEffect::AuthorizeClose { .. } => {
                    self.allow_native_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    stop_after_commit = true;
                }
            }
            if !committed {
                ctx.request_repaint_after(ADAPTER_RETRY_DELAY);
                break;
            }
            self.pending_effects.pop_front();
            if !self.queued_events.is_empty() {
                ctx.request_repaint();
            }
            if stop_after_commit {
                break;
            }
        }
    }

    fn dispatch_file_job(&mut self, job: FileJob) -> bool {
        if self.file_disconnect_reported {
            self.queued_events
                .push_back(ModelEvent::File(job.failure_event(FileFailureKind::Other)));
            self.adapter_error = Some("The file service is unavailable.".to_owned());
            return true;
        }
        let AppBackend::Native { files, .. } = &self.backend else {
            return false;
        };
        let accepted = job.clone();
        match files.try_submit(job) {
            Ok(()) => {
                self.accepted_file_job = Some(accepted);
                true
            }
            Err(FileSubmitError::Busy(_)) => false,
            Err(error @ FileSubmitError::Closed(_)) => {
                self.queued_events.push_back(ModelEvent::File(
                    error.into_failure_event(FileFailureKind::Other),
                ));
                self.file_disconnect_reported = true;
                self.adapter_error = Some("The file service is unavailable.".to_owned());
                true
            }
        }
    }

    fn handle_file_disconnect(&mut self) {
        if self.file_disconnect_reported {
            return;
        }
        self.file_disconnect_reported = true;
        self.adapter_error = Some("The file service stopped unexpectedly.".to_owned());
        if let Some(job) = self.accepted_file_job.take() {
            self.queued_events
                .push_back(ModelEvent::File(job.failure_event(FileFailureKind::Other)));
        }
    }

    fn handle_runtime_disconnect(&mut self, message: &str) {
        if self.runtime_disconnect_reported {
            return;
        }
        self.runtime_disconnect_reported = true;
        self.adapter_error = Some(message.to_owned());
        self.queued_events.push_back(ModelEvent::Supervisor(
            SupervisorModelEvent::RuntimeDisconnected,
        ));
    }

    fn shutdown_backend(&self) {
        if let AppBackend::Native { runtime, .. } = &self.backend {
            runtime.shutdown();
        }
    }

    fn show_menu_bar(&mut self, ui: &mut egui::Ui) {
        let catalog = self.action_catalog();
        let mut chosen = None;
        Panel::top("oxide_menu_bar").show(ui, |ui| {
            MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    for spec in catalog
                        .iter()
                        .filter(|spec| spec.section == ActionSection::File)
                    {
                        if action_menu_item(ui, *spec) {
                            chosen = Some(spec.action);
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Debug", |ui| {
                    for spec in catalog
                        .iter()
                        .filter(|spec| spec.section == ActionSection::Debug)
                    {
                        if action_menu_item(ui, *spec) {
                            chosen = Some(spec.action);
                            ui.close();
                        }
                    }
                });
                ui.separator();
                ui.label(RichText::new("Oxide IDE").strong());
            });
        });
        if let Some(action) = chosen {
            self.defer_action(ui.ctx(), action);
        }
    }

    fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        let catalog = self.action_catalog();
        let mut chosen = None;
        let mut toggle_console = false;
        let mut toggle_inspector = false;
        Panel::top("oxide_toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                for spec in catalog.iter().filter(|spec| toolbar_action_visible(**spec)) {
                    if action_toolbar_button(ui, *spec) {
                        chosen = Some(spec.action);
                    }
                    if matches!(spec.action, AppAction::Save | AppAction::Stop) {
                        ui.separator();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    toggle_inspector = ui
                        .button(if self.inspector_expanded {
                            "Hide inspector"
                        } else {
                            "Show inspector"
                        })
                        .clicked();
                    toggle_console = ui
                        .button(if self.console_expanded {
                            "Hide console"
                        } else {
                            "Show console"
                        })
                        .clicked();
                });
            });
        });
        if toggle_console {
            self.console_expanded = !self.console_expanded;
        }
        if toggle_inspector {
            self.inspector_expanded = !self.inspector_expanded;
        }
        if let Some(action) = chosen {
            self.defer_action(ui.ctx(), action);
        }
    }

    fn show_status(&self, ui: &mut egui::Ui) {
        Panel::bottom("oxide_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(status_label(self.model.execution_state()));
                if let Some(document) = self.model.document() {
                    ui.separator();
                    ui.label(document.display_name());
                    if document.is_dirty() {
                        ui.label("Modified");
                    }
                }
                if let Some(message) = self
                    .adapter_error
                    .as_deref()
                    .or_else(|| self.model.status().map(model_status_label))
                {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(164, 42, 42), message);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("UTF-8  •  LF");
                });
            });
        });
    }

    fn show_console(&mut self, ui: &mut egui::Ui) {
        let pending_input = self.model.pending_input().cloned();
        let can_submit_input = self.model.controls().submit_input
            && pending_input
                .as_ref()
                .is_some_and(|pending| self.input_submitted_for != Some(pending.request_id));
        let mut selected_problem = None;
        let mut submitted_input = None;
        Panel::bottom("oxide_console")
            .default_size(190.0)
            .min_size(96.0)
            .max_size(420.0)
            .resizable(true)
            .show_collapsible(ui, &mut self.console_expanded, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.console_tab, ConsoleTab::Output, "Output");
                    ui.selectable_value(&mut self.console_tab, ConsoleTab::Problems, "Problems");
                });
                ui.separator();
                if self.console_tab == ConsoleTab::Output
                    && let Some(pending) = pending_input.as_ref()
                {
                    ui.label(format!(
                        "Input requested: {}",
                        escape_display_text(&pending.prompt)
                    ));
                    ui.horizontal(|ui| {
                        let input_label = ui.label("Program input");
                        let mut buffer = BoundedTextBuffer::new(
                            &mut self.input_text,
                            crate::MAX_CONTROL_TEXT_BYTES,
                        );
                        let response = ui
                            .add_enabled(
                                can_submit_input,
                                TextEdit::singleline(&mut buffer)
                                    .id_salt("oxide_program_input")
                                    .hint_text("Enter program input"),
                            )
                            .labelled_by(input_label.id);
                        let rejected = buffer.rejection().is_some();
                        drop(buffer);
                        if self.input_focus_pending && can_submit_input {
                            response.request_focus();
                            self.input_focus_pending = false;
                        }
                        let send = ui
                            .add_enabled(can_submit_input, Button::new("Send input"))
                            .clicked();
                        let enter = can_submit_input
                            && response.has_focus()
                            && ui.input(|input| input.key_pressed(Key::Enter));
                        if send || enter {
                            submitted_input = Some((pending.request_id, self.input_text.clone()));
                        }
                        if rejected {
                            ui.colored_label(
                                Color32::from_rgb(164, 42, 42),
                                "Input is too long and was not changed.",
                            );
                        }
                    });
                    ui.separator();
                }
                ScrollArea::vertical()
                    .id_salt("oxide_console_scroll")
                    .show(ui, |ui| match self.console_tab {
                        ConsoleTab::Output => {
                            ui.label(RichText::new("Program output").strong());
                            let output = self.model.rendered_output();
                            if output.is_empty() {
                                ui.label("Program output will appear here.");
                            } else {
                                ui.add(
                                    egui::Label::new(RichText::new(output.as_ref()).monospace())
                                        .selectable(true),
                                );
                            }
                        }
                        ConsoleTab::Problems => {
                            if self.model.problems().is_empty() {
                                ui.label("No problems.");
                            } else {
                                for problem in self.model.problems() {
                                    let diagnostic = problem.diagnostic();
                                    let label = format!(
                                        "{} {}, line {}, column {}: {}",
                                        severity_label(diagnostic.severity),
                                        phase_label(diagnostic.phase),
                                        diagnostic.span.start.line,
                                        diagnostic.span.start.column,
                                        escape_display_text(&diagnostic.message),
                                    );
                                    if ui
                                        .add(Button::new(label).selected(
                                            self.model.selected_problem() == Some(problem.id()),
                                        ))
                                        .clicked()
                                    {
                                        selected_problem = Some(problem.id());
                                    }
                                    if diagnostic.message_truncated {
                                        ui.small("Message was truncated by the worker.");
                                    }
                                    if diagnostic.frames_truncated {
                                        ui.small("Some runtime frames were omitted.");
                                    }
                                }
                                if self.model.problems_were_truncated() {
                                    ui.small("Additional problems were omitted.");
                                }
                            }
                        }
                    });
            });
        if let Some(problem_id) = selected_problem {
            self.defer_ui_action(ui.ctx(), UiAction::SelectProblem(problem_id));
        }
        if let Some((request_id, text)) = submitted_input {
            self.input_submitted_for = Some(request_id);
            self.queued_events
                .push_back(ModelEvent::Ui(UiAction::SubmitInput {
                    in_reply_to: request_id,
                    text,
                }));
            ui.ctx().request_repaint();
        }
    }

    fn show_inspector(&mut self, ui: &mut egui::Ui) {
        let mut selected_frame = None;
        Panel::right("oxide_inspector")
            .default_size(280.0)
            .min_size(180.0)
            .max_size(520.0)
            .resizable(true)
            .show_collapsible(ui, &mut self.inspector_expanded, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.inspector_tab, InspectorTab::Symbols, "Symbols");
                    ui.selectable_value(
                        &mut self.inspector_tab,
                        InspectorTab::CallStack,
                        "Call Stack",
                    );
                });
                ui.separator();
                ScrollArea::vertical()
                    .id_salt("oxide_inspector_scroll")
                    .show(ui, |ui| match self.inspector_tab {
                        InspectorTab::Symbols => {
                            show_symbols(ui, &self.model);
                        }
                        InspectorTab::CallStack => {
                            selected_frame = show_call_stack(ui, &self.model);
                        }
                    });
            });
        if let Some(action) = selected_frame {
            self.defer_ui_action(ui.ctx(), action);
        }
    }

    fn editor_marker_plan(&mut self) -> Option<(MarkerPlan, Option<crate::MappedSpan>)> {
        let document = self.model.document()?;
        let document_stamp = document.stamp();
        if self.editor_text != document.text() {
            return None;
        }
        if self.pending_navigation.is_some_and(|navigation| {
            navigation.source.document != document_stamp
                || self.model.navigation_span() != Some(navigation.span)
        }) {
            self.pending_navigation = None;
            self.navigation_focus_pending = false;
        }

        let retained_source = self
            .model
            .retained_snapshot()
            .filter(|snapshot| snapshot.document() == document_stamp)
            .map(|snapshot| EditorSourceKey::new(snapshot.document(), snapshot.run()));
        let active_source = self
            .model
            .active_run()
            .map(|run| EditorSourceKey::new(document_stamp, run));
        let source = self
            .pending_navigation
            .map(|navigation| navigation.source)
            .or(retained_source)
            .or(active_source)?;
        if self
            .editor_mapper
            .as_ref()
            .is_none_or(|mapper| mapper.key() != source)
        {
            self.editor_mapper = Some(SourceMapper::new(source, &self.editor_text));
        }
        let current = retained_source
            .filter(|retained| *retained == source)
            .and_then(|retained| {
                self.model
                    .current_span()
                    .map(|span| MarkerSpan::new(retained, span))
            });
        let fault = retained_source
            .filter(|retained| *retained == source)
            .and_then(|retained| {
                self.model
                    .fault_span()
                    .map(|span| MarkerSpan::new(retained, span))
            });
        let navigation = self
            .pending_navigation
            .filter(|navigation| navigation.source == source);
        let mapper = self.editor_mapper.as_ref().expect("mapper was initialized");
        let plan = mapper.resolve_markers(MarkerInputs {
            current,
            fault,
            navigation,
        });
        let focus = if self.navigation_focus_pending {
            navigation.and_then(|marker| mapper.map_span(marker).ok())
        } else {
            None
        };
        if self.navigation_focus_pending && focus.is_none() {
            self.navigation_focus_pending = false;
            self.editor_notice = Some("That source location is no longer available.".to_owned());
        }
        Some((plan, focus))
    }

    fn show_editor(&mut self, ui: &mut egui::Ui) {
        let marker_state = self.editor_marker_plan();
        CentralPanel::default().show(ui, |ui| {
            let heading = self.model.document().map_or_else(
                || "No document".to_owned(),
                |document| {
                    let marker = if document.is_dirty() { " •" } else { "" };
                    format!("{}{}", document.display_name(), marker)
                },
            );
            ui.heading(heading);
            if let Some(notice) = self.editor_notice.as_deref() {
                ui.colored_label(Color32::from_rgb(164, 42, 42), notice);
            }
            ui.separator();
            let source_label = ui.label("Source editor");
            let editor_id = egui::Id::new("oxide_source_editor").with(
                self.editor_stamp
                    .map(|stamp| stamp.document_id.get())
                    .unwrap_or_default(),
            );
            let (marker_plan, navigation) = marker_state
                .as_ref()
                .map_or((None, None), |(plan, navigation)| (Some(plan), *navigation));
            ui.small(marker_summary(marker_plan));
            let editor_size = ui.available_size();
            ScrollArea::vertical()
                .id_salt((
                    "oxide_source_vertical_scroll",
                    self.editor_stamp
                        .map(|stamp| stamp.document_id.get())
                        .unwrap_or_default(),
                ))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        let line_count = self.editor_mapper.as_ref().map_or_else(
                            || {
                                self.editor_text
                                    .bytes()
                                    .filter(|byte| *byte == b'\n')
                                    .count()
                                    + 1
                            },
                            SourceMapper::line_count,
                        );
                        let gutter = egui::Label::new(
                            RichText::new(gutter_text(line_count, marker_plan)).monospace(),
                        )
                        .selectable(false);
                        let (gutter_position, gutter_galley, gutter_response) =
                            gutter.layout_in_ui(ui);
                        gutter_response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Label,
                                ui.is_enabled(),
                                "Line number gutter",
                            )
                        });
                        if ui.is_rect_visible(gutter_response.rect) {
                            ui.painter().galley(
                                gutter_position,
                                gutter_galley,
                                ui.visuals().text_color(),
                            );
                        }
                        gutter_response.on_hover_text("Line numbers and source markers");
                        let mut vertical_scroll_target = None;
                        ScrollArea::horizontal()
                            .id_salt((
                                "oxide_source_horizontal_scroll",
                                self.editor_stamp
                                    .map(|stamp| stamp.document_id.get())
                                    .unwrap_or_default(),
                            ))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let mut layouter =
                                    |layout_ui: &egui::Ui,
                                     buffer: &dyn egui::TextBuffer,
                                     _wrap_width: f32| {
                                        if let Some(plan) = marker_plan {
                                            let job = build_layout_job(
                                                buffer.as_str(),
                                                plan,
                                                |markers| {
                                                    marker_text_format(layout_ui, markers)
                                                },
                                            );
                                            layout_ui.painter().layout_job(job)
                                        } else {
                                            layout_ui.painter().layout_no_wrap(
                                                buffer.as_str().to_owned(),
                                                egui::TextStyle::Monospace
                                                    .resolve(layout_ui.style()),
                                                layout_ui.visuals().text_color(),
                                            )
                                        }
                                    };
                                let editor_available = ui.available_size();
                                let (mut output, rejection) = if self.model.controls().editor {
                                    let mut buffer = BoundedTextBuffer::with_default_limit(
                                        &mut self.editor_text,
                                    );
                                    let output = TextEdit::multiline(&mut buffer)
                                        .id(editor_id)
                                        .code_editor()
                                        .lock_focus(false)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(1)
                                        .min_size(egui::vec2(
                                            editor_available.x.max(editor_size.x * 0.75),
                                            editor_size.y,
                                        ))
                                        .layouter(&mut layouter)
                                        .show(ui);
                                    let rejection = buffer.rejection();
                                    (output, rejection)
                                } else {
                                    let mut read_only = self.editor_text.as_str();
                                    (
                                        TextEdit::multiline(&mut read_only)
                                            .id(editor_id)
                                            .code_editor()
                                            .lock_focus(false)
                                            .desired_width(f32::INFINITY)
                                            .desired_rows(1)
                                            .min_size(egui::vec2(
                                                editor_available.x.max(editor_size.x * 0.75),
                                                editor_size.y,
                                            ))
                                            .layouter(&mut layouter)
                                            .show(ui),
                                        None,
                                    )
                                };
                                if let Some(rejection) = rejection {
                                    self.editor_notice = Some(match rejection.limit {
                                        SourceGrowthLimit::Bytes => format!(
                                            "Source is limited to {} bytes; the {}-byte edit was not applied.",
                                            rejection.max_bytes, rejection.inserted_bytes,
                                        ),
                                        SourceGrowthLimit::Lines => format!(
                                            "Source is limited to {} lines; the edit would create {} lines.",
                                            rejection.max_lines, rejection.attempted_lines,
                                        ),
                                    });
                                }
                                let response =
                                    output.response.response.labelled_by(source_label.id);
                                if rejection.is_none() && response.changed() {
                                    self.editor_notice = None;
                                }
                                let cursor_changed =
                                    self.editor_cursor_range != output.cursor_range;
                                self.editor_cursor_range = output.cursor_range;
                                if response.has_focus()
                                    && (response.changed() || cursor_changed)
                                    && let Some(cursor_range) = output.cursor_range
                                {
                                    vertical_scroll_target = Some((
                                        output
                                            .galley
                                            .pos_from_cursor(cursor_range.primary)
                                            .translate(output.galley_pos.to_vec2()),
                                        None,
                                    ));
                                }
                                if let Some(span) = navigation {
                                    let range = span.egui_cursor_range();
                                    output.state.cursor.set_char_range(Some(range));
                                    output.state.store(ui.ctx(), response.id);
                                    response.request_focus();
                                    let start = output
                                        .galley
                                        .pos_from_cursor(range.primary)
                                        .translate(output.galley_pos.to_vec2());
                                    let end = output
                                        .galley
                                        .pos_from_cursor(range.secondary)
                                        .translate(output.galley_pos.to_vec2());
                                    let navigation_rect = start.union(end);
                                    ui.scroll_to_rect(navigation_rect, Some(egui::Align::Center));
                                    vertical_scroll_target =
                                        Some((navigation_rect, Some(egui::Align::Center)));
                                    self.navigation_focus_pending = false;
                                }
                                if response.changed()
                                    && let Some(document) = self.model.document()
                                    && self.editor_text != document.text()
                                {
                                    self.queued_events.push_back(ModelEvent::Ui(UiAction::Edit {
                                        document: document.stamp(),
                                        text: self.editor_text.clone(),
                                    }));
                                    ui.ctx().request_repaint();
                                }
                            });
                        if let Some((rect, align)) = vertical_scroll_target {
                            ui.scroll_to_rect(rect, align);
                        }
                    });
                });
        });
    }
}

impl eframe::App for OxideApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.configure_theme(ctx);
        self.poll_adapters(ctx);
        if ctx.input(|input| input.viewport().close_requested()) && !self.allow_native_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.native_close_pending {
                self.native_close_pending = true;
                self.native_close_request_enqueued = false;
            }
        }
        self.consume_shortcut(ctx);
        self.reduce_pending_events(ctx);
        self.dispatch_native_effects(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        Frame::central_panel(ui.style()).show(ui, |ui| {
            self.show_menu_bar(ui);
            self.show_toolbar(ui);
            self.show_status(ui);
            self.show_console(ui);
            self.show_inspector(ui);
            self.show_editor(ui);
        });
        self.flush_deferred_actions(ui.ctx());
        self.stage_native_close_request(ui.ctx());
    }

    fn on_exit(&mut self) {
        self.shutdown_backend();
    }
}

impl Drop for OxideApp {
    fn drop(&mut self) {
        self.shutdown_backend();
    }
}

fn show_call_stack(ui: &mut egui::Ui, model: &AppModel) -> Option<UiAction> {
    let Some(retained) = model.retained_snapshot() else {
        ui.label("Call frames appear while debugging.");
        return None;
    };
    let snapshot = retained.snapshot();
    ui.label(snapshot_accessible_name(
        retained.provenance(),
        snapshot.frames.len(),
        snapshot.frames_truncated,
        snapshot.globals_truncated,
    ));
    let mut selected = None;
    for (ordinal, frame) in snapshot.frames.iter().enumerate() {
        let is_selected = model.selected_activation() == Some(frame.activation_id);
        if ui
            .add(
                Button::new(frame_accessible_name(frame, ordinal, is_selected))
                    .selected(is_selected),
            )
            .clicked()
        {
            selected = Some(UiAction::SelectFrame {
                snapshot: retained.key(),
                activation_id: frame.activation_id,
            });
        }
    }
    if snapshot.frames_truncated {
        ui.small("Additional call frames were omitted.");
    }
    selected
}

fn show_symbols(ui: &mut egui::Ui, model: &AppModel) {
    let Some(retained) = model.retained_snapshot() else {
        ui.label("Symbols appear while debugging.");
        return;
    };
    let snapshot = retained.snapshot();
    ui.label(snapshot_provenance_label(retained.provenance()));
    let selected_frame = model.selected_frame().or_else(|| snapshot.frames.first());
    if let Some(frame) = selected_frame {
        ui.heading(format!("Function {}", escape_display_text(&frame.function)));
        show_binding_section(
            ui,
            "Parameters",
            retained.key(),
            frame.activation_id,
            BindingScope::Parameters,
            &frame.parameters,
            frame.parameters_truncated,
        );
        show_binding_section(
            ui,
            "Locals",
            retained.key(),
            frame.activation_id,
            BindingScope::Locals,
            &frame.locals,
            frame.locals_truncated,
        );
        show_binding_section(
            ui,
            "Captured variables",
            retained.key(),
            frame.activation_id,
            BindingScope::Upvalues,
            &frame.upvalues,
            frame.upvalues_truncated,
        );
    } else {
        ui.label("No call frame is available.");
    }

    egui::CollapsingHeader::new("Globals")
        .id_salt((retained.key(), "globals"))
        .default_open(true)
        .show(ui, |ui| {
            if snapshot.globals.is_empty() {
                ui.label("No globals.");
            }
            for (ordinal, binding) in snapshot.globals.iter().enumerate() {
                let path = ValuePath::global_binding(retained.key(), ordinal);
                show_presented_binding(ui, &present_binding(binding, path));
            }
            if snapshot.globals_truncated {
                ui.small("Additional globals were omitted.");
            }
        });
}

fn show_binding_section(
    ui: &mut egui::Ui,
    title: &'static str,
    snapshot: crate::SnapshotKey,
    activation: rlox::ActivationId,
    scope: BindingScope,
    bindings: &[rlox::BindingSnapshot],
    truncated: bool,
) {
    egui::CollapsingHeader::new(title)
        .id_salt((snapshot, activation, scope))
        .default_open(true)
        .show(ui, |ui| {
            if bindings.is_empty() {
                ui.label(format!("No {}.", title.to_ascii_lowercase()));
            }
            for (ordinal, binding) in bindings.iter().enumerate() {
                let path = ValuePath::frame_binding(
                    snapshot,
                    activation,
                    scope,
                    binding.binding_id,
                    ordinal,
                );
                show_presented_binding(ui, &present_binding(binding, path));
            }
            if truncated {
                ui.small("Additional bindings were omitted.");
            }
        });
}

fn show_presented_binding(ui: &mut egui::Ui, binding: &PresentedBinding) {
    let label = format!(
        "{} · {} · {} = {}",
        binding.name, binding.kind_label, binding.value_kind_label, binding.value.summary,
    );
    if binding.value.is_expandable() {
        egui::CollapsingHeader::new(label)
            .id_salt(&binding.value.path)
            .show(ui, |ui| show_presented_children(ui, &binding.value));
    } else {
        ui.label(label).on_hover_text(&binding.accessible_name);
    }
    if binding.name_truncated || binding.value.is_truncated() {
        ui.small("Part of this symbol was omitted.");
    }
}

fn show_presented_children(ui: &mut egui::Ui, value: &PresentedValue) {
    for (index, child) in value.children.iter().enumerate() {
        let label = format!("[{index}] {} · {}", child.kind_label, child.summary);
        if child.is_expandable() {
            egui::CollapsingHeader::new(label)
                .id_salt(&child.path)
                .show(ui, |ui| show_presented_children(ui, child));
        } else {
            ui.label(label);
        }
        if matches!(
            child.state,
            PresentedValueState::Cycle | PresentedValueState::Truncated
        ) {
            ui.small("This item cannot be expanded further.");
        }
    }
    if value.is_truncated() {
        ui.small("Additional list items were omitted.");
    }
}

fn gutter_text(line_count: usize, plan: Option<&MarkerPlan>) -> String {
    let digits = gutter_digits(line_count);
    let mut text = String::with_capacity(line_count.saturating_mul(digits.saturating_add(4)));
    for line_index in 0..line_count {
        let markers = plan
            .and_then(|plan| {
                plan.gutter_markers()
                    .iter()
                    .find(|marker| marker.line_index == line_index)
            })
            .map_or(MarkerMask::NONE, |marker| marker.markers);
        let marker = match markers.primary() {
            Some(MarkerKind::Fault) => 'E',
            Some(MarkerKind::Current) => 'C',
            Some(MarkerKind::Navigation) => 'N',
            None => ' ',
        };
        let _ = write!(text, "{marker} {:>digits$}", line_index + 1);
        if line_index + 1 < line_count {
            text.push('\n');
        }
    }
    text
}

fn marker_summary(plan: Option<&MarkerPlan>) -> String {
    let Some(plan) = plan else {
        return "Line numbers. No active source markers.".to_owned();
    };
    if plan.gutter_markers().is_empty() {
        return "Line numbers. No active source markers.".to_owned();
    }
    let mut summary = String::from("Line numbers and source markers: ");
    for (index, marker) in plan.gutter_markers().iter().enumerate() {
        if index != 0 {
            summary.push_str("; ");
        }
        let _ = write!(summary, "line {}", marker.line_index + 1);
        for kind in [
            MarkerKind::Fault,
            MarkerKind::Current,
            MarkerKind::Navigation,
        ] {
            if marker.markers.contains(kind) {
                summary.push_str(match kind {
                    MarkerKind::Fault => " error",
                    MarkerKind::Current => " current execution",
                    MarkerKind::Navigation => " navigation target",
                });
            }
        }
    }
    summary.push('.');
    summary
}

fn marker_text_format(ui: &egui::Ui, markers: MarkerMask) -> egui::TextFormat {
    let mut format = egui::TextFormat {
        font_id: egui::TextStyle::Monospace.resolve(ui.style()),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    match markers.primary() {
        Some(MarkerKind::Fault) => {
            format.background = Color32::from_rgb(255, 220, 220);
            format.underline = egui::Stroke::new(1.5, Color32::from_rgb(180, 35, 35));
        }
        Some(MarkerKind::Current) => {
            format.background = Color32::from_rgb(255, 242, 184);
        }
        Some(MarkerKind::Navigation) => {
            format.background = Color32::from_rgb(218, 234, 255);
            format.underline = egui::Stroke::new(1.0, Color32::from_rgb(42, 101, 172));
        }
        None => {}
    }
    format
}

fn phase_label(phase: rlox::DiagnosticPhase) -> &'static str {
    match phase {
        rlox::DiagnosticPhase::Scanner => "scanner",
        rlox::DiagnosticPhase::Parser => "parser",
        rlox::DiagnosticPhase::Compiler => "compiler",
        rlox::DiagnosticPhase::Runtime => "runtime",
        rlox::DiagnosticPhase::Worker => "worker",
    }
}

fn severity_label(severity: rlox::DiagnosticSeverity) -> &'static str {
    match severity {
        rlox::DiagnosticSeverity::Error => "Error",
        rlox::DiagnosticSeverity::Warning => "Warning",
    }
}

fn action_menu_item(ui: &mut egui::Ui, spec: ActionSpec) -> bool {
    let mut button = Button::new(spec.label);
    if let Some(shortcut) = spec.shortcut {
        button = button.shortcut_text(ui.ctx().format_shortcut(&shortcut));
    }
    ui.add_enabled(spec.enabled, button).clicked()
}

fn action_toolbar_button(ui: &mut egui::Ui, spec: ActionSpec) -> bool {
    let response = ui.add_enabled(spec.enabled, Button::new(spec.label));
    let response = if let Some(shortcut) = spec.shortcut {
        response.on_hover_text(format!(
            "{} ({})",
            spec.label,
            ui.ctx().format_shortcut(&shortcut)
        ))
    } else {
        response.on_hover_text(spec.label)
    };
    response.clicked()
}

fn toolbar_action_visible(spec: ActionSpec) -> bool {
    if !spec.show_in_toolbar {
        return false;
    }
    match spec.section {
        ActionSection::File => true,
        ActionSection::Debug => spec.enabled || spec.action == AppAction::Stop,
    }
}

fn status_label(state: ExecutionViewState) -> &'static str {
    match state {
        ExecutionViewState::Idle => "Ready",
        ExecutionViewState::Starting => "Starting",
        ExecutionViewState::Running => "Running",
        ExecutionViewState::WaitingForInput => "Waiting for input",
        ExecutionViewState::Paused => "Paused",
        ExecutionViewState::Completed => "Completed",
        ExecutionViewState::Cancelled => "Stopped",
        ExecutionViewState::Faulted => "Error",
        ExecutionViewState::WorkerCrashed => "Worker crashed",
    }
}

fn model_status_label(status: &ModelStatus) -> &'static str {
    match status {
        ModelStatus::IdExhausted => "The IDE reached its internal request limit.",
        ModelStatus::RuntimeDisconnected => "The execution service disconnected.",
        ModelStatus::StartFailed(_) => "The execution service could not start.",
        ModelStatus::StartRejected(_) => "The run request was rejected.",
        ModelStatus::CommandRejected { .. } => "The debug command was rejected.",
        ModelStatus::WorkerCommandRejected { .. } => "The interpreter rejected a command.",
        ModelStatus::ProtocolDesynchronized => "The execution service lost synchronization.",
        ModelStatus::WorkerTerminated(_) => "The interpreter stopped unexpectedly.",
        ModelStatus::CleanupWarning(_) => "The previous interpreter did not close cleanly.",
        ModelStatus::FileFailed(_) => "The file operation failed.",
        ModelStatus::InvalidUtf8 => "The selected file is not valid UTF-8.",
        ModelStatus::ProblemLimitReached => "Additional problems were omitted.",
        ModelStatus::SourceLimitReached => "The source exceeds the editor size or line limit.",
    }
}

fn action(
    action: AppAction,
    section: ActionSection,
    label: &'static str,
    shortcut: Option<KeyboardShortcut>,
    enabled: bool,
    show_in_toolbar: bool,
) -> ActionSpec {
    ActionSpec {
        action,
        section,
        label,
        shortcut,
        enabled,
        show_in_toolbar,
    }
}

fn key(key: Key) -> Option<KeyboardShortcut> {
    Some(KeyboardShortcut::new(Modifiers::NONE, key))
}

fn ctrl(key: Key) -> Option<KeyboardShortcut> {
    Some(KeyboardShortcut::new(Modifiers::CTRL, key))
}

fn shift(key: Key) -> Option<KeyboardShortcut> {
    Some(KeyboardShortcut::new(Modifiers::SHIFT, key))
}

fn ctrl_shift(key: Key) -> Option<KeyboardShortcut> {
    Some(KeyboardShortcut::new(
        Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        },
        key,
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::FileModelEvent;

    #[test]
    fn file_disconnect_finishes_the_exact_accepted_operation_once() {
        let mut app = OxideApp::headless(AppModel::new());
        let picker = apply_event(&mut app.model, ModelEvent::Ui(UiAction::Open));
        let [ModelEffect::PickOpen { operation_id }] = picker.as_slice() else {
            panic!("open should request a picker");
        };
        let operation_id = *operation_id;
        let path = PathBuf::from("accepted.ox");
        let read = apply_event(
            &mut app.model,
            ModelEvent::File(FileModelEvent::OpenPicked {
                operation_id,
                path: Some(path.clone()),
            }),
        );
        let [ModelEffect::ReadFile { max_bytes, .. }] = read.as_slice() else {
            panic!("selected source should request a bounded read");
        };
        app.accepted_file_job = Some(FileJob::Read {
            operation_id,
            path,
            max_bytes: *max_bytes,
        });

        app.handle_file_disconnect();
        app.handle_file_disconnect();

        assert_eq!(app.pending_event_count(), 1);
        assert_eq!(app.pump_events(32), 1);
        assert_eq!(
            app.model.status(),
            Some(&ModelStatus::FileFailed(FileFailureKind::Other))
        );
        assert!(app.accepted_file_job.is_none());
    }

    #[test]
    fn runtime_disconnect_queues_only_one_sticky_disconnect_event() {
        let mut app = OxideApp::headless(AppModel::new());

        app.handle_runtime_disconnect("first");
        app.handle_runtime_disconnect("second");

        assert_eq!(app.pending_event_count(), 1);
        assert_eq!(app.adapter_error.as_deref(), Some("first"));
        assert_eq!(app.pump_events(32), 1);
        assert_eq!(app.model.status(), Some(&ModelStatus::RuntimeDisconnected));
    }

    #[test]
    fn reconciling_an_accepted_edit_clears_the_previous_editor_notice() {
        let mut app = OxideApp::headless(AppModel::new());
        let stamp = app
            .model
            .document()
            .expect("headless app starts with a document")
            .stamp();
        app.editor_notice = Some("old warning".to_owned());

        let effects = apply_event(
            &mut app.model,
            ModelEvent::Ui(UiAction::Edit {
                document: stamp,
                text: "print 1;".to_owned(),
            }),
        );
        assert!(effects.is_empty());
        app.reconcile_editor_buffer();

        assert!(app.editor_notice.is_none());
    }

    #[test]
    fn embedded_app_icon_decodes_to_square_rgba_pixels() {
        let icon = load_app_icon().expect("embedded application icon should decode");

        assert_eq!(icon.width, icon.height);
        assert!(icon.width >= 256);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }
}

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::text::{CCursorRange, CharIndex};
use eframe::egui::{
    self, Button, CentralPanel, Color32, Frame, Key, KeyboardShortcut, MenuBar, Modifiers, Panel,
    RichText, ScrollArea, TextEdit,
};

use crate::file_dialog::{
    FileEventReceiver, FileExecutor, FileJob, FileSubmitError, show_open_dialog, show_save_dialog,
    show_unsaved_dialog,
};
use crate::language::{
    AnalysisPhase, DefinitionResultId, DiagnosticItemId, DiagnosticSetRevision,
    DiagnosticSeverity as AnalysisSeverity, DiagnosticSnapshot, LanguageCoordinator,
    LanguageSnapshot, LanguageStatus, NoticeId, ProcessGeneration, SyntaxKind, synthetic_uri,
};
use crate::language_ui::{
    AnalysisRevealBatch, AnalysisRevealLatch, AnalysisRevealScope, CapturedDefinitionIntent,
    EditorCaret, LanguageUiState, SelectionIdentity,
};
use crate::{
    ActivationId, AppModel, BindingScope, BoundedTextBuffer, DocumentMarkerInputs, DocumentRange,
    DocumentStamp, EditorSourceKey, ExecutionViewState, FileFailureKind, MarkerKind, MarkerMask,
    MarkerSpan, ModelEffect, ModelEvent, ModelStatus, NavigationGeneration, PreparedLayoutPlan,
    PreparedSyntaxRun, PresentedBinding, PresentedValue, PresentedValueState, RequestId,
    RuntimeCoordinator, RuntimeDispatchError, SnapshotKey, SourceGrowthLimit, SourceMapper,
    SupervisorConfig, SupervisorModelEvent, SyntaxClass, UiAction, ValuePath, apply_event,
    build_prepared_layout_job, escape_display_text, frame_accessible_name, gutter_digits,
    present_binding, snapshot_accessible_name, snapshot_provenance_label,
};

const MAX_PENDING_EVENTS_PER_PASS: usize = 32;
const MAX_ADAPTER_EVENTS_PER_PASS: usize = 32;
const MAX_EFFECTS_PER_PASS: usize = 32;
const ADAPTER_RETRY_DELAY: Duration = Duration::from_millis(4);
pub const APP_NAME: &str = "Oxide IDE";
const SHORTCUT_PRIORITY: [AppAction; 12] = [
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
    AppAction::GoToDefinition,
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
    GoToDefinition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionSection {
    File,
    Debug,
    Navigate,
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
    pub fn ui_action(self) -> Option<UiAction> {
        match self {
            Self::New => Some(UiAction::New),
            Self::Open => Some(UiAction::Open),
            Self::Save => Some(UiAction::Save),
            Self::SaveAs => Some(UiAction::SaveAs),
            Self::CloseDocument => Some(UiAction::CloseDocument),
            Self::Exit => Some(UiAction::RequestExit),
            Self::Run => Some(UiAction::Run),
            Self::Debug => Some(UiAction::Debug),
            Self::Pause => Some(UiAction::Pause),
            Self::Continue => Some(UiAction::Continue),
            Self::StepInto => Some(UiAction::StepInto),
            Self::StepOver => Some(UiAction::StepOver),
            Self::StepOut => Some(UiAction::StepOut),
            Self::Stop => Some(UiAction::Stop),
            Self::GoToDefinition => None,
        }
    }
}

pub struct OxideApp {
    model: AppModel,
    backend: AppBackend,
    language: LanguageUiState,
    analysis_reveal_latch: AnalysisRevealLatch,
    pending_definition: Option<CapturedDefinitionIntent>,
    pending_analysis_click: Option<PendingAnalysisClick>,
    handled_definition: Option<DefinitionResultId>,
    handled_notices: Vec<NoticeId>,
    language_process: Option<ProcessGeneration>,
    language_notice: Option<String>,
    language_error: Option<String>,
    queued_events: VecDeque<ModelEvent>,
    deferred_actions: VecDeque<AppAction>,
    deferred_ui_actions: VecDeque<UiAction>,
    next_navigation_generation: Option<NavigationGeneration>,
    highest_navigation_intent: Option<NavigationGeneration>,
    navigation_target: Option<NavigationTarget>,
    pending_effects: VecDeque<ModelEffect>,
    editor_text: String,
    editor_stamp: Option<DocumentStamp>,
    editor_mapper: Option<SourceMapper>,
    editor_cursor_range: Option<CCursorRange>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationOrigin {
    Language {
        process_generation: crate::language::ProcessGeneration,
    },
    Run {
        binding: crate::RunBinding,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NavigationTarget {
    navigation_generation: NavigationGeneration,
    origin: NavigationOrigin,
    range: DocumentRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingAnalysisClick {
    navigation_generation: NavigationGeneration,
    process_generation: ProcessGeneration,
    stamp: DocumentStamp,
    diagnostic_revision: DiagnosticSetRevision,
    item_id: DiagnosticItemId,
    range: DocumentRange,
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
        Self::headless_with_language(model, LanguageUiState::unavailable())
    }

    fn headless_with_language(model: AppModel, language: LanguageUiState) -> Self {
        let language_process = language.snapshot().process_generation();
        let (editor_stamp, editor_text) = model.document().map_or_else(
            || (None, String::new()),
            |document| (Some(document.stamp()), document.text().to_owned()),
        );
        Self {
            model,
            backend: AppBackend::Headless,
            language,
            analysis_reveal_latch: AnalysisRevealLatch::default(),
            pending_definition: None,
            pending_analysis_click: None,
            handled_definition: None,
            handled_notices: Vec::new(),
            language_process,
            language_notice: None,
            language_error: None,
            queued_events: VecDeque::new(),
            deferred_actions: VecDeque::new(),
            deferred_ui_actions: VecDeque::new(),
            next_navigation_generation: NavigationGeneration::from_raw(1),
            highest_navigation_intent: None,
            navigation_target: None,
            pending_effects: VecDeque::new(),
            editor_text,
            editor_stamp,
            editor_mapper: None,
            editor_cursor_range: None,
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

    #[cfg(test)]
    fn headless_with_language_port(
        model: AppModel,
        port: Box<dyn crate::language_ui::LanguagePort>,
    ) -> Self {
        Self::headless_with_language(model, LanguageUiState::new(port))
    }

    pub fn native(cc: &eframe::CreationContext<'_>) -> io::Result<Self> {
        let supervisor = SupervisorConfig::sibling_rlox().map_err(|error| {
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
        let language_context = cc.egui_ctx.clone();
        app.language = LanguageUiState::native(LanguageCoordinator::start(move || {
            language_context.request_repaint();
        }));
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

    pub fn action_catalog(&self) -> [ActionSpec; 15] {
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
            action(
                AppAction::GoToDefinition,
                ActionSection::Navigate,
                "Go to Definition",
                key(Key::F12),
                !self.language.caret_exhausted() && self.language.definition_available(),
                false,
            ),
        ]
    }

    pub fn queue_action(&mut self, action: AppAction) {
        if action == AppAction::GoToDefinition {
            self.capture_definition_action();
            return;
        }
        if let Some(action) = action.ui_action() {
            self.queued_events.push_back(ModelEvent::Ui(action));
        }
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
            let effects = apply_event(&mut self.model, event);
            for effect in &effects {
                self.stage_local_effect(effect);
            }
            self.pending_effects.extend(effects);
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
        if action == AppAction::GoToDefinition {
            self.capture_definition_action();
            ctx.request_repaint();
            return;
        }
        self.deferred_actions.push_back(action);
        ctx.request_repaint();
    }

    fn defer_ui_action(&mut self, ctx: &egui::Context, action: UiAction) {
        self.deferred_ui_actions.push_back(action);
        ctx.request_repaint();
    }

    fn allocate_navigation_generation(&mut self) -> Option<NavigationGeneration> {
        let generation = self.next_navigation_generation?;
        self.next_navigation_generation = generation.checked_next();
        self.highest_navigation_intent = Some(generation);
        self.navigation_target = None;
        self.navigation_focus_pending = false;
        Some(generation)
    }

    fn install_navigation_target(
        &mut self,
        navigation_generation: NavigationGeneration,
        origin: NavigationOrigin,
        range: DocumentRange,
    ) -> bool {
        if self.highest_navigation_intent != Some(navigation_generation) {
            return false;
        }
        let Some(document) = self.model.document() else {
            return false;
        };
        if range.document != document.stamp() || self.editor_text != document.text() {
            return false;
        }
        let mapper = SourceMapper::for_document(document.stamp(), document.text());
        if mapper.map_document_range(&range).is_err() {
            return false;
        }
        self.navigation_target = Some(NavigationTarget {
            navigation_generation,
            origin,
            range,
        });
        self.navigation_focus_pending = true;
        true
    }

    fn capture_definition_action(&mut self) {
        let Some(generation) = self.allocate_navigation_generation() else {
            return;
        };
        self.pending_definition = self.language.capture_definition(generation);
    }

    fn current_editor_caret(&self) -> Option<EditorCaret> {
        let document = self.model.document()?;
        if self.editor_stamp != Some(document.stamp()) || self.editor_text != document.text() {
            return None;
        }
        let (primary_character, selection) =
            self.editor_cursor_range.map_or((None, None), |range| {
                let primary = range.primary.index.0;
                let secondary = range.secondary.index.0;
                (
                    Some(primary),
                    Some(SelectionIdentity {
                        anchor_character: secondary,
                        focus_character: primary,
                    }),
                )
            });
        Some(EditorCaret {
            stamp: document.stamp(),
            primary_character,
            selection,
        })
    }

    fn reconcile_language(&mut self, ctx: &egui::Context) {
        let document = self.model.document();
        let caret = self.current_editor_caret();
        let mut closed = false;
        if self.language.reconcile_document(document).is_err() {
            closed = true;
        }
        let snapshot = self.language.refresh_snapshot();
        self.reconcile_language_process(snapshot.process_generation());
        if self.language.reconcile_caret(caret).is_err() {
            closed = true;
        }
        self.observe_analysis_batch(&snapshot);
        self.apply_pending_analysis_click(&snapshot);
        self.consume_definition_result(&snapshot);
        self.consume_language_notices(&snapshot);
        if let Some(captured) = self.pending_definition.take() {
            match self.language.submit_definition(captured) {
                Ok(_) => {}
                Err(_) => closed = true,
            }
        }
        if self.language.flush_acknowledgements().is_err() {
            closed = true;
        }
        if closed {
            self.language_error = Some("The language service is unavailable.".to_owned());
        }
        if closed || self.pending_definition.is_some() {
            ctx.request_repaint();
        }
    }

    fn reconcile_language_process(&mut self, process_generation: Option<ProcessGeneration>) {
        if self.language_process == process_generation {
            return;
        }
        self.language_process = process_generation;
        self.handled_definition = None;
        self.handled_notices.clear();
        self.pending_definition = None;
        self.pending_analysis_click = None;
        self.language_notice = None;
        if self
            .navigation_target
            .as_ref()
            .is_some_and(|target| matches!(target.origin, NavigationOrigin::Language { .. }))
        {
            self.navigation_target = None;
            self.navigation_focus_pending = false;
        }
    }

    fn analysis_is_current(
        &self,
        snapshot: &LanguageSnapshot,
        diagnostics: &DiagnosticSnapshot,
    ) -> bool {
        let Some(document) = self.model.document() else {
            return false;
        };
        let stamp = document.stamp();
        self.editor_stamp == Some(stamp)
            && self.editor_text == document.text()
            && snapshot.process_generation() == Some(diagnostics.process_generation)
            && snapshot.desired_stamp() == Some(stamp)
            && snapshot.written_document().is_some_and(|written| {
                written.stamp == stamp
                    && written.uri.as_ref() == diagnostics.uri.as_ref()
                    && written.lsp_version == Some(diagnostics.lsp_version)
            })
            && diagnostics.stamp == stamp
            && diagnostics.uri.as_ref() == synthetic_uri(stamp.document_id)
    }

    fn observe_analysis_batch(&mut self, snapshot: &LanguageSnapshot) {
        let current_scope = self.model.document().and_then(|document| {
            snapshot
                .process_generation()
                .map(|process_generation| AnalysisRevealScope {
                    document: document.stamp().document_id,
                    process_generation,
                })
        });
        let accepted_batch = snapshot.diagnostics().and_then(|diagnostics| {
            self.analysis_is_current(snapshot, diagnostics)
                .then_some(AnalysisRevealBatch {
                    scope: AnalysisRevealScope {
                        document: diagnostics.stamp.document_id,
                        process_generation: diagnostics.process_generation,
                    },
                    revision: diagnostics.revision,
                    item_count: diagnostics.items.len(),
                })
        });
        if self
            .analysis_reveal_latch
            .observe(current_scope, accepted_batch)
        {
            self.console_tab = ConsoleTab::Problems;
            self.console_expanded = true;
        }
    }

    fn apply_pending_analysis_click(&mut self, snapshot: &LanguageSnapshot) {
        let Some(click) = self.pending_analysis_click.take() else {
            return;
        };
        let valid = snapshot.diagnostics().is_some_and(|diagnostics| {
            self.analysis_is_current(snapshot, diagnostics)
                && diagnostics.process_generation == click.process_generation
                && diagnostics.stamp == click.stamp
                && diagnostics.revision == click.diagnostic_revision
                && diagnostics.items.iter().any(|item| {
                    item.id == click.item_id
                        && item.range.as_ref().is_some_and(|range| {
                            range.bytes == click.range.byte_range
                                && range.characters.start == click.range.char_range.start.0
                                && range.characters.end == click.range.char_range.end.0
                        })
                })
        });
        if valid {
            self.install_navigation_target(
                click.navigation_generation,
                NavigationOrigin::Language {
                    process_generation: click.process_generation,
                },
                click.range,
            );
        }
    }

    fn consume_definition_result(&mut self, snapshot: &LanguageSnapshot) {
        let Some(definition) = snapshot.definition() else {
            return;
        };
        if self.handled_definition == Some(definition.id) {
            return;
        }
        self.handled_definition = Some(definition.id);
        let current_document = self.model.document().map(|document| document.stamp());
        let fence_matches = current_document == Some(definition.stamp)
            && self.editor_stamp == current_document
            && self.model.document().is_some_and(|document| {
                self.editor_text == document.text()
                    && definition.uri.as_ref() == synthetic_uri(document.stamp().document_id)
            })
            && snapshot.process_generation() == Some(definition.process_generation)
            && snapshot.desired_stamp() == Some(definition.stamp)
            && snapshot.written_document().is_some_and(|written| {
                written.stamp == definition.stamp
                    && written.uri.as_ref() == definition.uri.as_ref()
                    && written.lsp_version == Some(definition.lsp_version)
            })
            && self.language.definition_result_current(definition)
            && self.highest_navigation_intent == Some(definition.navigation_generation);
        if fence_matches {
            if let Some(target) = definition.targets.first() {
                let range = DocumentRange::new(
                    definition.stamp,
                    target.range.bytes.clone(),
                    CharIndex(target.range.characters.start)
                        ..CharIndex(target.range.characters.end),
                );
                self.install_navigation_target(
                    definition.navigation_generation,
                    NavigationOrigin::Language {
                        process_generation: definition.process_generation,
                    },
                    range,
                );
                if definition.targets.len() > 1 {
                    self.language_notice = Some(format!(
                        "{} definitions found; showing the first.",
                        definition.targets.len()
                    ));
                }
            } else {
                self.language_notice = Some("No definition was found.".to_owned());
            }
        }
        self.language.acknowledge_definition(definition.id);
    }

    fn consume_language_notices(&mut self, snapshot: &LanguageSnapshot) {
        self.handled_notices
            .retain(|id| snapshot.notices().iter().any(|notice| notice.id == *id));
        for notice in snapshot.notices() {
            if self.handled_notices.contains(&notice.id) {
                continue;
            }
            self.language_notice = Some(notice.message.to_string());
            self.handled_notices.push(notice.id);
            self.language.acknowledge_notice(notice.id);
        }
    }

    fn stage_local_effect(&mut self, effect: &ModelEffect) {
        let ModelEffect::Navigate {
            navigation_generation,
            document,
            run,
            span,
        } = effect
        else {
            return;
        };
        let Some(current_document) = self.model.document() else {
            return;
        };
        if current_document.stamp() != *document || self.editor_text != current_document.text() {
            return;
        }
        let provenance_is_current = self.model.active_run() == Some(*run)
            || self
                .model
                .retained_snapshot()
                .is_some_and(|snapshot| snapshot.document() == *document && snapshot.run() == *run)
            || self.model.problems().iter().any(|problem| {
                problem.document() == *document
                    && problem.run() == *run
                    && problem.diagnostic().span == **span
            });
        if !provenance_is_current {
            return;
        }
        let mapper = SourceMapper::for_document(*document, current_document.text());
        let key = EditorSourceKey::new(*document, *run);
        let marker = MarkerSpan::new(key, **span);
        let Ok(range) = mapper.map_run_range(key, marker) else {
            return;
        };
        self.install_navigation_target(
            *navigation_generation,
            NavigationOrigin::Run { binding: *run },
            range,
        );
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
                .filter_map(AppAction::ui_action)
                .map(ModelEvent::Ui),
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
                    navigation_generation: _,
                    document: _,
                    run: _,
                    span: _,
                } => {}
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

    fn shutdown_backend(&mut self) {
        self.language.request_shutdown();
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
                ui.menu_button("Navigate", |ui| {
                    for spec in catalog
                        .iter()
                        .filter(|spec| spec.section == ActionSection::Navigate)
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
                ui.separator();
                ui.label(format!(
                    "Language: {}",
                    language_status_label(self.language.snapshot().status())
                ));
                if let Some(message) = self.language_error.as_deref() {
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
        let snapshot = Arc::clone(self.language.snapshot());
        let analysis = snapshot
            .diagnostics()
            .filter(|diagnostics| self.analysis_is_current(&snapshot, diagnostics))
            .cloned();
        let language_status = snapshot.status();
        let stderr_tail = snapshot.stderr_tail().cloned();
        let mut selected_analysis = None;
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
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Analysis").strong());
                                if analysis.is_none() {
                                    ui.small(match language_status {
                                        LanguageStatus::Unavailable | LanguageStatus::Disabled => {
                                            "Unavailable"
                                        }
                                        LanguageStatus::Starting | LanguageStatus::Initializing => {
                                            "Starting"
                                        }
                                        LanguageStatus::ShuttingDown => "Shutting down",
                                        LanguageStatus::Ready | LanguageStatus::Limited => {
                                            "Waiting for current document"
                                        }
                                    });
                                }
                            });
                            if let Some(diagnostics) = analysis.as_ref() {
                                if diagnostics.items.is_empty() {
                                    ui.label("No analysis problems.");
                                }
                                for item in diagnostics.items.iter() {
                                    let mut label = format!(
                                        "{} {}: {}",
                                        analysis_severity_label(item.severity),
                                        item.phase.map_or("analysis", analysis_phase_label),
                                        escape_display_text(&item.message),
                                    );
                                    if let Some(code) = item.code.as_deref() {
                                        let _ = write!(label, " [{code}]");
                                    }
                                    if let Some(source) = item.source.as_deref() {
                                        let _ = write!(label, " ({source})");
                                    }
                                    if let Some(range) = item.range.as_ref() {
                                        let _ = write!(
                                            label,
                                            " — characters {}–{}",
                                            range.characters.start, range.characters.end
                                        );
                                        if ui.add(Button::new(label)).clicked() {
                                            selected_analysis = Some((
                                                diagnostics.process_generation,
                                                diagnostics.stamp,
                                                diagnostics.revision,
                                                item.id,
                                                DocumentRange::new(
                                                    diagnostics.stamp,
                                                    range.bytes.clone(),
                                                    CharIndex(range.characters.start)
                                                        ..CharIndex(range.characters.end),
                                                ),
                                            ));
                                        }
                                    } else {
                                        ui.label(label);
                                    }
                                    if item.local_limit {
                                        ui.small("This local limit item has no source location.");
                                    }
                                }
                            }
                            if let Some(stderr) = stderr_tail.as_ref() {
                                ui.collapsing("Language service details", |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(stderr.text.as_ref()).monospace(),
                                        )
                                        .selectable(true),
                                    );
                                    if stderr.truncated {
                                        ui.small("Earlier language-service output was omitted.");
                                    }
                                });
                            }
                            ui.separator();
                            ui.label(RichText::new("Run").strong());
                            if self.model.problems().is_empty() {
                                ui.label("No run problems.");
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
        if let Some((process_generation, stamp, revision, item_id, range)) = selected_analysis
            && let Some(navigation_generation) = self.allocate_navigation_generation()
        {
            self.pending_analysis_click = Some(PendingAnalysisClick {
                navigation_generation,
                process_generation,
                stamp,
                diagnostic_revision: revision,
                item_id,
                range,
            });
            ui.ctx().request_repaint();
        }
        if let Some(problem_id) = selected_problem
            && let Some(navigation_generation) = self.allocate_navigation_generation()
        {
            self.defer_ui_action(
                ui.ctx(),
                UiAction::SelectProblem {
                    navigation_generation,
                    problem_id,
                },
            );
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
        if let Some((snapshot, activation_id)) = selected_frame
            && let Some(navigation_generation) = self.allocate_navigation_generation()
        {
            self.defer_ui_action(
                ui.ctx(),
                UiAction::SelectFrame {
                    navigation_generation,
                    snapshot,
                    activation_id,
                },
            );
        }
    }

    fn editor_layout_plan(&mut self) -> (Option<PreparedLayoutPlan>, Option<crate::MappedSpan>) {
        let Some(document) = self.model.document() else {
            self.editor_mapper = None;
            return (None, None);
        };
        if self.editor_text != document.text() {
            self.editor_mapper = None;
            return (None, None);
        }
        let document_stamp = document.stamp();
        if self
            .editor_mapper
            .as_ref()
            .is_none_or(|mapper| mapper.document() != document_stamp || mapper.run_key().is_some())
        {
            self.editor_mapper = Some(SourceMapper::for_document(document_stamp, document.text()));
        }
        let mapper = self
            .editor_mapper
            .as_ref()
            .expect("current document mapper was initialized");
        let retained_source = self
            .model
            .retained_snapshot()
            .filter(|snapshot| snapshot.document() == document_stamp)
            .map(|snapshot| EditorSourceKey::new(snapshot.document(), snapshot.run()));
        let map_run_span = |source: EditorSourceKey, span| {
            mapper
                .map_run_range(source, MarkerSpan::new(source, span))
                .ok()
        };
        let current = retained_source.and_then(|source| {
            self.model
                .current_span()
                .and_then(|span| map_run_span(source, span))
        });
        let fault = retained_source.and_then(|source| {
            self.model
                .fault_span()
                .and_then(|span| map_run_span(source, span))
        });

        let navigation_is_current = self.navigation_target.as_ref().is_some_and(|target| {
            target.navigation_generation
                == self
                    .highest_navigation_intent
                    .unwrap_or(target.navigation_generation)
                && target.range.document == document_stamp
                && match target.origin {
                    NavigationOrigin::Language { process_generation } => {
                        self.language.snapshot().process_generation() == Some(process_generation)
                    }
                    NavigationOrigin::Run { binding } => {
                        self.model.active_run() == Some(binding)
                            || self.model.retained_snapshot().is_some_and(|snapshot| {
                                snapshot.document() == document_stamp && snapshot.run() == binding
                            })
                            || self.model.problems().iter().any(|problem| {
                                problem.document() == document_stamp && problem.run() == binding
                            })
                    }
                }
        });
        if self.navigation_target.is_some() && !navigation_is_current {
            self.navigation_target = None;
            self.navigation_focus_pending = false;
        }
        let navigation = self
            .navigation_target
            .as_ref()
            .map(|target| target.range.clone());
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs {
            current,
            fault,
            navigation: navigation.clone(),
        });

        let snapshot = self.language.snapshot();
        let syntax = snapshot.syntax().filter(|syntax| {
            snapshot.process_generation() == Some(syntax.process_generation)
                && snapshot.desired_stamp() == Some(document_stamp)
                && snapshot.written_document().is_some_and(|written| {
                    written.stamp == document_stamp
                        && written.uri.as_ref() == syntax.uri.as_ref()
                        && written.lsp_version == Some(syntax.lsp_version)
                })
                && syntax.stamp == document_stamp
                && syntax.uri.as_ref() == synthetic_uri(document_stamp.document_id)
        });
        let prepared_syntax = syntax.map_or_else(Vec::new, |syntax| {
            syntax
                .runs
                .iter()
                .map(|run| {
                    PreparedSyntaxRun::new(
                        DocumentRange::new(
                            document_stamp,
                            run.range.bytes.clone(),
                            CharIndex(run.range.characters.start)
                                ..CharIndex(run.range.characters.end),
                        ),
                        syntax_class(run.kind),
                    )
                })
                .collect()
        });
        let plan = PreparedLayoutPlan::compose(mapper, &markers, &prepared_syntax);
        let focus = self
            .navigation_focus_pending
            .then_some(navigation.as_ref())
            .flatten()
            .and_then(|range| mapper.map_document_range(range).ok());
        if self.navigation_focus_pending && focus.is_none() {
            self.navigation_focus_pending = false;
            self.editor_notice = Some("That source location is no longer available.".to_owned());
        }
        (Some(plan), focus)
    }

    fn show_editor(&mut self, ui: &mut egui::Ui) {
        let layout_state = self.editor_layout_plan();
        let layout_source = self.model.document().map(|document| document.shared_text());
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
            if let Some(notice) = self.language_notice.as_deref() {
                ui.colored_label(Color32::from_rgb(82, 72, 25), notice);
            }
            ui.separator();
            let source_label = ui.label("Source editor");
            let editor_id = egui::Id::new("oxide_source_editor").with(
                self.editor_stamp
                    .map(|stamp| stamp.document_id.get())
                    .unwrap_or_default(),
            );
            let (layout_plan, navigation) = layout_state;
            let layout_plan = layout_plan.as_ref();
            ui.small(marker_summary(layout_plan));
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
                            RichText::new(gutter_text(line_count, layout_plan)).monospace(),
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
                                        if let Some(job) = current_prepared_layout_job(
                                            self.editor_stamp,
                                            layout_source.as_ref(),
                                            buffer.as_str(),
                                            layout_plan,
                                            |syntax, markers| {
                                                prepared_text_format(
                                                    layout_ui,
                                                    syntax,
                                                    markers,
                                                )
                                            },
                                        ) {
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
                                let prepared_source_is_current =
                                    prepared_editor_source_is_current(
                                        layout_source.as_ref(),
                                        self.editor_text.as_str(),
                                    );
                                let navigation_input_is_current =
                                    prepared_source_is_current && !cursor_changed;
                                if !navigation_input_is_current && navigation.is_some() {
                                    self.navigation_target = None;
                                    self.navigation_focus_pending = false;
                                }
                                if navigation_input_is_current
                                    && let Some(span) = navigation
                                {
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
        self.reduce_pending_events(ctx);
        self.reconcile_language(ctx);
        self.consume_shortcut(ctx);
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

fn show_call_stack(ui: &mut egui::Ui, model: &AppModel) -> Option<(SnapshotKey, ActivationId)> {
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
            selected = Some((retained.key(), frame.activation_id));
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
    activation: rlox_protocol::ActivationId,
    scope: BindingScope,
    bindings: &[rlox_protocol::BindingSnapshot],
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

fn gutter_text(line_count: usize, plan: Option<&PreparedLayoutPlan>) -> String {
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

fn marker_summary(plan: Option<&PreparedLayoutPlan>) -> String {
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

fn syntax_class(kind: SyntaxKind) -> SyntaxClass {
    match kind {
        SyntaxKind::Keyword => SyntaxClass::Keyword,
        SyntaxKind::Comment => SyntaxClass::Comment,
        SyntaxKind::String => SyntaxClass::String,
        SyntaxKind::Number => SyntaxClass::Number,
        SyntaxKind::Variable => SyntaxClass::Variable,
        SyntaxKind::Operator => SyntaxClass::Operator,
    }
}

fn prepared_editor_source_is_current(planned_source: Option<&Arc<str>>, editor_text: &str) -> bool {
    planned_source.is_some_and(|planned_source| planned_source.as_ref() == editor_text)
}

fn current_prepared_layout_job(
    document: Option<DocumentStamp>,
    planned_source: Option<&Arc<str>>,
    editor_text: &str,
    plan: Option<&PreparedLayoutPlan>,
    format_for: impl FnMut(Option<SyntaxClass>, MarkerMask) -> egui::text::TextFormat,
) -> Option<egui::text::LayoutJob> {
    if !prepared_editor_source_is_current(planned_source, editor_text) {
        return None;
    }
    Some(build_prepared_layout_job(
        document?,
        editor_text,
        plan?,
        format_for,
    ))
}

fn prepared_text_format(
    ui: &egui::Ui,
    syntax: Option<SyntaxClass>,
    markers: MarkerMask,
) -> egui::TextFormat {
    let mut format = marker_text_format(ui, markers);
    format.color = match syntax {
        Some(SyntaxClass::Keyword) => Color32::from_rgb(94, 53, 177),
        Some(SyntaxClass::Comment) => Color32::from_rgb(82, 112, 82),
        Some(SyntaxClass::String) => Color32::from_rgb(172, 67, 31),
        Some(SyntaxClass::Number) => Color32::from_rgb(24, 99, 160),
        Some(SyntaxClass::Variable) => Color32::from_rgb(35, 55, 79),
        Some(SyntaxClass::Operator) => Color32::from_rgb(86, 86, 86),
        None => ui.visuals().text_color(),
    };
    format
}

fn phase_label(phase: rlox_protocol::DiagnosticPhase) -> &'static str {
    match phase {
        rlox_protocol::DiagnosticPhase::Scanner => "scanner",
        rlox_protocol::DiagnosticPhase::Parser => "parser",
        rlox_protocol::DiagnosticPhase::Compiler => "compiler",
        rlox_protocol::DiagnosticPhase::Runtime => "runtime",
        rlox_protocol::DiagnosticPhase::Worker => "worker",
    }
}

fn severity_label(severity: rlox_protocol::DiagnosticSeverity) -> &'static str {
    match severity {
        rlox_protocol::DiagnosticSeverity::Error => "Error",
        rlox_protocol::DiagnosticSeverity::Warning => "Warning",
    }
}

fn analysis_phase_label(phase: AnalysisPhase) -> &'static str {
    match phase {
        AnalysisPhase::Scanner => "scanner",
        AnalysisPhase::Parser => "parser",
        AnalysisPhase::Compiler => "compiler",
        AnalysisPhase::Runtime => "runtime",
        AnalysisPhase::Worker => "worker",
    }
}

fn analysis_severity_label(severity: AnalysisSeverity) -> &'static str {
    match severity {
        AnalysisSeverity::Error => "Error",
        AnalysisSeverity::Warning => "Warning",
        AnalysisSeverity::Information => "Information",
        AnalysisSeverity::Hint => "Hint",
    }
}

fn language_status_label(status: LanguageStatus) -> &'static str {
    match status {
        LanguageStatus::Starting => "starting",
        LanguageStatus::Initializing => "initializing",
        LanguageStatus::Ready => "ready",
        LanguageStatus::Unavailable => "unavailable",
        LanguageStatus::ShuttingDown => "shutting down",
        LanguageStatus::Disabled => "disabled",
        LanguageStatus::Limited => "limited",
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
        ActionSection::Navigate => false,
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
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use eframe::egui::text::{CCursor, CharIndex};
    use egui_kittest::{Harness, kittest::Queryable};
    use rlox_protocol::{RevisionId, SourceId, SourceSpan, TextPosition};

    use super::*;
    use crate::FileModelEvent;
    use crate::language::{
        AcknowledgementBatch, AnalysisDiagnostic, AnalysisPhase, CaretCommand, CaretGeneration,
        DefinitionIntent, DefinitionResultId, DefinitionSnapshot, DefinitionTargetSnapshot,
        DesiredDocument, DiagnosticItemId, DiagnosticSetRevision, DiagnosticSeverity,
        DiagnosticSnapshot, DocumentSyncSnapshot, LanguageClosed, LanguageNotice, LanguageSnapshot,
        LanguageSnapshotDraft, LspVersion, NoticeId, NoticeKind, ProcessGeneration,
        SnapshotRevision, SyntaxKind, SyntaxRun, SyntaxSnapshot, TextRange, WriterState,
    };
    use crate::language_ui::{LanguagePort, unavailable_snapshot};

    #[derive(Default)]
    struct CapturedLanguageCommands {
        documents: Vec<Option<DesiredDocument>>,
        carets: Vec<CaretCommand>,
        definitions: Vec<DefinitionIntent>,
        acknowledgements: Vec<AcknowledgementBatch>,
        shutdowns: usize,
    }

    struct FakeLanguagePort {
        snapshot: Arc<Mutex<Arc<LanguageSnapshot>>>,
        commands: Arc<Mutex<CapturedLanguageCommands>>,
    }

    struct SequencedLanguagePort {
        snapshots: Mutex<VecDeque<Arc<LanguageSnapshot>>>,
        commands: Arc<Mutex<CapturedLanguageCommands>>,
    }

    impl LanguagePort for FakeLanguagePort {
        fn load_snapshot(&self) -> Arc<LanguageSnapshot> {
            Arc::clone(&self.snapshot.lock().expect("snapshot"))
        }

        fn submit_document(&self, desired: Option<DesiredDocument>) -> Result<(), LanguageClosed> {
            self.commands
                .lock()
                .expect("commands")
                .documents
                .push(desired);
            Ok(())
        }

        fn submit_caret(&self, command: CaretCommand) -> Result<(), LanguageClosed> {
            self.commands.lock().expect("commands").carets.push(command);
            Ok(())
        }

        fn request_definition(&self, intent: DefinitionIntent) -> Result<(), LanguageClosed> {
            self.commands
                .lock()
                .expect("commands")
                .definitions
                .push(intent);
            Ok(())
        }

        fn acknowledge_items(&self, batch: AcknowledgementBatch) -> Result<(), LanguageClosed> {
            self.commands
                .lock()
                .expect("commands")
                .acknowledgements
                .push(batch);
            Ok(())
        }

        fn request_shutdown(&self) {
            self.commands.lock().expect("commands").shutdowns += 1;
        }
    }

    impl LanguagePort for SequencedLanguagePort {
        fn load_snapshot(&self) -> Arc<LanguageSnapshot> {
            let mut snapshots = self.snapshots.lock().expect("snapshots");
            if snapshots.len() > 1 {
                snapshots.pop_front().expect("nonempty snapshot sequence")
            } else {
                Arc::clone(snapshots.front().expect("nonempty snapshot sequence"))
            }
        }

        fn submit_document(&self, desired: Option<DesiredDocument>) -> Result<(), LanguageClosed> {
            self.commands
                .lock()
                .expect("commands")
                .documents
                .push(desired);
            Ok(())
        }

        fn submit_caret(&self, command: CaretCommand) -> Result<(), LanguageClosed> {
            self.commands.lock().expect("commands").carets.push(command);
            Ok(())
        }

        fn request_definition(&self, intent: DefinitionIntent) -> Result<(), LanguageClosed> {
            self.commands
                .lock()
                .expect("commands")
                .definitions
                .push(intent);
            Ok(())
        }

        fn acknowledge_items(&self, batch: AcknowledgementBatch) -> Result<(), LanguageClosed> {
            self.commands
                .lock()
                .expect("commands")
                .acknowledgements
                .push(batch);
            Ok(())
        }

        fn request_shutdown(&self) {
            self.commands.lock().expect("commands").shutdowns += 1;
        }
    }

    type SharedLanguageSnapshot = Arc<Mutex<Arc<LanguageSnapshot>>>;
    type SharedLanguageCommands = Arc<Mutex<CapturedLanguageCommands>>;

    fn app_with_language_capture() -> (OxideApp, SharedLanguageSnapshot, SharedLanguageCommands) {
        let snapshot = Arc::new(Mutex::new(unavailable_snapshot()));
        let commands = Arc::new(Mutex::new(CapturedLanguageCommands::default()));
        let port = FakeLanguagePort {
            snapshot: Arc::clone(&snapshot),
            commands: Arc::clone(&commands),
        };
        (
            OxideApp::headless_with_language_port(AppModel::new(), Box::new(port)),
            snapshot,
            commands,
        )
    }

    fn app_with_language_sequence(
        snapshots: impl IntoIterator<Item = Arc<LanguageSnapshot>>,
    ) -> (OxideApp, SharedLanguageCommands) {
        let snapshots = snapshots.into_iter().collect::<VecDeque<_>>();
        assert!(!snapshots.is_empty(), "snapshot sequence must not be empty");
        let commands = Arc::new(Mutex::new(CapturedLanguageCommands::default()));
        let port = SequencedLanguagePort {
            snapshots: Mutex::new(snapshots),
            commands: Arc::clone(&commands),
        };
        (
            OxideApp::headless_with_language_port(AppModel::new(), Box::new(port)),
            commands,
        )
    }

    fn ready_language_snapshot(stamp: DocumentStamp) -> Arc<LanguageSnapshot> {
        language_snapshot(stamp, None, None)
    }

    fn language_snapshot(
        stamp: DocumentStamp,
        diagnostics: Option<DiagnosticSnapshot>,
        definition: Option<DefinitionSnapshot>,
    ) -> Arc<LanguageSnapshot> {
        language_snapshot_with_syntax(stamp, diagnostics, None, definition)
    }

    fn language_snapshot_with_syntax(
        stamp: DocumentStamp,
        diagnostics: Option<DiagnosticSnapshot>,
        syntax: Option<SyntaxSnapshot>,
        definition: Option<DefinitionSnapshot>,
    ) -> Arc<LanguageSnapshot> {
        language_snapshot_for_process(
            stamp,
            ProcessGeneration::from_raw(1).expect("process"),
            diagnostics,
            syntax,
            definition,
        )
    }

    fn language_snapshot_for_process(
        stamp: DocumentStamp,
        process_generation: ProcessGeneration,
        diagnostics: Option<DiagnosticSnapshot>,
        syntax: Option<SyntaxSnapshot>,
        definition: Option<DefinitionSnapshot>,
    ) -> Arc<LanguageSnapshot> {
        language_snapshot_for_process_with_notices(
            stamp,
            process_generation,
            diagnostics,
            syntax,
            definition,
            Vec::new(),
        )
    }

    fn language_snapshot_for_process_with_notices(
        stamp: DocumentStamp,
        process_generation: ProcessGeneration,
        diagnostics: Option<DiagnosticSnapshot>,
        syntax: Option<SyntaxSnapshot>,
        definition: Option<DefinitionSnapshot>,
        notices: Vec<LanguageNotice>,
    ) -> Arc<LanguageSnapshot> {
        let uri: Arc<str> = Arc::from(synthetic_uri(stamp.document_id));
        let sync = DocumentSyncSnapshot {
            stamp,
            uri,
            lsp_version: Some(LspVersion::from_raw(1).expect("version")),
        };
        Arc::new(LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(2).expect("revision"),
            process_generation: Some(process_generation),
            status: LanguageStatus::Ready,
            desired_document: Some(sync.clone()),
            written_document: Some(sync),
            diagnostics,
            syntax,
            definition,
            notices,
            stderr_tail: None,
            writer: WriterState::Idle,
        }))
    }

    fn definition_result(
        stamp: DocumentStamp,
        navigation_generation: NavigationGeneration,
        targets: &[std::ops::Range<usize>],
    ) -> DefinitionSnapshot {
        definition_result_for_process(
            stamp,
            ProcessGeneration::from_raw(1).expect("process"),
            navigation_generation,
            targets,
        )
    }

    fn definition_result_for_process(
        stamp: DocumentStamp,
        process_generation: ProcessGeneration,
        navigation_generation: NavigationGeneration,
        targets: &[std::ops::Range<usize>],
    ) -> DefinitionSnapshot {
        DefinitionSnapshot {
            id: DefinitionResultId::from_raw(1).expect("definition id"),
            process_generation,
            uri: Arc::from(synthetic_uri(stamp.document_id)),
            stamp,
            lsp_version: LspVersion::from_raw(1).expect("version"),
            caret_generation: CaretGeneration::from_raw(1).expect("caret"),
            navigation_generation,
            caret_character: 0,
            targets: targets
                .iter()
                .cloned()
                .map(|range| DefinitionTargetSnapshot {
                    range: TextRange {
                        bytes: range.clone(),
                        characters: range,
                    },
                })
                .collect(),
        }
    }

    fn diagnostics(
        stamp: DocumentStamp,
        revision: u64,
        ranges: &[Option<std::ops::Range<usize>>],
    ) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            process_generation: ProcessGeneration::from_raw(1).expect("process"),
            uri: Arc::from(synthetic_uri(stamp.document_id)),
            stamp,
            lsp_version: LspVersion::from_raw(1).expect("version"),
            revision: DiagnosticSetRevision::from_raw(revision).expect("diagnostic revision"),
            items: ranges
                .iter()
                .enumerate()
                .map(|(index, range)| AnalysisDiagnostic {
                    id: DiagnosticItemId::from_raw(index as u64 + 1).expect("diagnostic id"),
                    range: range.as_ref().map(|range| TextRange {
                        bytes: range.clone(),
                        characters: range.clone(),
                    }),
                    severity: DiagnosticSeverity::Error,
                    phase: Some(AnalysisPhase::Parser),
                    code: Some(Arc::from("parse.test")),
                    source: Some(Arc::from("rlox")),
                    message: Arc::from("problem"),
                    local_limit: range.is_none(),
                })
                .collect(),
        }
    }

    fn collapsed_cursor(character: usize) -> CCursorRange {
        let cursor = CCursor::new(character);
        CCursorRange {
            primary: cursor,
            secondary: cursor,
            h_pos: None,
        }
    }

    fn set_source(app: &mut OxideApp, source: &str) -> DocumentStamp {
        let stamp = app.model.document().expect("document").stamp();
        app.queue_event(ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: source.to_owned(),
        }));
        assert_eq!(app.pump_events(1), 1);
        app.model.document().expect("edited document").stamp()
    }

    fn language_ready_app(source: &str) -> OxideApp {
        let (mut app, snapshot, _) = app_with_language_capture();
        let stamp = set_source(&mut app, source);
        *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
        app.reconcile_language(&egui::Context::default());
        assert_eq!(
            app.language_process,
            Some(ProcessGeneration::from_raw(1).expect("process"))
        );
        app
    }

    fn running_app(source: &str) -> (OxideApp, crate::RunBinding) {
        let mut model = AppModel::new();
        let stamp = model.document().expect("document").stamp();
        assert!(
            apply_event(
                &mut model,
                ModelEvent::Ui(UiAction::Edit {
                    document: stamp,
                    text: source.to_owned(),
                }),
            )
            .is_empty()
        );
        let start = apply_event(&mut model, ModelEvent::Ui(UiAction::Run));
        let [ModelEffect::Start(intent)] = start.as_slice() else {
            panic!("run should start");
        };
        let run = crate::RunBinding {
            worker_session_id: crate::WorkerSessionId(1),
            run_id: crate::RunId(1),
            source_id: SourceId(1),
            source_revision: RevisionId(1),
        };
        assert!(
            apply_event(
                &mut model,
                ModelEvent::Supervisor(SupervisorModelEvent::Started {
                    client_start_id: intent.client_start_id,
                    mode: crate::RunMode::Run,
                    run,
                    request_id: RequestId(1),
                    next_event_sequence: crate::EventSequence(2),
                }),
            )
            .is_empty()
        );
        (OxideApp::headless(model), run)
    }

    fn source_span(run: crate::RunBinding, start: usize, end: usize) -> SourceSpan {
        SourceSpan {
            source_id: run.source_id,
            revision: run.source_revision,
            start: TextPosition {
                byte_offset: start,
                line: 1,
                column: start + 1,
            },
            end: TextPosition {
                byte_offset: end,
                line: 1,
                column: end + 1,
            },
        }
    }

    #[test]
    fn navigation_allocator_uses_the_last_generation_once_then_fails_closed() {
        let mut app = OxideApp::headless(AppModel::new());
        let last = NavigationGeneration::from_raw(u64::MAX).expect("maximum value is valid");
        app.next_navigation_generation = Some(last);

        assert_eq!(app.allocate_navigation_generation(), Some(last));
        assert_eq!(app.allocate_navigation_generation(), None);
        assert_eq!(app.allocate_navigation_generation(), None);
    }

    #[test]
    fn a_new_navigation_intent_clears_the_old_target_and_rejects_its_delayed_result() {
        let mut app = OxideApp::headless(AppModel::new());
        let stamp = set_source(&mut app, "print 1;");
        let process = crate::language::ProcessGeneration::from_raw(1).expect("process");
        let first = app.allocate_navigation_generation().expect("first intent");
        let first_range = crate::DocumentRange::new(stamp, 0..5, CharIndex(0)..CharIndex(5));

        assert!(app.install_navigation_target(
            first,
            NavigationOrigin::Language {
                process_generation: process,
            },
            first_range.clone(),
        ));
        assert_eq!(
            app.navigation_target
                .as_ref()
                .map(|target| target.range.clone()),
            Some(first_range.clone())
        );

        let second = app.allocate_navigation_generation().expect("second intent");
        assert!(app.navigation_target.is_none());
        assert!(!app.install_navigation_target(
            first,
            NavigationOrigin::Language {
                process_generation: process,
            },
            first_range,
        ));
        assert!(app.navigation_target.is_none());

        let second_range = crate::DocumentRange::new(stamp, 6..7, CharIndex(6)..CharIndex(7));
        assert!(app.install_navigation_target(
            second,
            NavigationOrigin::Language {
                process_generation: process,
            },
            second_range.clone(),
        ));
        assert_eq!(
            app.navigation_target
                .as_ref()
                .map(|target| target.range.clone()),
            Some(second_range)
        );
    }

    #[test]
    fn delayed_run_navigation_is_mapped_with_its_real_binding_and_obeys_last_intent_wins() {
        let (mut app, run) = running_app("print 1;");
        let stamp = app.model.document().expect("document").stamp();
        let stale = app.allocate_navigation_generation().expect("stale intent");
        let current = app
            .allocate_navigation_generation()
            .expect("current intent");

        app.stage_local_effect(&ModelEffect::Navigate {
            navigation_generation: stale,
            document: stamp,
            run,
            span: Box::new(source_span(run, 0, 5)),
        });
        assert!(app.navigation_target.is_none());

        app.stage_local_effect(&ModelEffect::Navigate {
            navigation_generation: current,
            document: stamp,
            run,
            span: Box::new(source_span(run, 0, 5)),
        });
        let target = app.navigation_target.as_ref().expect("current target");
        assert_eq!(target.navigation_generation, current);
        assert_eq!(target.origin, NavigationOrigin::Run { binding: run });
        assert_eq!(target.range.document, stamp);
        assert_eq!(target.range.byte_range, 0..5);
        assert_eq!(target.range.char_range, CharIndex(0)..CharIndex(5));
    }

    #[test]
    fn editor_layout_is_document_scoped_even_without_a_run() {
        let mut app = OxideApp::headless(AppModel::new());
        let stamp = app.model.document().expect("document").stamp();

        let (plan, navigation) = app.editor_layout_plan();

        assert_eq!(plan.expect("document layout").document(), stamp);
        assert!(navigation.is_none());
        assert_eq!(
            app.editor_mapper.as_ref().and_then(SourceMapper::run_key),
            None
        );
    }

    #[test]
    fn app_reconciliation_submits_only_the_reduced_document_arc() {
        let (mut app, _snapshot, commands) = app_with_language_capture();
        let initial = app.model.document().expect("document").stamp();
        app.queue_event(ModelEvent::Ui(UiAction::Edit {
            document: initial,
            text: "print 42;".to_owned(),
        }));
        assert_eq!(app.pump_events(32), 1);
        let expected = app.model.document().expect("edited document").shared_text();

        app.reconcile_language(&egui::Context::default());
        app.reconcile_language(&egui::Context::default());

        let commands = commands.lock().expect("commands");
        assert_eq!(commands.documents.len(), 1);
        let desired = commands.documents[0].as_ref().expect("open document");
        assert!(Arc::ptr_eq(&desired.text, &expected));
    }

    #[test]
    fn language_shutdown_is_signal_only_and_idempotent() {
        let (mut app, _snapshot, commands) = app_with_language_capture();

        app.shutdown_backend();
        app.shutdown_backend();
        drop(app);

        assert_eq!(commands.lock().expect("commands").shutdowns, 1);
    }

    #[test]
    fn f12_is_submitted_only_after_its_exact_document_and_caret_survive_a_logic_pass() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = app.model.document().expect("document").stamp();
        *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
        app.editor_cursor_range = Some(collapsed_cursor(0));
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        assert!(
            app.action_catalog()
                .iter()
                .any(|spec| { spec.action == AppAction::GoToDefinition && spec.enabled })
        );

        app.queue_action(AppAction::GoToDefinition);
        assert!(commands.lock().expect("commands").definitions.is_empty());
        app.reconcile_language(&ctx);

        assert_eq!(commands.lock().expect("commands").definitions.len(), 1);
    }

    #[test]
    fn same_frame_edit_drops_a_captured_f12_before_it_reaches_the_port() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = app.model.document().expect("document").stamp();
        *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
        app.editor_cursor_range = Some(collapsed_cursor(0));
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        app.queue_action(AppAction::GoToDefinition);

        app.queue_event(ModelEvent::Ui(UiAction::Edit {
            document: stamp,
            text: "print 1;".to_owned(),
        }));
        assert_eq!(app.pump_events(32), 1);
        app.reconcile_language(&ctx);

        assert!(commands.lock().expect("commands").definitions.is_empty());
    }

    #[test]
    fn same_frame_selection_change_drops_a_captured_f12() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = app.model.document().expect("document").stamp();
        *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
        app.editor_cursor_range = Some(collapsed_cursor(0));
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        app.queue_action(AppAction::GoToDefinition);

        app.editor_cursor_range = Some(CCursorRange {
            primary: CCursor::new(0),
            secondary: CCursor::new(1),
            h_pos: None,
        });
        app.reconcile_language(&ctx);

        assert!(commands.lock().expect("commands").definitions.is_empty());
    }

    #[test]
    fn losing_the_primary_cursor_drops_a_captured_f12() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = app.model.document().expect("document").stamp();
        *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
        app.editor_cursor_range = Some(collapsed_cursor(0));
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        app.queue_action(AppAction::GoToDefinition);

        app.editor_cursor_range = None;
        app.reconcile_language(&ctx);

        assert!(commands.lock().expect("commands").definitions.is_empty());
        assert!(matches!(
            commands.lock().expect("commands").carets.last(),
            Some(CaretCommand::Current(intent)) if intent.primary_character.is_none()
        ));
    }

    #[test]
    fn definition_result_installs_one_neutral_target_and_is_acknowledged_once() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = set_source(&mut app, "print 1;");
        *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
        app.editor_cursor_range = Some(collapsed_cursor(0));
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        app.queue_action(AppAction::GoToDefinition);
        app.reconcile_language(&ctx);
        let navigation = NavigationGeneration::from_raw(1).expect("navigation");
        *snapshot.lock().expect("snapshot") = language_snapshot(
            stamp,
            None,
            Some(definition_result(
                stamp,
                navigation,
                std::slice::from_ref(&(0..5)),
            )),
        );

        app.reconcile_language(&ctx);
        app.reconcile_language(&ctx);

        let target = app.navigation_target.as_ref().expect("definition target");
        assert_eq!(target.navigation_generation, navigation);
        assert_eq!(target.range.byte_range, 0..5);
        let commands = commands.lock().expect("commands");
        assert_eq!(commands.acknowledgements.len(), 1);
        assert_eq!(
            commands.acknowledgements[0].definition,
            DefinitionResultId::from_raw(1)
        );
    }

    #[test]
    fn stale_definition_is_acknowledged_but_cannot_replace_a_newer_navigation_intent() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = set_source(&mut app, "print 1;");
        *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
        app.editor_cursor_range = Some(collapsed_cursor(0));
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        app.queue_action(AppAction::GoToDefinition);
        app.reconcile_language(&ctx);
        let old = NavigationGeneration::from_raw(1).expect("old navigation");
        let newer = app
            .allocate_navigation_generation()
            .expect("newer navigation");
        assert!(newer > old);
        *snapshot.lock().expect("snapshot") = language_snapshot(
            stamp,
            None,
            Some(definition_result(stamp, old, std::slice::from_ref(&(0..5)))),
        );

        app.reconcile_language(&ctx);

        assert!(app.navigation_target.is_none());
        assert_eq!(commands.lock().expect("commands").acknowledgements.len(), 1);
    }

    #[test]
    fn analysis_reveal_latch_ignores_stale_batches_and_resets_on_matching_empty_publish() {
        let (mut app, snapshot, _commands) = app_with_language_capture();
        let stamp = set_source(&mut app, "print 1;");
        let ctx = egui::Context::default();
        *snapshot.lock().expect("snapshot") =
            language_snapshot(stamp, Some(diagnostics(stamp, 1, &[Some(0..5)])), None);
        app.console_tab = ConsoleTab::Output;
        app.reconcile_language(&ctx);
        assert_eq!(app.console_tab, ConsoleTab::Problems);

        app.console_tab = ConsoleTab::Output;
        app.reconcile_language(&ctx);
        assert_eq!(app.console_tab, ConsoleTab::Output);

        *snapshot.lock().expect("snapshot") =
            language_snapshot(stamp, Some(diagnostics(stamp, 2, &[])), None);
        app.reconcile_language(&ctx);
        assert_eq!(app.console_tab, ConsoleTab::Output);

        *snapshot.lock().expect("snapshot") =
            language_snapshot(stamp, Some(diagnostics(stamp, 3, &[Some(0..5)])), None);
        app.reconcile_language(&ctx);
        assert_eq!(app.console_tab, ConsoleTab::Problems);
    }

    #[test]
    fn diagnostic_click_is_dropped_when_its_batch_is_replaced_before_application() {
        let (mut app, snapshot, _commands) = app_with_language_capture();
        let stamp = set_source(&mut app, "print 1;");
        let current = diagnostics(stamp, 1, &[Some(0..5)]);
        *snapshot.lock().expect("snapshot") = language_snapshot(stamp, Some(current.clone()), None);
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        let navigation_generation = app.allocate_navigation_generation().expect("navigation");
        app.pending_analysis_click = Some(PendingAnalysisClick {
            navigation_generation,
            process_generation: current.process_generation,
            stamp,
            diagnostic_revision: current.revision,
            item_id: current.items[0].id,
            range: DocumentRange::new(stamp, 0..5, CharIndex(0)..CharIndex(5)),
        });
        *snapshot.lock().expect("snapshot") =
            language_snapshot(stamp, Some(diagnostics(stamp, 2, &[Some(6..7)])), None);

        app.reconcile_language(&ctx);

        assert!(app.navigation_target.is_none());
    }

    #[test]
    fn current_diagnostic_click_installs_its_exact_document_range() {
        let (mut app, snapshot, _commands) = app_with_language_capture();
        let stamp = set_source(&mut app, "print 1;");
        let current = diagnostics(stamp, 1, &[Some(0..5)]);
        *snapshot.lock().expect("snapshot") = language_snapshot(stamp, Some(current.clone()), None);
        let ctx = egui::Context::default();
        app.reconcile_language(&ctx);
        let navigation_generation = app.allocate_navigation_generation().expect("navigation");
        app.pending_analysis_click = Some(PendingAnalysisClick {
            navigation_generation,
            process_generation: current.process_generation,
            stamp,
            diagnostic_revision: current.revision,
            item_id: current.items[0].id,
            range: DocumentRange::new(stamp, 0..5, CharIndex(0)..CharIndex(5)),
        });

        app.reconcile_language(&ctx);

        let target = app.navigation_target.as_ref().expect("analysis target");
        assert_eq!(target.navigation_generation, navigation_generation);
        assert_eq!(target.range.byte_range, 0..5);
    }

    #[test]
    fn zero_and_multiple_definition_results_are_bounded_and_choose_source_order() {
        let run_case = |targets: &[std::ops::Range<usize>]| {
            let (mut app, snapshot, commands) = app_with_language_capture();
            let stamp = set_source(&mut app, "print 1;");
            *snapshot.lock().expect("snapshot") = ready_language_snapshot(stamp);
            app.editor_cursor_range = Some(collapsed_cursor(0));
            let ctx = egui::Context::default();
            app.reconcile_language(&ctx);
            app.queue_action(AppAction::GoToDefinition);
            app.reconcile_language(&ctx);
            let navigation = NavigationGeneration::from_raw(1).expect("navigation");
            *snapshot.lock().expect("snapshot") = language_snapshot(
                stamp,
                None,
                Some(definition_result(stamp, navigation, targets)),
            );
            app.reconcile_language(&ctx);
            (app, commands)
        };

        let (zero, zero_commands) = run_case(&[]);
        assert!(zero.navigation_target.is_none());
        assert_eq!(
            zero.language_notice.as_deref(),
            Some("No definition was found.")
        );
        assert_eq!(
            zero_commands
                .lock()
                .expect("zero commands")
                .acknowledgements
                .len(),
            1
        );

        let (multiple, multiple_commands) = run_case(&[0..5, 6..7]);
        assert_eq!(
            multiple
                .navigation_target
                .as_ref()
                .expect("first definition")
                .range
                .byte_range,
            0..5
        );
        assert_eq!(
            multiple.language_notice.as_deref(),
            Some("2 definitions found; showing the first.")
        );
        assert_eq!(
            multiple_commands
                .lock()
                .expect("multiple commands")
                .acknowledgements
                .len(),
            1
        );
    }

    #[test]
    fn current_snapshot_maps_all_six_syntax_kinds_into_the_prepared_editor_plan() {
        let (mut app, snapshot, _commands) = app_with_language_capture();
        let stamp = set_source(&mut app, "abcdef");
        let process_generation = ProcessGeneration::from_raw(1).expect("process");
        let version = LspVersion::from_raw(1).expect("version");
        let kinds = [
            SyntaxKind::Keyword,
            SyntaxKind::Comment,
            SyntaxKind::String,
            SyntaxKind::Number,
            SyntaxKind::Variable,
            SyntaxKind::Operator,
        ];
        let syntax = SyntaxSnapshot {
            process_generation,
            uri: Arc::from(synthetic_uri(stamp.document_id)),
            stamp,
            lsp_version: version,
            runs: kinds
                .iter()
                .copied()
                .enumerate()
                .map(|(index, kind)| SyntaxRun {
                    range: TextRange {
                        bytes: index..index + 1,
                        characters: index..index + 1,
                    },
                    kind,
                })
                .collect(),
        };
        *snapshot.lock().expect("snapshot") =
            language_snapshot_with_syntax(stamp, None, Some(syntax), None);
        app.reconcile_language(&egui::Context::default());

        let (plan, _) = app.editor_layout_plan();
        let actual = plan
            .expect("layout")
            .runs()
            .iter()
            .filter_map(|run| run.syntax)
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                SyntaxClass::Keyword,
                SyntaxClass::Comment,
                SyntaxClass::String,
                SyntaxClass::Number,
                SyntaxClass::Variable,
                SyntaxClass::Operator,
            ]
        );
    }

    #[test]
    fn fresh_process_can_reuse_a_definition_id_and_is_processed_and_acknowledged() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = set_source(&mut app, "print 1;");
        let process_one = ProcessGeneration::from_raw(1).expect("process one");
        let process_two = ProcessGeneration::from_raw(2).expect("process two");
        let ctx = egui::Context::default();
        app.editor_cursor_range = Some(collapsed_cursor(0));

        *snapshot.lock().expect("snapshot") =
            language_snapshot_for_process(stamp, process_one, None, None, None);
        app.reconcile_language(&ctx);
        app.queue_action(AppAction::GoToDefinition);
        app.reconcile_language(&ctx);
        let first_navigation = NavigationGeneration::from_raw(1).expect("first navigation");
        *snapshot.lock().expect("snapshot") = language_snapshot_for_process(
            stamp,
            process_one,
            None,
            None,
            Some(definition_result_for_process(
                stamp,
                process_one,
                first_navigation,
                std::slice::from_ref(&(0..5)),
            )),
        );
        app.reconcile_language(&ctx);
        assert_eq!(commands.lock().expect("commands").acknowledgements.len(), 1);

        *snapshot.lock().expect("snapshot") =
            language_snapshot_for_process(stamp, process_two, None, None, None);
        app.reconcile_language(&ctx);
        assert!(app.navigation_target.is_none());
        assert!(app.language_notice.is_none());
        app.queue_action(AppAction::GoToDefinition);
        app.reconcile_language(&ctx);
        let second_navigation = NavigationGeneration::from_raw(2).expect("second navigation");
        *snapshot.lock().expect("snapshot") = language_snapshot_for_process(
            stamp,
            process_two,
            None,
            None,
            Some(definition_result_for_process(
                stamp,
                process_two,
                second_navigation,
                std::slice::from_ref(&(6..7)),
            )),
        );

        app.reconcile_language(&ctx);

        assert_eq!(commands.lock().expect("commands").acknowledgements.len(), 2);
        let target = app.navigation_target.as_ref().expect("process two target");
        assert_eq!(target.navigation_generation, second_navigation);
        assert_eq!(target.range.byte_range, 6..7);
    }

    #[test]
    fn fresh_process_can_reuse_a_notice_id_without_suppressing_the_new_message() {
        let (mut app, snapshot, commands) = app_with_language_capture();
        let stamp = app.model.document().expect("document").stamp();
        let process_one = ProcessGeneration::from_raw(1).expect("process one");
        let process_two = ProcessGeneration::from_raw(2).expect("process two");
        let notice_id = NoticeId::from_raw(1).expect("notice id");
        let make_notice = |message: &'static str| LanguageNotice {
            id: notice_id,
            kind: NoticeKind::Information,
            message: Arc::from(message),
        };
        let ctx = egui::Context::default();

        *snapshot.lock().expect("snapshot") = language_snapshot_for_process_with_notices(
            stamp,
            process_one,
            None,
            None,
            None,
            vec![make_notice("first process")],
        );
        app.reconcile_language(&ctx);
        assert_eq!(app.language_notice.as_deref(), Some("first process"));
        assert_eq!(commands.lock().expect("commands").acknowledgements.len(), 1);

        *snapshot.lock().expect("snapshot") = language_snapshot_for_process_with_notices(
            stamp,
            process_two,
            None,
            None,
            None,
            vec![make_notice("second process")],
        );
        app.reconcile_language(&ctx);

        assert_eq!(app.language_notice.as_deref(), Some("second process"));
        assert_eq!(commands.lock().expect("commands").acknowledgements.len(), 2);
    }

    #[test]
    fn one_reconciliation_pass_cannot_mix_reused_items_from_two_process_snapshots() {
        let model = AppModel::new();
        let stamp = model.document().expect("document").stamp();
        let process_one = ProcessGeneration::from_raw(1).expect("process one");
        let process_two = ProcessGeneration::from_raw(2).expect("process two");
        let definition_id = DefinitionResultId::from_raw(1).expect("definition id");
        let notice_id = NoticeId::from_raw(1).expect("notice id");
        let navigation = NavigationGeneration::from_raw(1).expect("navigation");
        let snapshot = |process_generation, message: &'static str| {
            let mut definition =
                definition_result_for_process(stamp, process_generation, navigation, &[]);
            definition.id = definition_id;
            language_snapshot_for_process_with_notices(
                stamp,
                process_generation,
                None,
                None,
                Some(definition),
                vec![LanguageNotice {
                    id: notice_id,
                    kind: NoticeKind::Information,
                    message: Arc::from(message),
                }],
            )
        };
        let (mut app, commands) = app_with_language_sequence([
            unavailable_snapshot(),
            snapshot(process_one, "process one"),
            snapshot(process_two, "process two"),
        ]);
        let ctx = egui::Context::default();

        app.reconcile_language(&ctx);
        assert_eq!(app.language_process, Some(process_one));
        assert_eq!(app.language_notice.as_deref(), Some("process one"));

        app.reconcile_language(&ctx);
        assert_eq!(app.language_process, Some(process_two));
        assert_eq!(app.language_notice.as_deref(), Some("process two"));

        let commands = commands.lock().expect("commands");
        assert_eq!(commands.acknowledgements.len(), 2);
        for (acknowledgement, process_generation) in commands
            .acknowledgements
            .iter()
            .zip([process_one, process_two])
        {
            assert_eq!(acknowledgement.process_generation, process_generation);
            assert_eq!(acknowledgement.definition, Some(definition_id));
            assert_eq!(acknowledgement.notices.as_ref(), [notice_id]);
        }
    }

    #[test]
    fn same_length_edit_invalidates_the_shared_prepared_layout_and_navigation_source() {
        let planned: Arc<str> = Arc::from("print 1;");

        assert!(prepared_editor_source_is_current(
            Some(&planned),
            "print 1;"
        ));
        assert!(!prepared_editor_source_is_current(
            Some(&planned),
            "print 2;"
        ));
        assert!(!prepared_editor_source_is_current(None, "print 1;"));
    }

    #[test]
    fn same_length_replacement_skips_prepared_syntax_and_marker_formatting() {
        let stamp = AppModel::new().document().expect("document").stamp();
        let planned_source: Arc<str> = Arc::from("print 1;");
        let mapper = SourceMapper::for_document(stamp, planned_source.as_ref());
        let range = DocumentRange::new(stamp, 0..5, CharIndex(0)..CharIndex(5));
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs {
            navigation: Some(range.clone()),
            ..DocumentMarkerInputs::default()
        });
        let plan = PreparedLayoutPlan::compose(
            &mapper,
            &markers,
            &[PreparedSyntaxRun::new(range, SyntaxClass::Keyword)],
        );
        assert!(plan.runs().iter().any(|run| {
            run.syntax == Some(SyntaxClass::Keyword) && run.markers.contains(MarkerKind::Navigation)
        }));
        let mut formatting_calls = 0;

        let job = current_prepared_layout_job(
            Some(stamp),
            Some(&planned_source),
            "print 2;",
            Some(&plan),
            |_, _| {
                formatting_calls += 1;
                egui::TextFormat::default()
            },
        );

        assert!(job.is_none());
        assert_eq!(formatting_calls, 0);
    }

    #[test]
    fn same_length_ui_edit_does_not_apply_a_pre_edit_navigation_selection() {
        let app = language_ready_app("print 1;");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1_100.0, 700.0))
            .build_eframe(move |_| app);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .focus();
        harness.run();

        {
            let app = harness.state_mut();
            let stamp = app.model.document().expect("document").stamp();
            let navigation = app
                .allocate_navigation_generation()
                .expect("navigation generation");
            assert!(app.install_navigation_target(
                navigation,
                NavigationOrigin::Language {
                    process_generation: ProcessGeneration::from_raw(1).expect("process"),
                },
                DocumentRange::new(stamp, 0..5, CharIndex(0)..CharIndex(5)),
            ));
        }

        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text("print 2;");
        harness.run();
        harness.run();

        let app = harness.state();
        assert_eq!(app.model.document().expect("document").text(), "print 2;");
        assert!(app.navigation_target.is_none());
        let cursor = app.editor_cursor_range.expect("editor cursor");
        assert_eq!(cursor.primary.index.0, "print 2;".chars().count());
        assert_eq!(cursor.secondary.index.0, cursor.primary.index.0);
    }

    #[test]
    fn same_frame_caret_change_cancels_a_pending_navigation_selection() {
        let app = language_ready_app("print 1;");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1_100.0, 700.0))
            .build_eframe(move |_| app);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .focus();
        harness.run();
        let initial_cursor = harness
            .state()
            .editor_cursor_range
            .expect("initial editor cursor");
        assert_eq!(
            initial_cursor.primary.index.0,
            initial_cursor.secondary.index.0
        );
        let (movement, expected_character) = if initial_cursor.primary.index.0 == 0 {
            (egui::Key::ArrowRight, 1)
        } else {
            (egui::Key::ArrowLeft, initial_cursor.primary.index.0 - 1)
        };

        {
            let app = harness.state_mut();
            let stamp = app.model.document().expect("document").stamp();
            let navigation = app
                .allocate_navigation_generation()
                .expect("navigation generation");
            assert!(app.install_navigation_target(
                navigation,
                NavigationOrigin::Language {
                    process_generation: ProcessGeneration::from_raw(1).expect("process"),
                },
                DocumentRange::new(stamp, 0..5, CharIndex(0)..CharIndex(5)),
            ));
        }

        harness.key_press(movement);
        harness.run();
        harness.run();

        let app = harness.state();
        assert!(app.navigation_target.is_none());
        let cursor = app.editor_cursor_range.expect("moved editor cursor");
        assert_eq!(cursor.primary.index.0, expected_character);
        assert_eq!(cursor.secondary.index.0, expected_character);
    }

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

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rlox::{
    ActivationId, PauseReason, RevisionId, SnapshotReason, SourceId, SourceSpan, TextPosition,
    VmSnapshot,
};

use crate::{
    ClosureHealth, Envelope, EventSequence, RequestId, RunId, SubmitError, SupervisorCommandKind,
    WireDiagnostic, WireDocument, WorkerEvent, WorkerSessionId, WorkerTerminationReason,
};

pub const MAX_VISIBLE_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_VISIBLE_OUTPUT_LINES: usize = 10_000;
pub const OUTPUT_TRUNCATION_MARKER: &str = "\n[output truncated]\n";
pub const MAX_OPEN_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SOURCE_LINES: usize = 10_000;
const MAX_PROBLEM_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROBLEMS: usize = 256;

macro_rules! checked_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u64);

        impl $name {
            pub fn from_raw(value: u64) -> Option<Self> {
                (value != 0).then_some(Self(value))
            }

            pub fn get(self) -> u64 {
                self.0
            }
        }
    };
}

checked_id!(DocumentId);
checked_id!(EditRevision);
checked_id!(ClientStartId);
checked_id!(ClientCommandId);
checked_id!(FileOperationId);
checked_id!(ProblemId);
checked_id!(CloseRequestId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentStamp {
    pub document_id: DocumentId,
    pub edit_revision: EditRevision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentBuffer {
    id: DocumentId,
    edit_revision: EditRevision,
    path: Option<PathBuf>,
    text: Arc<str>,
    clean_text: Arc<str>,
    wire_compatible: bool,
}

impl DocumentBuffer {
    fn untitled(id: DocumentId) -> Self {
        Self {
            id,
            edit_revision: EditRevision(1),
            path: None,
            text: Arc::from(""),
            clean_text: Arc::from(""),
            wire_compatible: true,
        }
    }

    fn loaded(id: DocumentId, path: PathBuf, text: String) -> Self {
        let text: Arc<str> = Arc::from(normalize_source(&text));
        let display_name = safe_display_name(&path);
        let wire_compatible = text.len() <= MAX_OPEN_FILE_BYTES
            && source_line_count(&text) <= MAX_SOURCE_LINES
            && WireDocument::validate_content(&display_name, &text).is_ok();
        Self {
            id,
            edit_revision: EditRevision(1),
            path: Some(path),
            clean_text: text.clone(),
            text,
            wire_compatible,
        }
    }

    pub fn id(&self) -> DocumentId {
        self.id
    }

    pub fn edit_revision(&self) -> EditRevision {
        self.edit_revision
    }

    pub fn stamp(&self) -> DocumentStamp {
        DocumentStamp {
            document_id: self.id,
            edit_revision: self.edit_revision,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_dirty(&self) -> bool {
        self.text != self.clean_text
    }

    pub fn display_name(&self) -> String {
        self.path
            .as_deref()
            .map(safe_display_name)
            .unwrap_or_else(|| "untitled.ox".to_string())
    }
}

fn normalize_source(value: &str) -> String {
    value
        .strip_prefix('\u{feff}')
        .unwrap_or(value)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn source_line_count(value: &str) -> usize {
    value
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1)
}

fn safe_display_name(path: &Path) -> String {
    let raw = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let mut value = String::with_capacity(raw.len().min(4096));
    for character in raw.chars() {
        let replacement = character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            );
        let candidate = if replacement { '_' } else { character };
        if value.len() + candidate.len_utf8() > 4096 {
            break;
        }
        value.push(candidate);
    }
    let trimmed = value.trim();
    if trimmed.is_empty() || matches!(trimmed, "." | "..") {
        "source.ox".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Run,
    Debug,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionViewState {
    Idle,
    Starting,
    Running,
    WaitingForInput,
    Paused,
    Completed,
    Cancelled,
    Faulted,
    WorkerCrashed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlAvailability {
    pub run: bool,
    pub debug: bool,
    pub pause: bool,
    pub continue_execution: bool,
    pub step_into: bool,
    pub step_over: bool,
    pub step_out: bool,
    pub stop: bool,
    pub editor: bool,
    pub submit_input: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileControlAvailability {
    pub new: bool,
    pub open: bool,
    pub save: bool,
    pub save_as: bool,
    pub close_document: bool,
    pub exit: bool,
}

impl ControlAvailability {
    fn disabled() -> Self {
        Self {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunBinding {
    pub worker_session_id: WorkerSessionId,
    pub run_id: RunId,
    pub source_id: SourceId,
    pub source_revision: RevisionId,
}

impl RunBinding {
    fn is_valid(self) -> bool {
        self.worker_session_id.0 != 0
            && self.run_id.0 != 0
            && self.source_id.0 != 0
            && self.source_revision.0 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionCommandKind {
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
    ProvideInput,
}

impl ExecutionCommandKind {
    pub fn supervisor_kind(self) -> SupervisorCommandKind {
        match self {
            Self::Pause => SupervisorCommandKind::Pause,
            Self::Continue => SupervisorCommandKind::Continue,
            Self::StepInto => SupervisorCommandKind::StepInto,
            Self::StepOver => SupervisorCommandKind::StepOver,
            Self::StepOut => SupervisorCommandKind::StepOut,
            Self::Stop => SupervisorCommandKind::Stop,
            Self::ProvideInput => SupervisorCommandKind::ProvideInput,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartIntent {
    pub client_start_id: ClientStartId,
    pub mode: RunMode,
    pub display_name: String,
    pub normalized_source: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandIntent {
    pub client_command_id: ClientCommandId,
    pub run: RunBinding,
    pub command: ExecutionCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionCommand {
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
    ProvideInput {
        in_reply_to: RequestId,
        text: String,
    },
}

impl ExecutionCommand {
    fn kind(&self) -> ExecutionCommandKind {
        match self {
            Self::Pause => ExecutionCommandKind::Pause,
            Self::Continue => ExecutionCommandKind::Continue,
            Self::StepInto => ExecutionCommandKind::StepInto,
            Self::StepOver => ExecutionCommandKind::StepOver,
            Self::StepOut => ExecutionCommandKind::StepOut,
            Self::Stop => ExecutionCommandKind::Stop,
            Self::ProvideInput { .. } => ExecutionCommandKind::ProvideInput,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsavedContinuation {
    New,
    Open,
    CloseDocument,
    Exit(CloseRequestId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsavedChoice {
    Save,
    Discard,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileFailureKind {
    NotFound,
    PermissionDenied,
    InvalidData,
    AlreadyExists,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelEffect {
    Start(StartIntent),
    SubmitCommand(CommandIntent),
    CloseWorker {
        target: WorkerTarget,
    },
    PromptUnsaved {
        operation_id: FileOperationId,
        continuation: UnsavedContinuation,
    },
    PickOpen {
        operation_id: FileOperationId,
    },
    PickSaveAs {
        operation_id: FileOperationId,
        suggested_path: Option<PathBuf>,
    },
    ReadFile {
        operation_id: FileOperationId,
        path: PathBuf,
        max_bytes: usize,
    },
    WriteFile {
        operation_id: FileOperationId,
        path: PathBuf,
        contents: Arc<[u8]>,
    },
    Navigate {
        document: DocumentStamp,
        run: RunBinding,
        span: SourceSpan,
    },
    AuthorizeClose {
        close_request_id: CloseRequestId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerTarget {
    PendingStart(ClientStartId),
    Run(RunBinding),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    Edit {
        document: DocumentStamp,
        text: String,
    },
    Run,
    Debug,
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
    SubmitInput {
        in_reply_to: RequestId,
        text: String,
    },
    New,
    Open,
    Save,
    SaveAs,
    CloseDocument,
    RequestExit,
    ResolveUnsaved {
        operation_id: FileOperationId,
        choice: UnsavedChoice,
    },
    SelectProblem(ProblemId),
    SelectFrame {
        snapshot: SnapshotKey,
        activation_id: ActivationId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientSubmission {
    Start(ClientStartId),
    Command(ClientCommandId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorModelEvent {
    RuntimeDisconnected,
    Started {
        client_start_id: ClientStartId,
        mode: RunMode,
        run: RunBinding,
        request_id: RequestId,
        next_event_sequence: EventSequence,
    },
    CommandAdmitted {
        client_command_id: ClientCommandId,
        command: ExecutionCommandKind,
        run: RunBinding,
        request_id: RequestId,
        next_event_sequence: EventSequence,
    },
    CloseAdmitted {
        run: RunBinding,
        request_id: RequestId,
        next_event_sequence: EventSequence,
    },
    SubmissionRejected {
        submission: ClientSubmission,
        error: SubmitError,
    },
    StartFailed {
        client_start_id: ClientStartId,
        kind: std::io::ErrorKind,
    },
    Worker(Box<Envelope<WorkerEvent>>),
    WorkerTerminated {
        client_start_id: Option<ClientStartId>,
        worker_session_id: WorkerSessionId,
        run: Option<RunBinding>,
        reason: WorkerTerminationReason,
    },
    Closed {
        client_start_id: Option<ClientStartId>,
        worker_session_id: WorkerSessionId,
        run: Option<RunBinding>,
        health: ClosureHealth,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileModelEvent {
    OpenPicked {
        operation_id: FileOperationId,
        path: Option<PathBuf>,
    },
    SavePicked {
        operation_id: FileOperationId,
        path: Option<PathBuf>,
    },
    ReadFinished {
        operation_id: FileOperationId,
        result: Result<Vec<u8>, FileFailureKind>,
    },
    WriteFinished {
        operation_id: FileOperationId,
        result: Result<(), FileFailureKind>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelEvent {
    Ui(UiAction),
    Supervisor(SupervisorModelEvent),
    File(FileModelEvent),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    IdExhausted,
    RuntimeDisconnected,
    StartFailed(std::io::ErrorKind),
    StartRejected(SubmitError),
    CommandRejected {
        command: ExecutionCommandKind,
        error: SubmitError,
    },
    WorkerCommandRejected {
        code: String,
        message: String,
    },
    ProtocolDesynchronized,
    WorkerTerminated(WorkerTerminationReason),
    CleanupWarning(ClosureHealth),
    FileFailed(FileFailureKind),
    InvalidUtf8,
    ProblemLimitReached,
    SourceLimitReached,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    id: ProblemId,
    diagnostic: WireDiagnostic,
    document: DocumentStamp,
    run: RunBinding,
    sequence: EventSequence,
}

impl Problem {
    pub fn id(&self) -> ProblemId {
        self.id
    }

    pub fn diagnostic(&self) -> &WireDiagnostic {
        &self.diagnostic
    }

    pub fn document(&self) -> DocumentStamp {
        self.document
    }

    pub fn run(&self) -> RunBinding {
        self.run
    }

    pub fn sequence(&self) -> EventSequence {
        self.sequence
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotProvenance {
    Paused,
    Faulted,
    Cancelled,
    LastSafePause,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedSnapshot {
    snapshot: VmSnapshot,
    key: SnapshotKey,
    document: DocumentStamp,
    run: RunBinding,
    provenance: SnapshotProvenance,
    live: bool,
}

impl RetainedSnapshot {
    pub fn snapshot(&self) -> &VmSnapshot {
        &self.snapshot
    }

    pub fn key(&self) -> SnapshotKey {
        self.key
    }

    pub fn provenance(&self) -> SnapshotProvenance {
        self.provenance
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn document(&self) -> DocumentStamp {
        self.document
    }

    pub fn run(&self) -> RunBinding {
        self.run
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotKey {
    pub run: RunBinding,
    pub sequence: EventSequence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingInput {
    pub request_id: RequestId,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct ProgramOutput {
    text: String,
    line_breaks: usize,
    model_truncated: bool,
    worker_truncated: bool,
}

impl ProgramOutput {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn append(&mut self, value: &str) {
        if self.model_truncated {
            return;
        }
        let available = MAX_VISIBLE_OUTPUT_BYTES.saturating_sub(self.text.len());
        let mut end = available.min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let remaining_line_breaks = MAX_VISIBLE_OUTPUT_LINES
            .saturating_sub(1)
            .saturating_sub(self.line_breaks);
        let mut accepted_line_breaks = 0;
        for (index, byte) in value.as_bytes()[..end].iter().enumerate() {
            if *byte == b'\n' {
                if accepted_line_breaks == remaining_line_breaks {
                    end = index;
                    break;
                }
                accepted_line_breaks += 1;
            }
        }
        self.text.push_str(&value[..end]);
        self.line_breaks = self.line_breaks.saturating_add(accepted_line_breaks);
        self.model_truncated = end < value.len();
    }

    fn rendered(&self) -> Cow<'_, str> {
        if self.model_truncated || self.worker_truncated {
            let maximum_prefix = MAX_VISIBLE_OUTPUT_BYTES - OUTPUT_TRUNCATION_MARKER.len();
            let mut end = self.text.len().min(maximum_prefix);
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            let mut rendered = String::with_capacity(end + OUTPUT_TRUNCATION_MARKER.len());
            rendered.push_str(&self.text[..end]);
            rendered.push_str(OUTPUT_TRUNCATION_MARKER);
            Cow::Owned(rendered)
        } else {
            Cow::Borrowed(&self.text)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingRun {
    client_start_id: ClientStartId,
    mode: RunMode,
    display_name: String,
    source: Arc<str>,
    document: DocumentStamp,
    fallback_state: ExecutionViewState,
    dispatched: bool,
}

impl PendingRun {
    fn intent(&self) -> StartIntent {
        StartIntent {
            client_start_id: self.client_start_id,
            mode: self.mode,
            display_name: self.display_name.clone(),
            normalized_source: self.source.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveRun {
    mode: RunMode,
    binding: RunBinding,
    driver: RequestId,
    driver_kind: Option<ExecutionCommandKind>,
    pause_request: Option<RequestId>,
    close_request: Option<RequestId>,
    next_sequence: Option<EventSequence>,
    document: DocumentStamp,
    source: Arc<str>,
    source_index: SourceIndex,
    desynchronized: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceIndex {
    text_bytes: usize,
    line_starts: Arc<[usize]>,
    scalar_extras: Arc<[(usize, usize)]>,
}

impl SourceIndex {
    fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        let mut scalar_extras = Vec::new();
        let mut cumulative_extra = 0usize;
        for (offset, character) in source.char_indices() {
            let width = character.len_utf8();
            if width > 1 {
                cumulative_extra += width - 1;
                scalar_extras.push((offset + width, cumulative_extra));
            }
            if character == '\n' {
                line_starts.push(offset + 1);
            }
        }
        Self {
            text_bytes: source.len(),
            line_starts: line_starts.into(),
            scalar_extras: scalar_extras.into(),
        }
    }

    fn position(&self, source: &str, byte_offset: usize) -> Option<TextPosition> {
        if byte_offset > self.text_bytes || !source.is_char_boundary(byte_offset) {
            return None;
        }
        let line_index = self
            .line_starts
            .partition_point(|start| *start <= byte_offset)
            .checked_sub(1)?;
        let line_start = self.line_starts[line_index];
        let scalar_extra = self.extra_before(byte_offset) - self.extra_before(line_start);
        Some(TextPosition {
            byte_offset,
            line: line_index + 1,
            column: 1 + byte_offset - line_start - scalar_extra,
        })
    }

    fn extra_before(&self, byte_offset: usize) -> usize {
        let count = self
            .scalar_extras
            .partition_point(|(end, _)| *end <= byte_offset);
        count
            .checked_sub(1)
            .map_or(0, |index| self.scalar_extras[index].1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingCommand {
    client_command_id: ClientCommandId,
    command: ExecutionCommand,
    admitted_request: Option<RequestId>,
    previous_state: ExecutionViewState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalBarrier {
    run: RunBinding,
    sequence: EventSequence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitState {
    None,
    ResolvingDocument(CloseRequestId),
    WaitingForWorker(CloseRequestId),
    Authorized(CloseRequestId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingFile {
    UnsavedPrompt {
        operation_id: FileOperationId,
        document: DocumentStamp,
        continuation: UnsavedContinuation,
    },
    PickOpen {
        operation_id: FileOperationId,
    },
    ReadOpen {
        operation_id: FileOperationId,
        path: PathBuf,
    },
    PickSave {
        operation_id: FileOperationId,
        document: DocumentStamp,
        source: Arc<str>,
        continuation: Option<UnsavedContinuation>,
    },
    Write {
        operation_id: FileOperationId,
        document: DocumentStamp,
        target: PathBuf,
        source: Arc<str>,
        continuation: Option<UnsavedContinuation>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppModel {
    document: Option<DocumentBuffer>,
    execution_state: ExecutionViewState,
    stop_requested: bool,
    pending_run: Option<PendingRun>,
    active_run: Option<ActiveRun>,
    cleanup_run: Option<RunBinding>,
    terminal_barrier: Option<TerminalBarrier>,
    pending_command: Option<PendingCommand>,
    pending_stop: Option<PendingCommand>,
    output: ProgramOutput,
    problems: Vec<Problem>,
    problem_bytes: usize,
    problems_truncated: bool,
    current_span: Option<SourceSpan>,
    fault_span: Option<SourceSpan>,
    navigation_span: Option<SourceSpan>,
    retained_snapshot: Option<RetainedSnapshot>,
    selected_activation: Option<ActivationId>,
    selected_problem: Option<ProblemId>,
    pending_input: Option<PendingInput>,
    pending_file: Option<PendingFile>,
    exit_state: ExitState,
    status: Option<ModelStatus>,
    next_document_id: Option<u64>,
    next_start_id: Option<u64>,
    next_command_id: Option<u64>,
    next_file_id: Option<u64>,
    next_problem_id: Option<u64>,
    next_close_id: Option<u64>,
}

impl Default for AppModel {
    fn default() -> Self {
        Self::new()
    }
}

impl AppModel {
    pub fn new() -> Self {
        Self {
            document: Some(DocumentBuffer::untitled(DocumentId(1))),
            execution_state: ExecutionViewState::Idle,
            stop_requested: false,
            pending_run: None,
            active_run: None,
            cleanup_run: None,
            terminal_barrier: None,
            pending_command: None,
            pending_stop: None,
            output: ProgramOutput::default(),
            problems: Vec::new(),
            problem_bytes: 0,
            problems_truncated: false,
            current_span: None,
            fault_span: None,
            navigation_span: None,
            retained_snapshot: None,
            selected_activation: None,
            selected_problem: None,
            pending_input: None,
            pending_file: None,
            exit_state: ExitState::None,
            status: None,
            next_document_id: Some(2),
            next_start_id: Some(1),
            next_command_id: Some(1),
            next_file_id: Some(1),
            next_problem_id: Some(1),
            next_close_id: Some(1),
        }
    }

    pub fn document(&self) -> Option<&DocumentBuffer> {
        self.document.as_ref()
    }

    pub fn execution_state(&self) -> ExecutionViewState {
        self.execution_state
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    pub fn controls(&self) -> ControlAvailability {
        if self.exit_state != ExitState::None {
            return ControlAvailability::disabled();
        }
        if self.pending_file.is_some() {
            let can_stop = matches!(
                self.execution_state,
                ExecutionViewState::Starting
                    | ExecutionViewState::Running
                    | ExecutionViewState::WaitingForInput
                    | ExecutionViewState::Paused
            ) && !self.stop_requested
                && self.pending_stop.is_none();
            return ControlAvailability {
                stop: can_stop,
                ..ControlAvailability::disabled()
            };
        }
        let has_document = self.document.is_some();
        let runnable_document = self
            .document
            .as_ref()
            .is_some_and(|document| document.wire_compatible);
        let pending_control = self.pending_command.is_some();
        let pause_pending = self
            .active_run
            .as_ref()
            .is_some_and(|run| run.pause_request.is_some());
        let mut controls = ControlAvailability::disabled();
        match self.execution_state {
            ExecutionViewState::Idle
            | ExecutionViewState::Completed
            | ExecutionViewState::Cancelled
            | ExecutionViewState::Faulted
            | ExecutionViewState::WorkerCrashed => {
                controls.run = runnable_document;
                controls.debug = runnable_document;
                controls.editor = has_document;
            }
            ExecutionViewState::Starting => {
                controls.stop = !self.stop_requested && self.pending_stop.is_none();
            }
            ExecutionViewState::Running => {
                controls.pause = !pending_control && !pause_pending && !self.stop_requested;
                controls.stop = !self.stop_requested && self.pending_stop.is_none();
            }
            ExecutionViewState::WaitingForInput => {
                controls.stop = !self.stop_requested && self.pending_stop.is_none();
                controls.submit_input =
                    self.pending_input.is_some() && !pending_control && !self.stop_requested;
            }
            ExecutionViewState::Paused => {
                let resume = !pending_control && !self.stop_requested;
                controls.continue_execution = resume;
                controls.step_into = resume;
                controls.step_over = resume;
                controls.step_out = resume;
                controls.stop = !self.stop_requested && self.pending_stop.is_none();
            }
        }
        controls
    }

    pub fn file_controls(&self) -> FileControlAvailability {
        let idle_file_lane = self.pending_file.is_none() && self.exit_state == ExitState::None;
        let can_replace = document_replacement_allowed(self);
        FileControlAvailability {
            new: can_replace,
            open: can_replace,
            save: idle_file_lane && self.document.is_some(),
            save_as: idle_file_lane && self.document.is_some(),
            close_document: can_replace && self.document.is_some(),
            exit: idle_file_lane,
        }
    }

    pub fn program_output(&self) -> &str {
        &self.output.text
    }

    pub fn rendered_output(&self) -> Cow<'_, str> {
        self.output.rendered()
    }

    pub fn output_was_truncated(&self) -> bool {
        self.output.model_truncated || self.output.worker_truncated
    }

    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    pub fn problems_were_truncated(&self) -> bool {
        self.problems_truncated
    }

    pub fn current_span(&self) -> Option<SourceSpan> {
        self.current_span
    }

    pub fn fault_span(&self) -> Option<SourceSpan> {
        self.fault_span
    }

    pub fn navigation_span(&self) -> Option<SourceSpan> {
        self.navigation_span
    }

    pub fn retained_snapshot(&self) -> Option<&RetainedSnapshot> {
        self.retained_snapshot.as_ref()
    }

    pub fn selected_activation(&self) -> Option<ActivationId> {
        self.selected_activation
    }

    pub fn selected_problem(&self) -> Option<ProblemId> {
        self.selected_problem
    }

    pub fn selected_frame(&self) -> Option<&rlox::FrameSnapshot> {
        let activation = self.selected_activation?;
        self.retained_snapshot
            .as_ref()?
            .snapshot
            .frames
            .iter()
            .find(|frame| frame.activation_id == activation)
    }

    pub fn pending_input(&self) -> Option<&PendingInput> {
        self.pending_input.as_ref()
    }

    pub fn status(&self) -> Option<&ModelStatus> {
        self.status.as_ref()
    }

    pub fn active_run(&self) -> Option<RunBinding> {
        self.active_run.as_ref().map(|run| run.binding)
    }

    pub fn cleanup_pending(&self) -> bool {
        self.cleanup_run.is_some()
    }

    pub fn exit_resolution_active(&self) -> bool {
        self.exit_state != ExitState::None
    }

    fn allocate_id(slot: &mut Option<u64>) -> Option<u64> {
        let value = (*slot)?;
        if value == 0 {
            *slot = None;
            return None;
        }
        *slot = value.checked_add(1);
        Some(value)
    }

    fn allocate_document_id(&mut self) -> Option<DocumentId> {
        Self::allocate_id(&mut self.next_document_id).map(DocumentId)
    }

    fn allocate_start_id(&mut self) -> Option<ClientStartId> {
        Self::allocate_id(&mut self.next_start_id).map(ClientStartId)
    }

    fn allocate_command_id(&mut self) -> Option<ClientCommandId> {
        Self::allocate_id(&mut self.next_command_id).map(ClientCommandId)
    }

    fn allocate_file_id(&mut self) -> Option<FileOperationId> {
        Self::allocate_id(&mut self.next_file_id).map(FileOperationId)
    }

    fn allocate_problem_id(&mut self) -> Option<ProblemId> {
        Self::allocate_id(&mut self.next_problem_id).map(ProblemId)
    }

    fn allocate_close_id(&mut self) -> Option<CloseRequestId> {
        Self::allocate_id(&mut self.next_close_id).map(CloseRequestId)
    }

    fn exhausted(&mut self) {
        self.status = Some(ModelStatus::IdExhausted);
    }
}

pub fn apply_event(model: &mut AppModel, event: ModelEvent) -> Vec<ModelEffect> {
    match event {
        ModelEvent::Ui(action) => apply_ui_action(model, action),
        ModelEvent::Supervisor(event) => apply_supervisor_event(model, event),
        ModelEvent::File(event) => apply_file_event(model, event),
    }
}

fn apply_ui_action(model: &mut AppModel, action: UiAction) -> Vec<ModelEffect> {
    match action {
        UiAction::Edit { document, text } => {
            apply_edit(model, document, text);
            Vec::new()
        }
        UiAction::Run => begin_run(model, RunMode::Run),
        UiAction::Debug => begin_run(model, RunMode::Debug),
        UiAction::Pause => begin_command(model, ExecutionCommand::Pause),
        UiAction::Continue => begin_command(model, ExecutionCommand::Continue),
        UiAction::StepInto => begin_command(model, ExecutionCommand::StepInto),
        UiAction::StepOver => begin_command(model, ExecutionCommand::StepOver),
        UiAction::StepOut => begin_command(model, ExecutionCommand::StepOut),
        UiAction::Stop => request_stop(model),
        UiAction::SubmitInput { in_reply_to, text } => {
            let matching = model
                .pending_input
                .as_ref()
                .is_some_and(|input| input.request_id == in_reply_to);
            if !matching || !model.controls().submit_input {
                Vec::new()
            } else {
                begin_command(model, ExecutionCommand::ProvideInput { in_reply_to, text })
            }
        }
        UiAction::New => begin_destructive(model, UnsavedContinuation::New),
        UiAction::Open => begin_destructive(model, UnsavedContinuation::Open),
        UiAction::Save => begin_save(model, false, None),
        UiAction::SaveAs => begin_save(model, true, None),
        UiAction::CloseDocument => begin_destructive(model, UnsavedContinuation::CloseDocument),
        UiAction::RequestExit => request_exit(model),
        UiAction::ResolveUnsaved {
            operation_id,
            choice,
        } => resolve_unsaved(model, operation_id, choice),
        UiAction::SelectProblem(problem_id) => select_problem(model, problem_id),
        UiAction::SelectFrame {
            snapshot,
            activation_id,
        } => select_frame(model, snapshot, activation_id),
    }
}

fn apply_edit(model: &mut AppModel, stamp: DocumentStamp, text: String) {
    if !model.controls().editor {
        return;
    }
    let normalized = normalize_source(&text);
    if normalized.len() > MAX_OPEN_FILE_BYTES || source_line_count(&normalized) > MAX_SOURCE_LINES {
        model.status = Some(ModelStatus::SourceLimitReached);
        return;
    }
    let Some(document) = model.document.as_mut() else {
        return;
    };
    if document.stamp() != stamp || document.text.as_ref() == normalized {
        return;
    }
    let Some(next_revision) = document.edit_revision.0.checked_add(1) else {
        model.exhausted();
        return;
    };
    let display_name = document.display_name();
    let wire_compatible = normalized.len() <= MAX_OPEN_FILE_BYTES
        && source_line_count(&normalized) <= MAX_SOURCE_LINES
        && WireDocument::validate_content(&display_name, &normalized).is_ok();
    document.edit_revision = EditRevision(next_revision);
    document.text = Arc::from(normalized);
    document.wire_compatible = wire_compatible;
    if matches!(model.status, Some(ModelStatus::SourceLimitReached)) {
        model.status = None;
    }
    if matches!(
        model.execution_state,
        ExecutionViewState::Completed
            | ExecutionViewState::Cancelled
            | ExecutionViewState::Faulted
            | ExecutionViewState::WorkerCrashed
    ) {
        model.execution_state = ExecutionViewState::Idle;
        clear_source_artifacts(model);
    } else {
        model.navigation_span = None;
        model.selected_problem = None;
    }
}

fn begin_run(model: &mut AppModel, mode: RunMode) -> Vec<ModelEffect> {
    let controls = model.controls();
    if !(if mode == RunMode::Run {
        controls.run
    } else {
        controls.debug
    }) {
        return Vec::new();
    }
    let Some(document) = model.document.as_ref() else {
        return Vec::new();
    };
    let document_stamp = document.stamp();
    let display_name = document.display_name();
    let source = document.text.clone();
    let fallback_state = model.execution_state;
    let Some(client_start_id) = model.allocate_start_id() else {
        model.exhausted();
        return Vec::new();
    };
    let dispatched = model.cleanup_run.is_none();
    model.pending_run = Some(PendingRun {
        client_start_id,
        mode,
        display_name,
        source,
        document: document_stamp,
        fallback_state,
        dispatched,
    });
    model.execution_state = ExecutionViewState::Starting;
    model.stop_requested = false;
    model.status = None;
    if dispatched {
        vec![ModelEffect::Start(
            model.pending_run.as_ref().expect("pending run").intent(),
        )]
    } else {
        Vec::new()
    }
}

fn begin_command(model: &mut AppModel, command: ExecutionCommand) -> Vec<ModelEffect> {
    if matches!(
        &command,
        ExecutionCommand::ProvideInput { text, .. } if text.len() > crate::MAX_CONTROL_TEXT_BYTES
    ) {
        model.status = Some(ModelStatus::CommandRejected {
            command: ExecutionCommandKind::ProvideInput,
            error: SubmitError::InvalidCommand,
        });
        return Vec::new();
    }
    let controls = model.controls();
    let permitted = match command.kind() {
        ExecutionCommandKind::Pause => controls.pause,
        ExecutionCommandKind::Continue => controls.continue_execution,
        ExecutionCommandKind::StepInto => controls.step_into,
        ExecutionCommandKind::StepOver => controls.step_over,
        ExecutionCommandKind::StepOut => controls.step_out,
        ExecutionCommandKind::Stop => controls.stop,
        ExecutionCommandKind::ProvideInput => controls.submit_input,
    };
    if !permitted {
        return Vec::new();
    }
    let Some(run) = model.active_run.as_ref().map(|active| active.binding) else {
        return Vec::new();
    };
    let Some(client_command_id) = model.allocate_command_id() else {
        model.exhausted();
        return Vec::new();
    };
    let previous_state = model.execution_state;
    model.pending_command = Some(PendingCommand {
        client_command_id,
        command: command.clone(),
        admitted_request: None,
        previous_state,
    });
    vec![ModelEffect::SubmitCommand(CommandIntent {
        client_command_id,
        run,
        command,
    })]
}

fn request_stop(model: &mut AppModel) -> Vec<ModelEffect> {
    if !model.controls().stop {
        return Vec::new();
    }
    model.stop_requested = true;
    if model.execution_state == ExecutionViewState::Starting {
        let pending_start = model.pending_run.as_ref().map(|run| run.client_start_id);
        if let Some(run) = model.pending_run.as_ref()
            && !run.dispatched
        {
            let fallback = run.fallback_state;
            model.pending_run = None;
            model.execution_state = fallback;
            model.stop_requested = false;
            return Vec::new();
        }
        return pending_start.map_or_else(Vec::new, |start| {
            vec![ModelEffect::CloseWorker {
                target: WorkerTarget::PendingStart(start),
            }]
        });
    }
    begin_stop_command(model)
}

fn begin_stop_command(model: &mut AppModel) -> Vec<ModelEffect> {
    let Some(run) = model.active_run.as_ref().map(|active| active.binding) else {
        return Vec::new();
    };
    let Some(client_command_id) = model.allocate_command_id() else {
        model.exhausted();
        return vec![ModelEffect::CloseWorker {
            target: WorkerTarget::Run(run),
        }];
    };
    model.pending_stop = Some(PendingCommand {
        client_command_id,
        command: ExecutionCommand::Stop,
        admitted_request: None,
        previous_state: model.execution_state,
    });
    vec![ModelEffect::SubmitCommand(CommandIntent {
        client_command_id,
        run,
        command: ExecutionCommand::Stop,
    })]
}

fn clear_source_artifacts(model: &mut AppModel) {
    model.problems.clear();
    model.problem_bytes = 0;
    model.problems_truncated = false;
    model.current_span = None;
    model.fault_span = None;
    model.navigation_span = None;
    model.retained_snapshot = None;
    model.selected_activation = None;
    model.selected_problem = None;
    model.pending_input = None;
}

fn document_replacement_allowed(model: &AppModel) -> bool {
    model.pending_file.is_none()
        && model.exit_state == ExitState::None
        && matches!(
            model.execution_state,
            ExecutionViewState::Idle
                | ExecutionViewState::Completed
                | ExecutionViewState::Cancelled
                | ExecutionViewState::Faulted
                | ExecutionViewState::WorkerCrashed
        )
}

fn begin_destructive(model: &mut AppModel, continuation: UnsavedContinuation) -> Vec<ModelEffect> {
    if !document_replacement_allowed(model) {
        return Vec::new();
    }
    if let Some(document) = model.document.as_ref()
        && document.is_dirty()
    {
        return begin_unsaved_prompt(model, continuation);
    }
    execute_continuation(model, continuation)
}

fn begin_unsaved_prompt(
    model: &mut AppModel,
    continuation: UnsavedContinuation,
) -> Vec<ModelEffect> {
    let Some(document) = model.document.as_ref().map(DocumentBuffer::stamp) else {
        return execute_continuation(model, continuation);
    };
    let Some(operation_id) = model.allocate_file_id() else {
        model.exhausted();
        return match continuation {
            UnsavedContinuation::Exit(close_request_id) => {
                abandon_exit_resolution(model, close_request_id)
            }
            _ => Vec::new(),
        };
    };
    model.pending_file = Some(PendingFile::UnsavedPrompt {
        operation_id,
        document,
        continuation,
    });
    vec![ModelEffect::PromptUnsaved {
        operation_id,
        continuation,
    }]
}

fn begin_save(
    model: &mut AppModel,
    force_picker: bool,
    continuation: Option<UnsavedContinuation>,
) -> Vec<ModelEffect> {
    let resolving_same_exit = matches!(
        (model.exit_state, continuation),
        (
            ExitState::ResolvingDocument(expected),
            Some(UnsavedContinuation::Exit(actual))
        ) if expected == actual
    );
    if model.pending_file.is_some() || (model.exit_state != ExitState::None && !resolving_same_exit)
    {
        return Vec::new();
    }
    let Some(document) = model.document.as_ref() else {
        return Vec::new();
    };
    if !force_picker && continuation.is_none() && !document.is_dirty() && document.path.is_some() {
        return Vec::new();
    }
    let stamp = document.stamp();
    let source = document.text.clone();
    let existing_path = document.path.clone();
    let Some(operation_id) = model.allocate_file_id() else {
        model.exhausted();
        return match continuation {
            Some(UnsavedContinuation::Exit(close_request_id)) => {
                abandon_exit_resolution(model, close_request_id)
            }
            _ => Vec::new(),
        };
    };
    if force_picker {
        model.pending_file = Some(PendingFile::PickSave {
            operation_id,
            document: stamp,
            source,
            continuation,
        });
        vec![ModelEffect::PickSaveAs {
            operation_id,
            suggested_path: existing_path,
        }]
    } else if let Some(target) = existing_path {
        let contents: Arc<[u8]> = Arc::from(source.as_bytes().to_vec());
        model.pending_file = Some(PendingFile::Write {
            operation_id,
            document: stamp,
            target: target.clone(),
            source,
            continuation,
        });
        vec![ModelEffect::WriteFile {
            operation_id,
            path: target,
            contents,
        }]
    } else {
        model.pending_file = Some(PendingFile::PickSave {
            operation_id,
            document: stamp,
            source,
            continuation,
        });
        vec![ModelEffect::PickSaveAs {
            operation_id,
            suggested_path: None,
        }]
    }
}

fn resolve_unsaved(
    model: &mut AppModel,
    operation_id: FileOperationId,
    choice: UnsavedChoice,
) -> Vec<ModelEffect> {
    let Some(PendingFile::UnsavedPrompt {
        operation_id: expected,
        document,
        continuation,
    }) = model.pending_file.clone()
    else {
        return Vec::new();
    };
    if operation_id != expected
        || model
            .document
            .as_ref()
            .is_none_or(|current| current.stamp() != document)
    {
        return Vec::new();
    }
    model.pending_file = None;
    match choice {
        UnsavedChoice::Cancel => match continuation {
            UnsavedContinuation::Exit(close_request_id) => {
                abandon_exit_resolution(model, close_request_id)
            }
            _ => Vec::new(),
        },
        UnsavedChoice::Discard => execute_continuation(model, continuation),
        UnsavedChoice::Save => begin_save(model, false, Some(continuation)),
    }
}

fn execute_continuation(
    model: &mut AppModel,
    continuation: UnsavedContinuation,
) -> Vec<ModelEffect> {
    match continuation {
        UnsavedContinuation::New => {
            let Some(document_id) = model.allocate_document_id() else {
                model.exhausted();
                return Vec::new();
            };
            model.document = Some(DocumentBuffer::untitled(document_id));
            model.execution_state = ExecutionViewState::Idle;
            model.output.clear();
            clear_source_artifacts(model);
            Vec::new()
        }
        UnsavedContinuation::Open => {
            let Some(operation_id) = model.allocate_file_id() else {
                model.exhausted();
                return Vec::new();
            };
            model.pending_file = Some(PendingFile::PickOpen { operation_id });
            vec![ModelEffect::PickOpen { operation_id }]
        }
        UnsavedContinuation::CloseDocument => {
            model.document = None;
            model.execution_state = ExecutionViewState::Idle;
            model.output.clear();
            clear_source_artifacts(model);
            Vec::new()
        }
        UnsavedContinuation::Exit(close_request_id) => {
            model.exit_state = ExitState::ResolvingDocument(close_request_id);
            finish_exit_document_resolution(model, close_request_id)
        }
    }
}

fn request_exit(model: &mut AppModel) -> Vec<ModelEffect> {
    if model.exit_state != ExitState::None || model.pending_file.is_some() {
        return Vec::new();
    }
    let Some(close_request_id) = model.allocate_close_id() else {
        model.exhausted();
        return Vec::new();
    };
    model.exit_state = ExitState::ResolvingDocument(close_request_id);
    if model
        .document
        .as_ref()
        .is_some_and(DocumentBuffer::is_dirty)
    {
        begin_unsaved_prompt(model, UnsavedContinuation::Exit(close_request_id))
    } else {
        finish_exit_document_resolution(model, close_request_id)
    }
}

fn abandon_exit_resolution(
    model: &mut AppModel,
    close_request_id: CloseRequestId,
) -> Vec<ModelEffect> {
    if model.exit_state != ExitState::ResolvingDocument(close_request_id) {
        return Vec::new();
    }
    model.exit_state = ExitState::None;
    dispatch_deferred_run_if_ready(model)
}

fn dispatch_deferred_run_if_ready(model: &mut AppModel) -> Vec<ModelEffect> {
    if model.exit_state != ExitState::None || model.cleanup_run.is_some() {
        return Vec::new();
    }
    let Some(pending) = model.pending_run.as_mut() else {
        return Vec::new();
    };
    if pending.dispatched {
        return Vec::new();
    }
    pending.dispatched = true;
    vec![ModelEffect::Start(pending.intent())]
}

fn finish_exit_document_resolution(
    model: &mut AppModel,
    close_request_id: CloseRequestId,
) -> Vec<ModelEffect> {
    if model.exit_state != ExitState::ResolvingDocument(close_request_id) {
        return Vec::new();
    }
    let target = if let Some(pending) = model.pending_run.as_ref() {
        if pending.dispatched {
            Some(WorkerTarget::PendingStart(pending.client_start_id))
        } else {
            model.cleanup_run.map(WorkerTarget::Run)
        }
    } else {
        model
            .active_run
            .as_ref()
            .map(|active| WorkerTarget::Run(active.binding))
            .or_else(|| model.cleanup_run.map(WorkerTarget::Run))
    };
    if let Some(target) = target {
        model.exit_state = ExitState::WaitingForWorker(close_request_id);
        model.stop_requested = true;
        vec![ModelEffect::CloseWorker { target }]
    } else {
        model.exit_state = ExitState::Authorized(close_request_id);
        vec![ModelEffect::AuthorizeClose { close_request_id }]
    }
}

fn apply_file_event(model: &mut AppModel, event: FileModelEvent) -> Vec<ModelEffect> {
    match event {
        FileModelEvent::OpenPicked { operation_id, path } => {
            let Some(PendingFile::PickOpen {
                operation_id: expected,
            }) = model.pending_file.as_ref()
            else {
                return Vec::new();
            };
            if operation_id != *expected {
                return Vec::new();
            }
            let Some(path) = path else {
                model.pending_file = None;
                return Vec::new();
            };
            model.pending_file = Some(PendingFile::ReadOpen {
                operation_id,
                path: path.clone(),
            });
            vec![ModelEffect::ReadFile {
                operation_id,
                path,
                max_bytes: MAX_OPEN_FILE_BYTES,
            }]
        }
        FileModelEvent::SavePicked { operation_id, path } => {
            let Some(PendingFile::PickSave {
                operation_id: expected,
                document,
                source,
                continuation,
            }) = model.pending_file.clone()
            else {
                return Vec::new();
            };
            if operation_id != expected {
                return Vec::new();
            }
            let Some(target) = path else {
                model.pending_file = None;
                return match continuation {
                    Some(UnsavedContinuation::Exit(close_request_id)) => {
                        abandon_exit_resolution(model, close_request_id)
                    }
                    _ => Vec::new(),
                };
            };
            let contents: Arc<[u8]> = Arc::from(source.as_bytes().to_vec());
            model.pending_file = Some(PendingFile::Write {
                operation_id,
                document,
                target: target.clone(),
                source,
                continuation,
            });
            vec![ModelEffect::WriteFile {
                operation_id,
                path: target,
                contents,
            }]
        }
        FileModelEvent::ReadFinished {
            operation_id,
            result,
        } => finish_read(model, operation_id, result),
        FileModelEvent::WriteFinished {
            operation_id,
            result,
        } => finish_write(model, operation_id, result),
    }
}

fn finish_read(
    model: &mut AppModel,
    operation_id: FileOperationId,
    result: Result<Vec<u8>, FileFailureKind>,
) -> Vec<ModelEffect> {
    let Some(PendingFile::ReadOpen {
        operation_id: expected,
        path,
    }) = model.pending_file.clone()
    else {
        return Vec::new();
    };
    if operation_id != expected {
        return Vec::new();
    }
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(error) => {
            model.pending_file = None;
            model.status = Some(ModelStatus::FileFailed(error));
            return Vec::new();
        }
    };
    if bytes.len() > MAX_OPEN_FILE_BYTES {
        model.pending_file = None;
        model.status = Some(ModelStatus::FileFailed(FileFailureKind::InvalidData));
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(bytes) else {
        model.pending_file = None;
        model.status = Some(ModelStatus::InvalidUtf8);
        return Vec::new();
    };
    let text = normalize_source(&text);
    if source_line_count(&text) > MAX_SOURCE_LINES {
        model.pending_file = None;
        model.status = Some(ModelStatus::SourceLimitReached);
        return Vec::new();
    }
    let Some(document_id) = model.allocate_document_id() else {
        model.pending_file = None;
        model.exhausted();
        return Vec::new();
    };
    model.document = Some(DocumentBuffer::loaded(document_id, path, text));
    model.pending_file = None;
    model.execution_state = ExecutionViewState::Idle;
    model.output.clear();
    clear_source_artifacts(model);
    model.status = None;
    Vec::new()
}

fn finish_write(
    model: &mut AppModel,
    operation_id: FileOperationId,
    result: Result<(), FileFailureKind>,
) -> Vec<ModelEffect> {
    let Some(PendingFile::Write {
        operation_id: expected,
        document,
        target,
        source,
        continuation,
    }) = model.pending_file.clone()
    else {
        return Vec::new();
    };
    if operation_id != expected {
        return Vec::new();
    }
    if let Err(error) = result {
        model.pending_file = None;
        model.status = Some(ModelStatus::FileFailed(error));
        return match continuation {
            Some(UnsavedContinuation::Exit(close_request_id)) => {
                abandon_exit_resolution(model, close_request_id)
            }
            _ => Vec::new(),
        };
    }
    let Some(current) = model.document.as_mut() else {
        model.pending_file = None;
        return match continuation {
            Some(UnsavedContinuation::Exit(close_request_id)) => {
                abandon_exit_resolution(model, close_request_id)
            }
            _ => Vec::new(),
        };
    };
    if current.stamp() != document || current.text != source {
        model.pending_file = None;
        return match continuation {
            Some(UnsavedContinuation::Exit(close_request_id)) => {
                abandon_exit_resolution(model, close_request_id)
            }
            _ => Vec::new(),
        };
    }
    current.path = Some(target);
    current.clean_text = source;
    current.wire_compatible =
        WireDocument::validate_content(&current.display_name(), &current.text).is_ok();
    model.pending_file = None;
    model.status = None;
    continuation.map_or_else(Vec::new, |value| execute_continuation(model, value))
}

fn apply_supervisor_event(model: &mut AppModel, event: SupervisorModelEvent) -> Vec<ModelEffect> {
    match event {
        SupervisorModelEvent::RuntimeDisconnected => runtime_disconnected(model),
        SupervisorModelEvent::Started {
            client_start_id,
            mode,
            run,
            request_id,
            next_event_sequence,
        } => start_admitted(
            model,
            client_start_id,
            mode,
            run,
            request_id,
            next_event_sequence,
        ),
        SupervisorModelEvent::CommandAdmitted {
            client_command_id,
            command,
            run,
            request_id,
            next_event_sequence,
        } => command_admitted(
            model,
            client_command_id,
            command,
            run,
            request_id,
            next_event_sequence,
        ),
        SupervisorModelEvent::CloseAdmitted {
            run,
            request_id,
            next_event_sequence,
        } => close_admitted(model, run, request_id, next_event_sequence),
        SupervisorModelEvent::SubmissionRejected { submission, error } => {
            submission_rejected(model, submission, error)
        }
        SupervisorModelEvent::StartFailed {
            client_start_id,
            kind,
        } => start_failed(model, client_start_id, kind),
        SupervisorModelEvent::Worker(envelope) => apply_worker_event(model, *envelope),
        SupervisorModelEvent::WorkerTerminated {
            client_start_id,
            worker_session_id,
            run,
            reason,
        } => worker_terminated(model, client_start_id, worker_session_id, run, reason),
        SupervisorModelEvent::Closed {
            client_start_id,
            worker_session_id,
            run,
            health,
        } => worker_closed(model, client_start_id, worker_session_id, run, health),
    }
}

fn runtime_disconnected(model: &mut AppModel) -> Vec<ModelEffect> {
    model.pending_run = None;
    model.active_run = None;
    model.cleanup_run = None;
    model.terminal_barrier = None;
    model.pending_command = None;
    model.pending_stop = None;
    model.pending_input = None;
    model.current_span = None;
    model.stop_requested = false;
    if let Some(snapshot) = model.retained_snapshot.as_mut() {
        snapshot.live = false;
        if snapshot.provenance == SnapshotProvenance::Paused {
            snapshot.provenance = SnapshotProvenance::LastSafePause;
        }
    }
    model.execution_state = ExecutionViewState::WorkerCrashed;
    model.status = Some(ModelStatus::RuntimeDisconnected);
    authorize_exit_after_reap(model)
}

fn start_admitted(
    model: &mut AppModel,
    client_start_id: ClientStartId,
    mode: RunMode,
    run: RunBinding,
    request_id: RequestId,
    next_event_sequence: EventSequence,
) -> Vec<ModelEffect> {
    let Some(pending) = model.pending_run.as_ref() else {
        return Vec::new();
    };
    if pending.client_start_id != client_start_id {
        return Vec::new();
    }
    if pending.mode != mode
        || !pending.dispatched
        || !run.is_valid()
        || request_id.0 == 0
        || next_event_sequence != EventSequence(2)
        || model.cleanup_run.is_some()
        || model
            .document
            .as_ref()
            .is_none_or(|document| document.stamp() != pending.document)
    {
        model.stop_requested = true;
        model.status = Some(ModelStatus::ProtocolDesynchronized);
        return vec![ModelEffect::CloseWorker {
            target: if run.is_valid() {
                WorkerTarget::Run(run)
            } else {
                WorkerTarget::PendingStart(client_start_id)
            },
        }];
    }
    let pending = model.pending_run.take().expect("checked pending run");
    model.output.clear();
    clear_source_artifacts(model);
    model.status = None;
    model.terminal_barrier = None;
    let source_index = SourceIndex::new(&pending.source);
    model.active_run = Some(ActiveRun {
        mode,
        binding: run,
        driver: request_id,
        driver_kind: None,
        pause_request: None,
        close_request: None,
        next_sequence: Some(next_event_sequence),
        document: pending.document,
        source: pending.source,
        source_index,
        desynchronized: false,
    });
    model.cleanup_run = Some(run);
    model.execution_state = ExecutionViewState::Running;
    Vec::new()
}

fn command_admitted(
    model: &mut AppModel,
    client_command_id: ClientCommandId,
    command: ExecutionCommandKind,
    run: RunBinding,
    request_id: RequestId,
    next_event_sequence: EventSequence,
) -> Vec<ModelEffect> {
    let Some(active) = model.active_run.as_ref() else {
        return Vec::new();
    };
    if active.binding != run
        || request_id.0 == 0
        || active.next_sequence != Some(next_event_sequence)
    {
        if active.binding == run {
            return mark_desynchronized(model);
        }
        return Vec::new();
    }
    let pending = if command == ExecutionCommandKind::Stop {
        model.pending_stop.as_ref()
    } else {
        model.pending_command.as_ref()
    };
    let Some(pending) = pending else {
        return Vec::new();
    };
    if pending.client_command_id != client_command_id {
        return Vec::new();
    }
    if pending.command.kind() != command {
        return mark_desynchronized(model);
    }
    match command {
        ExecutionCommandKind::Continue
        | ExecutionCommandKind::StepInto
        | ExecutionCommandKind::StepOver
        | ExecutionCommandKind::StepOut => {
            if let Some(active) = model.active_run.as_mut() {
                active.driver = request_id;
                active.driver_kind = Some(command);
                active.pause_request = None;
            }
            model.pending_command = None;
            model.execution_state = ExecutionViewState::Running;
            model.current_span = None;
            if let Some(snapshot) = model.retained_snapshot.as_mut() {
                snapshot.live = false;
                if snapshot.provenance == SnapshotProvenance::Paused {
                    snapshot.provenance = SnapshotProvenance::LastSafePause;
                }
            }
        }
        ExecutionCommandKind::ProvideInput => {
            if let Some(active) = model.active_run.as_mut() {
                active.driver = request_id;
                active.driver_kind = Some(command);
            }
            model.pending_command = None;
            model.execution_state = ExecutionViewState::Running;
            model.pending_input = None;
        }
        ExecutionCommandKind::Pause => {
            if let Some(active) = model.active_run.as_mut() {
                active.pause_request = Some(request_id);
            }
            model.pending_command = None;
        }
        ExecutionCommandKind::Stop => {
            if let Some(stop) = model.pending_stop.as_mut() {
                stop.admitted_request = Some(request_id);
            }
        }
    }
    Vec::new()
}

fn close_admitted(
    model: &mut AppModel,
    run: RunBinding,
    request_id: RequestId,
    next_event_sequence: EventSequence,
) -> Vec<ModelEffect> {
    let Some(active) = model.active_run.as_ref() else {
        return Vec::new();
    };
    if active.binding != run {
        return Vec::new();
    }
    if !model.stop_requested
        || request_id.0 == 0
        || active.next_sequence != Some(next_event_sequence)
    {
        return mark_desynchronized(model);
    }
    if active.close_request == Some(request_id) {
        return Vec::new();
    }
    if active.close_request.is_some() {
        return mark_desynchronized(model);
    }
    model
        .active_run
        .as_mut()
        .expect("checked active run")
        .close_request = Some(request_id);
    Vec::new()
}

fn submission_rejected(
    model: &mut AppModel,
    submission: ClientSubmission,
    error: SubmitError,
) -> Vec<ModelEffect> {
    match submission {
        ClientSubmission::Start(client_start_id) => {
            let Some(pending) = model.pending_run.as_ref() else {
                return Vec::new();
            };
            if pending.client_start_id != client_start_id || !pending.dispatched {
                return Vec::new();
            }
            let close_already_requested = model.stop_requested;
            model.stop_requested = true;
            model.status = Some(ModelStatus::StartRejected(error));
            if close_already_requested {
                Vec::new()
            } else {
                vec![ModelEffect::CloseWorker {
                    target: WorkerTarget::PendingStart(client_start_id),
                }]
            }
        }
        ClientSubmission::Command(client_command_id) => {
            if model
                .pending_stop
                .as_ref()
                .is_some_and(|pending| pending.client_command_id == client_command_id)
            {
                let command = model
                    .pending_stop
                    .take()
                    .expect("checked pending stop")
                    .command
                    .kind();
                model.status = Some(ModelStatus::CommandRejected { command, error });
                let Some(run) = model.active_run.as_ref().map(|active| active.binding) else {
                    return Vec::new();
                };
                return vec![ModelEffect::CloseWorker {
                    target: WorkerTarget::Run(run),
                }];
            }
            let Some(pending) = model.pending_command.as_ref() else {
                return Vec::new();
            };
            if pending.client_command_id != client_command_id {
                return Vec::new();
            }
            let pending = model
                .pending_command
                .take()
                .expect("checked pending command");
            model.execution_state = pending.previous_state;
            model.status = Some(ModelStatus::CommandRejected {
                command: pending.command.kind(),
                error,
            });
            Vec::new()
        }
    }
}

fn start_failed(
    model: &mut AppModel,
    client_start_id: ClientStartId,
    kind: std::io::ErrorKind,
) -> Vec<ModelEffect> {
    let Some(pending) = model.pending_run.as_ref() else {
        return Vec::new();
    };
    if pending.client_start_id != client_start_id || !pending.dispatched {
        return Vec::new();
    }
    let fallback = pending.fallback_state;
    model.pending_run = None;
    model.execution_state = fallback;
    model.stop_requested = false;
    model.status = Some(ModelStatus::StartFailed(kind));
    authorize_exit_after_reap(model)
}

fn worker_terminated(
    model: &mut AppModel,
    client_start_id: Option<ClientStartId>,
    worker_session_id: WorkerSessionId,
    run: Option<RunBinding>,
    reason: WorkerTerminationReason,
) -> Vec<ModelEffect> {
    if worker_session_id.0 == 0
        || run.is_some_and(|binding| binding.worker_session_id != worker_session_id)
    {
        return Vec::new();
    }
    let pending_matches = model.pending_run.as_ref().is_some_and(|pending| {
        pending.dispatched && client_start_id == Some(pending.client_start_id)
    });
    let active_matches = model
        .active_run
        .as_ref()
        .is_some_and(|active| run == Some(active.binding));
    if !pending_matches && !active_matches {
        return Vec::new();
    }
    if pending_matches {
        model.pending_run = None;
    }
    if active_matches {
        model.active_run = None;
    }
    if run.is_some() && model.cleanup_run == run {
        model.cleanup_run = None;
    }
    model.pending_command = None;
    model.pending_stop = None;
    model.pending_input = None;
    model.current_span = None;
    model.stop_requested = false;
    if let Some(snapshot) = model.retained_snapshot.as_mut() {
        snapshot.live = false;
        if snapshot.provenance == SnapshotProvenance::Paused {
            snapshot.provenance = SnapshotProvenance::LastSafePause;
        }
    }
    model.execution_state = ExecutionViewState::WorkerCrashed;
    model.status = Some(ModelStatus::WorkerTerminated(reason));
    authorize_exit_after_reap(model)
}

fn worker_closed(
    model: &mut AppModel,
    client_start_id: Option<ClientStartId>,
    worker_session_id: WorkerSessionId,
    run: Option<RunBinding>,
    health: ClosureHealth,
) -> Vec<ModelEffect> {
    if worker_session_id.0 == 0
        || run.is_some_and(|binding| binding.worker_session_id != worker_session_id)
    {
        return Vec::new();
    }
    if let Some(pending) = model.pending_run.as_ref()
        && !pending.dispatched
        && run == model.cleanup_run
    {
        model.cleanup_run = None;
        if health != ClosureHealth::Clean {
            model.status = Some(ModelStatus::CleanupWarning(health));
        }
        if model.exit_state != ExitState::None {
            return authorize_exit_after_reap(model);
        }
        let pending = model.pending_run.as_mut().expect("checked pending run");
        pending.dispatched = true;
        return vec![ModelEffect::Start(pending.intent())];
    }
    let stopped_start = model.pending_run.as_ref().is_some_and(|pending| {
        pending.dispatched
            && model.cleanup_run.is_none()
            && client_start_id == Some(pending.client_start_id)
            && model.stop_requested
    });
    if stopped_start {
        let fallback = model
            .pending_run
            .take()
            .expect("checked pending start")
            .fallback_state;
        model.execution_state = fallback;
        model.stop_requested = false;
        return authorize_exit_after_reap(model);
    }
    let matching_cleanup = model.cleanup_run.is_some() && run == model.cleanup_run;
    if !matching_cleanup {
        return Vec::new();
    }
    model.cleanup_run = None;
    let desynchronized = model
        .active_run
        .as_ref()
        .is_some_and(|active| Some(active.binding) == run && active.desynchronized);
    if desynchronized {
        model.active_run = None;
        model.pending_command = None;
        model.pending_stop = None;
        model.pending_input = None;
        model.current_span = None;
        model.stop_requested = false;
        if let Some(snapshot) = model.retained_snapshot.as_mut() {
            snapshot.live = false;
            if snapshot.provenance == SnapshotProvenance::Paused {
                snapshot.provenance = SnapshotProvenance::LastSafePause;
            }
        }
        model.execution_state = ExecutionViewState::WorkerCrashed;
    } else if health != ClosureHealth::Clean {
        model.status = Some(ModelStatus::CleanupWarning(health));
    }
    authorize_exit_after_reap(model)
}

fn authorize_exit_after_reap(model: &mut AppModel) -> Vec<ModelEffect> {
    let ExitState::WaitingForWorker(close_request_id) = model.exit_state else {
        return Vec::new();
    };
    model.exit_state = ExitState::Authorized(close_request_id);
    vec![ModelEffect::AuthorizeClose { close_request_id }]
}

fn apply_worker_event(model: &mut AppModel, envelope: Envelope<WorkerEvent>) -> Vec<ModelEffect> {
    let Some(active) = model.active_run.clone() else {
        return Vec::new();
    };
    if envelope.worker_session_id != active.binding.worker_session_id
        || envelope.run_id != active.binding.run_id
        || envelope.source_revision != active.binding.source_revision
    {
        return Vec::new();
    }
    if active.desynchronized {
        return Vec::new();
    }
    let Some(expected) = active.next_sequence else {
        return Vec::new();
    };
    if envelope.sequence.0 < expected.0 {
        return Vec::new();
    }
    if envelope.sequence.0 > expected.0 {
        return mark_desynchronized(model);
    }
    if !validate_worker_payload(model, &active, &envelope) {
        return mark_desynchronized(model);
    }
    let terminal = envelope.payload.is_terminal();
    let next_sequence = if terminal {
        None
    } else {
        let Some(value) = envelope.sequence.0.checked_add(1) else {
            return mark_desynchronized(model);
        };
        Some(EventSequence(value))
    };

    let effects =
        match envelope.payload {
            WorkerEvent::Hello => return mark_desynchronized(model),
            WorkerEvent::Output { text } => {
                model.output.append(&text);
                Vec::new()
            }
            WorkerEvent::OutputTruncated => {
                model.output.worker_truncated = true;
                Vec::new()
            }
            WorkerEvent::CommandRejected { code, message } => {
                apply_worker_command_rejection(model, envelope.request_id, code, message)
            }
            WorkerEvent::InputRequested { prompt } => {
                model.execution_state = ExecutionViewState::WaitingForInput;
                model.pending_input = Some(PendingInput {
                    request_id: envelope.request_id,
                    prompt,
                });
                Vec::new()
            }
            WorkerEvent::Diagnostic { diagnostic } => {
                add_problem(model, &active, envelope.sequence, diagnostic);
                Vec::new()
            }
            WorkerEvent::Paused { location, snapshot } => {
                apply_pause(
                    model,
                    &active,
                    envelope.sequence,
                    location.activation_id,
                    snapshot,
                );
                Vec::new()
            }
            WorkerEvent::Completed => {
                finish_terminal(
                    model,
                    active,
                    envelope.sequence,
                    ExecutionViewState::Completed,
                    None,
                    None,
                );
                Vec::new()
            }
            WorkerEvent::Cancelled { snapshot } => {
                finish_terminal(
                    model,
                    active,
                    envelope.sequence,
                    ExecutionViewState::Cancelled,
                    Some((snapshot, SnapshotProvenance::Cancelled)),
                    None,
                );
                Vec::new()
            }
            WorkerEvent::Faulted {
                diagnostic,
                snapshot,
            } => {
                if !model.problems.iter().any(|problem| {
                    problem.run == active.binding && problem.diagnostic == diagnostic
                }) {
                    add_problem(model, &active, envelope.sequence, diagnostic.clone());
                }
                let fault_span = diagnostic.span;
                finish_terminal(
                    model,
                    active,
                    envelope.sequence,
                    ExecutionViewState::Faulted,
                    Some((snapshot, SnapshotProvenance::Faulted)),
                    Some(fault_span),
                );
                Vec::new()
            }
        };
    if !terminal && let Some(active) = model.active_run.as_mut() {
        active.next_sequence = next_sequence;
    }
    effects
}

fn validate_worker_payload(
    model: &AppModel,
    active: &ActiveRun,
    envelope: &Envelope<WorkerEvent>,
) -> bool {
    let binding = active.binding;
    let source = active.source.as_ref();
    let source_index = &active.source_index;
    let driver = active.driver;
    match &envelope.payload {
        WorkerEvent::Hello => false,
        WorkerEvent::Output { .. }
        | WorkerEvent::OutputTruncated
        | WorkerEvent::Diagnostic { .. }
        | WorkerEvent::InputRequested { .. }
        | WorkerEvent::Completed
        | WorkerEvent::Faulted { .. } => {
            if envelope.request_id != driver {
                return false;
            }
            if matches!(envelope.payload, WorkerEvent::Output { .. })
                && model.output.worker_truncated
            {
                return false;
            }
            if matches!(envelope.payload, WorkerEvent::OutputTruncated)
                && model.output.worker_truncated
            {
                return false;
            }
            validate_nested_payload(&envelope.payload, binding, source, source_index)
        }
        WorkerEvent::CommandRejected { .. } => {
            envelope.request_id == driver
                || active.pause_request == Some(envelope.request_id)
                || pending_request_matches(model.pending_stop.as_ref(), envelope.request_id)
        }
        WorkerEvent::Paused { location, snapshot } => {
            let causal = match snapshot.reason {
                SnapshotReason::Paused(PauseReason::DebugPoint) => {
                    active.mode == RunMode::Debug
                        && active.driver_kind.is_none()
                        && envelope.request_id == driver
                }
                SnapshotReason::Paused(PauseReason::Explicit) => {
                    active.pause_request == Some(envelope.request_id)
                }
                SnapshotReason::Paused(PauseReason::Step) => {
                    matches!(
                        active.driver_kind,
                        Some(
                            ExecutionCommandKind::StepInto
                                | ExecutionCommandKind::StepOver
                                | ExecutionCommandKind::StepOut
                        )
                    ) && envelope.request_id == driver
                }
                _ => false,
            };
            causal
                && location.source_id == binding.source_id
                && location.revision == binding.source_revision
                && location.span == snapshot.current_span
                && validate_span(location.span, binding, source, source_index)
                && validate_snapshot(snapshot, binding, source, source_index)
        }
        WorkerEvent::Cancelled { snapshot } => {
            let causal = model
                .pending_stop
                .as_ref()
                .and_then(|stop| stop.admitted_request)
                == Some(envelope.request_id)
                || active.close_request == Some(envelope.request_id);
            causal
                && snapshot.reason == SnapshotReason::Cancelled
                && validate_snapshot(snapshot, binding, source, source_index)
        }
    }
}

fn pending_request_matches(pending: Option<&PendingCommand>, request_id: RequestId) -> bool {
    pending.is_some_and(|pending| pending.admitted_request == Some(request_id))
}

fn validate_nested_payload(
    event: &WorkerEvent,
    binding: RunBinding,
    source: &str,
    source_index: &SourceIndex,
) -> bool {
    match event {
        WorkerEvent::Diagnostic { diagnostic } => {
            validate_diagnostic(diagnostic, binding, source, source_index)
        }
        WorkerEvent::Paused { location, snapshot } => {
            location.source_id == binding.source_id
                && location.revision == binding.source_revision
                && location.span == snapshot.current_span
                && validate_span(location.span, binding, source, source_index)
                && validate_snapshot(snapshot, binding, source, source_index)
        }
        WorkerEvent::Cancelled { snapshot } => {
            validate_snapshot(snapshot, binding, source, source_index)
        }
        WorkerEvent::Faulted {
            diagnostic,
            snapshot,
        } => {
            snapshot.reason == SnapshotReason::Faulted
                && diagnostic.span == snapshot.current_span
                && validate_diagnostic(diagnostic, binding, source, source_index)
                && validate_snapshot(snapshot, binding, source, source_index)
        }
        _ => true,
    }
}

fn validate_diagnostic(
    diagnostic: &WireDiagnostic,
    binding: RunBinding,
    source: &str,
    source_index: &SourceIndex,
) -> bool {
    diagnostic.validate().is_ok()
        && validate_span(diagnostic.span, binding, source, source_index)
        && diagnostic
            .frames
            .iter()
            .all(|frame| validate_span(frame.span, binding, source, source_index))
}

fn validate_snapshot(
    snapshot: &VmSnapshot,
    binding: RunBinding,
    source: &str,
    source_index: &SourceIndex,
) -> bool {
    validate_span(snapshot.current_span, binding, source, source_index)
        && snapshot.frames.iter().all(|frame| {
            frame.activation_id.0 != 0
                && validate_span(frame.current_span, binding, source, source_index)
                && frame
                    .call_site
                    .is_none_or(|span| validate_span(span, binding, source, source_index))
        })
}

fn validate_span(
    span: SourceSpan,
    binding: RunBinding,
    source: &str,
    source_index: &SourceIndex,
) -> bool {
    if span.source_id != binding.source_id
        || span.revision != binding.source_revision
        || span.start.byte_offset > span.end.byte_offset
        || span.end.byte_offset > source.len()
        || !source.is_char_boundary(span.start.byte_offset)
        || !source.is_char_boundary(span.end.byte_offset)
    {
        return false;
    }
    source_index.position(source, span.start.byte_offset) == Some(span.start)
        && source_index.position(source, span.end.byte_offset) == Some(span.end)
}

fn apply_pause(
    model: &mut AppModel,
    active: &ActiveRun,
    sequence: EventSequence,
    activation_id: ActivationId,
    snapshot: VmSnapshot,
) {
    let stopping = model.stop_requested;
    if let Some(run) = model.active_run.as_mut() {
        run.pause_request = None;
    }
    let selected = model.selected_activation.filter(|selected| {
        snapshot
            .frames
            .iter()
            .any(|frame| frame.activation_id == *selected)
    });
    model.selected_activation = selected.or_else(|| {
        snapshot
            .frames
            .iter()
            .any(|frame| frame.activation_id == activation_id)
            .then_some(activation_id)
    });
    model.current_span = (!stopping).then_some(snapshot.current_span);
    model.retained_snapshot = Some(RetainedSnapshot {
        snapshot,
        key: SnapshotKey {
            run: active.binding,
            sequence,
        },
        document: active.document,
        run: active.binding,
        provenance: if stopping {
            SnapshotProvenance::LastSafePause
        } else {
            SnapshotProvenance::Paused
        },
        live: !stopping,
    });
    if !stopping {
        model.execution_state = ExecutionViewState::Paused;
    }
    model.pending_input = None;
    model.pending_command = None;
}

fn finish_terminal(
    model: &mut AppModel,
    active: ActiveRun,
    sequence: EventSequence,
    state: ExecutionViewState,
    snapshot: Option<(VmSnapshot, SnapshotProvenance)>,
    fault_span: Option<SourceSpan>,
) {
    model.terminal_barrier = Some(TerminalBarrier {
        run: active.binding,
        sequence,
    });
    model.active_run = None;
    model.pending_run = None;
    model.pending_command = None;
    model.pending_stop = None;
    model.pending_input = None;
    model.stop_requested = false;
    model.current_span = None;
    model.fault_span = fault_span;
    model.execution_state = state;
    model.retained_snapshot = snapshot.map(|(snapshot, provenance)| RetainedSnapshot {
        snapshot,
        key: SnapshotKey {
            run: active.binding,
            sequence,
        },
        document: active.document,
        run: active.binding,
        provenance,
        live: false,
    });
    model.selected_activation = model
        .retained_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.snapshot.frames.first())
        .map(|frame| frame.activation_id);
}

fn apply_worker_command_rejection(
    model: &mut AppModel,
    request_id: RequestId,
    code: String,
    message: String,
) -> Vec<ModelEffect> {
    let _ = request_id;
    let effects = mark_desynchronized(model);
    model.status = Some(ModelStatus::WorkerCommandRejected { code, message });
    effects
}

fn add_problem(
    model: &mut AppModel,
    active: &ActiveRun,
    sequence: EventSequence,
    diagnostic: WireDiagnostic,
) {
    let bytes = diagnostic_bytes(&diagnostic);
    if model.problems.len() >= MAX_PROBLEMS
        || model
            .problem_bytes
            .checked_add(bytes)
            .is_none_or(|total| total > MAX_PROBLEM_BYTES)
    {
        model.problems_truncated = true;
        model.status = Some(ModelStatus::ProblemLimitReached);
        return;
    }
    let Some(id) = model.allocate_problem_id() else {
        model.exhausted();
        return;
    };
    model.problem_bytes += bytes;
    model.problems.push(Problem {
        id,
        diagnostic,
        document: active.document,
        run: active.binding,
        sequence,
    });
}

fn diagnostic_bytes(diagnostic: &WireDiagnostic) -> usize {
    diagnostic
        .code
        .len()
        .saturating_add(diagnostic.message.len())
        .saturating_add(
            diagnostic
                .frames
                .iter()
                .map(|frame| frame.function.len().saturating_add(128))
                .sum::<usize>(),
        )
        .saturating_add(256)
}

fn mark_desynchronized(model: &mut AppModel) -> Vec<ModelEffect> {
    let Some(active) = model.active_run.as_mut() else {
        return Vec::new();
    };
    if active.desynchronized {
        return Vec::new();
    }
    active.desynchronized = true;
    let run = active.binding;
    model.stop_requested = true;
    model.status = Some(ModelStatus::ProtocolDesynchronized);
    vec![ModelEffect::CloseWorker {
        target: WorkerTarget::Run(run),
    }]
}

fn select_problem(model: &mut AppModel, problem_id: ProblemId) -> Vec<ModelEffect> {
    let Some(problem) = model
        .problems
        .iter()
        .find(|problem| problem.id == problem_id)
    else {
        return Vec::new();
    };
    if model
        .document
        .as_ref()
        .is_none_or(|document| document.stamp() != problem.document)
    {
        return Vec::new();
    }
    let span = problem.diagnostic.span;
    let document = problem.document;
    let run = problem.run;
    model.selected_problem = Some(problem_id);
    model.navigation_span = Some(span);
    vec![ModelEffect::Navigate {
        document,
        run,
        span,
    }]
}

fn select_frame(
    model: &mut AppModel,
    snapshot_key: SnapshotKey,
    activation_id: ActivationId,
) -> Vec<ModelEffect> {
    let Some(retained) = model.retained_snapshot.as_ref() else {
        return Vec::new();
    };
    if retained.key != snapshot_key
        || model
            .document
            .as_ref()
            .is_none_or(|document| document.stamp() != retained.document)
    {
        return Vec::new();
    }
    let Some(frame) = retained
        .snapshot
        .frames
        .iter()
        .find(|frame| frame.activation_id == activation_id)
    else {
        return Vec::new();
    };
    let span = frame.current_span;
    let document = retained.document;
    let run = retained.run;
    model.selected_activation = Some(activation_id);
    model.navigation_span = Some(span);
    vec![ModelEffect::Navigate {
        document,
        run,
        span,
    }]
}

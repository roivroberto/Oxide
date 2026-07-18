use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::{DocumentStamp, NavigationGeneration};

use super::framing::{
    JsonRpcMessage, RpcEnvelope, RpcId, RpcOutcome, RpcResponseId, encode_message,
};
use super::snapshot::{
    AnalysisDiagnostic, CaretGeneration, ClientRequestId, DefinitionResultId, DefinitionSnapshot,
    DefinitionTargetSnapshot, DiagnosticItemId, DiagnosticSetRevision, DiagnosticSeverity,
    DiagnosticSnapshot, DocumentSyncSnapshot, LanguageNotice, LanguageSnapshot,
    LanguageSnapshotDraft, LanguageStatus, LspVersion, NoticeId, NoticeKind, ProcessGeneration,
    SnapshotRevision, StderrTailSnapshot, SyntaxSnapshot, WriteSequence, WriterState,
    draft_fields_are_valid, synthetic_uri,
};
use super::text_index::{
    DefinitionTarget, DiagnosticPlan, SemanticLegend, SyntaxRun, TextIndex, decode_semantic_tokens,
    validate_definitions, validate_diagnostics,
};

pub(crate) const READER_READ_CHUNK_BYTES: usize = 64 * 1024;
pub(crate) const READER_INBOX_ITEMS: usize = 64;
pub(crate) const READER_INBOX_BODY_BYTES: usize = 24 * 1024 * 1024;
pub(crate) const WRITER_INPUT_ITEMS: usize = 1;
pub(crate) const WRITER_RESULT_SLOT_ITEMS: usize = 1;
pub(crate) const CHILD_STATUS_SLOT_ITEMS: usize = 1;
pub(crate) const FINALIZATION_SLOT_ITEMS: usize = 1;
pub(crate) const EVERY_WAKE_ITEMS: usize = 1;
pub(crate) const MAX_LEDGER_ENTRIES: usize = 4;
pub(crate) const MAX_SERVER_REPLIES: usize = 32;
pub(crate) const MAX_SERVER_REPLY_FRAME_BYTES: usize = 8 * 1024;
pub(crate) const MAX_SERVER_REPLY_BYTES: usize = 64 * 1024;
pub(crate) const MAX_ACTOR_STATE_BYTES: usize = 128 * 1024 * 1024;
const MAX_REQUEST_ERROR_MESSAGE_BYTES: usize = 4 * 1024;
pub(crate) const MAX_STDERR_BYTES: usize = super::snapshot::MAX_STDERR_BYTES;
pub(crate) const MAX_STDERR_LINES: usize = super::snapshot::MAX_STDERR_LINES;
pub(crate) const MAX_NOTICES: usize = 32;

pub(crate) const LAUNCH_TIMEOUT_MS: u64 = 5_000;
pub(crate) const WRITE_ACK_TIMEOUT_MS: u64 = 2_000;
pub(crate) const INITIALIZE_RESPONSE_TIMEOUT_MS: u64 = 10_000;
pub(crate) const FEATURE_RESPONSE_TIMEOUT_MS: u64 = 5_000;
pub(crate) const SHUTDOWN_RESPONSE_TIMEOUT_MS: u64 = 2_000;
pub(crate) const BACKOFF_INITIAL_MS: u64 = 250;
pub(crate) const BACKOFF_MAX_MS: u64 = 8_000;
pub(crate) const BACKOFF_MAX_EXPONENT: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ManualTick(u64);

impl ManualTick {
    pub(crate) fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn get(self) -> u64 {
        self.0
    }

    fn checked_add(self, duration: u64) -> Option<Self> {
        self.0.checked_add(duration).map(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DurableDesiredDocument {
    pub(crate) stamp: DocumentStamp,
    pub(crate) source: Arc<str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UiDocumentFence {
    pub(crate) process_generation: ProcessGeneration,
    pub(crate) stamp: DocumentStamp,
    pub(crate) lsp_version: Option<LspVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundedStderrTail {
    pub(crate) text: Arc<str>,
    pub(crate) line_count: usize,
    pub(crate) truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchFailure {
    MissingSibling,
    Spawn,
    Containment,
    Pipe,
    Thread,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchOutcome {
    Ready,
    FailedBeforeOwnership { cause: LaunchFailure },
    FailedWithOwnedResources { cause: LaunchFailure },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriterOutcome {
    Flushed,
    HandoffRejected,
    WriteFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupMode {
    ForceTerminate,
    GracefulExitFlushed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReaderFatalCause {
    Framing,
    InboxOverflow,
    Io,
    AdapterInvariant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriterFatalCause {
    ResultOverflow,
    Io,
    AdapterInvariant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundedChildExit {
    Success,
    Failure(Option<i32>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupFailure {
    Terminate,
    Reap,
    Join,
    VerifyTreeEmpty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupCause {
    Launch,
    Reader,
    Writer,
    WriteTimeout,
    ResponseTimeout,
    LaunchTimeout,
    Protocol,
    ChildExited,
    LspVersionExhausted,
    CounterExhausted,
    ActorBudget,
    Shutdown,
    InvalidTime,
    AdapterInvariant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalCause {
    Shutdown,
    InvalidTime,
    ModelInvariant,
    ActorBudget,
    CounterExhausted,
    AdapterInvariant,
    CleanupFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountedJsonRpcMessage {
    body_bytes: usize,
    message: JsonRpcMessage,
}

impl AccountedJsonRpcMessage {
    pub(crate) fn from_message(message: JsonRpcMessage) -> Self {
        Self {
            body_bytes: message.body_bytes(),
            message,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_accounting(message: JsonRpcMessage, body_bytes: usize) -> Self {
        Self {
            body_bytes,
            message,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestKind {
    Initialize,
    SemanticTokens,
    Definition,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocumentFence {
    pub(crate) generation: ProcessGeneration,
    pub(crate) uri: Arc<str>,
    pub(crate) lsp_version: LspVersion,
    pub(crate) stamp: DocumentStamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrameKind {
    ClientRequest {
        id: ClientRequestId,
        kind: RequestKind,
    },
    Initialized,
    DidClose {
        uri: Arc<str>,
    },
    DidOpen {
        fence: DocumentFence,
    },
    DidChange {
        fence: DocumentFence,
    },
    ServerResponse {
        id: RpcId,
    },
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolPhase {
    Launching,
    NeedInitialize,
    Initializing,
    NeedInitialized,
    InitializedWriting,
    Ready,
    ShutdownAwaitResponse,
    NeedExit,
    ExitWriting,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActorEvent {
    Start {
        now: ManualTick,
    },
    Drive,
    DesiredDocumentChanged {
        desired: Option<DurableDesiredDocument>,
    },
    CaretChanged {
        fence: UiDocumentFence,
        caret_generation: CaretGeneration,
    },
    CaretGenerationExhausted {
        fence: UiDocumentFence,
    },
    DefinitionRequested {
        fence: UiDocumentFence,
        caret_generation: CaretGeneration,
        navigation_generation: NavigationGeneration,
        primary_character: usize,
    },
    SnapshotItemsAcknowledged {
        process_generation: ProcessGeneration,
        observed_revision: SnapshotRevision,
        definition: Option<DefinitionResultId>,
        notices: Arc<[NoticeId]>,
    },
    ShutdownRequested,
    LaunchFinished {
        generation: ProcessGeneration,
        outcome: LaunchOutcome,
    },
    ReaderMessage {
        generation: ProcessGeneration,
        message: AccountedJsonRpcMessage,
    },
    ReaderFatal {
        generation: ProcessGeneration,
        cause: ReaderFatalCause,
    },
    WriterFinished {
        generation: ProcessGeneration,
        sequence: WriteSequence,
        outcome: WriterOutcome,
    },
    WriterFatal {
        generation: ProcessGeneration,
        cause: WriterFatalCause,
    },
    StderrTailChanged {
        generation: ProcessGeneration,
        tail: BoundedStderrTail,
    },
    ChildExited {
        generation: ProcessGeneration,
        status: BoundedChildExit,
    },
    FinalizedGeneration {
        generation: ProcessGeneration,
        stderr_tail: Option<BoundedStderrTail>,
    },
    CleanupFailed {
        generation: ProcessGeneration,
        cause: CleanupFailure,
        stderr_tail: Option<BoundedStderrTail>,
    },
    Tick {
        now: ManualTick,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActorEffect {
    LaunchGeneration {
        generation: ProcessGeneration,
    },
    CancelLaunch {
        generation: ProcessGeneration,
    },
    SendFrame {
        generation: ProcessGeneration,
        sequence: WriteSequence,
        kind: FrameKind,
        bytes: Arc<[u8]>,
    },
    BeginCleanup {
        generation: ProcessGeneration,
        mode: CleanupMode,
        cause: CleanupCause,
    },
    PublishSnapshot {
        snapshot: Box<LanguageSnapshot>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActorSeeds {
    pub(crate) next_generation: u64,
    pub(crate) next_request: i32,
    pub(crate) next_write: u64,
    pub(crate) next_lsp_version: i32,
    pub(crate) next_diagnostic_revision: u64,
    pub(crate) next_diagnostic_item: u64,
    pub(crate) next_definition_result: u64,
    pub(crate) next_notice: u64,
    pub(crate) next_snapshot_revision: u64,
}

impl Default for ActorSeeds {
    fn default() -> Self {
        Self {
            next_generation: 1,
            next_request: 1,
            next_write: 1,
            next_lsp_version: 1,
            next_diagnostic_revision: 1,
            next_diagnostic_item: 1,
            next_definition_result: 1,
            next_notice: 1,
            next_snapshot_revision: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActorStartError {
    InvalidSeeds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotPublicationError {
    CounterExhausted,
    InvalidCandidate,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum SnapshotFenceCorruption {
    ProcessGeneration,
    DocumentStamp,
    LspVersion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryState {
    Dormant,
    Waiting { deadline: ManualTick },
    Due,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CleanupBarrier {
    generation: ProcessGeneration,
    caused_by_shutdown: bool,
    unresolved_failure: bool,
}

#[derive(Clone, Debug)]
struct WrittenDocument {
    fence: DocumentFence,
    source: Arc<str>,
}

#[derive(Clone, Debug)]
struct IndexedDocument {
    stamp: DocumentStamp,
    source: Arc<str>,
    index: Arc<TextIndex>,
}

#[derive(Clone, Debug)]
struct DefinitionIntent {
    fence: DocumentFence,
    caret_generation: CaretGeneration,
    navigation_generation: NavigationGeneration,
    primary_character: usize,
}

#[derive(Clone, Debug)]
struct ServerReply {
    id: RpcId,
    frame: Arc<[u8]>,
}

#[derive(Clone, Debug)]
struct PreAckDiagnostics {
    generation: ProcessGeneration,
    write_sequence: WriteSequence,
    uri: Arc<str>,
    lsp_version: LspVersion,
    stamp: DocumentStamp,
    plan: DiagnosticPlan,
}

#[derive(Clone, Debug)]
struct RetainedDefinition {
    snapshot: DefinitionSnapshot,
    first_published_revision: Option<SnapshotRevision>,
}

#[derive(Clone, Debug)]
struct RetainedNotice {
    notice: LanguageNotice,
    first_published_revision: Option<SnapshotRevision>,
}

#[derive(Clone, Debug)]
struct RetainedStderrTail {
    generation: ProcessGeneration,
    snapshot: StderrTailSnapshot,
}

#[derive(Clone, Debug)]
struct WriterFlight {
    generation: ProcessGeneration,
    sequence: WriteSequence,
    kind: FrameKind,
    bytes: Arc<[u8]>,
    write_deadline: ManualTick,
    on_flush: FlushCommit,
}

#[derive(Clone, Debug)]
enum FlushCommit {
    ClientRequest {
        id: ClientRequestId,
    },
    Initialized,
    DidClose {
        uri: Arc<str>,
    },
    DidOpen {
        document: WrittenDocument,
        index: IndexedDocument,
    },
    DidChange {
        document: WrittenDocument,
        index: IndexedDocument,
    },
    ServerResponse,
    Exit,
}

#[derive(Clone, Debug)]
struct RequestEntry {
    id: ClientRequestId,
    kind: RequestKind,
    generation: ProcessGeneration,
    write_sequence: WriteSequence,
    fence: Option<DocumentFence>,
    caret_generation: Option<CaretGeneration>,
    navigation_generation: Option<NavigationGeneration>,
    primary_character: Option<usize>,
    response_deadline: Option<ManualTick>,
    write_acked: bool,
    validated_response: Option<ValidatedResponse>,
}

#[derive(Clone, Debug)]
enum ValidatedResponse {
    Initialize(SemanticLegend),
    Semantic(Vec<SyntaxRun>),
    Definition(Vec<DefinitionTarget>),
    ShutdownOk,
    RpcError(Arc<str>),
    InvalidForKind(Arc<str>),
    Stale,
}

#[derive(Clone, Debug)]
enum PreparedNotice {
    NoChange,
    Push {
        retained: RetainedNotice,
        next_notice: Option<u64>,
    },
}

#[derive(Clone, Debug)]
struct GenerationState {
    generation: ProcessGeneration,
    phase: ProtocolPhase,
    launch_deadline: Option<ManualTick>,
    next_lsp_version: Option<i32>,
    last_caret_generation: Option<CaretGeneration>,
    last_navigation_generation: Option<NavigationGeneration>,
    definition_enabled: bool,
    open_uri: Option<Arc<str>>,
    written: Option<WrittenDocument>,
    index: Option<IndexedDocument>,
    writer_flight: Option<WriterFlight>,
    ledger: Vec<RequestEntry>,
    server_replies: VecDeque<ServerReply>,
    server_reply_bytes: usize,
    pre_ack_diagnostics: Option<PreAckDiagnostics>,
    semantic_intent: Option<DocumentFence>,
    definition_intent: Option<DefinitionIntent>,
    diagnostics: Option<DiagnosticSnapshot>,
    syntax: Option<SyntaxSnapshot>,
    definition: Option<RetainedDefinition>,
    notices: Vec<RetainedNotice>,
    stderr_tail: Option<BoundedStderrTail>,
    legend: Option<SemanticLegend>,
}

#[derive(Clone, Debug)]
pub(crate) struct LanguageActor {
    now: ManualTick,
    started: bool,
    shutdown_requested: bool,
    terminal: Option<TerminalCause>,
    desired: Option<DurableDesiredDocument>,
    active: Option<GenerationState>,
    cleanup_pending: Option<CleanupBarrier>,
    launch_cancel_pending: Option<ProcessGeneration>,
    retry: RetryState,
    failure_exponent: u32,
    initial_lsp_version: i32,
    next_generation: Option<u64>,
    next_request: Option<i32>,
    next_write: Option<u64>,
    next_diagnostic_revision: Option<u64>,
    next_diagnostic_item: Option<u64>,
    next_definition_result: Option<u64>,
    next_notice: Option<u64>,
    next_snapshot_revision: Option<u64>,
    last_allocated_generation: Option<ProcessGeneration>,
    last_finalized_generation: Option<ProcessGeneration>,
    final_stderr_tail: Option<RetainedStderrTail>,
    snapshot_dirty: bool,
    snapshot_terminal_published: bool,
    #[cfg(test)]
    test_accounting_bias: usize,
    #[cfg(test)]
    test_orphan_diagnostics: Option<DiagnosticSnapshot>,
}

impl LanguageActor {
    pub(crate) fn new(seeds: ActorSeeds) -> Result<Self, ActorStartError> {
        if ProcessGeneration::from_raw(seeds.next_generation).is_none()
            || ClientRequestId::from_raw(seeds.next_request).is_none()
            || WriteSequence::from_raw(seeds.next_write).is_none()
            || LspVersion::from_raw(seeds.next_lsp_version).is_none()
            || DiagnosticSetRevision::from_raw(seeds.next_diagnostic_revision).is_none()
            || DiagnosticItemId::from_raw(seeds.next_diagnostic_item).is_none()
            || DefinitionResultId::from_raw(seeds.next_definition_result).is_none()
            || NoticeId::from_raw(seeds.next_notice).is_none()
            || SnapshotRevision::from_raw(seeds.next_snapshot_revision).is_none()
        {
            return Err(ActorStartError::InvalidSeeds);
        }
        Ok(Self {
            now: ManualTick::from_raw(0),
            started: false,
            shutdown_requested: false,
            terminal: None,
            desired: None,
            active: None,
            cleanup_pending: None,
            launch_cancel_pending: None,
            retry: RetryState::Dormant,
            failure_exponent: 0,
            initial_lsp_version: seeds.next_lsp_version,
            next_generation: Some(seeds.next_generation),
            next_request: Some(seeds.next_request),
            next_write: Some(seeds.next_write),
            next_diagnostic_revision: Some(seeds.next_diagnostic_revision),
            next_diagnostic_item: Some(seeds.next_diagnostic_item),
            next_definition_result: Some(seeds.next_definition_result),
            next_notice: Some(seeds.next_notice),
            next_snapshot_revision: Some(seeds.next_snapshot_revision),
            last_allocated_generation: None,
            last_finalized_generation: None,
            final_stderr_tail: None,
            snapshot_dirty: false,
            snapshot_terminal_published: false,
            #[cfg(test)]
            test_accounting_bias: 0,
            #[cfg(test)]
            test_orphan_diagnostics: None,
        })
    }

    pub(crate) fn reduce(&mut self, event: ActorEvent) -> Vec<ActorEffect> {
        let is_drive = matches!(&event, ActorEvent::Drive);
        let snapshot_exhausted = is_drive
            && self.snapshot_dirty
            && self.next_snapshot_revision.is_none()
            && !self.snapshot_terminal_published;
        let snapshot_invalid = is_drive && self.snapshot_dirty && !self.snapshot_state_is_valid();
        let mut lifecycle = if snapshot_exhausted {
            self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            )
        } else if snapshot_invalid {
            self.enter_terminal(
                TerminalCause::AdapterInvariant,
                CleanupCause::AdapterInvariant,
            )
        } else {
            match event {
                ActorEvent::Start { now } => self.start(now),
                ActorEvent::Drive => self.drive_lifecycle(),
                ActorEvent::DesiredDocumentChanged { desired } => self.set_desired(desired),
                ActorEvent::CaretChanged {
                    fence,
                    caret_generation,
                } => self.caret_changed(fence, caret_generation),
                ActorEvent::CaretGenerationExhausted { fence } => {
                    self.caret_generation_exhausted(fence)
                }
                ActorEvent::DefinitionRequested {
                    fence,
                    caret_generation,
                    navigation_generation,
                    primary_character,
                } => self.definition_requested(
                    fence,
                    caret_generation,
                    navigation_generation,
                    primary_character,
                ),
                ActorEvent::SnapshotItemsAcknowledged {
                    process_generation,
                    observed_revision,
                    definition,
                    notices,
                } => {
                    self.acknowledge_snapshot_items(
                        process_generation,
                        observed_revision,
                        definition,
                        &notices,
                    );
                    None
                }
                ActorEvent::ShutdownRequested => self.shutdown(),
                ActorEvent::LaunchFinished {
                    generation,
                    outcome,
                } => self.launch_finished(generation, outcome),
                ActorEvent::ReaderMessage {
                    generation,
                    message,
                } => self.reader_message(generation, message),
                ActorEvent::ReaderFatal { generation, .. } => {
                    self.generation_failure(generation, CleanupCause::Reader)
                }
                ActorEvent::WriterFinished {
                    generation,
                    sequence,
                    outcome,
                } => self.writer_finished(generation, sequence, outcome),
                ActorEvent::WriterFatal { generation, .. } => {
                    self.generation_failure(generation, CleanupCause::Writer)
                }
                ActorEvent::StderrTailChanged { generation, tail } => {
                    self.stderr_tail_changed(generation, tail)
                }
                ActorEvent::ChildExited { generation, .. } => {
                    self.generation_failure(generation, CleanupCause::ChildExited)
                }
                ActorEvent::FinalizedGeneration {
                    generation,
                    stderr_tail,
                } => self.finalized_generation(generation, stderr_tail),
                ActorEvent::CleanupFailed {
                    generation,
                    stderr_tail,
                    ..
                } => self.cleanup_failed(generation, stderr_tail),
                ActorEvent::Tick { now } => self.tick(now),
            }
        };

        let mut effects = Vec::with_capacity(2);
        if let Some(effect) = lifecycle.take() {
            effects.push(effect);
        }
        if is_drive {
            match self.publish_snapshot_effect() {
                Ok(Some(snapshot)) => effects.push(snapshot),
                Ok(None) => {}
                Err(error) => {
                    let terminal = match error {
                        SnapshotPublicationError::CounterExhausted => {
                            TerminalCause::CounterExhausted
                        }
                        SnapshotPublicationError::InvalidCandidate => {
                            TerminalCause::AdapterInvariant
                        }
                    };
                    let cleanup = match error {
                        SnapshotPublicationError::CounterExhausted => {
                            CleanupCause::CounterExhausted
                        }
                        SnapshotPublicationError::InvalidCandidate => {
                            CleanupCause::AdapterInvariant
                        }
                    };
                    effects.clear();
                    if let Some(effect) = self.enter_terminal(terminal, cleanup) {
                        effects.push(effect);
                    }
                    if let Ok(Some(snapshot)) = self.publish_snapshot_effect() {
                        effects.push(snapshot);
                    }
                }
            }
        }
        effects
    }

    pub(crate) fn protocol_phase(&self) -> Option<ProtocolPhase> {
        self.active.as_ref().map(|active| active.phase)
    }

    pub(crate) fn active_generation(&self) -> Option<ProcessGeneration> {
        self.active.as_ref().map(|active| active.generation)
    }

    pub(crate) fn cleanup_pending(&self) -> Option<ProcessGeneration> {
        self.cleanup_pending.map(|barrier| barrier.generation)
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    pub(crate) fn writer_sequence(&self) -> Option<WriteSequence> {
        self.active
            .as_ref()
            .and_then(|active| active.writer_flight.as_ref())
            .map(|flight| flight.sequence)
    }

    pub(crate) fn next_deadline(&self) -> Option<ManualTick> {
        let mut deadline = match self.retry {
            RetryState::Waiting { deadline } => Some(deadline),
            RetryState::Dormant | RetryState::Due => None,
        };
        let mut include = |candidate: ManualTick| {
            deadline = Some(deadline.map_or(candidate, |current| current.min(candidate)));
        };
        if let Some(active) = &self.active {
            if let Some(candidate) = active.launch_deadline {
                include(candidate);
            }
            if let Some(flight) = &active.writer_flight {
                include(flight.write_deadline);
            }
            for entry in &active.ledger {
                if let Some(candidate) = entry.response_deadline {
                    include(candidate);
                }
            }
        }
        deadline
    }

    pub(crate) fn written_fence(&self) -> Option<&DocumentFence> {
        self.active
            .as_ref()
            .and_then(|active| active.written.as_ref())
            .map(|written| &written.fence)
    }

    pub(crate) fn has_request(&self, id: ClientRequestId) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.ledger.iter().any(|entry| entry.id == id))
    }

    pub(crate) fn assert_invariants(&self) -> Result<(), &'static str> {
        if self.cleanup_pending.is_some() && self.active.is_some() {
            return Err("cleanup and active generation overlap");
        }
        if let Some(active) = &self.active {
            if active.ledger.len() > MAX_LEDGER_ENTRIES {
                return Err("ledger capacity exceeded");
            }
            if active.ledger.len() > 3 {
                return Err("unreachable request-ledger cardinality");
            }
            let semantic = active
                .ledger
                .iter()
                .filter(|entry| entry.kind == RequestKind::SemanticTokens)
                .count();
            let definition = active
                .ledger
                .iter()
                .filter(|entry| entry.kind == RequestKind::Definition)
                .count();
            let shutdown = active
                .ledger
                .iter()
                .filter(|entry| entry.kind == RequestKind::Shutdown)
                .count();
            if semantic > 1 || definition > 1 || shutdown > 1 {
                return Err("duplicate request kind");
            }
            if active.phase == ProtocolPhase::Initializing && active.ledger.len() != 1 {
                return Err("initialize is not isolated");
            }
            if active.server_replies.len() > MAX_SERVER_REPLIES
                || active.server_reply_bytes > MAX_SERVER_REPLY_BYTES
            {
                return Err("server reply capacity exceeded");
            }
            if let Some(flight) = &active.writer_flight {
                if flight.generation != active.generation {
                    return Err("writer generation mismatch");
                }
                if let FlushCommit::ClientRequest { id } = flight.on_flush {
                    let Some(entry) = active.ledger.iter().find(|entry| entry.id == id) else {
                        return Err("request flight without ledger entry");
                    };
                    if entry.generation != flight.generation
                        || entry.write_sequence != flight.sequence
                    {
                        return Err("request flight ledger mismatch");
                    }
                }
            }
            if let Some(written) = &active.written {
                if written.fence.generation != active.generation
                    || active.open_uri.as_deref() != Some(written.fence.uri.as_ref())
                {
                    return Err("written document fence mismatch");
                }
                let Some(index) = &active.index else {
                    return Err("written document without index");
                };
                if index.stamp != written.fence.stamp
                    || index.source.as_ref() != written.source.as_ref()
                {
                    return Err("written index mismatch");
                }
                if self.desired.as_ref().is_some_and(|desired| {
                    desired.stamp == written.fence.stamp
                        && desired.source.as_ref() != written.source.as_ref()
                }) {
                    return Err("equal desired and written stamps disagree on source");
                }
            }
            if active.definition.as_ref().is_some_and(|retained| {
                Some(retained.snapshot.caret_generation) != active.last_caret_generation
                    || Some(retained.snapshot.navigation_generation)
                        != active.last_navigation_generation
            }) {
                return Err("definition result fence mismatch");
            }
        }
        if let Some(retained) = &self.final_stderr_tail
            && (self.active.is_some()
                || !stderr_tail_snapshot_is_valid(&retained.snapshot)
                || (self.cleanup_pending.map(|barrier| barrier.generation)
                    != Some(retained.generation)
                    && self.last_finalized_generation != Some(retained.generation)))
        {
            return Err("final stderr tail fence mismatch");
        }
        #[cfg(test)]
        if self.test_orphan_diagnostics.is_some() {
            return Err("language artifact survived cleanup");
        }
        if self.accounted_state_bytes() > MAX_ACTOR_STATE_BYTES {
            return Err("actor byte budget exceeded");
        }
        Ok(())
    }

    pub(crate) fn accounted_state_bytes(&self) -> usize {
        self.accounted_state_bytes_with_desired(self.desired.as_ref())
    }

    fn accounted_state_bytes_with_desired(
        &self,
        projected_desired: Option<&DurableDesiredDocument>,
    ) -> usize {
        let mut bytes = 512usize;
        #[cfg(test)]
        {
            bytes = bytes.saturating_add(self.test_accounting_bias);
        }
        if let Some(desired) = projected_desired {
            bytes = bytes.saturating_add(desired.source.len());
        }
        if let Some(active) = &self.active {
            bytes = bytes
                .saturating_add(
                    active
                        .writer_flight
                        .as_ref()
                        .map_or(0, |flight| flight.bytes.len()),
                )
                .saturating_add(active.server_reply_bytes)
                .saturating_add(
                    MAX_SERVER_REPLIES.saturating_mul(std::mem::size_of::<ServerReply>()),
                )
                .saturating_add(active.server_replies.iter().fold(0usize, |total, reply| {
                    total.saturating_add(match &reply.id {
                        RpcId::Number(_) => 0,
                        RpcId::String(value) => value.capacity(),
                    })
                }))
                .saturating_add(active.index.as_ref().map_or(0, |index| {
                    let retained = index.index.retained_bytes();
                    if projected_desired
                        .is_some_and(|desired| Arc::ptr_eq(&desired.source, &index.source))
                    {
                        retained.saturating_sub(index.source.len())
                    } else {
                        retained
                    }
                }))
                .saturating_add(active.writer_flight.as_ref().map_or(0, |flight| {
                    flush_commit_retained_bytes(
                        &flight.on_flush,
                        projected_desired,
                        active.index.as_ref(),
                    )
                }))
                .saturating_add(
                    MAX_LEDGER_ENTRIES.saturating_mul(std::mem::size_of::<RequestEntry>()),
                )
                .saturating_add(active.ledger.iter().fold(0usize, |total, entry| {
                    total.saturating_add(
                        entry
                            .validated_response
                            .as_ref()
                            .map_or(0, validated_response_retained_bytes),
                    )
                }))
                .saturating_add(MAX_NOTICES.saturating_mul(std::mem::size_of::<RetainedNotice>()))
                .saturating_add(active.notices.iter().fold(0usize, |total, retained| {
                    total.saturating_add(retained.notice.message.len())
                }))
                .saturating_add(
                    active
                        .stderr_tail
                        .as_ref()
                        .map_or(0, |tail| tail.text.len()),
                );
            if let Some(written) = &active.written {
                bytes = bytes.saturating_add(written.fence.uri.len());
            }
            if active.legend.is_some() {
                bytes = bytes.saturating_add(64);
            }
            if let Some(pre_ack) = &active.pre_ack_diagnostics {
                bytes = bytes.saturating_add(diagnostic_plan_retained_bytes(&pre_ack.plan));
            }
            if let Some(diagnostics) = &active.diagnostics {
                bytes = bytes.saturating_add(diagnostic_snapshot_retained_bytes(diagnostics));
            }
            bytes = bytes
                .saturating_add(active.syntax.as_ref().map_or(0, |syntax| {
                    syntax
                        .runs
                        .len()
                        .saturating_mul(std::mem::size_of::<SyntaxRun>())
                }))
                .saturating_add(active.definition.as_ref().map_or(0, |definition| {
                    definition
                        .snapshot
                        .targets
                        .len()
                        .saturating_mul(std::mem::size_of::<DefinitionTargetSnapshot>())
                }));
        }
        bytes = bytes.saturating_add(
            self.final_stderr_tail
                .as_ref()
                .map_or(0, |retained| retained.snapshot.text.len()),
        );
        #[cfg(test)]
        {
            bytes = bytes.saturating_add(
                self.test_orphan_diagnostics
                    .as_ref()
                    .map_or(0, diagnostic_snapshot_retained_bytes),
            );
        }
        bytes
    }

    #[cfg(test)]
    pub(crate) fn test_counter_state(&self) -> (Option<i32>, Option<u64>, Option<u64>) {
        (
            self.next_request,
            self.next_write,
            self.next_definition_result,
        )
    }

    #[cfg(test)]
    pub(crate) fn test_next_lsp_version(&self) -> Option<i32> {
        self.active
            .as_ref()
            .and_then(|active| active.next_lsp_version)
    }

    #[cfg(test)]
    pub(crate) fn test_set_accounting_bias(&mut self, bias: usize) {
        self.test_accounting_bias = bias;
    }

    #[cfg(test)]
    pub(crate) fn test_diagnostic_counters(&self) -> (Option<u64>, Option<u64>) {
        (self.next_diagnostic_revision, self.next_diagnostic_item)
    }

    #[cfg(test)]
    pub(crate) fn test_next_notice(&self) -> Option<u64> {
        self.next_notice
    }

    #[cfg(test)]
    pub(crate) fn test_corrupt_definition_caret_fence(&mut self) {
        let active = self.active.as_mut().expect("active generation");
        let current = active
            .last_caret_generation
            .expect("retained definition has a caret");
        active.last_caret_generation = current.checked_next().ok();
        self.snapshot_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn test_corrupt_current_desired_source(&mut self) {
        let desired = self.desired.as_mut().expect("desired document");
        desired.source = Arc::from("corrupted source");
        self.snapshot_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn test_corrupt_diagnostic_fence(&mut self, corruption: SnapshotFenceCorruption) {
        let diagnostics = self
            .active
            .as_mut()
            .and_then(|active| active.diagnostics.as_mut())
            .expect("visible diagnostics");
        match corruption {
            SnapshotFenceCorruption::ProcessGeneration => {
                diagnostics.process_generation = ProcessGeneration::from_raw(
                    diagnostics
                        .process_generation
                        .get()
                        .checked_add(1)
                        .expect("bounded corruption"),
                )
                .expect("corrupt generation");
            }
            SnapshotFenceCorruption::DocumentStamp => {
                diagnostics.stamp.edit_revision = crate::EditRevision::from_raw(
                    diagnostics
                        .stamp
                        .edit_revision
                        .get()
                        .checked_add(1)
                        .expect("bounded corruption"),
                )
                .expect("corrupt revision");
            }
            SnapshotFenceCorruption::LspVersion => {
                diagnostics.lsp_version = LspVersion::from_raw(
                    diagnostics
                        .lsp_version
                        .get()
                        .checked_add(1)
                        .expect("bounded corruption"),
                )
                .expect("corrupt version");
            }
        }
        self.snapshot_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn test_corrupt_writer_sequence(&mut self) {
        let flight = self
            .active
            .as_mut()
            .and_then(|active| active.writer_flight.as_mut())
            .expect("writer flight");
        flight.sequence = WriteSequence::from_raw(
            flight
                .sequence
                .get()
                .checked_add(1)
                .expect("bounded corruption"),
        )
        .expect("corrupt writer sequence");
        self.snapshot_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn test_stage_orphan_diagnostics(&mut self) {
        self.test_orphan_diagnostics = Some(
            self.active
                .as_ref()
                .and_then(|active| active.diagnostics.clone())
                .expect("visible diagnostics"),
        );
        self.snapshot_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn test_trace_key(&self) -> String {
        let retry = match self.retry {
            RetryState::Dormant => "dormant",
            RetryState::Waiting { .. } => "waiting",
            RetryState::Due => "due",
        };
        let active = self.active.as_ref().map_or_else(
            || "none".to_owned(),
            |active| {
                let relation = match (&self.desired, &active.written) {
                    (None, None) => "empty",
                    (Some(_), None) => "desired",
                    (None, Some(_)) => "closing",
                    (Some(desired), Some(written))
                        if desired.stamp == written.fence.stamp
                            && desired.source.as_ref() == written.source.as_ref() =>
                    {
                        "current"
                    }
                    (Some(desired), Some(written))
                        if desired.stamp.document_id == written.fence.stamp.document_id =>
                    {
                        "edit"
                    }
                    (Some(_), Some(_)) => "replacement",
                };
                let writer = active
                    .writer_flight
                    .as_ref()
                    .map_or("idle", |flight| frame_kind_tag(&flight.kind));
                let ledger = active
                    .ledger
                    .iter()
                    .map(|entry| {
                        format!(
                            "{}:{}:{}",
                            request_kind_tag(entry.kind),
                            entry.write_acked,
                            entry.validated_response.is_some()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{:?}:{relation}:{writer}:{ledger}:srv{}:pre{}:sem{}:defi{}:diag{}:syn{}:def{}:not{}:caret{}:nav{}:enabled{}",
                    active.phase,
                    active.server_replies.len(),
                    active.pre_ack_diagnostics.is_some(),
                    active.semantic_intent.is_some(),
                    active.definition_intent.is_some(),
                    active.diagnostics.is_some(),
                    active.syntax.is_some(),
                    active.definition.is_some(),
                    active.notices.len(),
                    active.last_caret_generation.is_some(),
                    active.last_navigation_generation.is_some(),
                    active.definition_enabled,
                )
            },
        );
        format!(
            "start{}:shutdown{}:terminal{:?}:retry{retry}:active{active}:cleanup{:?}:cancel{}:dirty{}:terminalpub{}:counters{}{}{}{}{}{}{}{}{}",
            self.started,
            self.shutdown_requested,
            self.terminal,
            self.cleanup_pending
                .map(|barrier| (barrier.caused_by_shutdown, barrier.unresolved_failure)),
            self.launch_cancel_pending.is_some(),
            self.snapshot_dirty,
            self.snapshot_terminal_published,
            self.next_generation.is_some(),
            self.next_request.is_some(),
            self.next_write.is_some(),
            self.next_diagnostic_revision.is_some(),
            self.next_diagnostic_item.is_some(),
            self.next_definition_result.is_some(),
            self.next_notice.is_some(),
            self.next_snapshot_revision.is_some(),
            self.last_finalized_generation.is_some(),
        )
    }

    #[cfg(test)]
    pub(crate) fn test_pending_initialize(&self) -> Option<(ProcessGeneration, ClientRequestId)> {
        let active = self.active.as_ref()?;
        active
            .ledger
            .iter()
            .find(|entry| {
                entry.kind == RequestKind::Initialize && entry.validated_response.is_none()
            })
            .map(|entry| (active.generation, entry.id))
    }

    #[cfg(test)]
    pub(crate) fn test_next_trace_desired(&self) -> DurableDesiredDocument {
        match &self.desired {
            None => DurableDesiredDocument {
                stamp: DocumentStamp {
                    document_id: crate::DocumentId::from_raw(1).expect("trace document"),
                    edit_revision: crate::EditRevision::from_raw(1).expect("trace revision"),
                },
                source: Arc::from("var a = 1;"),
            },
            Some(desired) => DurableDesiredDocument {
                stamp: DocumentStamp {
                    document_id: desired.stamp.document_id,
                    edit_revision: crate::EditRevision::from_raw(
                        desired
                            .stamp
                            .edit_revision
                            .get()
                            .checked_add(1)
                            .expect("bounded trace revision"),
                    )
                    .expect("bounded trace revision"),
                },
                source: if desired.source.as_ref() == "var a = 1;" {
                    Arc::from("var b = 2;")
                } else {
                    Arc::from("var a = 1;")
                },
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn test_next_definition_event(&self) -> Option<ActorEvent> {
        let active = self.active.as_ref()?;
        let written = active.written.as_ref()?;
        if !self.fence_is_current(&written.fence) {
            return None;
        }
        let fence = UiDocumentFence {
            process_generation: active.generation,
            stamp: written.fence.stamp,
            lsp_version: Some(written.fence.lsp_version),
        };
        match active.last_caret_generation {
            None => Some(ActorEvent::CaretChanged {
                fence,
                caret_generation: CaretGeneration::from_raw(1).expect("trace caret"),
            }),
            Some(caret_generation) => {
                let navigation_generation = match active.last_navigation_generation {
                    Some(previous) => previous.checked_next()?,
                    None => NavigationGeneration::from_raw(1)?,
                };
                Some(ActorEvent::DefinitionRequested {
                    fence,
                    caret_generation,
                    navigation_generation,
                    primary_character: 0,
                })
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn test_next_trace_tick(&self) -> ManualTick {
        self.next_deadline().unwrap_or_else(|| {
            self.now
                .checked_add(1)
                .unwrap_or(ManualTick::from_raw(u64::MAX))
        })
    }

    fn start(&mut self, now: ManualTick) -> Option<ActorEffect> {
        if self.started {
            if now < self.now {
                return self.enter_terminal(TerminalCause::InvalidTime, CleanupCause::InvalidTime);
            }
            return None;
        }
        self.started = true;
        self.now = now;
        self.retry = RetryState::Due;
        self.snapshot_dirty = true;
        None
    }

    fn drive_lifecycle(&mut self) -> Option<ActorEffect> {
        if self.snapshot_terminal_published {
            return None;
        }
        if self.terminal.is_some() || self.cleanup_pending.is_some() || !self.started {
            return None;
        }
        if self.active.is_none() {
            if self.shutdown_requested {
                self.terminal = Some(TerminalCause::Shutdown);
                self.snapshot_dirty = true;
                return None;
            }
            if self.retry == RetryState::Due {
                return self.prepare_launch();
            }
            return None;
        }
        let generation = self.active.as_ref().expect("active generation").generation;
        let phase = self.active.as_ref().expect("active generation").phase;
        if phase == ProtocolPhase::Launching
            || self
                .active
                .as_ref()
                .is_some_and(|active| active.writer_flight.is_some())
        {
            return None;
        }
        if phase == ProtocolPhase::NeedInitialize {
            return self.prepare_request(generation, RequestKind::Initialize, None);
        }
        if self.shutdown_requested && phase == ProtocolPhase::Ready {
            let has_shutdown = self.active.as_ref().is_some_and(|active| {
                active
                    .ledger
                    .iter()
                    .any(|entry| entry.kind == RequestKind::Shutdown)
            });
            if !has_shutdown {
                return self.prepare_request(generation, RequestKind::Shutdown, None);
            }
        }
        if phase == ProtocolPhase::NeedExit {
            if self
                .active
                .as_ref()
                .is_some_and(|active| !active.server_replies.is_empty())
            {
                return self.prepare_server_reply(generation);
            }
            return self.prepare_exit(generation);
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.server_replies.is_empty())
        {
            return self.prepare_server_reply(generation);
        }
        if phase == ProtocolPhase::NeedInitialized {
            return self.prepare_initialized(generation);
        }
        if phase != ProtocolPhase::Ready || self.shutdown_requested {
            return None;
        }
        if let Some(effect) = self.prepare_sync(generation) {
            return Some(effect);
        }
        if let Some(intent) = self
            .active
            .as_ref()
            .and_then(|active| active.definition_intent.clone())
        {
            return self.prepare_request(
                generation,
                RequestKind::Definition,
                Some(RequestContext::Definition(intent)),
            );
        }
        if let Some(fence) = self
            .active
            .as_ref()
            .and_then(|active| active.semantic_intent.clone())
        {
            return self.prepare_request(
                generation,
                RequestKind::SemanticTokens,
                Some(RequestContext::Semantic(fence)),
            );
        }
        None
    }

    fn prepare_launch(&mut self) -> Option<ActorEffect> {
        let Some(raw) = self.next_generation else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(generation) = ProcessGeneration::from_raw(raw) else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(deadline) = self.now.checked_add(LAUNCH_TIMEOUT_MS) else {
            return self.enter_terminal(TerminalCause::InvalidTime, CleanupCause::InvalidTime);
        };
        let next_generation = raw.checked_add(1);
        self.next_generation = next_generation;
        self.last_allocated_generation = Some(generation);
        self.final_stderr_tail = None;
        self.retry = RetryState::Dormant;
        self.active = Some(GenerationState {
            generation,
            phase: ProtocolPhase::Launching,
            launch_deadline: Some(deadline),
            next_lsp_version: Some(if self.last_finalized_generation.is_some() {
                1
            } else {
                self.initial_lsp_version
            }),
            last_caret_generation: None,
            last_navigation_generation: None,
            definition_enabled: true,
            open_uri: None,
            written: None,
            index: None,
            writer_flight: None,
            ledger: Vec::new(),
            server_replies: VecDeque::new(),
            server_reply_bytes: 0,
            pre_ack_diagnostics: None,
            semantic_intent: None,
            definition_intent: None,
            diagnostics: None,
            syntax: None,
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            legend: None,
        });
        self.snapshot_dirty = true;
        Some(ActorEffect::LaunchGeneration { generation })
    }

    fn launch_finished(
        &mut self,
        generation: ProcessGeneration,
        outcome: LaunchOutcome,
    ) -> Option<ActorEffect> {
        if !self.is_current_generation(generation) {
            return self.handle_noncurrent_generation(generation);
        }
        if self.active.as_ref().expect("current generation").phase != ProtocolPhase::Launching {
            return self.enter_terminal(
                TerminalCause::AdapterInvariant,
                CleanupCause::AdapterInvariant,
            );
        }
        if self.launch_cancel_pending == Some(generation) || self.shutdown_requested {
            self.launch_cancel_pending = None;
            let cancelled_for_shutdown = self.shutdown_requested;
            return match outcome {
                LaunchOutcome::FailedBeforeOwnership { .. } => {
                    self.active = None;
                    self.snapshot_dirty = true;
                    if cancelled_for_shutdown {
                        self.terminal = Some(TerminalCause::Shutdown);
                    } else {
                        self.schedule_retry();
                    }
                    None
                }
                LaunchOutcome::Ready | LaunchOutcome::FailedWithOwnedResources { .. } => self
                    .begin_cleanup(
                        generation,
                        CleanupMode::ForceTerminate,
                        if cancelled_for_shutdown {
                            CleanupCause::Shutdown
                        } else {
                            CleanupCause::Launch
                        },
                    ),
            };
        }
        match outcome {
            LaunchOutcome::Ready => {
                let active = self.active.as_mut().expect("current generation");
                active.phase = ProtocolPhase::NeedInitialize;
                active.launch_deadline = None;
                self.snapshot_dirty = true;
                None
            }
            LaunchOutcome::FailedBeforeOwnership { .. } => {
                self.active = None;
                self.snapshot_dirty = true;
                self.schedule_retry();
                None
            }
            LaunchOutcome::FailedWithOwnedResources { .. } => self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Launch,
            ),
        }
    }

    fn set_desired(&mut self, desired: Option<DurableDesiredDocument>) -> Option<ActorEffect> {
        if let (Some(existing), Some(candidate)) = (&self.desired, &desired) {
            if existing.stamp == candidate.stamp {
                if existing.source.as_ref() == candidate.source.as_ref() {
                    return None;
                }
                return self.enter_terminal(TerminalCause::ModelInvariant, CleanupCause::Protocol);
            }
            if existing.stamp.document_id == candidate.stamp.document_id
                && candidate.stamp.edit_revision.get() < existing.stamp.edit_revision.get()
            {
                return None;
            }
        }
        if let Some(candidate) = &desired
            && TextIndex::new(candidate.source.clone()).is_err()
        {
            return self.enter_terminal(TerminalCause::ModelInvariant, CleanupCause::ActorBudget);
        }
        if self.accounted_state_bytes_with_desired(desired.as_ref()) > MAX_ACTOR_STATE_BYTES {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        self.desired = desired;
        if let Some(active) = &mut self.active {
            active.pre_ack_diagnostics = None;
            let desired_stamp = self.desired.as_ref().map(|document| document.stamp);
            if active
                .definition_intent
                .as_ref()
                .is_some_and(|intent| Some(intent.fence.stamp) != desired_stamp)
            {
                active.definition_intent = None;
            }
            if active
                .definition
                .as_ref()
                .is_some_and(|result| Some(result.snapshot.stamp) != desired_stamp)
            {
                active.definition = None;
            }
        }
        self.snapshot_dirty = true;
        None
    }

    fn shutdown(&mut self) -> Option<ActorEffect> {
        if self.shutdown_requested {
            return None;
        }
        self.shutdown_requested = true;
        self.retry = RetryState::Dormant;
        if let Some(active) = &mut self.active {
            active.semantic_intent = None;
            active.definition_intent = None;
            if active.phase == ProtocolPhase::Launching {
                if self.launch_cancel_pending.is_none() {
                    self.launch_cancel_pending = Some(active.generation);
                    self.snapshot_dirty = true;
                    return Some(ActorEffect::CancelLaunch {
                        generation: active.generation,
                    });
                }
                return None;
            }
            if !matches!(
                active.phase,
                ProtocolPhase::Ready
                    | ProtocolPhase::ShutdownAwaitResponse
                    | ProtocolPhase::NeedExit
                    | ProtocolPhase::ExitWriting
            ) {
                let generation = active.generation;
                return self.begin_cleanup(
                    generation,
                    CleanupMode::ForceTerminate,
                    CleanupCause::Shutdown,
                );
            }
            self.snapshot_dirty = true;
            return None;
        }
        if self.cleanup_pending.is_none() {
            self.terminal = Some(TerminalCause::Shutdown);
            self.snapshot_dirty = true;
        }
        None
    }

    fn tick(&mut self, now: ManualTick) -> Option<ActorEffect> {
        if now < self.now {
            return self.enter_terminal(TerminalCause::InvalidTime, CleanupCause::InvalidTime);
        }
        self.now = now;
        if let RetryState::Waiting { deadline } = self.retry
            && now >= deadline
        {
            self.retry = RetryState::Due;
            self.snapshot_dirty = true;
        }
        let Some(active) = &self.active else {
            return None;
        };
        let generation = active.generation;
        if active
            .writer_flight
            .as_ref()
            .is_some_and(|flight| now >= flight.write_deadline)
        {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::WriteTimeout,
            );
        }
        if active.ledger.iter().any(|entry| {
            entry
                .response_deadline
                .is_some_and(|deadline| now >= deadline)
        }) {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::ResponseTimeout,
            );
        }
        if active.phase == ProtocolPhase::Launching
            && active
                .launch_deadline
                .is_some_and(|deadline| now >= deadline)
            && self.launch_cancel_pending.is_none()
        {
            self.launch_cancel_pending = Some(generation);
            self.snapshot_dirty = true;
            return Some(ActorEffect::CancelLaunch { generation });
        }
        None
    }

    fn schedule_retry(&mut self) {
        if self.shutdown_requested || self.terminal.is_some() {
            self.retry = RetryState::Dormant;
            return;
        }
        let exponent = self.failure_exponent.min(BACKOFF_MAX_EXPONENT);
        let delay = BACKOFF_INITIAL_MS
            .checked_mul(1u64 << exponent)
            .unwrap_or(BACKOFF_MAX_MS)
            .min(BACKOFF_MAX_MS);
        let Some(deadline) = self.now.checked_add(delay) else {
            self.terminal = Some(TerminalCause::InvalidTime);
            self.retry = RetryState::Dormant;
            self.snapshot_dirty = true;
            return;
        };
        self.failure_exponent = self
            .failure_exponent
            .saturating_add(1)
            .min(BACKOFF_MAX_EXPONENT);
        self.retry = if self.now >= deadline {
            RetryState::Due
        } else {
            RetryState::Waiting { deadline }
        };
        self.snapshot_dirty = true;
    }

    fn generation_failure(
        &mut self,
        generation: ProcessGeneration,
        cause: CleanupCause,
    ) -> Option<ActorEffect> {
        if self.is_current_generation(generation) {
            self.begin_cleanup(generation, CleanupMode::ForceTerminate, cause)
        } else {
            self.handle_noncurrent_generation(generation)
        }
    }

    fn begin_cleanup(
        &mut self,
        generation: ProcessGeneration,
        mode: CleanupMode,
        cause: CleanupCause,
    ) -> Option<ActorEffect> {
        if self.cleanup_pending.is_some() {
            return None;
        }
        if !self.is_current_generation(generation) {
            return self.handle_noncurrent_generation(generation);
        }
        self.active = None;
        self.launch_cancel_pending = None;
        self.cleanup_pending = Some(CleanupBarrier {
            generation,
            caused_by_shutdown: self.shutdown_requested || cause == CleanupCause::Shutdown,
            unresolved_failure: false,
        });
        if self.shutdown_requested
            || matches!(
                self.terminal,
                Some(
                    TerminalCause::InvalidTime
                        | TerminalCause::ModelInvariant
                        | TerminalCause::ActorBudget
                        | TerminalCause::CounterExhausted
                        | TerminalCause::AdapterInvariant
                        | TerminalCause::CleanupFailed
                )
            )
        {
            self.retry = RetryState::Dormant;
        } else {
            self.schedule_retry();
        }
        self.snapshot_dirty = true;
        Some(ActorEffect::BeginCleanup {
            generation,
            mode,
            cause,
        })
    }

    fn finalized_generation(
        &mut self,
        generation: ProcessGeneration,
        stderr_tail: Option<BoundedStderrTail>,
    ) -> Option<ActorEffect> {
        if let Some(barrier) = self.cleanup_pending {
            if generation < barrier.generation {
                return None;
            }
            if generation > barrier.generation {
                self.terminal = Some(TerminalCause::AdapterInvariant);
                self.retry = RetryState::Dormant;
                self.snapshot_dirty = true;
                return None;
            }
            let tail_error = self
                .replace_final_stderr_tail(generation, stderr_tail)
                .err();
            self.cleanup_pending = None;
            self.last_finalized_generation = Some(generation);
            if let Some(terminal) = tail_error {
                self.terminal = Some(terminal);
                self.retry = RetryState::Dormant;
            } else if self.shutdown_requested || barrier.caused_by_shutdown {
                self.terminal = Some(TerminalCause::Shutdown);
                self.retry = RetryState::Dormant;
            } else if barrier.unresolved_failure || self.terminal.is_some() {
                self.retry = RetryState::Dormant;
            }
            self.snapshot_dirty = true;
            return None;
        }
        if self
            .last_finalized_generation
            .is_some_and(|last| generation <= last)
        {
            return None;
        }
        if self.active_generation() == Some(generation) {
            return self.enter_terminal(
                TerminalCause::AdapterInvariant,
                CleanupCause::AdapterInvariant,
            );
        }
        if self
            .last_allocated_generation
            .is_none_or(|last| generation > last)
        {
            return self.enter_terminal(
                TerminalCause::AdapterInvariant,
                CleanupCause::AdapterInvariant,
            );
        }
        None
    }

    fn cleanup_failed(
        &mut self,
        generation: ProcessGeneration,
        stderr_tail: Option<BoundedStderrTail>,
    ) -> Option<ActorEffect> {
        let Some(mut barrier) = self.cleanup_pending else {
            if self.active_generation() == Some(generation) {
                return self.enter_terminal(
                    TerminalCause::AdapterInvariant,
                    CleanupCause::AdapterInvariant,
                );
            }
            return self.handle_noncurrent_generation(generation);
        };
        if generation < barrier.generation {
            return None;
        }
        if generation > barrier.generation {
            self.terminal = Some(TerminalCause::AdapterInvariant);
        } else {
            barrier.unresolved_failure = true;
            self.cleanup_pending = Some(barrier);
            self.terminal = Some(
                self.replace_final_stderr_tail(generation, stderr_tail)
                    .err()
                    .unwrap_or(TerminalCause::CleanupFailed),
            );
        }
        self.retry = RetryState::Dormant;
        self.snapshot_dirty = true;
        None
    }

    fn replace_final_stderr_tail(
        &mut self,
        generation: ProcessGeneration,
        tail: Option<BoundedStderrTail>,
    ) -> Result<(), TerminalCause> {
        let Some(tail) = tail else {
            self.final_stderr_tail = None;
            return Ok(());
        };
        if !bounded_stderr_tail_is_valid(&tail) {
            self.final_stderr_tail = None;
            return Err(TerminalCause::AdapterInvariant);
        }
        let previous_bytes = self
            .final_stderr_tail
            .as_ref()
            .map_or(0, |retained| retained.snapshot.text.len());
        if self
            .accounted_state_bytes()
            .saturating_sub(previous_bytes)
            .checked_add(tail.text.len())
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            self.final_stderr_tail = None;
            return Err(TerminalCause::ActorBudget);
        }
        self.final_stderr_tail = Some(RetainedStderrTail {
            generation,
            snapshot: StderrTailSnapshot {
                text: tail.text,
                line_count: tail.line_count,
                truncated: tail.truncated,
            },
        });
        Ok(())
    }

    fn enter_terminal(
        &mut self,
        terminal: TerminalCause,
        cleanup: CleanupCause,
    ) -> Option<ActorEffect> {
        if self.terminal.is_none() {
            self.terminal = Some(terminal);
        }
        #[cfg(test)]
        {
            self.test_orphan_diagnostics = None;
        }
        self.retry = RetryState::Dormant;
        self.snapshot_dirty = true;
        if let Some(generation) = self.active_generation() {
            self.begin_cleanup(generation, CleanupMode::ForceTerminate, cleanup)
        } else {
            None
        }
    }

    fn is_current_generation(&self, generation: ProcessGeneration) -> bool {
        self.active_generation() == Some(generation)
    }

    fn handle_noncurrent_generation(
        &mut self,
        generation: ProcessGeneration,
    ) -> Option<ActorEffect> {
        if self
            .last_allocated_generation
            .is_none_or(|last| generation > last)
        {
            self.enter_terminal(
                TerminalCause::AdapterInvariant,
                CleanupCause::AdapterInvariant,
            )
        } else {
            None
        }
    }

    fn publish_snapshot_effect(&mut self) -> Result<Option<ActorEffect>, SnapshotPublicationError> {
        if !self.snapshot_dirty || self.snapshot_terminal_published {
            return Ok(None);
        }
        let (revision, next_revision, terminal_publication) =
            if self.terminal.is_some() && self.next_snapshot_revision.is_none() {
                (SnapshotRevision::terminal(), None, true)
            } else {
                let raw = self
                    .next_snapshot_revision
                    .ok_or(SnapshotPublicationError::CounterExhausted)?;
                let revision = SnapshotRevision::from_raw(raw)
                    .ok_or(SnapshotPublicationError::CounterExhausted)?;
                let next = raw
                    .checked_add(1)
                    .filter(|next| SnapshotRevision::from_raw(*next).is_some());
                (revision, next, false)
            };
        let draft = self.build_snapshot_draft(revision);
        if !draft_fields_are_valid(&draft) || self.assert_invariants().is_err() {
            return Err(SnapshotPublicationError::InvalidCandidate);
        }
        let snapshot = LanguageSnapshot::bounded(draft.clone());
        if !snapshot_matches_actor_draft(&snapshot, &draft) {
            return Err(SnapshotPublicationError::InvalidCandidate);
        }
        self.next_snapshot_revision = next_revision;
        self.snapshot_terminal_published |= terminal_publication;
        self.mark_items_published(&snapshot);
        self.snapshot_dirty = false;
        Ok(Some(ActorEffect::PublishSnapshot {
            snapshot: Box::new(snapshot),
        }))
    }

    fn mark_items_published(&mut self, snapshot: &LanguageSnapshot) {
        let Some(active) = &mut self.active else {
            return;
        };
        if let Some(definition) = &mut active.definition
            && definition.first_published_revision.is_none()
            && snapshot.definition() == Some(&definition.snapshot)
        {
            definition.first_published_revision = Some(snapshot.revision());
        }
        for notice in &mut active.notices {
            if notice.first_published_revision.is_none()
                && snapshot
                    .notices()
                    .iter()
                    .any(|published| published == &notice.notice)
            {
                notice.first_published_revision = Some(snapshot.revision());
            }
        }
    }

    fn snapshot_state_is_valid(&self) -> bool {
        self.assert_invariants().is_ok()
            && draft_fields_are_valid(&self.build_snapshot_draft(SnapshotRevision::terminal()))
    }

    fn build_snapshot_draft(&self, revision: SnapshotRevision) -> LanguageSnapshotDraft {
        let process_generation = self
            .active_generation()
            .or_else(|| self.cleanup_pending.map(|barrier| barrier.generation))
            .or_else(|| {
                self.final_stderr_tail
                    .as_ref()
                    .map(|retained| retained.generation)
            });
        let desired_document = self.desired.as_ref().map(|desired| DocumentSyncSnapshot {
            stamp: desired.stamp,
            uri: Arc::from(synthetic_uri(desired.stamp.document_id)),
            lsp_version: None,
        });
        let (written_document, diagnostics, syntax, definition, notices, writer) =
            if let Some(active) = &self.active {
                (
                    active.written.as_ref().map(|written| DocumentSyncSnapshot {
                        stamp: written.fence.stamp,
                        uri: written.fence.uri.clone(),
                        lsp_version: Some(written.fence.lsp_version),
                    }),
                    active.diagnostics.clone(),
                    active.syntax.clone(),
                    active
                        .definition
                        .as_ref()
                        .map(|retained| retained.snapshot.clone()),
                    active
                        .notices
                        .iter()
                        .map(|retained| retained.notice.clone())
                        .collect(),
                    active
                        .writer_flight
                        .as_ref()
                        .map_or(WriterState::Idle, |flight| WriterState::Writing {
                            sequence: flight.sequence,
                        }),
                )
            } else {
                (
                    None,
                    {
                        #[cfg(test)]
                        {
                            self.test_orphan_diagnostics.clone()
                        }
                        #[cfg(not(test))]
                        {
                            None
                        }
                    },
                    None,
                    None,
                    Vec::new(),
                    if self.cleanup_pending.is_some() {
                        WriterState::Closed
                    } else {
                        WriterState::Idle
                    },
                )
            };
        let status = if self.terminal.is_some() {
            LanguageStatus::Disabled
        } else if let Some(barrier) = self.cleanup_pending {
            if barrier.caused_by_shutdown {
                LanguageStatus::ShuttingDown
            } else {
                LanguageStatus::Unavailable
            }
        } else if let Some(active) = &self.active {
            match active.phase {
                ProtocolPhase::Launching | ProtocolPhase::NeedInitialize => {
                    LanguageStatus::Starting
                }
                ProtocolPhase::Initializing
                | ProtocolPhase::NeedInitialized
                | ProtocolPhase::InitializedWriting => LanguageStatus::Initializing,
                ProtocolPhase::Ready => LanguageStatus::Ready,
                ProtocolPhase::ShutdownAwaitResponse
                | ProtocolPhase::NeedExit
                | ProtocolPhase::ExitWriting => LanguageStatus::ShuttingDown,
            }
        } else if self.shutdown_requested {
            LanguageStatus::Disabled
        } else {
            LanguageStatus::Unavailable
        };
        let stderr_tail = self
            .final_stderr_tail
            .as_ref()
            .filter(|retained| process_generation == Some(retained.generation))
            .map(|retained| retained.snapshot.clone());
        LanguageSnapshotDraft {
            revision,
            process_generation,
            status,
            desired_document,
            written_document,
            diagnostics,
            syntax,
            definition,
            notices,
            stderr_tail,
            writer,
        }
    }
}

#[derive(Clone, Debug)]
enum RequestContext {
    Semantic(DocumentFence),
    Definition(DefinitionIntent),
}

impl LanguageActor {
    fn prepare_request(
        &mut self,
        generation: ProcessGeneration,
        kind: RequestKind,
        context: Option<RequestContext>,
    ) -> Option<ActorEffect> {
        let active = self.active.as_ref()?;
        if active.generation != generation
            || active.writer_flight.is_some()
            || active.ledger.len() >= MAX_LEDGER_ENTRIES
        {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        if active.ledger.iter().any(|entry| entry.kind == kind) {
            return if matches!(kind, RequestKind::SemanticTokens | RequestKind::Definition) {
                None
            } else {
                self.begin_cleanup(
                    generation,
                    CleanupMode::ForceTerminate,
                    CleanupCause::AdapterInvariant,
                )
            };
        }
        if kind == RequestKind::Initialize && !active.ledger.is_empty() {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }

        let Some(raw_id) = self.next_request else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(id) = ClientRequestId::from_raw(raw_id) else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(raw_sequence) = self.next_write else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(sequence) = WriteSequence::from_raw(raw_sequence) else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let response_timeout = match kind {
            RequestKind::Initialize => INITIALIZE_RESPONSE_TIMEOUT_MS,
            RequestKind::SemanticTokens | RequestKind::Definition => FEATURE_RESPONSE_TIMEOUT_MS,
            RequestKind::Shutdown => SHUTDOWN_RESPONSE_TIMEOUT_MS,
        };
        let Some(write_deadline) = self.now.checked_add(WRITE_ACK_TIMEOUT_MS) else {
            return self.enter_terminal(TerminalCause::InvalidTime, CleanupCause::InvalidTime);
        };
        let Some(response_deadline) = self.now.checked_add(response_timeout) else {
            return self.enter_terminal(TerminalCause::InvalidTime, CleanupCause::InvalidTime);
        };

        let (value, fence, caret_generation, navigation_generation, primary_character) =
            match (kind, context) {
                (RequestKind::Initialize, None) => (initialize_request(id), None, None, None, None),
                (RequestKind::Shutdown, None) => (
                    json!({"jsonrpc":"2.0","id":id.get(),"method":"shutdown"}),
                    None,
                    None,
                    None,
                    None,
                ),
                (RequestKind::SemanticTokens, Some(RequestContext::Semantic(fence))) => {
                    let value = json!({
                        "jsonrpc":"2.0",
                        "id":id.get(),
                        "method":"textDocument/semanticTokens/full",
                        "params":{"textDocument":{"uri":fence.uri}}
                    });
                    (value, Some(fence), None, None, None)
                }
                (RequestKind::Definition, Some(RequestContext::Definition(intent))) => {
                    let index = active.index.as_ref()?;
                    if index.stamp != intent.fence.stamp
                        || index.source.as_ref()
                            != active
                                .written
                                .as_ref()
                                .map_or("", |written| written.source.as_ref())
                    {
                        return None;
                    }
                    let position = index
                        .index
                        .position_for_character(intent.primary_character)?;
                    let value = json!({
                        "jsonrpc":"2.0",
                        "id":id.get(),
                        "method":"textDocument/definition",
                        "params":{
                            "textDocument":{"uri":intent.fence.uri},
                            "position":{"line":position.line,"character":position.character}
                        }
                    });
                    (
                        value,
                        Some(intent.fence),
                        Some(intent.caret_generation),
                        Some(intent.navigation_generation),
                        Some(intent.primary_character),
                    )
                }
                _ => {
                    return self.begin_cleanup(
                        generation,
                        CleanupMode::ForceTerminate,
                        CleanupCause::AdapterInvariant,
                    );
                }
            };
        let Ok(frame) = encode_message(&value) else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            );
        };
        let bytes: Arc<[u8]> = frame.into();
        let frame_kind = FrameKind::ClientRequest { id, kind };
        let entry = RequestEntry {
            id,
            kind,
            generation,
            write_sequence: sequence,
            fence,
            caret_generation,
            navigation_generation,
            primary_character,
            response_deadline: Some(response_deadline),
            write_acked: false,
            validated_response: None,
        };
        if self.accounted_state_bytes().saturating_add(bytes.len()) > MAX_ACTOR_STATE_BYTES {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }

        self.next_request = raw_id.checked_add(1);
        self.next_write = raw_sequence.checked_add(1);
        let active = self.active.as_mut().expect("preflighted active generation");
        match kind {
            RequestKind::Initialize => active.phase = ProtocolPhase::Initializing,
            RequestKind::Shutdown => active.phase = ProtocolPhase::ShutdownAwaitResponse,
            RequestKind::SemanticTokens => active.semantic_intent = None,
            RequestKind::Definition => active.definition_intent = None,
        }
        active.ledger.push(entry);
        active.writer_flight = Some(WriterFlight {
            generation,
            sequence,
            kind: frame_kind.clone(),
            bytes: bytes.clone(),
            write_deadline,
            on_flush: FlushCommit::ClientRequest { id },
        });
        self.snapshot_dirty = true;
        Some(ActorEffect::SendFrame {
            generation,
            sequence,
            kind: frame_kind,
            bytes,
        })
    }

    fn prepare_initialized(&mut self, generation: ProcessGeneration) -> Option<ActorEffect> {
        let value = json!({"jsonrpc":"2.0","method":"initialized","params":{}});
        self.prepare_notification(
            generation,
            FrameKind::Initialized,
            value,
            FlushCommit::Initialized,
            Some(ProtocolPhase::InitializedWriting),
        )
    }

    fn prepare_exit(&mut self, generation: ProcessGeneration) -> Option<ActorEffect> {
        let value = json!({"jsonrpc":"2.0","method":"exit"});
        self.prepare_notification(
            generation,
            FrameKind::Exit,
            value,
            FlushCommit::Exit,
            Some(ProtocolPhase::ExitWriting),
        )
    }

    fn prepare_notification(
        &mut self,
        generation: ProcessGeneration,
        kind: FrameKind,
        value: Value,
        on_flush: FlushCommit,
        phase: Option<ProtocolPhase>,
    ) -> Option<ActorEffect> {
        let Some(active) = &self.active else {
            return None;
        };
        if active.generation != generation || active.writer_flight.is_some() {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        let Some(raw_sequence) = self.next_write else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(sequence) = WriteSequence::from_raw(raw_sequence) else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(write_deadline) = self.now.checked_add(WRITE_ACK_TIMEOUT_MS) else {
            return self.enter_terminal(TerminalCause::InvalidTime, CleanupCause::InvalidTime);
        };
        let Ok(frame) = encode_message(&value) else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            );
        };
        let bytes: Arc<[u8]> = frame.into();
        let retained_commit_bytes = self.active.as_ref().map_or(0, |active| {
            flush_commit_retained_bytes(&on_flush, self.desired.as_ref(), active.index.as_ref())
        });
        if self
            .accounted_state_bytes()
            .checked_add(bytes.len())
            .and_then(|bytes| bytes.checked_add(retained_commit_bytes))
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        self.next_write = raw_sequence.checked_add(1);
        let active = self.active.as_mut().expect("preflighted active generation");
        if let Some(phase) = phase {
            active.phase = phase;
        }
        active.writer_flight = Some(WriterFlight {
            generation,
            sequence,
            kind: kind.clone(),
            bytes: bytes.clone(),
            write_deadline,
            on_flush,
        });
        self.snapshot_dirty = true;
        Some(ActorEffect::SendFrame {
            generation,
            sequence,
            kind,
            bytes,
        })
    }

    fn prepare_sync(&mut self, generation: ProcessGeneration) -> Option<ActorEffect> {
        let (open_uri, written, desired) = {
            let active = self.active.as_ref()?;
            (
                active.open_uri.clone(),
                active.written.clone(),
                self.desired.clone(),
            )
        };
        let desired_uri = desired
            .as_ref()
            .map(|document| Arc::<str>::from(synthetic_uri(document.stamp.document_id)));
        if let Some(ref open_uri) = open_uri
            && desired_uri.as_deref() != Some(open_uri.as_ref())
        {
            let value = json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didClose",
                "params":{"textDocument":{"uri":open_uri}}
            });
            return self.prepare_notification(
                generation,
                FrameKind::DidClose {
                    uri: open_uri.clone(),
                },
                value,
                FlushCommit::DidClose {
                    uri: open_uri.clone(),
                },
                None,
            );
        }
        let desired = desired?;
        let uri = desired_uri.expect("desired URI");
        if let Some(written) = &written
            && written.fence.stamp == desired.stamp
            && written.source.as_ref() == desired.source.as_ref()
        {
            return None;
        }

        let Some(raw_version) = self
            .active
            .as_ref()
            .and_then(|active| active.next_lsp_version)
        else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::LspVersionExhausted,
            );
        };
        let Some(version) = LspVersion::from_raw(raw_version) else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::LspVersionExhausted,
            );
        };
        let Ok(index) = TextIndex::new(desired.source.clone()) else {
            return self.enter_terminal(TerminalCause::ModelInvariant, CleanupCause::ActorBudget);
        };
        let index = IndexedDocument {
            stamp: desired.stamp,
            source: desired.source.clone(),
            index: Arc::new(index),
        };
        let fence = DocumentFence {
            generation,
            uri: uri.clone(),
            lsp_version: version,
            stamp: desired.stamp,
        };
        let document = WrittenDocument {
            fence: fence.clone(),
            source: desired.source.clone(),
        };
        let (kind, value, on_flush) = if open_uri.is_none() {
            (
                FrameKind::DidOpen {
                    fence: fence.clone(),
                },
                json!({
                    "jsonrpc":"2.0",
                    "method":"textDocument/didOpen",
                    "params":{"textDocument":{
                        "uri":uri,
                        "languageId":"lox",
                        "version":version.get(),
                        "text":desired.source
                    }}
                }),
                FlushCommit::DidOpen { document, index },
            )
        } else {
            (
                FrameKind::DidChange {
                    fence: fence.clone(),
                },
                json!({
                    "jsonrpc":"2.0",
                    "method":"textDocument/didChange",
                    "params":{
                        "textDocument":{"uri":uri,"version":version.get()},
                        "contentChanges":[{"text":desired.source}]
                    }
                }),
                FlushCommit::DidChange { document, index },
            )
        };
        let effect = self.prepare_notification(generation, kind, value, on_flush, None);
        if matches!(effect, Some(ActorEffect::SendFrame { .. }))
            && let Some(active) = &mut self.active
        {
            active.next_lsp_version = raw_version.checked_add(1);
            active.pre_ack_diagnostics = None;
        }
        effect
    }

    fn prepare_server_reply(&mut self, generation: ProcessGeneration) -> Option<ActorEffect> {
        let reply = self
            .active
            .as_ref()
            .and_then(|active| active.server_replies.front().cloned())?;
        let value = decode_owned_frame(&reply.frame)?;
        let effect = self.prepare_notification(
            generation,
            FrameKind::ServerResponse {
                id: reply.id.clone(),
            },
            value,
            FlushCommit::ServerResponse,
            None,
        );
        if effect.is_some()
            && let Some(active) = &mut self.active
            && let Some(removed) = active.server_replies.pop_front()
        {
            active.server_reply_bytes = active
                .server_reply_bytes
                .saturating_sub(removed.frame.len());
        }
        effect
    }

    fn writer_finished(
        &mut self,
        generation: ProcessGeneration,
        sequence: WriteSequence,
        outcome: WriterOutcome,
    ) -> Option<ActorEffect> {
        if !self.is_current_generation(generation) {
            return self.handle_noncurrent_generation(generation);
        }
        let matches = self
            .active
            .as_ref()
            .and_then(|active| active.writer_flight.as_ref())
            .is_some_and(|flight| flight.sequence == sequence && flight.generation == generation);
        if !matches {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        if outcome != WriterOutcome::Flushed {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Writer,
            );
        }
        let flight = self
            .active
            .as_mut()
            .expect("current generation")
            .writer_flight
            .take()
            .expect("matching writer flight");
        self.snapshot_dirty = true;
        match flight.on_flush {
            FlushCommit::ClientRequest { id } => {
                let active = self.active.as_mut().expect("current generation");
                let Some(position) = active.ledger.iter().position(|entry| entry.id == id) else {
                    return self.begin_cleanup(
                        generation,
                        CleanupMode::ForceTerminate,
                        CleanupCause::AdapterInvariant,
                    );
                };
                active.ledger[position].write_acked = true;
                if active.ledger[position].validated_response.is_some() {
                    let entry = active.ledger.remove(position);
                    return self.apply_completed_request(entry);
                }
                None
            }
            FlushCommit::Initialized => {
                let active = self.active.as_mut().expect("current generation");
                active.phase = ProtocolPhase::Ready;
                self.failure_exponent = 0;
                None
            }
            FlushCommit::DidClose { uri } => {
                let active = self.active.as_mut().expect("current generation");
                if active.open_uri.as_deref() != Some(uri.as_ref()) {
                    return self.begin_cleanup(
                        generation,
                        CleanupMode::ForceTerminate,
                        CleanupCause::AdapterInvariant,
                    );
                }
                active.open_uri = None;
                active.written = None;
                active.index = None;
                active.pre_ack_diagnostics = None;
                active.semantic_intent = None;
                active.definition_intent = None;
                active.diagnostics = None;
                active.syntax = None;
                active.definition = None;
                None
            }
            FlushCommit::DidOpen { document, index }
            | FlushCommit::DidChange { document, index } => {
                self.commit_written_document(generation, flight.sequence, document, index)
            }
            FlushCommit::ServerResponse => None,
            FlushCommit::Exit => self.begin_cleanup(
                generation,
                CleanupMode::GracefulExitFlushed,
                CleanupCause::Shutdown,
            ),
        }
    }

    fn commit_written_document(
        &mut self,
        generation: ProcessGeneration,
        sequence: WriteSequence,
        document: WrittenDocument,
        index: IndexedDocument,
    ) -> Option<ActorEffect> {
        let desired_matches = self.desired.as_ref().is_some_and(|desired| {
            desired.stamp == document.fence.stamp
                && desired.source.as_ref() == document.source.as_ref()
        });
        let active = self.active.as_mut().expect("current generation");
        active.open_uri = Some(document.fence.uri.clone());
        active.written = Some(document.clone());
        active.index = Some(index);
        active.diagnostics = None;
        active.syntax = None;
        active.definition = None;
        if active
            .definition_intent
            .as_ref()
            .is_some_and(|intent| intent.fence != document.fence)
        {
            active.definition_intent = None;
        }
        active.semantic_intent = desired_matches.then_some(document.fence.clone());
        let pre_ack = active.pre_ack_diagnostics.take();
        if let Some(pre_ack) = pre_ack
            && pre_ack.generation == generation
            && pre_ack.write_sequence == sequence
            && pre_ack.uri.as_ref() == document.fence.uri.as_ref()
            && pre_ack.lsp_version == document.fence.lsp_version
            && pre_ack.stamp == document.fence.stamp
            && desired_matches
        {
            return self.install_diagnostics(document.fence, pre_ack.plan);
        }
        None
    }

    fn reader_message(
        &mut self,
        generation: ProcessGeneration,
        accounted: AccountedJsonRpcMessage,
    ) -> Option<ActorEffect> {
        if !self.is_current_generation(generation) {
            return self.handle_noncurrent_generation(generation);
        }
        if accounted.body_bytes != accounted.message.body_bytes() {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        let envelope = accounted.message.envelope().clone();
        match envelope {
            RpcEnvelope::Response { id, outcome } => {
                self.reader_response(generation, id, outcome, accounted.message.into_value())
            }
            RpcEnvelope::Request { id, .. } => self.stage_server_reply(generation, id),
            RpcEnvelope::Notification { method } => {
                if method == "textDocument/publishDiagnostics" {
                    self.publish_diagnostics_notification(generation, accounted.message.value())
                } else {
                    None
                }
            }
        }
    }

    fn stage_server_reply(
        &mut self,
        generation: ProcessGeneration,
        id: RpcId,
    ) -> Option<ActorEffect> {
        let active = self.active.as_ref().expect("current generation");
        if matches!(
            active.phase,
            ProtocolPhase::NeedExit | ProtocolPhase::ExitWriting
        ) {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        let value = json!({
            "jsonrpc":"2.0",
            "id":rpc_id_value(&id),
            "error":{"code":-32601,"message":"Method not found"}
        });
        let Ok(frame) = encode_message(&value) else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            );
        };
        if frame.len() > MAX_SERVER_REPLY_FRAME_BYTES
            || active.server_replies.len() >= MAX_SERVER_REPLIES
            || active
                .server_reply_bytes
                .checked_add(frame.len())
                .is_none_or(|bytes| bytes > MAX_SERVER_REPLY_BYTES)
        {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        let id_bytes = match &id {
            RpcId::Number(_) => 0,
            RpcId::String(value) => value.capacity(),
        };
        if self
            .accounted_state_bytes()
            .checked_add(frame.len())
            .and_then(|bytes| bytes.checked_add(id_bytes))
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        let active = self.active.as_mut().expect("current generation");
        active.server_reply_bytes += frame.len();
        active.server_replies.push_back(ServerReply {
            id,
            frame: frame.into(),
        });
        self.snapshot_dirty = true;
        None
    }

    fn reader_response(
        &mut self,
        generation: ProcessGeneration,
        response_id: RpcResponseId,
        outcome: RpcOutcome,
        value: Value,
    ) -> Option<ActorEffect> {
        let RpcResponseId::Id(RpcId::Number(raw_id)) = response_id else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            );
        };
        let Some(id) = ClientRequestId::from_raw(raw_id) else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            );
        };
        let Some(position) = self
            .active
            .as_ref()
            .and_then(|active| active.ledger.iter().position(|entry| entry.id == id))
        else {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            );
        };
        if self.active.as_ref().expect("current generation").ledger[position]
            .validated_response
            .is_some()
        {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            );
        }
        let entry = self.active.as_ref().expect("current generation").ledger[position].clone();
        let projected = self.project_response(&entry, outcome, &value);
        let projected_bytes = validated_response_retained_bytes(&projected);
        if self
            .accounted_state_bytes()
            .checked_add(projected_bytes)
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        let write_acked = entry.write_acked;
        let active = self.active.as_mut().expect("current generation");
        active.ledger[position].validated_response = Some(projected);
        active.ledger[position].response_deadline = None;
        self.snapshot_dirty = true;
        if write_acked {
            let entry = self
                .active
                .as_mut()
                .expect("current generation")
                .ledger
                .remove(position);
            self.apply_completed_request(entry)
        } else {
            None
        }
    }

    fn project_response(
        &self,
        entry: &RequestEntry,
        outcome: RpcOutcome,
        value: &Value,
    ) -> ValidatedResponse {
        if outcome == RpcOutcome::Error {
            let message = value
                .get("error")
                .and_then(Value::as_object)
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("language request failed");
            let message = if message.len() <= MAX_REQUEST_ERROR_MESSAGE_BYTES {
                Arc::from(message)
            } else {
                Arc::from("language request failed with an oversized error message")
            };
            return ValidatedResponse::RpcError(message);
        }
        match entry.kind {
            RequestKind::Initialize => match validate_initialize_result(value.get("result")) {
                Ok(legend) => ValidatedResponse::Initialize(legend),
                Err(()) => ValidatedResponse::InvalidForKind(Arc::from(
                    "language server returned incompatible capabilities",
                )),
            },
            RequestKind::Shutdown => {
                if value.get("result") == Some(&Value::Null) {
                    ValidatedResponse::ShutdownOk
                } else {
                    ValidatedResponse::InvalidForKind(Arc::from(
                        "language server returned an invalid shutdown result",
                    ))
                }
            }
            RequestKind::SemanticTokens => {
                let Some(fence) = &entry.fence else {
                    return ValidatedResponse::InvalidForKind(Arc::from(
                        "semantic request lost its document fence",
                    ));
                };
                let Some((index, legend)) = self.current_index_and_legend(fence) else {
                    return ValidatedResponse::Stale;
                };
                let data = match value.get("result") {
                    Some(Value::Null) => return ValidatedResponse::Semantic(Vec::new()),
                    Some(Value::Object(result)) => result.get("data"),
                    _ => None,
                };
                match data.and_then(Value::as_array) {
                    Some(_) => match decode_semantic_tokens(index, legend, data.expect("data")) {
                        Ok(runs) => ValidatedResponse::Semantic(runs),
                        Err(_) => ValidatedResponse::InvalidForKind(Arc::from(
                            "language server returned invalid syntax data",
                        )),
                    },
                    None => ValidatedResponse::InvalidForKind(Arc::from(
                        "language server returned invalid syntax data",
                    )),
                }
            }
            RequestKind::Definition => {
                let Some(fence) = &entry.fence else {
                    return ValidatedResponse::InvalidForKind(Arc::from(
                        "definition request lost its document fence",
                    ));
                };
                let Some((index, _)) = self.current_index_and_legend(fence) else {
                    return ValidatedResponse::Stale;
                };
                match value.get("result") {
                    Some(result) => match validate_definitions(index, &fence.uri, result) {
                        Ok(targets) => ValidatedResponse::Definition(targets),
                        Err(_) => ValidatedResponse::InvalidForKind(Arc::from(
                            "language server returned invalid definition targets",
                        )),
                    },
                    None => ValidatedResponse::InvalidForKind(Arc::from(
                        "language server returned invalid definition targets",
                    )),
                }
            }
        }
    }

    fn current_index_and_legend(
        &self,
        fence: &DocumentFence,
    ) -> Option<(&TextIndex, &SemanticLegend)> {
        let active = self.active.as_ref()?;
        let written = active.written.as_ref()?;
        let index = active.index.as_ref()?;
        if &written.fence != fence || index.stamp != fence.stamp {
            return None;
        }
        Some((&index.index, active.legend.as_ref()?))
    }

    fn apply_completed_request(&mut self, entry: RequestEntry) -> Option<ActorEffect> {
        if !entry.write_acked {
            return self.begin_cleanup(
                entry.generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        let Some(response) = entry.validated_response.clone() else {
            return self.begin_cleanup(
                entry.generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        };
        if self.shutdown_requested
            && matches!(
                entry.kind,
                RequestKind::SemanticTokens | RequestKind::Definition
            )
        {
            return None;
        }
        match (entry.kind, response) {
            (RequestKind::Initialize, ValidatedResponse::Initialize(legend)) => {
                let active = self.active.as_mut().expect("current generation");
                active.legend = Some(legend);
                active.phase = ProtocolPhase::NeedInitialized;
                self.snapshot_dirty = true;
                None
            }
            (RequestKind::Shutdown, ValidatedResponse::ShutdownOk) => {
                let active = self.active.as_mut().expect("current generation");
                active.phase = ProtocolPhase::NeedExit;
                self.snapshot_dirty = true;
                None
            }
            (RequestKind::SemanticTokens, ValidatedResponse::Semantic(runs)) => {
                self.apply_semantic_result(&entry, runs)
            }
            (RequestKind::Definition, ValidatedResponse::Definition(targets)) => {
                self.apply_definition_result(&entry, targets)
            }
            (RequestKind::SemanticTokens | RequestKind::Definition, ValidatedResponse::Stale) => {
                None
            }
            (
                RequestKind::SemanticTokens | RequestKind::Definition,
                ValidatedResponse::RpcError(message) | ValidatedResponse::InvalidForKind(message),
            ) => {
                if entry.kind == RequestKind::SemanticTokens
                    && let Some(active) = &mut self.active
                {
                    active.syntax = None;
                }
                self.add_notice(NoticeKind::RequestError, message)
            }
            (
                RequestKind::Initialize | RequestKind::Shutdown,
                ValidatedResponse::RpcError(_) | ValidatedResponse::InvalidForKind(_),
            ) => self.begin_cleanup(
                entry.generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            ),
            _ => self.begin_cleanup(
                entry.generation,
                CleanupMode::ForceTerminate,
                CleanupCause::Protocol,
            ),
        }
    }

    fn apply_semantic_result(
        &mut self,
        entry: &RequestEntry,
        runs: Vec<SyntaxRun>,
    ) -> Option<ActorEffect> {
        let Some(fence) = &entry.fence else {
            return self.begin_cleanup(
                entry.generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        };
        if !self.fence_is_current(fence) {
            return None;
        }
        let active = self.active.as_mut().expect("current generation");
        active.syntax = Some(SyntaxSnapshot {
            process_generation: fence.generation,
            uri: fence.uri.clone(),
            stamp: fence.stamp,
            lsp_version: fence.lsp_version,
            runs: runs.into(),
        });
        self.snapshot_dirty = true;
        None
    }

    fn apply_definition_result(
        &mut self,
        entry: &RequestEntry,
        targets: Vec<DefinitionTarget>,
    ) -> Option<ActorEffect> {
        let (Some(fence), Some(caret), Some(navigation), Some(primary_character)) = (
            &entry.fence,
            entry.caret_generation,
            entry.navigation_generation,
            entry.primary_character,
        ) else {
            return self.begin_cleanup(
                entry.generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        };
        let current = self.active.as_ref().is_some_and(|active| {
            self.fence_is_current(fence)
                && active.last_caret_generation == Some(caret)
                && active.last_navigation_generation == Some(navigation)
        });
        if !current {
            return None;
        }
        let target_count = targets.len();
        let notice = if target_count == 0 {
            match self.prepare_notice(NoticeKind::Information, Arc::from("no definition found")) {
                Ok(notice) => Some(notice),
                Err(()) => {
                    return self.enter_terminal(
                        TerminalCause::CounterExhausted,
                        CleanupCause::CounterExhausted,
                    );
                }
            }
        } else if target_count > 1 {
            match self.prepare_notice(
                NoticeKind::Information,
                Arc::from(format!("{target_count} definition candidates found")),
            ) {
                Ok(notice) => Some(notice),
                Err(()) => {
                    return self.enter_terminal(
                        TerminalCause::CounterExhausted,
                        CleanupCause::CounterExhausted,
                    );
                }
            }
        } else {
            None
        };
        let Some(raw_result) = self.next_definition_result else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(id) = DefinitionResultId::from_raw(raw_result) else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let snapshot = DefinitionSnapshot {
            id,
            process_generation: fence.generation,
            uri: fence.uri.clone(),
            stamp: fence.stamp,
            lsp_version: fence.lsp_version,
            caret_generation: caret,
            navigation_generation: navigation,
            caret_character: primary_character,
            targets: targets
                .into_iter()
                .map(|target| DefinitionTargetSnapshot {
                    range: target.range,
                })
                .collect::<Vec<_>>()
                .into(),
        };
        let added_bytes = snapshot
            .targets
            .len()
            .saturating_mul(std::mem::size_of::<DefinitionTargetSnapshot>())
            .saturating_add(notice.as_ref().map_or(0, prepared_notice_retained_bytes));
        if self
            .accounted_state_bytes()
            .checked_add(added_bytes)
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        self.next_definition_result = raw_result.checked_add(1);
        self.active.as_mut().expect("current generation").definition = Some(RetainedDefinition {
            snapshot,
            first_published_revision: None,
        });
        if let Some(notice) = notice {
            self.commit_notice(notice);
        }
        self.snapshot_dirty = true;
        None
    }

    fn fence_is_current(&self, fence: &DocumentFence) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.generation == fence.generation
                && active
                    .written
                    .as_ref()
                    .is_some_and(|written| &written.fence == fence)
                && self.desired.as_ref().is_some_and(|desired| {
                    desired.stamp == fence.stamp
                        && active.written.as_ref().is_some_and(|written| {
                            desired.source.as_ref() == written.source.as_ref()
                        })
                })
        })
    }

    fn publish_diagnostics_notification(
        &mut self,
        generation: ProcessGeneration,
        value: &Value,
    ) -> Option<ActorEffect> {
        let Some(params) = value.get("params").and_then(Value::as_object) else {
            return self.feature_notice("language server returned invalid diagnostics");
        };
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return self.feature_notice("language server returned invalid diagnostics");
        };
        let raw_version = params
            .get("version")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())?;
        let version = LspVersion::from_raw(raw_version)?;
        let Some(raw_diagnostics) = params.get("diagnostics") else {
            return self.feature_notice("language server returned invalid diagnostics");
        };

        let pre_ack_target = self.active.as_ref().and_then(|active| {
            let flight = active.writer_flight.as_ref()?;
            match &flight.on_flush {
                FlushCommit::DidOpen { document, index }
                | FlushCommit::DidChange { document, index }
                    if document.fence.uri.as_ref() == uri
                        && document.fence.lsp_version == version =>
                {
                    Some((flight.sequence, document.fence.clone(), index.index.clone()))
                }
                _ => None,
            }
        });
        if let Some((sequence, fence, index)) = pre_ack_target {
            let Ok(plan) = validate_diagnostics(&index, raw_diagnostics) else {
                return self.feature_notice("language server returned invalid diagnostics");
            };
            let previous_bytes = self
                .active
                .as_ref()
                .and_then(|active| active.pre_ack_diagnostics.as_ref())
                .map_or(0, |pre_ack| diagnostic_plan_retained_bytes(&pre_ack.plan));
            let projected_bytes = diagnostic_plan_retained_bytes(&plan);
            if self
                .accounted_state_bytes()
                .saturating_sub(previous_bytes)
                .checked_add(projected_bytes)
                .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
            {
                return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
            }
            let active = self.active.as_mut().expect("current generation");
            active.pre_ack_diagnostics = Some(PreAckDiagnostics {
                generation,
                write_sequence: sequence,
                uri: fence.uri,
                lsp_version: fence.lsp_version,
                stamp: fence.stamp,
                plan,
            });
            self.snapshot_dirty = true;
            return None;
        }

        let current = self.active.as_ref().and_then(|active| {
            let written = active.written.as_ref()?;
            let index = active.index.as_ref()?;
            (written.fence.uri.as_ref() == uri && written.fence.lsp_version == version)
                .then_some((written.fence.clone(), index.index.clone()))
        });
        let (fence, index) = current?;
        let Ok(plan) = validate_diagnostics(&index, raw_diagnostics) else {
            return self.feature_notice("language server returned invalid diagnostics");
        };
        self.install_diagnostics(fence, plan)
    }

    fn install_diagnostics(
        &mut self,
        fence: DocumentFence,
        plan: DiagnosticPlan,
    ) -> Option<ActorEffect> {
        if !self.fence_is_written(&fence) {
            return None;
        }
        let Some(raw_revision) = self.next_diagnostic_revision else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let Some(revision) = DiagnosticSetRevision::from_raw(raw_revision) else {
            return self.enter_terminal(
                TerminalCause::CounterExhausted,
                CleanupCause::CounterExhausted,
            );
        };
        let item_count = plan.items.len();
        let (raw_item, next_item) = if item_count == 0 {
            (None, self.next_diagnostic_item)
        } else {
            let Some(raw_item) = self.next_diagnostic_item else {
                return self.enter_terminal(
                    TerminalCause::CounterExhausted,
                    CleanupCause::CounterExhausted,
                );
            };
            let Some(next_item) = advance_counter(raw_item, item_count) else {
                return self.enter_terminal(
                    TerminalCause::CounterExhausted,
                    CleanupCause::CounterExhausted,
                );
            };
            (Some(raw_item), next_item)
        };
        let previous_bytes = self
            .active
            .as_ref()
            .and_then(|active| active.diagnostics.as_ref())
            .map_or(0, diagnostic_snapshot_retained_bytes);
        let projected_bytes = diagnostic_plan_retained_bytes(&plan);
        if self
            .accounted_state_bytes()
            .saturating_sub(previous_bytes)
            .checked_add(projected_bytes)
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        let mut items = Vec::with_capacity(item_count);
        for (offset, item) in plan.items.into_iter().enumerate() {
            let raw = raw_item.expect("nonempty diagnostic batch has an item seed")
                + u64::try_from(offset).expect("bounded diagnostic offset");
            let Some(id) = DiagnosticItemId::from_raw(raw) else {
                return self.enter_terminal(
                    TerminalCause::CounterExhausted,
                    CleanupCause::CounterExhausted,
                );
            };
            items.push(AnalysisDiagnostic {
                id,
                range: item.range,
                severity: match item.severity {
                    Some(1) => DiagnosticSeverity::Error,
                    Some(2) => DiagnosticSeverity::Warning,
                    Some(4) => DiagnosticSeverity::Hint,
                    _ => DiagnosticSeverity::Information,
                },
                phase: item.phase,
                code: item.code.map(Arc::from),
                source: item.source.map(Arc::from),
                message: Arc::from(item.message),
                local_limit: item.local_limit,
            });
        }
        self.next_diagnostic_revision = raw_revision.checked_add(1);
        self.next_diagnostic_item = next_item;
        self.active
            .as_mut()
            .expect("current generation")
            .diagnostics = Some(DiagnosticSnapshot {
            process_generation: fence.generation,
            uri: fence.uri,
            stamp: fence.stamp,
            lsp_version: fence.lsp_version,
            revision,
            items: items.into(),
        });
        self.snapshot_dirty = true;
        None
    }

    fn fence_is_written(&self, fence: &DocumentFence) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active
                .written
                .as_ref()
                .is_some_and(|written| &written.fence == fence)
        })
    }

    fn feature_notice(&mut self, message: &'static str) -> Option<ActorEffect> {
        self.add_notice(NoticeKind::RequestError, Arc::from(message))
    }

    fn add_notice(&mut self, kind: NoticeKind, message: Arc<str>) -> Option<ActorEffect> {
        self.active.as_ref()?;
        let prepared = match self.prepare_notice(kind, message) {
            Ok(prepared) => prepared,
            Err(()) => {
                return self.enter_terminal(
                    TerminalCause::CounterExhausted,
                    CleanupCause::CounterExhausted,
                );
            }
        };
        if self
            .accounted_state_bytes()
            .checked_add(prepared_notice_retained_bytes(&prepared))
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        self.commit_notice(prepared);
        None
    }

    fn prepare_notice(&self, kind: NoticeKind, message: Arc<str>) -> Result<PreparedNotice, ()> {
        let Some(active) = &self.active else {
            return Ok(PreparedNotice::NoChange);
        };
        if active.notices.len() >= MAX_NOTICES - 1 {
            if active
                .notices
                .iter()
                .any(|retained| retained.notice.id == NoticeId::limit())
            {
                return Ok(PreparedNotice::NoChange);
            }
            return Ok(PreparedNotice::Push {
                retained: RetainedNotice {
                    notice: LanguageNotice {
                        id: NoticeId::limit(),
                        kind: NoticeKind::Limit,
                        message: Arc::from("additional language notices were limited"),
                    },
                    first_published_revision: None,
                },
                next_notice: self.next_notice,
            });
        }
        let raw = self.next_notice.ok_or(())?;
        let id = NoticeId::from_raw(raw).ok_or(())?;
        let next_notice = raw
            .checked_add(1)
            .filter(|next| NoticeId::from_raw(*next).is_some());
        Ok(PreparedNotice::Push {
            retained: RetainedNotice {
                notice: LanguageNotice { id, kind, message },
                first_published_revision: None,
            },
            next_notice,
        })
    }

    fn commit_notice(&mut self, prepared: PreparedNotice) {
        let PreparedNotice::Push {
            retained,
            next_notice,
        } = prepared
        else {
            return;
        };
        self.next_notice = next_notice;
        self.active
            .as_mut()
            .expect("prepared notice requires an active generation")
            .notices
            .push(retained);
        self.snapshot_dirty = true;
    }

    fn caret_changed(
        &mut self,
        fence: UiDocumentFence,
        caret_generation: CaretGeneration,
    ) -> Option<ActorEffect> {
        if !self.ui_fence_matches_current(&fence) {
            return None;
        }
        let active = self.active.as_mut().expect("current UI fence");
        if active
            .last_caret_generation
            .is_some_and(|last| caret_generation <= last)
        {
            return None;
        }
        active.last_caret_generation = Some(caret_generation);
        active.definition_intent = None;
        active.definition = None;
        self.snapshot_dirty = true;
        None
    }

    fn definition_requested(
        &mut self,
        fence: UiDocumentFence,
        caret_generation: CaretGeneration,
        navigation_generation: NavigationGeneration,
        primary_character: usize,
    ) -> Option<ActorEffect> {
        if !self.ui_fence_matches_current(&fence) {
            return None;
        }
        let active = self.active.as_mut().expect("current UI fence");
        if !active.definition_enabled
            || active.generation != fence.process_generation
            || active.last_caret_generation != Some(caret_generation)
        {
            return None;
        }
        let Some(written) = &active.written else {
            return None;
        };
        if written.fence.stamp != fence.stamp
            || fence.lsp_version != Some(written.fence.lsp_version)
            || active
                .index
                .as_ref()
                .and_then(|index| index.index.position_for_character(primary_character))
                .is_none()
        {
            return None;
        }
        if active
            .last_navigation_generation
            .is_some_and(|last| navigation_generation <= last)
        {
            return None;
        }
        active.last_navigation_generation = Some(navigation_generation);
        active.definition = None;
        active.definition_intent = Some(DefinitionIntent {
            fence: written.fence.clone(),
            caret_generation,
            navigation_generation,
            primary_character,
        });
        self.snapshot_dirty = true;
        None
    }

    fn caret_generation_exhausted(&mut self, fence: UiDocumentFence) -> Option<ActorEffect> {
        if !self.ui_fence_matches_current(&fence) {
            return None;
        }
        let active = self.active.as_mut().expect("current UI fence");
        active.definition_enabled = false;
        active.definition_intent = None;
        active.definition = None;
        self.snapshot_dirty = true;
        None
    }

    fn ui_fence_matches_current(&self, fence: &UiDocumentFence) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        let Some(written) = &active.written else {
            return false;
        };
        active.generation == fence.process_generation
            && written.fence.stamp == fence.stamp
            && fence.lsp_version == Some(written.fence.lsp_version)
            && self.desired.as_ref().is_some_and(|desired| {
                desired.stamp == written.fence.stamp
                    && Arc::ptr_eq(&desired.source, &written.source)
            })
    }

    fn acknowledge_snapshot_items(
        &mut self,
        process_generation: ProcessGeneration,
        observed_revision: SnapshotRevision,
        definition: Option<DefinitionResultId>,
        notices: &[NoticeId],
    ) {
        if notices.len() > MAX_NOTICES {
            return;
        }
        let Some(active) = &mut self.active else {
            return;
        };
        if active.generation != process_generation {
            return;
        }
        let mut changed = false;
        if let Some(id) = definition
            && active.definition.as_ref().is_some_and(|retained| {
                retained.snapshot.id == id
                    && retained
                        .first_published_revision
                        .is_some_and(|published| observed_revision >= published)
            })
        {
            active.definition = None;
            changed = true;
        }
        active.notices.retain(|retained| {
            let acknowledged = notices.contains(&retained.notice.id)
                && retained
                    .first_published_revision
                    .is_some_and(|published| observed_revision >= published);
            changed |= acknowledged;
            !acknowledged
        });
        if changed {
            self.snapshot_dirty = true;
        }
    }

    fn stderr_tail_changed(
        &mut self,
        generation: ProcessGeneration,
        tail: BoundedStderrTail,
    ) -> Option<ActorEffect> {
        if !self.is_current_generation(generation) {
            return self.handle_noncurrent_generation(generation);
        }
        if !bounded_stderr_tail_is_valid(&tail) {
            return self.begin_cleanup(
                generation,
                CleanupMode::ForceTerminate,
                CleanupCause::AdapterInvariant,
            );
        }
        let previous_bytes = self
            .active
            .as_ref()
            .and_then(|active| active.stderr_tail.as_ref())
            .map_or(0, |previous| previous.text.len());
        if self
            .accounted_state_bytes()
            .saturating_sub(previous_bytes)
            .checked_add(tail.text.len())
            .is_none_or(|bytes| bytes > MAX_ACTOR_STATE_BYTES)
        {
            return self.enter_terminal(TerminalCause::ActorBudget, CleanupCause::ActorBudget);
        }
        self.active
            .as_mut()
            .expect("current generation")
            .stderr_tail = Some(tail);
        self.snapshot_dirty = true;
        None
    }
}

fn initialize_request(id: ClientRequestId) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id.get(),
        "method":"initialize",
        "params":{
            "processId":std::process::id(),
            "clientInfo":{"name":"Oxide","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{
                "general":{"positionEncodings":["utf-16"]},
                "textDocument":{
                    "synchronization":{"dynamicRegistration":false},
                    "definition":{"dynamicRegistration":false,"linkSupport":true},
                    "publishDiagnostics":{"versionSupport":true,"dataSupport":true},
                    "semanticTokens":{
                        "dynamicRegistration":false,
                        "requests":{"range":false,"full":true},
                        "tokenTypes":["keyword","comment","string","number","variable","operator"],
                        "tokenModifiers":[],
                        "formats":["relative"],
                        "overlappingTokenSupport":false,
                        "multilineTokenSupport":false,
                        "serverCancelSupport":false,
                        "augmentsSyntaxTokens":false
                    }
                }
            }
        }
    })
}

fn validate_initialize_result(result: Option<&Value>) -> Result<SemanticLegend, ()> {
    let result = result.and_then(Value::as_object).ok_or(())?;
    if let Some(server_info) = result.get("serverInfo") {
        let info = server_info.as_object().ok_or(())?;
        if info
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(|name| name.len() > 256)
            || info
                .get("version")
                .is_some_and(|version| version.as_str().is_none_or(|value| value.len() > 256))
        {
            return Err(());
        }
    }
    let capabilities = result
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or(())?;
    if capabilities.get("positionEncoding").and_then(Value::as_str) != Some("utf-16") {
        return Err(());
    }
    let sync = capabilities
        .get("textDocumentSync")
        .and_then(Value::as_object)
        .ok_or(())?;
    if sync.get("openClose").and_then(Value::as_bool) != Some(true)
        || sync.get("change").and_then(Value::as_u64) != Some(1)
    {
        return Err(());
    }
    match capabilities.get("definitionProvider") {
        Some(Value::Bool(true)) | Some(Value::Object(_)) => {}
        _ => return Err(()),
    }
    let semantic = capabilities
        .get("semanticTokensProvider")
        .and_then(Value::as_object)
        .ok_or(())?;
    match semantic.get("full") {
        Some(Value::Bool(true)) | Some(Value::Object(_)) => {}
        _ => return Err(()),
    }
    let legend = semantic
        .get("legend")
        .and_then(Value::as_object)
        .ok_or(())?;
    let types = legend
        .get("tokenTypes")
        .and_then(Value::as_array)
        .ok_or(())?
        .iter()
        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
        .collect::<Result<Vec<_>, _>>()?;
    let modifiers = legend
        .get("tokenModifiers")
        .and_then(Value::as_array)
        .ok_or(())?
        .iter()
        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
        .collect::<Result<Vec<_>, _>>()?;
    SemanticLegend::new(types, modifiers).map_err(|_| ())
}

fn rpc_id_value(id: &RpcId) -> Value {
    match id {
        RpcId::Number(value) => Value::from(*value),
        RpcId::String(value) => Value::from(value.clone()),
    }
}

fn decode_owned_frame(bytes: &[u8]) -> Option<Value> {
    let separator = bytes.windows(4).position(|window| window == b"\r\n\r\n")?;
    serde_json::from_slice(&bytes[separator + 4..]).ok()
}

fn advance_counter(raw: u64, count: usize) -> Option<Option<u64>> {
    if count == 0 {
        return Some(Some(raw));
    }
    let count = u64::try_from(count).ok()?;
    let last = raw.checked_add(count.checked_sub(1)?)?;
    if last == u64::MAX {
        Some(None)
    } else {
        last.checked_add(1).map(Some)
    }
}

fn validated_response_retained_bytes(response: &ValidatedResponse) -> usize {
    match response {
        ValidatedResponse::Initialize(_) => 64,
        ValidatedResponse::Semantic(runs) => runs
            .capacity()
            .saturating_mul(std::mem::size_of::<SyntaxRun>()),
        ValidatedResponse::Definition(targets) => targets
            .capacity()
            .saturating_mul(std::mem::size_of::<DefinitionTarget>()),
        ValidatedResponse::RpcError(message) | ValidatedResponse::InvalidForKind(message) => {
            message.len()
        }
        ValidatedResponse::ShutdownOk | ValidatedResponse::Stale => 0,
    }
}

fn diagnostic_plan_retained_bytes(plan: &DiagnosticPlan) -> usize {
    plan.items.iter().fold(
        plan.items
            .capacity()
            .saturating_mul(std::mem::size_of::<super::text_index::ValidatedDiagnostic>()),
        |total, item| {
            total
                .saturating_add(item.message.capacity())
                .saturating_add(item.code.as_ref().map_or(0, String::capacity))
                .saturating_add(item.source.as_ref().map_or(0, String::capacity))
        },
    )
}

fn diagnostic_snapshot_retained_bytes(snapshot: &DiagnosticSnapshot) -> usize {
    snapshot.items.iter().fold(
        snapshot
            .items
            .len()
            .saturating_mul(std::mem::size_of::<AnalysisDiagnostic>()),
        |total, item| {
            total
                .saturating_add(item.message.len())
                .saturating_add(item.code.as_ref().map_or(0, |value| value.len()))
                .saturating_add(item.source.as_ref().map_or(0, |value| value.len()))
        },
    )
}

fn flush_commit_retained_bytes(
    commit: &FlushCommit,
    desired: Option<&DurableDesiredDocument>,
    current_index: Option<&IndexedDocument>,
) -> usize {
    let (FlushCommit::DidOpen { document, index } | FlushCommit::DidChange { document, index }) =
        commit
    else {
        return 0;
    };
    let source_is_already_charged = desired
        .is_some_and(|desired| Arc::ptr_eq(&desired.source, &index.source))
        || current_index.is_some_and(|current| Arc::ptr_eq(&current.source, &index.source));
    let index_bytes = if source_is_already_charged {
        index
            .index
            .retained_bytes()
            .saturating_sub(index.source.len())
    } else {
        index.index.retained_bytes()
    };
    index_bytes.saturating_add(document.fence.uri.len())
}

fn prepared_notice_retained_bytes(prepared: &PreparedNotice) -> usize {
    match prepared {
        PreparedNotice::NoChange => 0,
        PreparedNotice::Push { retained, .. } => retained.notice.message.len(),
    }
}

fn bounded_stderr_tail_is_valid(tail: &BoundedStderrTail) -> bool {
    tail.text.len() <= MAX_STDERR_BYTES
        && tail.line_count <= MAX_STDERR_LINES
        && logical_line_count(tail.text.as_bytes()) == tail.line_count
}

fn stderr_tail_snapshot_is_valid(tail: &StderrTailSnapshot) -> bool {
    tail.text.len() <= MAX_STDERR_BYTES
        && tail.line_count <= MAX_STDERR_LINES
        && logical_line_count(tail.text.as_bytes()) == tail.line_count
}

fn logical_line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(bytes.last() != Some(&b'\n'))
}

#[cfg(test)]
fn request_kind_tag(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Initialize => "initialize",
        RequestKind::SemanticTokens => "semantic",
        RequestKind::Definition => "definition",
        RequestKind::Shutdown => "shutdown",
    }
}

#[cfg(test)]
fn frame_kind_tag(kind: &FrameKind) -> &'static str {
    match kind {
        FrameKind::ClientRequest { kind, .. } => request_kind_tag(*kind),
        FrameKind::Initialized => "initialized",
        FrameKind::DidClose { .. } => "did-close",
        FrameKind::DidOpen { .. } => "did-open",
        FrameKind::DidChange { .. } => "did-change",
        FrameKind::ServerResponse { .. } => "server-response",
        FrameKind::Exit => "exit",
    }
}

fn snapshot_matches_actor_draft(
    snapshot: &LanguageSnapshot,
    draft: &LanguageSnapshotDraft,
) -> bool {
    if snapshot.revision() != draft.revision
        || snapshot.process_generation() != draft.process_generation
        || snapshot.desired_document() != draft.desired_document.as_ref()
        || snapshot.written_document() != draft.written_document.as_ref()
        || snapshot.stderr_tail() != draft.stderr_tail.as_ref()
        || snapshot.writer() != draft.writer
        || snapshot.estimated_bytes() > super::snapshot::MAX_SNAPSHOT_BYTES
    {
        return false;
    }
    if snapshot.status() == LanguageStatus::Limited {
        return snapshot.diagnostics().is_none()
            && snapshot.syntax().is_none()
            && snapshot.definition().is_none()
            && snapshot.stderr_tail() == draft.stderr_tail.as_ref()
            && snapshot.notices()
                == [LanguageNotice {
                    id: NoticeId::limit(),
                    kind: NoticeKind::Limit,
                    message: Arc::from("language results exceed the display budget"),
                }];
    }
    snapshot.status() == draft.status
        && snapshot.diagnostics() == draft.diagnostics.as_ref()
        && snapshot.syntax() == draft.syntax.as_ref()
        && snapshot.definition() == draft.definition.as_ref()
        && snapshot.notices() == draft.notices.as_slice()
}

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

use arc_swap::ArcSwap;

use crate::{DocumentId, DocumentStamp, NavigationGeneration};

use super::text_index::{SyntaxRun, TextRange};

pub(crate) const MAX_SNAPSHOT_BYTES: usize = 512 * 1024;
pub(crate) const MAX_STDERR_BYTES: usize = 64 * 1024;
pub(crate) const MAX_STDERR_LINES: usize = 512;
const MAX_NOTICE_BYTES: usize = 4 * 1024;
const MAX_NOTICES: usize = 32;
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_DIAGNOSTIC_CODE_BYTES: usize = 256;
const MAX_DIAGNOSTICS: usize = 129;
const MAX_SYNTAX_RUNS: usize = 4_096;
const MAX_DEFINITION_TARGETS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CounterExhausted;

macro_rules! checked_u64_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name(u64);

        impl $name {
            pub(crate) fn from_raw(value: u64) -> Option<Self> {
                (value != 0).then_some(Self(value))
            }

            pub(crate) fn get(self) -> u64 {
                self.0
            }

            pub(crate) fn checked_next(self) -> Result<Self, CounterExhausted> {
                self.0
                    .checked_add(1)
                    .and_then(Self::from_raw)
                    .ok_or(CounterExhausted)
            }
        }
    };
}

checked_u64_id!(ProcessGeneration);
checked_u64_id!(CaretGeneration);
checked_u64_id!(DiagnosticSetRevision);
checked_u64_id!(WriteSequence);
checked_u64_id!(DiagnosticItemId);
checked_u64_id!(DefinitionResultId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SnapshotRevision(u64);

impl SnapshotRevision {
    pub(crate) fn from_raw(value: u64) -> Option<Self> {
        (value != 0 && value != u64::MAX).then_some(Self(value))
    }

    pub(crate) fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Result<Self, CounterExhausted> {
        self.0
            .checked_add(1)
            .and_then(Self::from_raw)
            .ok_or(CounterExhausted)
    }

    pub(crate) fn terminal() -> Self {
        Self(u64::MAX)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct NoticeId(u64);

impl NoticeId {
    pub(crate) fn from_raw(value: u64) -> Option<Self> {
        (value != 0 && value != u64::MAX).then_some(Self(value))
    }

    pub(crate) fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Result<Self, CounterExhausted> {
        self.0
            .checked_add(1)
            .and_then(Self::from_raw)
            .ok_or(CounterExhausted)
    }

    pub(crate) fn limit() -> Self {
        Self(u64::MAX)
    }
}

macro_rules! checked_i32_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name(i32);

        impl $name {
            pub(crate) fn from_raw(value: i32) -> Option<Self> {
                (value > 0).then_some(Self(value))
            }

            pub(crate) fn get(self) -> i32 {
                self.0
            }

            pub(crate) fn checked_next(self) -> Result<Self, CounterExhausted> {
                self.0
                    .checked_add(1)
                    .and_then(Self::from_raw)
                    .ok_or(CounterExhausted)
            }
        }
    };
}

checked_i32_id!(LspVersion);
checked_i32_id!(ClientRequestId);

pub(crate) fn synthetic_uri(document: DocumentId) -> String {
    format!("oxide-document://local/{}.ox", document.get())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LanguageStatus {
    Starting,
    Initializing,
    Ready,
    Unavailable,
    ShuttingDown,
    Disabled,
    Limited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriterState {
    Idle,
    Writing { sequence: WriteSequence },
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnalysisPhase {
    Scanner,
    Parser,
    Compiler,
    Runtime,
    Worker,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AnalysisDiagnostic {
    pub(crate) id: DiagnosticItemId,
    pub(crate) range: Option<TextRange>,
    pub(crate) severity: DiagnosticSeverity,
    pub(crate) phase: Option<AnalysisPhase>,
    pub(crate) code: Option<Arc<str>>,
    pub(crate) source: Option<Arc<str>>,
    pub(crate) message: Arc<str>,
    pub(crate) local_limit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocumentSyncSnapshot {
    pub(crate) stamp: DocumentStamp,
    pub(crate) uri: Arc<str>,
    pub(crate) lsp_version: Option<LspVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticSnapshot {
    pub(crate) process_generation: ProcessGeneration,
    pub(crate) uri: Arc<str>,
    pub(crate) stamp: DocumentStamp,
    pub(crate) lsp_version: LspVersion,
    pub(crate) revision: DiagnosticSetRevision,
    pub(crate) items: Arc<[AnalysisDiagnostic]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyntaxSnapshot {
    pub(crate) process_generation: ProcessGeneration,
    pub(crate) uri: Arc<str>,
    pub(crate) stamp: DocumentStamp,
    pub(crate) lsp_version: LspVersion,
    pub(crate) runs: Arc<[SyntaxRun]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefinitionTargetSnapshot {
    pub(crate) range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefinitionSnapshot {
    pub(crate) id: DefinitionResultId,
    pub(crate) process_generation: ProcessGeneration,
    pub(crate) uri: Arc<str>,
    pub(crate) stamp: DocumentStamp,
    pub(crate) lsp_version: LspVersion,
    pub(crate) caret_generation: CaretGeneration,
    pub(crate) navigation_generation: NavigationGeneration,
    pub(crate) caret_character: usize,
    pub(crate) targets: Arc<[DefinitionTargetSnapshot]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeKind {
    Information,
    RequestError,
    Limit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanguageNotice {
    pub(crate) id: NoticeId,
    pub(crate) kind: NoticeKind,
    pub(crate) message: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StderrTailSnapshot {
    pub(crate) text: Arc<str>,
    pub(crate) line_count: usize,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanguageSnapshotDraft {
    pub(crate) revision: SnapshotRevision,
    pub(crate) process_generation: Option<ProcessGeneration>,
    pub(crate) status: LanguageStatus,
    pub(crate) desired_document: Option<DocumentSyncSnapshot>,
    pub(crate) written_document: Option<DocumentSyncSnapshot>,
    pub(crate) diagnostics: Option<DiagnosticSnapshot>,
    pub(crate) syntax: Option<SyntaxSnapshot>,
    pub(crate) definition: Option<DefinitionSnapshot>,
    pub(crate) notices: Vec<LanguageNotice>,
    pub(crate) stderr_tail: Option<StderrTailSnapshot>,
    pub(crate) writer: WriterState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanguageSnapshot {
    revision: SnapshotRevision,
    process_generation: Option<ProcessGeneration>,
    status: LanguageStatus,
    desired_document: Option<DocumentSyncSnapshot>,
    written_document: Option<DocumentSyncSnapshot>,
    diagnostics: Option<DiagnosticSnapshot>,
    syntax: Option<SyntaxSnapshot>,
    definition: Option<DefinitionSnapshot>,
    notices: Arc<[LanguageNotice]>,
    stderr_tail: Option<StderrTailSnapshot>,
    writer: WriterState,
    estimated_bytes: usize,
}

impl LanguageSnapshot {
    pub(crate) fn bounded(draft: LanguageSnapshotDraft) -> Self {
        let estimated_bytes = estimate_draft(&draft);
        if draft_is_bounded(&draft) && estimated_bytes <= MAX_SNAPSHOT_BYTES {
            return Self {
                revision: draft.revision,
                process_generation: draft.process_generation,
                status: draft.status,
                desired_document: draft.desired_document,
                written_document: draft.written_document,
                diagnostics: draft.diagnostics,
                syntax: draft.syntax,
                definition: draft.definition,
                notices: draft.notices.into(),
                stderr_tail: draft.stderr_tail,
                writer: draft.writer,
                estimated_bytes,
            };
        }

        let notice = LanguageNotice {
            id: NoticeId::limit(),
            kind: NoticeKind::Limit,
            message: Arc::from("language results exceed the display budget"),
        };
        let desired_document = draft.desired_document.filter(sync_document_is_bounded);
        let written_document = draft.written_document.filter(|document| {
            draft.process_generation.is_some()
                && document.lsp_version.is_some()
                && sync_document_is_bounded(document)
        });
        let stderr_tail = draft
            .stderr_tail
            .filter(|tail| draft.process_generation.is_some() && stderr_tail_is_bounded(tail));
        let fallback_bytes = 256
            + notice.message.len()
            + desired_document
                .as_ref()
                .map_or(0, |document| 64 + document.uri.len())
            + written_document
                .as_ref()
                .map_or(0, |document| 64 + document.uri.len())
            + stderr_tail.as_ref().map_or(0, |tail| 64 + tail.text.len());
        Self {
            revision: draft.revision,
            process_generation: draft.process_generation,
            status: LanguageStatus::Limited,
            desired_document,
            written_document,
            diagnostics: None,
            syntax: None,
            definition: None,
            notices: Arc::from([notice]),
            stderr_tail,
            writer: draft.writer,
            estimated_bytes: fallback_bytes,
        }
    }

    pub(crate) fn revision(&self) -> SnapshotRevision {
        self.revision
    }

    pub(crate) fn process_generation(&self) -> Option<ProcessGeneration> {
        self.process_generation
    }

    pub(crate) fn status(&self) -> LanguageStatus {
        self.status
    }

    pub(crate) fn desired_stamp(&self) -> Option<DocumentStamp> {
        self.desired_document
            .as_ref()
            .map(|document| document.stamp)
    }

    pub(crate) fn written_stamp(&self) -> Option<DocumentStamp> {
        self.written_document
            .as_ref()
            .map(|document| document.stamp)
    }

    pub(crate) fn desired_document(&self) -> Option<&DocumentSyncSnapshot> {
        self.desired_document.as_ref()
    }

    pub(crate) fn written_document(&self) -> Option<&DocumentSyncSnapshot> {
        self.written_document.as_ref()
    }

    pub(crate) fn diagnostics(&self) -> Option<&DiagnosticSnapshot> {
        self.diagnostics.as_ref()
    }

    pub(crate) fn syntax(&self) -> Option<&SyntaxSnapshot> {
        self.syntax.as_ref()
    }

    pub(crate) fn definition(&self) -> Option<&DefinitionSnapshot> {
        self.definition.as_ref()
    }

    pub(crate) fn notices(&self) -> &[LanguageNotice] {
        &self.notices
    }

    pub(crate) fn stderr_tail(&self) -> Option<&StderrTailSnapshot> {
        self.stderr_tail.as_ref()
    }

    pub(crate) fn writer(&self) -> WriterState {
        self.writer
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }
}

fn draft_is_bounded(draft: &LanguageSnapshotDraft) -> bool {
    if draft
        .desired_document
        .as_ref()
        .is_some_and(|document| !sync_document_is_bounded(document))
        || draft
            .written_document
            .as_ref()
            .is_some_and(|document| !sync_document_is_bounded(document))
    {
        return false;
    }
    if draft
        .written_document
        .as_ref()
        .is_some_and(|document| document.lsp_version.is_none())
    {
        return false;
    }
    let artifact_matches_written =
        |generation: ProcessGeneration, uri: &str, stamp: DocumentStamp, version: LspVersion| {
            draft.process_generation == Some(generation)
                && uri == synthetic_uri(stamp.document_id)
                && draft.written_document.as_ref().is_some_and(|written| {
                    written.stamp == stamp
                        && written.uri.as_ref() == uri
                        && written.lsp_version == Some(version)
                })
        };
    if draft.notices.len() > MAX_NOTICES
        || draft
            .notices
            .iter()
            .any(|notice| notice.message.len() > MAX_NOTICE_BYTES)
    {
        return false;
    }
    if draft
        .stderr_tail
        .as_ref()
        .is_some_and(|tail| draft.process_generation.is_none() || !stderr_tail_is_bounded(tail))
    {
        return false;
    }
    if let Some(diagnostics) = &draft.diagnostics
        && (!artifact_matches_written(
            diagnostics.process_generation,
            &diagnostics.uri,
            diagnostics.stamp,
            diagnostics.lsp_version,
        ) || diagnostics.uri.len() > 4 * 1024
            || diagnostics.items.len() > MAX_DIAGNOSTICS
            || diagnostics.items.iter().any(|item| {
                item.message.len() > MAX_DIAGNOSTIC_MESSAGE_BYTES
                    || item
                        .code
                        .as_ref()
                        .is_some_and(|value| value.len() > MAX_DIAGNOSTIC_CODE_BYTES)
                    || item
                        .source
                        .as_ref()
                        .is_some_and(|value| value.len() > MAX_DIAGNOSTIC_CODE_BYTES)
            }))
    {
        return false;
    }
    if draft.syntax.as_ref().is_some_and(|syntax| {
        !artifact_matches_written(
            syntax.process_generation,
            &syntax.uri,
            syntax.stamp,
            syntax.lsp_version,
        ) || syntax.uri.len() > 4 * 1024
            || syntax.runs.len() > MAX_SYNTAX_RUNS
    }) || draft.definition.as_ref().is_some_and(|definition| {
        !artifact_matches_written(
            definition.process_generation,
            &definition.uri,
            definition.stamp,
            definition.lsp_version,
        ) || definition.uri.len() > 4 * 1024
            || definition.targets.len() > MAX_DEFINITION_TARGETS
    }) {
        return false;
    }
    true
}

pub(crate) fn draft_fields_are_valid(draft: &LanguageSnapshotDraft) -> bool {
    draft_is_bounded(draft)
}

fn sync_document_is_bounded(document: &DocumentSyncSnapshot) -> bool {
    document.uri.len() <= 4 * 1024
        && document.uri.as_ref() == synthetic_uri(document.stamp.document_id)
}

fn stderr_tail_is_bounded(tail: &StderrTailSnapshot) -> bool {
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

fn estimate_draft(draft: &LanguageSnapshotDraft) -> usize {
    let mut bytes = 256usize;
    bytes = bytes
        .saturating_add(
            draft
                .desired_document
                .as_ref()
                .map_or(0, |document| 64 + document.uri.len()),
        )
        .saturating_add(
            draft
                .written_document
                .as_ref()
                .map_or(0, |document| 64 + document.uri.len()),
        );
    if let Some(diagnostics) = &draft.diagnostics {
        bytes = bytes.saturating_add(128 + diagnostics.uri.len());
        for item in diagnostics.items.iter() {
            bytes = bytes
                .saturating_add(128)
                .saturating_add(item.message.len())
                .saturating_add(item.code.as_ref().map_or(0, |value| value.len()))
                .saturating_add(item.source.as_ref().map_or(0, |value| value.len()));
        }
    }
    if let Some(syntax) = &draft.syntax {
        bytes = bytes.saturating_add(128 + syntax.uri.len() + syntax.runs.len().saturating_mul(64));
    }
    if let Some(definition) = &draft.definition {
        bytes = bytes.saturating_add(
            128 + definition.uri.len() + definition.targets.len().saturating_mul(64),
        );
    }
    for notice in &draft.notices {
        bytes = bytes.saturating_add(64 + notice.message.len());
    }
    if let Some(tail) = &draft.stderr_tail {
        bytes = bytes.saturating_add(64 + tail.text.len());
    }
    bytes
}

struct SnapshotShared {
    latest: ArcSwap<LanguageSnapshot>,
    published_revision: AtomicU64,
    dirty: AtomicBool,
    wake_observed: AtomicBool,
}

pub(crate) struct SnapshotPublisher {
    shared: Arc<SnapshotShared>,
    wake: mpsc::SyncSender<()>,
}

pub(crate) struct SnapshotSubscriber {
    shared: Arc<SnapshotShared>,
    wake: mpsc::Receiver<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublishError {
    StaleRevision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoadPhase {
    Armed,
    WakeDrained,
    RevisionRead,
    SnapshotLoaded,
    DirtyRechecked,
    Returning,
}

pub(crate) fn snapshot_channel(
    initial: LanguageSnapshot,
) -> (SnapshotPublisher, SnapshotSubscriber) {
    let revision = initial.revision().get();
    let shared = Arc::new(SnapshotShared {
        latest: ArcSwap::from_pointee(initial),
        published_revision: AtomicU64::new(revision),
        dirty: AtomicBool::new(false),
        wake_observed: AtomicBool::new(false),
    });
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    (
        SnapshotPublisher {
            shared: shared.clone(),
            wake: wake_tx,
        },
        SnapshotSubscriber {
            shared,
            wake: wake_rx,
        },
    )
}

impl SnapshotPublisher {
    pub(crate) fn publish(&self, snapshot: LanguageSnapshot) -> Result<(), PublishError> {
        let next = snapshot.revision().get();
        let current = self.shared.published_revision.load(Ordering::Acquire);
        if next <= current {
            return Err(PublishError::StaleRevision);
        }
        self.shared.latest.store(Arc::new(snapshot));
        self.shared
            .published_revision
            .store(next, Ordering::Release);
        self.shared.dirty.store(true, Ordering::Release);
        match self.wake.try_send(()) {
            Ok(()) => self.shared.wake_observed.store(true, Ordering::Release),
            Err(mpsc::TrySendError::Full(())) | Err(mpsc::TrySendError::Disconnected(())) => {}
        }
        Ok(())
    }
}

impl SnapshotSubscriber {
    pub(crate) fn load_stable(&self) -> Arc<LanguageSnapshot> {
        self.load_stable_with_hook(|_| {})
    }

    pub(crate) fn load_stable_with_hook<F>(&self, mut hook: F) -> Arc<LanguageSnapshot>
    where
        F: FnMut(LoadPhase),
    {
        loop {
            self.shared.dirty.store(false, Ordering::Release);
            hook(LoadPhase::Armed);
            self.drain_wake();
            hook(LoadPhase::WakeDrained);
            let revision_before = self.shared.published_revision.load(Ordering::Acquire);
            hook(LoadPhase::RevisionRead);
            let snapshot = self.shared.latest.load_full();
            hook(LoadPhase::SnapshotLoaded);
            let revision_after = self.shared.published_revision.load(Ordering::Acquire);
            let dirty = self.shared.dirty.load(Ordering::Acquire);
            hook(LoadPhase::DirtyRechecked);
            if revision_before == revision_after
                && snapshot.revision().get() == revision_after
                && !dirty
                && !self.shared.dirty.load(Ordering::Acquire)
            {
                hook(LoadPhase::Returning);
                return snapshot;
            }
        }
    }

    pub(crate) fn drain_wake(&self) -> usize {
        let mut drained = 0;
        while self.wake.try_recv().is_ok() {
            drained += 1;
        }
        if drained > 0 {
            self.shared.wake_observed.store(false, Ordering::Release);
        }
        drained
    }

    pub(crate) fn pending_wakes(&self) -> usize {
        usize::from(self.shared.wake_observed.load(Ordering::Acquire))
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{DocumentId, DocumentStamp, EditRevision};

    use super::*;
    use crate::language::text_index::{SyntaxKind, SyntaxRun, TextRange};

    fn stamp(document: u64, edit: u64) -> DocumentStamp {
        DocumentStamp {
            document_id: DocumentId::from_raw(document).expect("document ID"),
            edit_revision: EditRevision::from_raw(edit).expect("edit revision"),
        }
    }

    fn desired(document_stamp: DocumentStamp) -> DocumentSyncSnapshot {
        DocumentSyncSnapshot {
            stamp: document_stamp,
            uri: Arc::from(synthetic_uri(document_stamp.document_id)),
            lsp_version: None,
        }
    }

    fn written(document_stamp: DocumentStamp, version: LspVersion) -> DocumentSyncSnapshot {
        DocumentSyncSnapshot {
            stamp: document_stamp,
            uri: Arc::from(synthetic_uri(document_stamp.document_id)),
            lsp_version: Some(version),
        }
    }

    fn snapshot(revision: u64) -> LanguageSnapshot {
        LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(revision).expect("snapshot revision"),
            process_generation: Some(ProcessGeneration::from_raw(1).expect("generation")),
            status: LanguageStatus::Ready,
            desired_document: Some(desired(stamp(7, revision))),
            written_document: Some(written(
                stamp(7, revision),
                LspVersion::from_raw(1).expect("version"),
            )),
            diagnostics: None,
            syntax: None,
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            writer: WriterState::Idle,
        })
    }

    #[test]
    fn checked_ids_are_nonzero_and_never_wrap_or_reuse() {
        assert!(ProcessGeneration::from_raw(0).is_none());
        let last = ProcessGeneration::from_raw(u64::MAX).expect("last generation");
        assert_eq!(last.checked_next(), Err(CounterExhausted));

        assert!(LspVersion::from_raw(0).is_none());
        let last_version = LspVersion::from_raw(i32::MAX).expect("last version");
        assert_eq!(last_version.checked_next(), Err(CounterExhausted));

        assert!(ClientRequestId::from_raw(-1).is_none());
        assert!(ClientRequestId::from_raw(0).is_none());
        let last_request = ClientRequestId::from_raw(i32::MAX).expect("last request");
        assert_eq!(last_request.checked_next(), Err(CounterExhausted));

        assert_eq!(
            CaretGeneration::from_raw(u64::MAX)
                .expect("last caret")
                .checked_next(),
            Err(CounterExhausted)
        );
        assert_eq!(
            DiagnosticSetRevision::from_raw(u64::MAX)
                .expect("last diagnostics")
                .checked_next(),
            Err(CounterExhausted)
        );
        assert_eq!(
            NavigationGeneration::from_raw(u64::MAX)
                .expect("last navigation")
                .checked_next(),
            None
        );
        assert!(SnapshotRevision::from_raw(u64::MAX).is_none());
        assert_eq!(
            SnapshotRevision::from_raw(u64::MAX - 1)
                .expect("last ordinary snapshot")
                .checked_next(),
            Err(CounterExhausted)
        );
        assert_eq!(SnapshotRevision::terminal().get(), u64::MAX);
        assert_eq!(
            WriteSequence::from_raw(u64::MAX)
                .expect("last write")
                .checked_next(),
            Err(CounterExhausted)
        );
        assert!(NoticeId::from_raw(u64::MAX).is_none());
        assert_eq!(
            NoticeId::from_raw(u64::MAX - 1)
                .expect("last allocatable notice")
                .checked_next(),
            Err(CounterExhausted)
        );
    }

    #[test]
    fn synthetic_uri_is_stable_for_document_identity_and_never_uses_a_path() {
        let document = DocumentId::from_raw(42).expect("document");
        let first = synthetic_uri(document);
        let after_save_as = synthetic_uri(document);
        assert_eq!(first, "oxide-document://local/42.ox");
        assert_eq!(first, after_save_as);
        assert!(!first.contains('\\'));
        assert!(!first.starts_with("file:"));
        assert_ne!(
            first,
            synthetic_uri(DocumentId::from_raw(43).expect("new document"))
        );
    }

    #[test]
    fn immutable_snapshot_keeps_all_artifacts_fenced_to_exact_stamps() {
        let document_stamp = stamp(7, 3);
        let generation = ProcessGeneration::from_raw(2).expect("generation");
        let version = LspVersion::from_raw(4).expect("version");
        let diagnostic_revision = DiagnosticSetRevision::from_raw(5).expect("batch");
        let range = TextRange {
            bytes: 1..2,
            characters: 1..2,
        };
        let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(9).expect("revision"),
            process_generation: Some(generation),
            status: LanguageStatus::Ready,
            desired_document: Some(desired(document_stamp)),
            written_document: Some(written(document_stamp, version)),
            diagnostics: Some(DiagnosticSnapshot {
                process_generation: generation,
                uri: Arc::from(synthetic_uri(document_stamp.document_id)),
                stamp: document_stamp,
                lsp_version: version,
                revision: diagnostic_revision,
                items: Arc::from([AnalysisDiagnostic {
                    id: DiagnosticItemId::from_raw(1).expect("item"),
                    range: Some(range.clone()),
                    severity: DiagnosticSeverity::Error,
                    phase: Some(AnalysisPhase::Parser),
                    code: Some(Arc::from("E.TEST")),
                    source: Some(Arc::from("parser")),
                    message: Arc::from("expected expression"),
                    local_limit: false,
                }]),
            }),
            syntax: Some(SyntaxSnapshot {
                process_generation: generation,
                uri: Arc::from(synthetic_uri(document_stamp.document_id)),
                stamp: document_stamp,
                lsp_version: version,
                runs: Arc::from([SyntaxRun {
                    range: range.clone(),
                    kind: SyntaxKind::Keyword,
                }]),
            }),
            definition: Some(DefinitionSnapshot {
                id: DefinitionResultId::from_raw(1).expect("definition result"),
                process_generation: generation,
                uri: Arc::from(synthetic_uri(document_stamp.document_id)),
                stamp: document_stamp,
                lsp_version: version,
                caret_generation: CaretGeneration::from_raw(7).expect("caret"),
                navigation_generation: NavigationGeneration::from_raw(11).expect("navigation"),
                caret_character: 1,
                targets: Arc::from([DefinitionTargetSnapshot { range }]),
            }),
            notices: vec![LanguageNotice {
                id: NoticeId::from_raw(1).expect("notice"),
                kind: NoticeKind::Information,
                message: Arc::from("two definitions found"),
            }],
            stderr_tail: None,
            writer: WriterState::Writing {
                sequence: WriteSequence::from_raw(3).expect("write sequence"),
            },
        });

        assert_eq!(snapshot.process_generation(), Some(generation));
        assert_eq!(snapshot.desired_stamp(), Some(document_stamp));
        assert_eq!(snapshot.written_stamp(), Some(document_stamp));
        assert_eq!(
            snapshot.desired_document().expect("desired").uri.as_ref(),
            "oxide-document://local/7.ox"
        );
        assert_eq!(
            snapshot.written_document().expect("written").lsp_version,
            Some(version)
        );
        assert_eq!(
            snapshot.diagnostics().expect("diagnostics").stamp,
            document_stamp
        );
        assert_eq!(snapshot.syntax().expect("syntax").stamp, document_stamp);
        assert_eq!(
            snapshot.definition().expect("definition").stamp,
            document_stamp
        );
        assert_eq!(
            snapshot
                .definition()
                .expect("definition")
                .navigation_generation,
            NavigationGeneration::from_raw(11).expect("navigation")
        );
        assert_eq!(
            snapshot.definition().expect("definition").caret_character,
            1
        );
        assert_eq!(
            snapshot.diagnostics().expect("diagnostics").items[0].phase,
            Some(AnalysisPhase::Parser)
        );
        assert!(snapshot.estimated_bytes() <= MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn aggregate_budget_exhaustion_fails_closed_to_one_bounded_notice() {
        let generation = ProcessGeneration::from_raw(1).expect("generation");
        let document_stamp = stamp(1, 1);
        let diagnostic = AnalysisDiagnostic {
            id: DiagnosticItemId::from_raw(1).expect("item"),
            range: Some(TextRange {
                bytes: 0..1,
                characters: 0..1,
            }),
            severity: DiagnosticSeverity::Error,
            phase: None,
            code: None,
            source: None,
            message: Arc::from("x".repeat(4 * 1024)),
            local_limit: false,
        };
        let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(2).expect("revision"),
            process_generation: Some(generation),
            status: LanguageStatus::Ready,
            desired_document: Some(desired(document_stamp)),
            written_document: Some(written(
                document_stamp,
                LspVersion::from_raw(1).expect("version"),
            )),
            diagnostics: Some(DiagnosticSnapshot {
                process_generation: generation,
                uri: Arc::from(synthetic_uri(document_stamp.document_id)),
                stamp: document_stamp,
                lsp_version: LspVersion::from_raw(1).expect("version"),
                revision: DiagnosticSetRevision::from_raw(1).expect("batch"),
                items: vec![diagnostic; 128].into(),
            }),
            syntax: None,
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            writer: WriterState::Idle,
        });

        assert_eq!(snapshot.status(), LanguageStatus::Limited);
        assert!(snapshot.diagnostics().is_none());
        assert!(snapshot.syntax().is_none());
        assert!(snapshot.definition().is_none());
        assert_eq!(snapshot.notices().len(), 1);
        assert!(snapshot.estimated_bytes() <= MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn snapshot_aggregate_budget_accepts_exact_n_and_limits_n_plus_one_stably() {
        fn draft_with_estimate(target: usize) -> LanguageSnapshotDraft {
            let generation = ProcessGeneration::from_raw(1).expect("generation");
            let document_stamp = stamp(1, 1);
            let version = LspVersion::from_raw(1).expect("version");
            let mut diagnostics = (0..MAX_DIAGNOSTICS)
                .map(|item| AnalysisDiagnostic {
                    id: DiagnosticItemId::from_raw(u64::try_from(item + 1).expect("item ID"))
                        .expect("item"),
                    range: Some(TextRange {
                        bytes: 0..1,
                        characters: 0..1,
                    }),
                    severity: DiagnosticSeverity::Error,
                    phase: None,
                    code: None,
                    source: None,
                    message: Arc::from(""),
                    local_limit: false,
                })
                .collect::<Vec<_>>();
            let mut draft = LanguageSnapshotDraft {
                revision: SnapshotRevision::from_raw(2).expect("revision"),
                process_generation: Some(generation),
                status: LanguageStatus::Ready,
                desired_document: Some(desired(document_stamp)),
                written_document: Some(written(document_stamp, version)),
                diagnostics: Some(DiagnosticSnapshot {
                    process_generation: generation,
                    uri: Arc::from(synthetic_uri(document_stamp.document_id)),
                    stamp: document_stamp,
                    lsp_version: version,
                    revision: DiagnosticSetRevision::from_raw(1).expect("batch"),
                    items: diagnostics.clone().into(),
                }),
                syntax: None,
                definition: None,
                notices: Vec::new(),
                stderr_tail: None,
                writer: WriterState::Idle,
            };
            let fixed = estimate_draft(&draft);
            let mut remaining = target.checked_sub(fixed).expect("target fits fixed draft");
            for diagnostic in &mut diagnostics {
                let bytes = remaining.min(MAX_DIAGNOSTIC_MESSAGE_BYTES);
                diagnostic.message = Arc::from("x".repeat(bytes));
                remaining -= bytes;
            }
            assert_eq!(remaining, 0, "diagnostic strings can fill the target");
            draft.diagnostics.as_mut().expect("diagnostics").items = diagnostics.into();
            assert_eq!(estimate_draft(&draft), target);
            draft
        }

        let exact = LanguageSnapshot::bounded(draft_with_estimate(MAX_SNAPSHOT_BYTES));
        assert_eq!(exact.status(), LanguageStatus::Ready);
        assert_eq!(exact.estimated_bytes(), MAX_SNAPSHOT_BYTES);
        assert!(exact.diagnostics().is_some());

        let oversized_draft = draft_with_estimate(MAX_SNAPSHOT_BYTES + 1);
        let first = LanguageSnapshot::bounded(oversized_draft.clone());
        let second = LanguageSnapshot::bounded(oversized_draft);
        assert_eq!(first, second);
        assert_eq!(first.status(), LanguageStatus::Limited);
        assert!(first.diagnostics().is_none());
        assert_eq!(first.notices().len(), 1);
        assert!(first.estimated_bytes() <= MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn matching_empty_diagnostic_batch_remains_distinct_from_no_batch() {
        let generation = ProcessGeneration::from_raw(1).expect("generation");
        let document_stamp = stamp(7, 3);
        let version = LspVersion::from_raw(4).expect("version");
        let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(9).expect("revision"),
            process_generation: Some(generation),
            status: LanguageStatus::Ready,
            desired_document: Some(desired(document_stamp)),
            written_document: Some(written(document_stamp, version)),
            diagnostics: Some(DiagnosticSnapshot {
                process_generation: generation,
                uri: Arc::from(synthetic_uri(document_stamp.document_id)),
                stamp: document_stamp,
                lsp_version: version,
                revision: DiagnosticSetRevision::from_raw(5).expect("batch"),
                items: Arc::from([]),
            }),
            syntax: None,
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            writer: WriterState::Idle,
        });

        let diagnostics = snapshot.diagnostics().expect("matching empty batch");
        assert!(diagnostics.items.is_empty());
        assert_eq!(diagnostics.revision.get(), 5);
    }

    #[test]
    fn snapshot_rejects_a_uri_that_disagrees_with_its_document_stamp() {
        let document_stamp = stamp(1, 1);
        let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(2).expect("revision"),
            process_generation: Some(ProcessGeneration::from_raw(1).expect("generation")),
            status: LanguageStatus::Ready,
            desired_document: Some(DocumentSyncSnapshot {
                stamp: document_stamp,
                uri: Arc::from("oxide-document://local/2.ox"),
                lsp_version: None,
            }),
            written_document: None,
            diagnostics: None,
            syntax: None,
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            writer: WriterState::Idle,
        });

        assert_eq!(snapshot.status(), LanguageStatus::Limited);
        assert_eq!(snapshot.notices().len(), 1);
        assert!(snapshot.desired_document().is_none());
        assert!(snapshot.written_document().is_none());
        assert!(snapshot.estimated_bytes() <= MAX_SNAPSHOT_BYTES);

        let oversized = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(3).expect("revision"),
            process_generation: None,
            status: LanguageStatus::Unavailable,
            desired_document: Some(DocumentSyncSnapshot {
                stamp: document_stamp,
                uri: Arc::from("x".repeat(4 * 1024 + 1)),
                lsp_version: None,
            }),
            written_document: None,
            diagnostics: None,
            syntax: None,
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            writer: WriterState::Idle,
        });
        assert_eq!(oversized.status(), LanguageStatus::Limited);
        assert!(oversized.desired_document().is_none());
        assert!(oversized.estimated_bytes() <= MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn maximum_standalone_semantic_plan_fits_the_snapshot_budget() {
        let generation = ProcessGeneration::from_raw(1).expect("generation");
        let document_stamp = stamp(1, 1);
        let runs: Vec<_> = (0..4_096)
            .map(|offset| SyntaxRun {
                range: TextRange {
                    bytes: offset..offset + 1,
                    characters: offset..offset + 1,
                },
                kind: SyntaxKind::Variable,
            })
            .collect();
        let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(2).expect("revision"),
            process_generation: Some(generation),
            status: LanguageStatus::Ready,
            desired_document: Some(desired(document_stamp)),
            written_document: Some(written(
                document_stamp,
                LspVersion::from_raw(1).expect("version"),
            )),
            diagnostics: None,
            syntax: Some(SyntaxSnapshot {
                process_generation: generation,
                uri: Arc::from(synthetic_uri(document_stamp.document_id)),
                stamp: document_stamp,
                lsp_version: LspVersion::from_raw(1).expect("version"),
                runs: runs.into(),
            }),
            definition: None,
            notices: Vec::new(),
            stderr_tail: None,
            writer: WriterState::Idle,
        });

        assert_eq!(snapshot.status(), LanguageStatus::Ready);
        assert_eq!(snapshot.syntax().expect("syntax").runs.len(), 4_096);
        assert!(snapshot.estimated_bytes() <= MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn publication_coalesces_repaint_wakes_but_never_loses_latest_snapshot() {
        let (publisher, subscriber) = snapshot_channel(snapshot(1));
        publisher.publish(snapshot(2)).expect("publish 2");
        publisher.publish(snapshot(3)).expect("publish 3");
        publisher.publish(snapshot(4)).expect("publish 4");

        assert_eq!(subscriber.pending_wakes(), 1);
        let latest = subscriber.load_stable();
        assert_eq!(latest.revision().get(), 4);
        assert_eq!(subscriber.pending_wakes(), 0);
        assert!(!subscriber.is_dirty());
    }

    #[test]
    fn publication_rejects_stale_or_reused_snapshot_revisions() {
        let (publisher, _subscriber) = snapshot_channel(snapshot(3));
        assert_eq!(
            publisher.publish(snapshot(3)),
            Err(PublishError::StaleRevision)
        );
        assert_eq!(
            publisher.publish(snapshot(2)),
            Err(PublishError::StaleRevision)
        );
        publisher.publish(snapshot(4)).expect("next revision");
    }

    #[test]
    fn dirty_revision_handshake_survives_publication_at_every_load_phase() {
        for phase in [
            LoadPhase::Armed,
            LoadPhase::WakeDrained,
            LoadPhase::RevisionRead,
            LoadPhase::SnapshotLoaded,
            LoadPhase::DirtyRechecked,
        ] {
            let (publisher, subscriber) = snapshot_channel(snapshot(1));
            let mut injected = false;
            let loaded = subscriber.load_stable_with_hook(|observed| {
                if observed == phase && !injected {
                    injected = true;
                    publisher.publish(snapshot(2)).expect("racing publish");
                }
            });
            assert!(injected, "phase {phase:?} was not reached");
            assert_eq!(loaded.revision().get(), 2, "lost publish at {phase:?}");
        }
    }

    #[test]
    fn publication_after_final_recheck_leaves_a_wake_and_dirty_revision() {
        let (publisher, subscriber) = snapshot_channel(snapshot(1));
        let first = subscriber.load_stable();
        assert_eq!(first.revision().get(), 1);

        publisher.publish(snapshot(2)).expect("publish after load");
        assert!(subscriber.is_dirty());
        assert_eq!(subscriber.pending_wakes(), 1);
        assert_eq!(subscriber.load_stable().revision().get(), 2);
    }

    #[test]
    fn publish_after_the_final_recheck_cannot_be_hidden_by_an_older_wake() {
        let (publisher, subscriber) = snapshot_channel(snapshot(1));
        publisher.publish(snapshot(2)).expect("older publish");
        assert_eq!(subscriber.load_stable().revision().get(), 2);

        let mut injected = false;
        let loaded = subscriber.load_stable_with_hook(|phase| {
            if phase == LoadPhase::Returning && !injected {
                injected = true;
                publisher.publish(snapshot(3)).expect("late publish");
            }
        });
        assert_eq!(loaded.revision().get(), 2);
        assert!(subscriber.is_dirty());
        assert_eq!(subscriber.pending_wakes(), 1);
        assert_eq!(subscriber.load_stable().revision().get(), 3);
        assert_eq!(subscriber.pending_wakes(), 0);
    }

    #[test]
    fn bounded_fallback_preserves_only_a_truthful_generation_fenced_stderr_tail() {
        let generation = ProcessGeneration::from_raw(7).expect("generation");
        let text: Arc<str> = format!("{}\n", "x".repeat(MAX_STDERR_BYTES - 1)).into();
        let snapshot = LanguageSnapshot::bounded(LanguageSnapshotDraft {
            revision: SnapshotRevision::from_raw(2).expect("revision"),
            process_generation: Some(generation),
            status: LanguageStatus::Unavailable,
            desired_document: None,
            written_document: None,
            diagnostics: None,
            syntax: None,
            definition: None,
            notices: vec![LanguageNotice {
                id: NoticeId::from_raw(1).expect("notice"),
                kind: NoticeKind::Information,
                message: Arc::from("x".repeat(MAX_NOTICE_BYTES + 1)),
            }],
            stderr_tail: Some(StderrTailSnapshot {
                text: text.clone(),
                line_count: 1,
                truncated: true,
            }),
            writer: WriterState::Closed,
        });
        assert_eq!(snapshot.status(), LanguageStatus::Limited);
        assert_eq!(
            snapshot.stderr_tail().map(|tail| tail.text.as_ref()),
            Some(text.as_ref())
        );
        assert!(snapshot.estimated_bytes() <= MAX_SNAPSHOT_BYTES);

        for (process_generation, line_count) in [(Some(generation), 2), (None, 1)] {
            let invalid = LanguageSnapshot::bounded(LanguageSnapshotDraft {
                revision: SnapshotRevision::from_raw(3).expect("revision"),
                process_generation,
                status: LanguageStatus::Unavailable,
                desired_document: None,
                written_document: None,
                diagnostics: None,
                syntax: None,
                definition: None,
                notices: Vec::new(),
                stderr_tail: Some(StderrTailSnapshot {
                    text: Arc::from("one line"),
                    line_count,
                    truncated: false,
                }),
                writer: WriterState::Closed,
            });
            assert_eq!(invalid.status(), LanguageStatus::Limited);
            assert!(invalid.stderr_tail().is_none());
        }
    }
}

use std::sync::Arc;

use crate::DocumentBuffer;
use crate::language::{
    AcknowledgementBatch, CaretCommand, CaretGeneration, CaretIntent, DefinitionIntent,
    DefinitionResultId, DefinitionSnapshot, DesiredDocument, DiagnosticSetRevision, LanguageClosed,
    LanguageCoordinator, LanguageSnapshot, LanguageSnapshotDraft, LanguageStatus, NoticeId,
    ProcessGeneration, SnapshotRevision, WriterState,
};

const MAX_PENDING_NOTICE_ACKS: usize = 32;

pub(crate) trait LanguagePort {
    fn load_snapshot(&self) -> Arc<LanguageSnapshot>;
    fn submit_document(&self, desired: Option<DesiredDocument>) -> Result<(), LanguageClosed>;
    fn submit_caret(&self, command: CaretCommand) -> Result<(), LanguageClosed>;
    fn request_definition(&self, intent: DefinitionIntent) -> Result<(), LanguageClosed>;
    fn acknowledge_items(&self, batch: AcknowledgementBatch) -> Result<(), LanguageClosed>;
    fn request_shutdown(&self);
}

impl LanguagePort for LanguageCoordinator {
    fn load_snapshot(&self) -> Arc<LanguageSnapshot> {
        LanguageCoordinator::load_snapshot(self)
    }

    fn submit_document(&self, desired: Option<DesiredDocument>) -> Result<(), LanguageClosed> {
        LanguageCoordinator::submit_document(self, desired)
    }

    fn submit_caret(&self, command: CaretCommand) -> Result<(), LanguageClosed> {
        LanguageCoordinator::submit_caret(self, command)
    }

    fn request_definition(&self, intent: DefinitionIntent) -> Result<(), LanguageClosed> {
        LanguageCoordinator::request_definition(self, intent)
    }

    fn acknowledge_items(&self, batch: AcknowledgementBatch) -> Result<(), LanguageClosed> {
        LanguageCoordinator::acknowledge_items(self, batch)
    }

    fn request_shutdown(&self) {
        LanguageCoordinator::request_shutdown(self);
    }
}

struct UnavailableLanguagePort {
    snapshot: Arc<LanguageSnapshot>,
}

impl Default for UnavailableLanguagePort {
    fn default() -> Self {
        Self {
            snapshot: unavailable_snapshot(),
        }
    }
}

impl LanguagePort for UnavailableLanguagePort {
    fn load_snapshot(&self) -> Arc<LanguageSnapshot> {
        Arc::clone(&self.snapshot)
    }

    fn submit_document(&self, _desired: Option<DesiredDocument>) -> Result<(), LanguageClosed> {
        Ok(())
    }

    fn submit_caret(&self, _command: CaretCommand) -> Result<(), LanguageClosed> {
        Ok(())
    }

    fn request_definition(&self, _intent: DefinitionIntent) -> Result<(), LanguageClosed> {
        Ok(())
    }

    fn acknowledge_items(&self, _batch: AcknowledgementBatch) -> Result<(), LanguageClosed> {
        Ok(())
    }

    fn request_shutdown(&self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubmittedDocument {
    Unknown,
    Closed,
    Open(crate::DocumentStamp),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectionIdentity {
    pub(crate) anchor_character: usize,
    pub(crate) focus_character: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EditorCaret {
    pub(crate) stamp: crate::DocumentStamp,
    pub(crate) primary_character: Option<usize>,
    pub(crate) selection: Option<SelectionIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedDefinitionIntent {
    intent: DefinitionIntent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnalysisRevealScope {
    pub(crate) document: crate::DocumentId,
    pub(crate) process_generation: ProcessGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnalysisRevealBatch {
    pub(crate) scope: AnalysisRevealScope,
    pub(crate) revision: DiagnosticSetRevision,
    pub(crate) item_count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AnalysisRevealLatch {
    scope: Option<AnalysisRevealScope>,
    revealed: bool,
    last_batch: Option<DiagnosticSetRevision>,
}

impl AnalysisRevealLatch {
    pub(crate) fn observe(
        &mut self,
        current_scope: Option<AnalysisRevealScope>,
        accepted_batch: Option<AnalysisRevealBatch>,
    ) -> bool {
        if self.scope != current_scope {
            self.scope = current_scope;
            self.revealed = false;
            self.last_batch = None;
        }
        let Some(current_scope) = current_scope else {
            return false;
        };
        let Some(batch) = accepted_batch.filter(|batch| batch.scope == current_scope) else {
            return false;
        };
        if self
            .last_batch
            .is_some_and(|last_batch| batch.revision <= last_batch)
        {
            return false;
        }
        self.last_batch = Some(batch.revision);
        if batch.item_count == 0 {
            self.revealed = false;
            return false;
        }
        if self.revealed {
            return false;
        }
        self.revealed = true;
        true
    }
}

pub(crate) struct LanguageUiState {
    port: Box<dyn LanguagePort>,
    snapshot: Arc<LanguageSnapshot>,
    submitted_document: SubmittedDocument,
    process_generation: Option<ProcessGeneration>,
    caret: Option<EditorCaret>,
    caret_generation: Option<CaretGeneration>,
    caret_exhausted: bool,
    exhausted_stamp: Option<crate::DocumentStamp>,
    pending_ack_generation: Option<ProcessGeneration>,
    pending_ack_revision: Option<SnapshotRevision>,
    pending_definition_ack: Option<DefinitionResultId>,
    pending_notice_acks: Vec<NoticeId>,
    shutdown_requested: bool,
}

impl LanguageUiState {
    pub(crate) fn new(port: Box<dyn LanguagePort>) -> Self {
        let snapshot = port.load_snapshot();
        let process_generation = snapshot.process_generation();
        Self {
            port,
            snapshot,
            submitted_document: SubmittedDocument::Unknown,
            process_generation,
            caret: None,
            caret_generation: None,
            caret_exhausted: false,
            exhausted_stamp: None,
            pending_ack_generation: None,
            pending_ack_revision: None,
            pending_definition_ack: None,
            pending_notice_acks: Vec::new(),
            shutdown_requested: false,
        }
    }

    pub(crate) fn native(coordinator: LanguageCoordinator) -> Self {
        Self::new(Box::new(coordinator))
    }

    pub(crate) fn unavailable() -> Self {
        Self::new(Box::<UnavailableLanguagePort>::default())
    }

    pub(crate) fn snapshot(&self) -> &Arc<LanguageSnapshot> {
        &self.snapshot
    }

    pub(crate) fn refresh_snapshot(&mut self) -> Arc<LanguageSnapshot> {
        let snapshot = self.port.load_snapshot();
        let process_generation = snapshot.process_generation();
        if process_generation != self.process_generation {
            self.clear_pending_acknowledgements();
        }
        if process_generation.is_some() && process_generation != self.process_generation {
            self.caret = None;
            self.caret_generation = None;
            self.caret_exhausted = false;
            self.exhausted_stamp = None;
        }
        self.process_generation = process_generation;
        self.snapshot = Arc::clone(&snapshot);
        snapshot
    }

    pub(crate) fn reconcile_document(
        &mut self,
        document: Option<&DocumentBuffer>,
    ) -> Result<bool, LanguageClosed> {
        let next = document.map_or(SubmittedDocument::Closed, |document| {
            SubmittedDocument::Open(document.stamp())
        });
        if self.submitted_document == next {
            return Ok(false);
        }
        let desired = document.map(|document| DesiredDocument {
            stamp: document.stamp(),
            text: document.shared_text(),
        });
        self.port.submit_document(desired)?;
        self.submitted_document = next;
        Ok(true)
    }

    pub(crate) fn reconcile_caret(
        &mut self,
        caret: Option<EditorCaret>,
    ) -> Result<bool, LanguageClosed> {
        let process_generation = self.snapshot.process_generation();
        let Some(caret) = caret else {
            self.caret = None;
            return Ok(false);
        };
        let Some(process_generation) = process_generation else {
            self.caret = Some(caret);
            return Ok(false);
        };
        if self.submitted_document != SubmittedDocument::Open(caret.stamp) {
            return Ok(false);
        }
        if self.caret == Some(caret) {
            return Ok(false);
        }
        if self.caret_exhausted {
            if self.exhausted_stamp == Some(caret.stamp) {
                self.caret = Some(caret);
                return Ok(false);
            }
            self.port.submit_caret(CaretCommand::Exhausted {
                process_generation,
                stamp: caret.stamp,
            })?;
            self.exhausted_stamp = Some(caret.stamp);
            self.caret = Some(caret);
            return Ok(true);
        }

        let generation = match self.caret_generation {
            Some(current) => current.checked_next().ok(),
            None => CaretGeneration::from_raw(1),
        };
        let Some(generation) = generation else {
            self.port.submit_caret(CaretCommand::Exhausted {
                process_generation,
                stamp: caret.stamp,
            })?;
            self.caret_exhausted = true;
            self.exhausted_stamp = Some(caret.stamp);
            self.caret = Some(caret);
            return Ok(true);
        };
        self.port.submit_caret(CaretCommand::Current(CaretIntent {
            process_generation,
            stamp: caret.stamp,
            generation,
            primary_character: caret.primary_character,
        }))?;
        self.caret = Some(caret);
        self.caret_generation = Some(generation);
        Ok(true)
    }

    pub(crate) fn caret_exhausted(&self) -> bool {
        self.caret_exhausted
    }

    pub(crate) fn definition_available(&self) -> bool {
        self.definition_intent(crate::NavigationGeneration::from_raw(1).expect("probe generation"))
            .is_some()
    }

    pub(crate) fn capture_definition(
        &self,
        navigation_generation: crate::NavigationGeneration,
    ) -> Option<CapturedDefinitionIntent> {
        self.definition_intent(navigation_generation)
            .map(|intent| CapturedDefinitionIntent { intent })
    }

    pub(crate) fn submit_definition(
        &mut self,
        captured: CapturedDefinitionIntent,
    ) -> Result<bool, LanguageClosed> {
        let Some(current) = self.definition_intent(captured.intent.navigation_generation) else {
            return Ok(false);
        };
        if current != captured.intent {
            return Ok(false);
        }
        self.port.request_definition(captured.intent)?;
        Ok(true)
    }

    pub(crate) fn definition_result_current(&self, result: &DefinitionSnapshot) -> bool {
        let Some(caret) = self.caret else {
            return false;
        };
        let Some(caret_generation) = self.caret_generation else {
            return false;
        };
        let Some(written) = self.snapshot.written_document() else {
            return false;
        };
        !self.caret_exhausted
            && self.submitted_document == SubmittedDocument::Open(result.stamp)
            && self.snapshot.process_generation() == Some(result.process_generation)
            && self
                .snapshot
                .definition()
                .is_some_and(|current| current.id == result.id)
            && written.stamp == result.stamp
            && written.uri == result.uri
            && written.lsp_version == Some(result.lsp_version)
            && caret.stamp == result.stamp
            && caret.primary_character == Some(result.caret_character)
            && caret_generation == result.caret_generation
    }

    fn definition_intent(
        &self,
        navigation_generation: crate::NavigationGeneration,
    ) -> Option<DefinitionIntent> {
        if self.caret_exhausted {
            return None;
        }
        let caret = self.caret?;
        let caret_character = caret.primary_character?;
        let caret_generation = self.caret_generation?;
        let process_generation = self.snapshot.process_generation()?;
        if self.submitted_document != SubmittedDocument::Open(caret.stamp)
            || self.snapshot.written_stamp() != Some(caret.stamp)
        {
            return None;
        }
        Some(DefinitionIntent {
            process_generation,
            navigation_generation,
            stamp: caret.stamp,
            caret_generation,
            caret_character,
        })
    }

    pub(crate) fn acknowledge_definition(&mut self, id: DefinitionResultId) -> bool {
        if self.snapshot.definition().map(|definition| definition.id) != Some(id) {
            return false;
        }
        let Some(process_generation) = self.snapshot.process_generation() else {
            return false;
        };
        self.record_acknowledgement_scope(process_generation);
        self.pending_definition_ack = Some(id);
        true
    }

    pub(crate) fn acknowledge_notice(&mut self, id: NoticeId) -> bool {
        if !self.snapshot.notices().iter().any(|notice| notice.id == id) {
            return false;
        }
        let Some(process_generation) = self.snapshot.process_generation() else {
            return false;
        };
        self.record_acknowledgement_scope(process_generation);
        if !self.pending_notice_acks.contains(&id) {
            if self.pending_notice_acks.len() == MAX_PENDING_NOTICE_ACKS {
                self.pending_notice_acks.remove(0);
            }
            self.pending_notice_acks.push(id);
        }
        true
    }

    pub(crate) fn flush_acknowledgements(&mut self) -> Result<bool, LanguageClosed> {
        let Some(process_generation) = self.pending_ack_generation else {
            return Ok(false);
        };
        let Some(observed_revision) = self.pending_ack_revision else {
            return Ok(false);
        };
        let Some(batch) = AcknowledgementBatch::new(
            process_generation,
            observed_revision,
            self.pending_definition_ack,
            Arc::from(self.pending_notice_acks.as_slice()),
        ) else {
            return Ok(false);
        };
        self.port.acknowledge_items(batch)?;
        self.clear_pending_acknowledgements();
        Ok(true)
    }

    fn record_acknowledgement_scope(&mut self, process_generation: ProcessGeneration) {
        if self.pending_ack_generation != Some(process_generation) {
            self.clear_pending_acknowledgements();
            self.pending_ack_generation = Some(process_generation);
        }
        self.pending_ack_revision = Some(
            self.pending_ack_revision
                .map_or(self.snapshot.revision(), |current| {
                    current.max(self.snapshot.revision())
                }),
        );
    }

    fn clear_pending_acknowledgements(&mut self) {
        self.pending_ack_generation = None;
        self.pending_ack_revision = None;
        self.pending_definition_ack = None;
        self.pending_notice_acks.clear();
    }

    pub(crate) fn request_shutdown(&mut self) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;
        self.port.request_shutdown();
    }

    #[cfg(test)]
    pub(crate) fn test_seed_caret_generation(&mut self, generation: CaretGeneration) {
        self.caret = None;
        self.caret_generation = Some(generation);
        self.caret_exhausted = false;
        self.exhausted_stamp = None;
    }
}

impl Drop for LanguageUiState {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

pub(crate) fn unavailable_snapshot() -> Arc<LanguageSnapshot> {
    Arc::new(LanguageSnapshot::bounded(LanguageSnapshotDraft {
        revision: SnapshotRevision::from_raw(1).expect("unavailable snapshot revision"),
        process_generation: None,
        status: LanguageStatus::Unavailable,
        desired_document: None,
        written_document: None,
        diagnostics: None,
        syntax: None,
        definition: None,
        notices: Vec::new(),
        stderr_tail: None,
        writer: WriterState::Closed,
    }))
}

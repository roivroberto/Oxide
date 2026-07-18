mod actor;
mod coordinator;
mod framing;
mod process_adapter;
mod snapshot;
mod text_index;

#[cfg(test)]
mod actor_contract_tests;
#[cfg(test)]
mod coordinator_tests;
#[cfg(test)]
mod process_adapter_tests;

#[allow(unused_imports)]
pub(crate) use coordinator::{
    AcknowledgementBatch, CaretCommand, CaretIntent, DefinitionIntent, DesiredDocument,
    LanguageClosed, LanguageCoordinator,
};
pub(crate) use snapshot::synthetic_uri;
#[allow(unused_imports)]
pub(crate) use snapshot::{
    AnalysisDiagnostic, AnalysisPhase, CaretGeneration, DefinitionResultId, DefinitionSnapshot,
    DefinitionTargetSnapshot, DiagnosticItemId, DiagnosticSetRevision, DiagnosticSeverity,
    DiagnosticSnapshot, DocumentSyncSnapshot, LanguageNotice, LanguageSnapshot,
    LanguageSnapshotDraft, LanguageStatus, LspVersion, NoticeId, NoticeKind, ProcessGeneration,
    SnapshotRevision, StderrTailSnapshot, SyntaxSnapshot, WriterState,
};
#[allow(unused_imports)]
pub(crate) use text_index::{SyntaxKind, SyntaxRun, TextRange};

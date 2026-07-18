use rlox::{PauseLocation, RevisionId, SourceDocument, SourceId, VmSnapshot};
use serde::{Deserialize, Deserializer, Serialize};

use super::WireDiagnostic;

#[cfg(not(target_pointer_width = "64"))]
compile_error!("oxide-ide protocol v1 requires a 64-bit target");

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_DISPLAY_NAME_BYTES: usize = 4 * 1024;
pub const MAX_WIRE_DOCUMENT_JSON_BYTES: usize = MAX_PAYLOAD_BYTES - 64 * 1024;
pub const MAX_OUTPUT_CHUNK_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_RUN_OUTPUT_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_CONTROL_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerSessionId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSequence(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub version: u16,
    pub worker_session_id: WorkerSessionId,
    pub run_id: RunId,
    pub source_revision: RevisionId,
    pub request_id: RequestId,
    pub sequence: EventSequence,
    pub payload: T,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireDocument {
    pub source_id: SourceId,
    pub revision: RevisionId,
    pub display_name: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireDocumentError {
    ZeroSourceId,
    ZeroRevision,
    EmptyDisplayName,
    DisplayNameTooLong,
    InvalidDisplayName,
    NonNormalizedText,
    SourceTooLarge,
}

impl WireDocument {
    pub fn validate(&self) -> Result<(), WireDocumentError> {
        if self.source_id.0 == 0 {
            return Err(WireDocumentError::ZeroSourceId);
        }
        if self.revision.0 == 0 {
            return Err(WireDocumentError::ZeroRevision);
        }
        Self::validate_content(&self.display_name, &self.text)
    }

    pub fn validate_content(display_name: &str, text: &str) -> Result<(), WireDocumentError> {
        if display_name.is_empty() {
            return Err(WireDocumentError::EmptyDisplayName);
        }
        if display_name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(WireDocumentError::DisplayNameTooLong);
        }
        if display_name == "."
            || display_name == ".."
            || display_name.trim() != display_name
            || display_name.chars().any(|character| {
                character.is_control()
                    || matches!(
                        character,
                        '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                    )
            })
        {
            return Err(WireDocumentError::InvalidDisplayName);
        }
        if text.starts_with('\u{feff}') || text.contains('\r') {
            return Err(WireDocumentError::NonNormalizedText);
        }
        if text.len() > MAX_PAYLOAD_BYTES
            || 256usize
                .checked_add(conservative_json_string_size(display_name))
                .and_then(|size| size.checked_add(conservative_json_string_size(text)))
                .is_none_or(|size| size > MAX_WIRE_DOCUMENT_JSON_BYTES)
        {
            return Err(WireDocumentError::SourceTooLarge);
        }
        Ok(())
    }

    pub fn into_source_document(self) -> Result<SourceDocument, WireDocumentError> {
        self.validate()?;
        Ok(SourceDocument::new(
            self.source_id,
            self.revision,
            self.display_name,
            self.text,
        ))
    }
}

fn conservative_json_string_size(value: &str) -> usize {
    let mut size = 2usize;
    for character in value.chars() {
        let bytes = match character {
            '"' | '\\' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        let Some(updated) = size.checked_add(bytes) else {
            return usize::MAX;
        };
        size = updated;
    }
    size
}

impl TryFrom<WireDocument> for SourceDocument {
    type Error = WireDocumentError;

    fn try_from(value: WireDocument) -> Result<Self, Self::Error> {
        value.into_source_document()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Command {
    LoadAndRun {
        document: WireDocument,
    },
    LoadAndDebug {
        document: WireDocument,
    },
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
    Shutdown,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CommandKind {
    LoadAndRun,
    LoadAndDebug,
    Pause,
    Continue,
    StepInto,
    StepOver,
    StepOut,
    Stop,
    ProvideInput,
    Shutdown,
}

#[derive(Default)]
enum PayloadField<T> {
    #[default]
    Missing,
    Present(T),
}

fn deserialize_present_payload<'de, D, T>(deserializer: D) -> Result<PayloadField<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(PayloadField::Present)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CommandPayload {
    Document(DocumentPayload),
    Input(InputPayload),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandWire {
    kind: CommandKind,
    #[serde(default, deserialize_with = "deserialize_present_payload")]
    payload: PayloadField<CommandPayload>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentPayload {
    document: WireDocument,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputPayload {
    in_reply_to: RequestId,
    text: String,
}

impl<'de> Deserialize<'de> for Command {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CommandWire::deserialize(deserializer)?;
        match (wire.kind, wire.payload) {
            (CommandKind::LoadAndRun, PayloadField::Present(CommandPayload::Document(payload))) => {
                Ok(Self::LoadAndRun {
                    document: payload.document,
                })
            }
            (
                CommandKind::LoadAndDebug,
                PayloadField::Present(CommandPayload::Document(payload)),
            ) => Ok(Self::LoadAndDebug {
                document: payload.document,
            }),
            (CommandKind::Pause, PayloadField::Missing) => Ok(Self::Pause),
            (CommandKind::Continue, PayloadField::Missing) => Ok(Self::Continue),
            (CommandKind::StepInto, PayloadField::Missing) => Ok(Self::StepInto),
            (CommandKind::StepOver, PayloadField::Missing) => Ok(Self::StepOver),
            (CommandKind::StepOut, PayloadField::Missing) => Ok(Self::StepOut),
            (CommandKind::Stop, PayloadField::Missing) => Ok(Self::Stop),
            (CommandKind::ProvideInput, PayloadField::Present(CommandPayload::Input(payload))) => {
                Ok(Self::ProvideInput {
                    in_reply_to: payload.in_reply_to,
                    text: payload.text,
                })
            }
            (CommandKind::Shutdown, PayloadField::Missing) => Ok(Self::Shutdown),
            _ => Err(serde::de::Error::custom(
                "payload does not match command kind",
            )),
        }
    }
}

impl Command {
    pub fn is_closing(&self) -> bool {
        matches!(self, Self::Shutdown)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkerEvent {
    Hello,
    Output {
        text: String,
    },
    OutputTruncated,
    CommandRejected {
        code: String,
        message: String,
    },
    InputRequested {
        prompt: String,
    },
    Diagnostic {
        diagnostic: WireDiagnostic,
    },
    Paused {
        location: PauseLocation,
        snapshot: VmSnapshot,
    },
    Completed,
    Cancelled {
        snapshot: VmSnapshot,
    },
    Faulted {
        diagnostic: WireDiagnostic,
        snapshot: VmSnapshot,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkerEventKind {
    Hello,
    Output,
    OutputTruncated,
    CommandRejected,
    InputRequested,
    Diagnostic,
    Paused,
    Completed,
    Cancelled,
    Faulted,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WorkerEventPayload {
    Output(OutputPayload),
    Rejected(RejectedPayload),
    InputRequested(InputRequestedPayload),
    Diagnostic(DiagnosticPayload),
    Paused(PausedPayload),
    Snapshot(SnapshotPayload),
    Faulted(FaultedPayload),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerEventWire {
    kind: WorkerEventKind,
    #[serde(default, deserialize_with = "deserialize_present_payload")]
    payload: PayloadField<WorkerEventPayload>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputPayload {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectedPayload {
    code: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRequestedPayload {
    prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticPayload {
    diagnostic: WireDiagnostic,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PausedPayload {
    location: PauseLocation,
    snapshot: VmSnapshot,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotPayload {
    snapshot: VmSnapshot,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FaultedPayload {
    diagnostic: WireDiagnostic,
    snapshot: VmSnapshot,
}

impl<'de> Deserialize<'de> for WorkerEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkerEventWire::deserialize(deserializer)?;
        match (wire.kind, wire.payload) {
            (WorkerEventKind::Hello, PayloadField::Missing) => Ok(Self::Hello),
            (
                WorkerEventKind::Output,
                PayloadField::Present(WorkerEventPayload::Output(payload)),
            ) => Ok(Self::Output { text: payload.text }),
            (WorkerEventKind::OutputTruncated, PayloadField::Missing) => Ok(Self::OutputTruncated),
            (
                WorkerEventKind::CommandRejected,
                PayloadField::Present(WorkerEventPayload::Rejected(payload)),
            ) => Ok(Self::CommandRejected {
                code: payload.code,
                message: payload.message,
            }),
            (
                WorkerEventKind::InputRequested,
                PayloadField::Present(WorkerEventPayload::InputRequested(payload)),
            ) => Ok(Self::InputRequested {
                prompt: payload.prompt,
            }),
            (
                WorkerEventKind::Diagnostic,
                PayloadField::Present(WorkerEventPayload::Diagnostic(payload)),
            ) => Ok(Self::Diagnostic {
                diagnostic: payload.diagnostic,
            }),
            (
                WorkerEventKind::Paused,
                PayloadField::Present(WorkerEventPayload::Paused(payload)),
            ) => Ok(Self::Paused {
                location: payload.location,
                snapshot: payload.snapshot,
            }),
            (WorkerEventKind::Completed, PayloadField::Missing) => Ok(Self::Completed),
            (
                WorkerEventKind::Cancelled,
                PayloadField::Present(WorkerEventPayload::Snapshot(payload)),
            ) => Ok(Self::Cancelled {
                snapshot: payload.snapshot,
            }),
            (
                WorkerEventKind::Faulted,
                PayloadField::Present(WorkerEventPayload::Faulted(payload)),
            ) => Ok(Self::Faulted {
                diagnostic: payload.diagnostic,
                snapshot: payload.snapshot,
            }),
            _ => Err(serde::de::Error::custom(
                "payload does not match worker event kind",
            )),
        }
    }
}

impl WorkerEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled { .. } | Self::Faulted { .. }
        )
    }
}

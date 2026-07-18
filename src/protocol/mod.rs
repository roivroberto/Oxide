mod codec;
mod diagnostic;
mod types;
mod validation;

pub use codec::{DecodeError, EncodeError, LineCodec};
pub use diagnostic::{
    MAX_DIAGNOSTIC_CODE_BYTES, MAX_DIAGNOSTIC_FRAMES, MAX_DIAGNOSTIC_FUNCTION_BYTES,
    MAX_DIAGNOSTIC_JSON_BYTES, MAX_DIAGNOSTIC_MESSAGE_BYTES, WireDiagnostic, WireDiagnosticError,
    WireRuntimeFrame,
};
pub use types::{
    Command, Envelope, EventSequence, MAX_CONTROL_TEXT_BYTES, MAX_DISPLAY_NAME_BYTES,
    MAX_OUTPUT_CHUNK_TEXT_BYTES, MAX_PAYLOAD_BYTES, MAX_RUN_OUTPUT_FRAME_BYTES,
    MAX_WIRE_DOCUMENT_JSON_BYTES, PROTOCOL_VERSION, RequestId, RunId, WireDocument,
    WireDocumentError, WorkerEvent, WorkerSessionId,
};
pub use validation::{CommandStreamValidator, StreamValidationError, WorkerEventStreamValidator};

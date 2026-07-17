use rlox::{Diagnostic, DiagnosticPhase, DiagnosticSeverity, RuntimeFrame, SourceSpan};
use serde::{Deserialize, Serialize};

pub const MAX_DIAGNOSTIC_JSON_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_DIAGNOSTIC_CODE_BYTES: usize = 128;
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_DIAGNOSTIC_FRAMES: usize = 64;
pub const MAX_DIAGNOSTIC_FUNCTION_BYTES: usize = 1024;

const INVALID_CODE: &str = "worker.invalid_diagnostic_code";
const FALLBACK_CODE: &str = "worker.diagnostic_too_large";
const FALLBACK_MESSAGE: &str = "The worker diagnostic exceeded the protocol budget.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireDiagnostic {
    pub phase: DiagnosticPhase,
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub code_truncated: bool,
    pub message: String,
    pub message_truncated: bool,
    pub span: SourceSpan,
    pub frames: Vec<WireRuntimeFrame>,
    pub frames_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRuntimeFrame {
    pub function: String,
    pub function_truncated: bool,
    pub span: SourceSpan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireDiagnosticError {
    InvalidCode,
    CodeTooLong,
    MessageTooLong,
    TooManyFrames,
    FunctionTooLong,
    JsonTooLarge,
    Serialization,
}

impl WireDiagnostic {
    pub fn from_diagnostic(value: &Diagnostic) -> Self {
        let (code, code_truncated) = if valid_code(&value.code) {
            (value.code.clone(), false)
        } else {
            (INVALID_CODE.to_string(), true)
        };
        let (message, message_truncated) =
            truncate_utf8(&value.message, MAX_DIAGNOSTIC_MESSAGE_BYTES);
        let frames_truncated = value.frames.len() > MAX_DIAGNOSTIC_FRAMES;
        let frames = value
            .frames
            .iter()
            .take(MAX_DIAGNOSTIC_FRAMES)
            .map(WireRuntimeFrame::from_runtime_frame)
            .collect();
        let candidate = Self {
            phase: value.phase,
            severity: value.severity,
            code,
            code_truncated,
            message,
            message_truncated,
            span: value.span,
            frames,
            frames_truncated,
        };

        match candidate.json_size() {
            Ok(size) if size <= MAX_DIAGNOSTIC_JSON_BYTES => candidate,
            _ => Self::fallback(value.phase, value.severity, value.span),
        }
    }

    pub fn validate(&self) -> Result<(), WireDiagnosticError> {
        if self.code.len() > MAX_DIAGNOSTIC_CODE_BYTES {
            return Err(WireDiagnosticError::CodeTooLong);
        }
        if !valid_code(&self.code) {
            return Err(WireDiagnosticError::InvalidCode);
        }
        if self.message.len() > MAX_DIAGNOSTIC_MESSAGE_BYTES {
            return Err(WireDiagnosticError::MessageTooLong);
        }
        if self.frames.len() > MAX_DIAGNOSTIC_FRAMES {
            return Err(WireDiagnosticError::TooManyFrames);
        }
        if self
            .frames
            .iter()
            .any(|frame| frame.function.len() > MAX_DIAGNOSTIC_FUNCTION_BYTES)
        {
            return Err(WireDiagnosticError::FunctionTooLong);
        }
        if self.json_size()? > MAX_DIAGNOSTIC_JSON_BYTES {
            return Err(WireDiagnosticError::JsonTooLarge);
        }
        Ok(())
    }

    pub fn json_size(&self) -> Result<usize, WireDiagnosticError> {
        serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .map_err(|_| WireDiagnosticError::Serialization)
    }

    fn fallback(phase: DiagnosticPhase, severity: DiagnosticSeverity, span: SourceSpan) -> Self {
        Self {
            phase,
            severity,
            code: FALLBACK_CODE.to_string(),
            code_truncated: true,
            message: FALLBACK_MESSAGE.to_string(),
            message_truncated: true,
            span,
            frames: Vec::new(),
            frames_truncated: true,
        }
    }
}

impl WireRuntimeFrame {
    fn from_runtime_frame(value: &RuntimeFrame) -> Self {
        let (function, function_truncated) =
            truncate_utf8(&value.function, MAX_DIAGNOSTIC_FUNCTION_BYTES);
        Self {
            function,
            function_truncated,
            span: value.span,
        }
    }
}

impl From<&Diagnostic> for WireDiagnostic {
    fn from(value: &Diagnostic) -> Self {
        Self::from_diagnostic(value)
    }
}

impl From<Diagnostic> for WireDiagnostic {
    fn from(value: Diagnostic) -> Self {
        Self::from_diagnostic(&value)
    }
}

fn valid_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DIAGNOSTIC_CODE_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn truncate_utf8(value: &str, maximum: usize) -> (String, bool) {
    if value.len() <= maximum {
        return (value.to_string(), false);
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

impl From<WireDiagnosticError> for super::validation::StreamValidationError {
    fn from(_: WireDiagnosticError) -> Self {
        super::validation::StreamValidationError::InvalidPayload
    }
}

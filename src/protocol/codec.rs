use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize, de::IgnoredAny};

use super::validation::{validate_command_envelope, validate_worker_event_envelope};
use super::{Command, Envelope, MAX_PAYLOAD_BYTES, PROTOCOL_VERSION, WorkerEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    InvalidEnvelope,
    Oversized,
    Serialization,
    Io(io::ErrorKind),
    Poisoned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Io(io::ErrorKind),
    BlankFrame,
    Oversized,
    InvalidUtf8,
    WrongTopLevel,
    MalformedJson,
    EofMidFrame,
    UnsupportedVersion { received: u16 },
    WrongDirection,
    InvalidEnvelope,
    Poisoned,
}

#[derive(Debug, Default)]
pub struct LineCodec {
    read_buffer: Vec<u8>,
    draining_oversized: bool,
    read_poisoned: bool,
    write_poisoned: bool,
}

impl LineCodec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read_command<R: BufRead>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<Envelope<Command>>, DecodeError> {
        let Some(frame) = self.read_frame(reader)? else {
            return Ok(None);
        };
        decode_command(&frame.bytes, frame.terminated).map(Some)
    }

    pub fn read_worker_event<R: BufRead>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<Envelope<WorkerEvent>>, DecodeError> {
        let Some(frame) = self.read_frame(reader)? else {
            return Ok(None);
        };
        decode_worker_event(&frame.bytes, frame.terminated).map(Some)
    }

    pub fn write_command<W: Write>(
        &mut self,
        writer: &mut W,
        envelope: &Envelope<Command>,
    ) -> Result<usize, EncodeError> {
        if self.write_poisoned {
            return Err(EncodeError::Poisoned);
        }
        validate_command_envelope(envelope).map_err(|_| EncodeError::InvalidEnvelope)?;
        self.write_json(writer, envelope)
    }

    pub fn write_worker_event<W: Write>(
        &mut self,
        writer: &mut W,
        envelope: &Envelope<WorkerEvent>,
    ) -> Result<usize, EncodeError> {
        if self.write_poisoned {
            return Err(EncodeError::Poisoned);
        }
        validate_worker_event_envelope(envelope).map_err(|_| EncodeError::InvalidEnvelope)?;
        self.write_json(writer, envelope)
    }

    pub fn worker_event_payload_len(
        &self,
        envelope: &Envelope<WorkerEvent>,
    ) -> Result<usize, EncodeError> {
        validate_worker_event_envelope(envelope).map_err(|_| EncodeError::InvalidEnvelope)?;
        encode_json(envelope).map(|bytes| bytes.len())
    }

    fn write_json<W: Write, T: Serialize>(
        &mut self,
        writer: &mut W,
        value: &T,
    ) -> Result<usize, EncodeError> {
        let mut encoded = encode_json(value)?;
        let payload_len = encoded.len();
        encoded.push(b'\n');
        if let Err(error) = writer.write_all(&encoded) {
            self.write_poisoned = true;
            return Err(EncodeError::Io(error.kind()));
        }
        if let Err(error) = writer.flush() {
            self.write_poisoned = true;
            return Err(EncodeError::Io(error.kind()));
        }
        Ok(payload_len)
    }

    fn read_frame<R: BufRead>(&mut self, reader: &mut R) -> Result<Option<Frame>, DecodeError> {
        if self.read_poisoned {
            return Err(DecodeError::Poisoned);
        }

        loop {
            let available = match reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.read_poisoned = true;
                    return Err(DecodeError::Io(error.kind()));
                }
            };

            if available.is_empty() {
                if self.draining_oversized {
                    self.draining_oversized = false;
                    self.read_buffer.clear();
                    return Err(DecodeError::Oversized);
                }
                if self.read_buffer.is_empty() {
                    return Ok(None);
                }
                if self.read_buffer.len() > MAX_PAYLOAD_BYTES {
                    self.read_buffer.clear();
                    return Err(DecodeError::Oversized);
                }
                return Ok(Some(Frame {
                    bytes: std::mem::take(&mut self.read_buffer),
                    terminated: false,
                }));
            }

            if self.draining_oversized {
                if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                    reader.consume(newline + 1);
                    self.draining_oversized = false;
                    self.read_buffer.clear();
                    return Err(DecodeError::Oversized);
                }
                let consumed = available.len();
                reader.consume(consumed);
                continue;
            }

            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                let framed_len = self.read_buffer.len().saturating_add(newline);
                let has_crlf = if newline > 0 {
                    available[newline - 1] == b'\r'
                } else {
                    self.read_buffer.last() == Some(&b'\r')
                };
                let payload_len = framed_len.saturating_sub(usize::from(has_crlf));
                if payload_len > MAX_PAYLOAD_BYTES {
                    reader.consume(newline + 1);
                    self.read_buffer.clear();
                    return Err(DecodeError::Oversized);
                }
                self.read_buffer.extend_from_slice(&available[..newline]);
                reader.consume(newline + 1);
                if self.read_buffer.last() == Some(&b'\r') {
                    self.read_buffer.pop();
                }
                if self.read_buffer.is_empty() {
                    return Err(DecodeError::BlankFrame);
                }
                return Ok(Some(Frame {
                    bytes: std::mem::take(&mut self.read_buffer),
                    terminated: true,
                }));
            }

            let combined = self.read_buffer.len().saturating_add(available.len());
            let may_be_split_crlf = combined == MAX_PAYLOAD_BYTES + 1
                && available
                    .last()
                    .copied()
                    .or_else(|| self.read_buffer.last().copied())
                    == Some(b'\r');
            if combined > MAX_PAYLOAD_BYTES + 1
                || (combined > MAX_PAYLOAD_BYTES && !may_be_split_crlf)
            {
                let consumed = available.len();
                reader.consume(consumed);
                self.read_buffer.clear();
                self.draining_oversized = true;
                continue;
            }
            self.read_buffer.extend_from_slice(available);
            let consumed = available.len();
            reader.consume(consumed);
        }
    }
}

struct Frame {
    bytes: Vec<u8>,
    terminated: bool,
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut writer = CappedBuffer::new(MAX_PAYLOAD_BYTES);
    if serde_json::to_writer(&mut writer, value).is_err() {
        return if writer.overflowed {
            Err(EncodeError::Oversized)
        } else {
            Err(EncodeError::Serialization)
        };
    }
    Ok(writer.bytes)
}

struct CappedBuffer {
    bytes: Vec<u8>,
    maximum: usize,
    overflowed: bool,
}

impl CappedBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
            overflowed: false,
        }
    }
}

impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(total) = self.bytes.len().checked_add(bytes.len()) else {
            self.overflowed = true;
            return Err(io::Error::other("protocol payload is too large"));
        };
        if total > self.maximum {
            self.overflowed = true;
            return Err(io::Error::other("protocol payload is too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionHeader {
    version: u16,
    worker_session_id: IgnoredAny,
    run_id: IgnoredAny,
    source_revision: IgnoredAny,
    request_id: IgnoredAny,
    sequence: IgnoredAny,
    payload: IgnoredAny,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectionHeader {
    version: IgnoredAny,
    worker_session_id: IgnoredAny,
    run_id: IgnoredAny,
    source_revision: IgnoredAny,
    request_id: IgnoredAny,
    sequence: IgnoredAny,
    payload: PayloadHeader,
}

#[derive(Deserialize)]
struct PayloadHeader {
    kind: String,
}

fn decode_command(bytes: &[u8], terminated: bool) -> Result<Envelope<Command>, DecodeError> {
    let kind = decode_header(bytes, terminated)?;
    if is_worker_event_kind(&kind) {
        return Err(DecodeError::WrongDirection);
    }
    let envelope: Envelope<Command> = decode_typed(bytes, terminated)?;
    validate_command_envelope(&envelope).map_err(|_| DecodeError::InvalidEnvelope)?;
    Ok(envelope)
}

fn decode_worker_event(
    bytes: &[u8],
    terminated: bool,
) -> Result<Envelope<WorkerEvent>, DecodeError> {
    let kind = decode_header(bytes, terminated)?;
    if is_command_kind(&kind) {
        return Err(DecodeError::WrongDirection);
    }
    let envelope: Envelope<WorkerEvent> = decode_typed(bytes, terminated)?;
    validate_worker_event_envelope(&envelope).map_err(|_| DecodeError::InvalidEnvelope)?;
    Ok(envelope)
}

fn decode_header(bytes: &[u8], terminated: bool) -> Result<String, DecodeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)?;
    let Some(first) = text.bytes().find(|byte| !byte.is_ascii_whitespace()) else {
        return Err(DecodeError::MalformedJson);
    };
    if first != b'{' {
        return Err(DecodeError::WrongTopLevel);
    }
    let header: VersionHeader = decode_typed(bytes, terminated)?;
    if header.version != PROTOCOL_VERSION {
        return Err(DecodeError::UnsupportedVersion {
            received: header.version,
        });
    }
    let _ = (
        header.worker_session_id,
        header.run_id,
        header.source_revision,
        header.request_id,
        header.sequence,
        header.payload,
    );
    let direction: DirectionHeader = decode_typed(bytes, terminated)?;
    let _ = (
        direction.version,
        direction.worker_session_id,
        direction.run_id,
        direction.source_revision,
        direction.request_id,
        direction.sequence,
    );
    Ok(direction.payload.kind)
}

fn decode_typed<'a, T: Deserialize<'a>>(
    bytes: &'a [u8],
    terminated: bool,
) -> Result<T, DecodeError> {
    serde_json::from_slice(bytes).map_err(|error| {
        if !terminated && error.is_eof() {
            DecodeError::EofMidFrame
        } else if error.to_string().starts_with("recursion limit exceeded") {
            DecodeError::MalformedJson
        } else if error.is_data() {
            DecodeError::InvalidEnvelope
        } else {
            DecodeError::MalformedJson
        }
    })
}

fn is_command_kind(kind: &str) -> bool {
    matches!(
        kind,
        "load_and_run"
            | "load_and_debug"
            | "pause"
            | "continue"
            | "step_into"
            | "step_over"
            | "step_out"
            | "stop"
            | "provide_input"
            | "shutdown"
    )
}

fn is_worker_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "hello"
            | "output"
            | "output_truncated"
            | "command_rejected"
            | "input_requested"
            | "diagnostic"
            | "paused"
            | "completed"
            | "cancelled"
            | "faulted"
    )
}

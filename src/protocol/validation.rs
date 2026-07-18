use rlox::{
    BindingSnapshot, DebugValue, MAX_SNAPSHOT_JSON_BYTES, RevisionId, SnapshotReason, SourceId,
    SourceSpan, ValueKind, VmSnapshot,
};

use super::{
    Command, Envelope, EventSequence, MAX_CONTROL_TEXT_BYTES, MAX_OUTPUT_CHUNK_TEXT_BYTES,
    PROTOCOL_VERSION, RequestId, RunId, WorkerEvent, WorkerSessionId,
};

const MAX_REJECTION_CODE_BYTES: usize = 128;
const MAX_REJECTION_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamValidationError {
    Closed,
    UnsupportedVersion {
        received: u16,
    },
    ZeroWorkerSession,
    WrongWorkerSession {
        expected: WorkerSessionId,
        received: WorkerSessionId,
    },
    UnexpectedSequence {
        expected: EventSequence,
        received: EventSequence,
    },
    SequenceExhausted,
    ZeroRequest,
    StaleRequest {
        previous: RequestId,
        received: RequestId,
    },
    RequestExhausted,
    InvalidTuple,
    InvalidDocument,
    InvalidPayload,
    ExpectedHello,
    DuplicateHello,
}

#[derive(Clone, Debug)]
pub struct CommandStreamValidator {
    worker_session_id: WorkerSessionId,
    next_sequence: Option<EventSequence>,
    last_request: Option<RequestId>,
    closed: bool,
}

impl CommandStreamValidator {
    pub fn new(worker_session_id: WorkerSessionId) -> Result<Self, StreamValidationError> {
        if worker_session_id.0 == 0 {
            return Err(StreamValidationError::ZeroWorkerSession);
        }
        Ok(Self {
            worker_session_id,
            next_sequence: Some(EventSequence(1)),
            last_request: None,
            closed: false,
        })
    }

    pub fn validate(&mut self, envelope: &Envelope<Command>) -> Result<(), StreamValidationError> {
        if self.closed {
            return Err(StreamValidationError::Closed);
        }
        validate_common_header(envelope.version, envelope.worker_session_id)?;
        if envelope.worker_session_id != self.worker_session_id {
            return Err(StreamValidationError::WrongWorkerSession {
                expected: self.worker_session_id,
                received: envelope.worker_session_id,
            });
        }
        let expected = self
            .next_sequence
            .ok_or(StreamValidationError::SequenceExhausted)?;
        if envelope.sequence != expected {
            return Err(StreamValidationError::UnexpectedSequence {
                expected,
                received: envelope.sequence,
            });
        }
        if envelope.request_id.0 == 0 {
            return Err(StreamValidationError::ZeroRequest);
        }
        if let Some(previous) = self.last_request
            && envelope.request_id.0 <= previous.0
        {
            return Err(StreamValidationError::StaleRequest {
                previous,
                received: envelope.request_id,
            });
        }
        validate_command_envelope(envelope)?;

        let closing = envelope.payload.is_closing();
        if envelope.sequence.0 == u64::MAX && !closing {
            self.closed = true;
            return Err(StreamValidationError::SequenceExhausted);
        }
        if envelope.request_id.0 == u64::MAX && !closing {
            self.closed = true;
            return Err(StreamValidationError::RequestExhausted);
        }

        self.last_request = Some(envelope.request_id);
        if closing {
            self.closed = true;
            self.next_sequence = None;
        } else {
            self.next_sequence = envelope.sequence.0.checked_add(1).map(EventSequence);
            if self.next_sequence.is_none() {
                self.closed = true;
            }
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

#[derive(Clone, Debug)]
pub struct WorkerEventStreamValidator {
    worker_session_id: WorkerSessionId,
    next_sequence: Option<EventSequence>,
    hello_seen: bool,
    closed: bool,
}

impl WorkerEventStreamValidator {
    pub fn new(worker_session_id: WorkerSessionId) -> Result<Self, StreamValidationError> {
        if worker_session_id.0 == 0 {
            return Err(StreamValidationError::ZeroWorkerSession);
        }
        Ok(Self {
            worker_session_id,
            next_sequence: Some(EventSequence(1)),
            hello_seen: false,
            closed: false,
        })
    }

    pub fn validate(
        &mut self,
        envelope: &Envelope<WorkerEvent>,
    ) -> Result<(), StreamValidationError> {
        if self.closed {
            return Err(StreamValidationError::Closed);
        }
        validate_common_header(envelope.version, envelope.worker_session_id)?;
        if envelope.worker_session_id != self.worker_session_id {
            return Err(StreamValidationError::WrongWorkerSession {
                expected: self.worker_session_id,
                received: envelope.worker_session_id,
            });
        }
        let expected = self
            .next_sequence
            .ok_or(StreamValidationError::SequenceExhausted)?;
        if envelope.sequence != expected {
            return Err(StreamValidationError::UnexpectedSequence {
                expected,
                received: envelope.sequence,
            });
        }
        if !self.hello_seen && !matches!(envelope.payload, WorkerEvent::Hello) {
            return Err(StreamValidationError::ExpectedHello);
        }
        if self.hello_seen && matches!(envelope.payload, WorkerEvent::Hello) {
            return Err(StreamValidationError::DuplicateHello);
        }
        validate_worker_event_envelope(envelope)?;

        let closing = envelope.payload.is_terminal();
        if envelope.sequence.0 == u64::MAX && !closing {
            self.closed = true;
            return Err(StreamValidationError::SequenceExhausted);
        }

        self.hello_seen = true;
        if closing {
            self.closed = true;
            self.next_sequence = None;
        } else {
            self.next_sequence = envelope.sequence.0.checked_add(1).map(EventSequence);
            if self.next_sequence.is_none() {
                self.closed = true;
            }
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

pub(crate) fn validate_command_envelope(
    envelope: &Envelope<Command>,
) -> Result<(), StreamValidationError> {
    validate_common_header(envelope.version, envelope.worker_session_id)?;
    if envelope.sequence.0 == 0 || envelope.request_id.0 == 0 {
        return Err(StreamValidationError::InvalidTuple);
    }
    match &envelope.payload {
        Command::LoadAndRun { document } | Command::LoadAndDebug { document } => {
            if envelope.run_id.0 == 0
                || envelope.source_revision.0 == 0
                || document.revision != envelope.source_revision
                || document.validate().is_err()
            {
                return Err(StreamValidationError::InvalidDocument);
            }
        }
        Command::ProvideInput { in_reply_to, text } => {
            validate_optional_run_tuple(envelope.run_id, envelope.source_revision)?;
            if in_reply_to.0 == 0 || text.len() > MAX_CONTROL_TEXT_BYTES {
                return Err(StreamValidationError::InvalidPayload);
            }
        }
        _ => validate_optional_run_tuple(envelope.run_id, envelope.source_revision)?,
    }
    Ok(())
}

pub(crate) fn validate_worker_event_envelope(
    envelope: &Envelope<WorkerEvent>,
) -> Result<(), StreamValidationError> {
    validate_common_header(envelope.version, envelope.worker_session_id)?;
    if envelope.sequence.0 == 0 {
        return Err(StreamValidationError::InvalidTuple);
    }
    match &envelope.payload {
        WorkerEvent::Hello => {
            if envelope.sequence.0 != 1
                || envelope.run_id.0 != 0
                || envelope.source_revision.0 != 0
                || envelope.request_id.0 != 0
            {
                return Err(StreamValidationError::InvalidTuple);
            }
        }
        WorkerEvent::CommandRejected { code, message } => {
            if envelope.request_id.0 == 0 {
                return Err(StreamValidationError::ZeroRequest);
            }
            validate_optional_run_tuple(envelope.run_id, envelope.source_revision)?;
            if !valid_machine_code(code)
                || code.len() > MAX_REJECTION_CODE_BYTES
                || message.len() > MAX_REJECTION_MESSAGE_BYTES
            {
                return Err(StreamValidationError::InvalidPayload);
            }
        }
        event => {
            validate_live_tuple(
                envelope.run_id,
                envelope.source_revision,
                envelope.request_id,
            )?;
            validate_event_payload(event, envelope.source_revision)?;
        }
    }
    Ok(())
}

fn validate_event_payload(
    event: &WorkerEvent,
    revision: RevisionId,
) -> Result<(), StreamValidationError> {
    match event {
        WorkerEvent::Hello | WorkerEvent::CommandRejected { .. } => {}
        WorkerEvent::Output { text } => {
            if text.len() > MAX_OUTPUT_CHUNK_TEXT_BYTES {
                return Err(StreamValidationError::InvalidPayload);
            }
        }
        WorkerEvent::OutputTruncated | WorkerEvent::Completed => {}
        WorkerEvent::InputRequested { prompt } => {
            if prompt.len() > MAX_CONTROL_TEXT_BYTES {
                return Err(StreamValidationError::InvalidPayload);
            }
        }
        WorkerEvent::Diagnostic { diagnostic } => {
            diagnostic.validate()?;
            validate_span(diagnostic.span, None, Some(revision))?;
            for frame in &diagnostic.frames {
                validate_span(frame.span, Some(diagnostic.span.source_id), Some(revision))?;
            }
        }
        WorkerEvent::Paused { location, snapshot } => {
            if !matches!(snapshot.reason, SnapshotReason::Paused(_))
                || location.revision != revision
                || location.source_id.0 == 0
                || location.debug_point_id.0 == 0
                || location.activation_id.0 == 0
                || location.dynamic_event == 0
                || location.span != snapshot.current_span
                || location.span.source_id != location.source_id
                || location.span.revision != location.revision
            {
                return Err(StreamValidationError::InvalidPayload);
            }
            validate_snapshot(snapshot, Some(location.source_id), revision)?;
        }
        WorkerEvent::Cancelled { snapshot } => {
            if snapshot.reason != SnapshotReason::Cancelled {
                return Err(StreamValidationError::InvalidPayload);
            }
            validate_snapshot(snapshot, None, revision)?;
        }
        WorkerEvent::Faulted {
            diagnostic,
            snapshot,
        } => {
            if snapshot.reason != SnapshotReason::Faulted
                || diagnostic.span != snapshot.current_span
            {
                return Err(StreamValidationError::InvalidPayload);
            }
            diagnostic.validate()?;
            validate_snapshot(snapshot, Some(diagnostic.span.source_id), revision)?;
            validate_span(
                diagnostic.span,
                Some(snapshot.current_span.source_id),
                Some(revision),
            )?;
            for frame in &diagnostic.frames {
                validate_span(frame.span, Some(diagnostic.span.source_id), Some(revision))?;
            }
        }
    }
    Ok(())
}

fn validate_snapshot(
    snapshot: &VmSnapshot,
    source_id: Option<SourceId>,
    revision: RevisionId,
) -> Result<(), StreamValidationError> {
    validate_span(snapshot.current_span, source_id, Some(revision))?;
    let expected_source = snapshot.current_span.source_id;
    for frame in &snapshot.frames {
        if frame.activation_id.0 == 0 {
            return Err(StreamValidationError::InvalidPayload);
        }
        validate_span(frame.current_span, Some(expected_source), Some(revision))?;
        if let Some(call_site) = frame.call_site {
            validate_span(call_site, Some(expected_source), Some(revision))?;
        }
        for binding in frame
            .parameters
            .iter()
            .chain(&frame.locals)
            .chain(&frame.upvalues)
        {
            validate_binding(binding)?;
        }
    }
    for binding in &snapshot.globals {
        validate_binding(binding)?;
    }
    let estimated = snapshot
        .conservative_json_size()
        .map_err(|_| StreamValidationError::InvalidPayload)?;
    if estimated > MAX_SNAPSHOT_JSON_BYTES {
        return Err(StreamValidationError::InvalidPayload);
    }
    let actual = serde_json::to_vec(snapshot)
        .map_err(|_| StreamValidationError::InvalidPayload)?
        .len();
    if actual > estimated {
        return Err(StreamValidationError::InvalidPayload);
    }
    Ok(())
}

fn validate_binding(binding: &BindingSnapshot) -> Result<(), StreamValidationError> {
    if binding.binding_id.is_some_and(|id| id.0 == 0)
        || !value_kind_matches(binding.value_kind, &binding.value)
    {
        return Err(StreamValidationError::InvalidPayload);
    }
    validate_debug_value(&binding.value, 0)
}

fn validate_debug_value(value: &DebugValue, depth: usize) -> Result<(), StreamValidationError> {
    if depth > 16 {
        return Err(StreamValidationError::InvalidPayload);
    }
    match value {
        DebugValue::Number(value) if !valid_number_text(value) => {
            return Err(StreamValidationError::InvalidPayload);
        }
        DebugValue::List {
            object_id, items, ..
        } => {
            if *object_id == 0 {
                return Err(StreamValidationError::InvalidPayload);
            }
            for item in items {
                validate_debug_value(item, depth + 1)?;
            }
        }
        DebugValue::Cycle { object_id } if *object_id == 0 => {
            return Err(StreamValidationError::InvalidPayload);
        }
        _ => {}
    }
    Ok(())
}

fn value_kind_matches(kind: ValueKind, value: &DebugValue) -> bool {
    matches!(
        (kind, value),
        (ValueKind::Nil, DebugValue::Nil)
            | (ValueKind::Bool, DebugValue::Bool(_))
            | (ValueKind::Number, DebugValue::Number(_))
            | (ValueKind::String, DebugValue::String(_))
            | (ValueKind::Function, DebugValue::Function(_))
            | (ValueKind::Closure, DebugValue::Closure(_))
            | (ValueKind::Native, DebugValue::Native(_))
            | (ValueKind::List, DebugValue::List { .. })
            | (ValueKind::Cycle, DebugValue::Cycle { .. })
            | (ValueKind::Truncated, DebugValue::Truncated)
    )
}

fn validate_span(
    span: SourceSpan,
    source_id: Option<SourceId>,
    revision: Option<RevisionId>,
) -> Result<(), StreamValidationError> {
    if span.source_id.0 == 0
        || span.revision.0 == 0
        || source_id.is_some_and(|expected| span.source_id != expected)
        || revision.is_some_and(|expected| span.revision != expected)
        || span.start.line == 0
        || span.start.column == 0
        || span.end.line == 0
        || span.end.column == 0
        || span.start.byte_offset > span.end.byte_offset
        || (span.start.byte_offset == span.end.byte_offset
            && (span.start.line != span.end.line || span.start.column != span.end.column))
        || span.start.line > span.end.line
        || (span.start.line == span.end.line && span.start.column > span.end.column)
    {
        return Err(StreamValidationError::InvalidPayload);
    }
    Ok(())
}

fn validate_common_header(
    version: u16,
    worker_session_id: WorkerSessionId,
) -> Result<(), StreamValidationError> {
    if version != PROTOCOL_VERSION {
        return Err(StreamValidationError::UnsupportedVersion { received: version });
    }
    if worker_session_id.0 == 0 {
        return Err(StreamValidationError::ZeroWorkerSession);
    }
    Ok(())
}

fn validate_optional_run_tuple(
    run_id: RunId,
    revision: RevisionId,
) -> Result<(), StreamValidationError> {
    if (run_id.0 == 0) != (revision.0 == 0) {
        return Err(StreamValidationError::InvalidTuple);
    }
    Ok(())
}

fn validate_live_tuple(
    run_id: RunId,
    revision: RevisionId,
    request_id: RequestId,
) -> Result<(), StreamValidationError> {
    if run_id.0 == 0 || revision.0 == 0 || request_id.0 == 0 {
        return Err(StreamValidationError::InvalidTuple);
    }
    Ok(())
}

fn valid_machine_code(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_number_text(value: &str) -> bool {
    if matches!(value, "nan" | "infinity" | "-infinity" | "-0") {
        return true;
    }
    value
        .parse::<f64>()
        .is_ok_and(|number| number.is_finite() && number.to_string() == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Command, Envelope, WorkerEvent};

    fn command(sequence: u64, payload: Command) -> Envelope<Command> {
        Envelope {
            version: PROTOCOL_VERSION,
            worker_session_id: WorkerSessionId(7),
            run_id: RunId(1),
            source_revision: RevisionId(1),
            request_id: RequestId(1),
            sequence: EventSequence(sequence),
            payload,
        }
    }

    fn event(sequence: u64, payload: WorkerEvent) -> Envelope<WorkerEvent> {
        Envelope {
            version: PROTOCOL_VERSION,
            worker_session_id: WorkerSessionId(7),
            run_id: RunId(1),
            source_revision: RevisionId(1),
            request_id: RequestId(1),
            sequence: EventSequence(sequence),
            payload,
        }
    }

    #[test]
    fn final_sequence_is_reserved_for_closing_frames() {
        let mut commands = CommandStreamValidator {
            worker_session_id: WorkerSessionId(7),
            next_sequence: Some(EventSequence(u64::MAX)),
            last_request: None,
            closed: false,
        };
        assert_eq!(
            commands.validate(&command(u64::MAX, Command::Pause)),
            Err(StreamValidationError::SequenceExhausted)
        );
        assert!(commands.is_closed());

        let mut commands = CommandStreamValidator {
            worker_session_id: WorkerSessionId(7),
            next_sequence: Some(EventSequence(u64::MAX)),
            last_request: None,
            closed: false,
        };
        commands
            .validate(&command(u64::MAX, Command::Shutdown))
            .unwrap();
        assert!(commands.is_closed());

        let mut events = WorkerEventStreamValidator {
            worker_session_id: WorkerSessionId(7),
            next_sequence: Some(EventSequence(u64::MAX)),
            hello_seen: true,
            closed: false,
        };
        assert_eq!(
            events.validate(&event(
                u64::MAX,
                WorkerEvent::Output {
                    text: "x".to_string(),
                }
            )),
            Err(StreamValidationError::SequenceExhausted)
        );
        assert!(events.is_closed());

        let mut events = WorkerEventStreamValidator {
            worker_session_id: WorkerSessionId(7),
            next_sequence: Some(EventSequence(u64::MAX)),
            hello_seen: true,
            closed: false,
        };
        events
            .validate(&event(u64::MAX, WorkerEvent::Completed))
            .unwrap();
        assert!(events.is_closed());
    }
}

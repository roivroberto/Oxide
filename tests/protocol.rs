use std::io::{self, BufRead, BufReader, Cursor, Read, Write};

use oxide_ide::{
    Command, CommandStreamValidator, DecodeError, EncodeError, Envelope, EventSequence, LineCodec,
    MAX_DIAGNOSTIC_CODE_BYTES, MAX_DIAGNOSTIC_FRAMES, MAX_DIAGNOSTIC_FUNCTION_BYTES,
    MAX_DIAGNOSTIC_MESSAGE_BYTES, MAX_OUTPUT_CHUNK_TEXT_BYTES, MAX_PAYLOAD_BYTES, PROTOCOL_VERSION,
    RequestId, RunId, StreamValidationError, WireDiagnostic, WireDocument, WireDocumentError,
    WireRuntimeFrame, WorkerEvent, WorkerEventStreamValidator, WorkerSessionId,
};
use rlox::{
    ActivationId, BindingId, BindingSnapshot, DebugPointId, DebugValue, Diagnostic,
    DiagnosticPhase, DiagnosticSeverity, FrameSnapshot, MAX_SNAPSHOT_JSON_BYTES, PauseLocation,
    PauseReason, RevisionId, RuntimeFrame, SnapshotReason, SourceId, SourceSpan, TextPosition,
    ValueKind, VmSnapshot,
};
use serde::{Serialize, de::DeserializeOwned};

fn span(source_id: u64, revision: u64) -> SourceSpan {
    SourceSpan {
        source_id: SourceId(source_id),
        revision: RevisionId(revision),
        start: TextPosition {
            byte_offset: 0,
            line: 1,
            column: 1,
        },
        end: TextPosition {
            byte_offset: 5,
            line: 1,
            column: 6,
        },
    }
}

fn snapshot(reason: SnapshotReason, source_id: u64, revision: u64) -> VmSnapshot {
    let source_span = span(source_id, revision);
    VmSnapshot {
        reason,
        current_span: source_span,
        frames: vec![FrameSnapshot {
            activation_id: ActivationId(1),
            function: "<script>".to_string(),
            function_truncated: false,
            current_span: source_span,
            call_site: None,
            parameters: Vec::new(),
            parameters_truncated: false,
            locals: vec![BindingSnapshot {
                binding_id: Some(BindingId(1)),
                name: "answer".to_string(),
                name_truncated: false,
                binding_kind: "local".to_string(),
                value_kind: ValueKind::Number,
                value: DebugValue::Number("42".to_string()),
            }],
            locals_truncated: false,
            upvalues: Vec::new(),
            upvalues_truncated: false,
        }],
        frames_truncated: false,
        globals: Vec::new(),
        globals_truncated: false,
    }
}

fn diagnostic(source_id: u64, revision: u64) -> Diagnostic {
    Diagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "runtime.type".to_string(),
        message: "Operands must be numbers.".to_string(),
        span: span(source_id, revision),
        frames: vec![RuntimeFrame {
            function: "<script>".to_string(),
            span: span(source_id, revision),
        }],
    }
}

fn document() -> WireDocument {
    WireDocument {
        source_id: SourceId(21),
        revision: RevisionId(9),
        display_name: "main.lox".to_string(),
        text: "print 1;\n".to_string(),
    }
}

fn command_envelope(sequence: u64, request: u64, payload: Command) -> Envelope<Command> {
    Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: WorkerSessionId(7),
        run_id: RunId(4),
        source_revision: RevisionId(9),
        request_id: RequestId(request),
        sequence: EventSequence(sequence),
        payload,
    }
}

fn hello() -> Envelope<WorkerEvent> {
    Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: WorkerSessionId(7),
        run_id: RunId(0),
        source_revision: RevisionId(0),
        request_id: RequestId(0),
        sequence: EventSequence(1),
        payload: WorkerEvent::Hello,
    }
}

fn event_envelope(sequence: u64, request: u64, payload: WorkerEvent) -> Envelope<WorkerEvent> {
    Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: WorkerSessionId(7),
        run_id: RunId(4),
        source_revision: RevisionId(9),
        request_id: RequestId(request),
        sequence: EventSequence(sequence),
        payload,
    }
}

#[test]
fn output_text_has_a_fixed_wire_chunk_limit() {
    let maximum = event_envelope(
        2,
        1,
        WorkerEvent::Output {
            text: "x".repeat(MAX_OUTPUT_CHUNK_TEXT_BYTES),
        },
    );
    assert!(
        LineCodec::new()
            .write_worker_event(&mut Vec::new(), &maximum)
            .is_ok()
    );

    let oversized = event_envelope(
        2,
        1,
        WorkerEvent::Output {
            text: "x".repeat(MAX_OUTPUT_CHUNK_TEXT_BYTES + 1),
        },
    );
    assert_eq!(
        LineCodec::new().write_worker_event(&mut Vec::new(), &oversized),
        Err(EncodeError::InvalidEnvelope)
    );
}

fn round_trip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = serde_json::to_vec(value).unwrap();
    assert_eq!(serde_json::from_slice::<T>(&bytes).unwrap(), *value);
}

fn assert_golden<T>(value: &T, expected: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_string(value).unwrap(), expected);
    assert_eq!(serde_json::from_str::<T>(expected).unwrap(), *value);
}

#[test]
fn canonical_envelopes_and_all_payload_variants_round_trip() {
    let hello = hello();
    assert_eq!(
        serde_json::to_string(&hello).unwrap(),
        r#"{"version":1,"worker_session_id":7,"run_id":0,"source_revision":0,"request_id":0,"sequence":1,"payload":{"kind":"hello"}}"#
    );
    round_trip(&hello);

    let load = command_envelope(
        1,
        11,
        Command::LoadAndRun {
            document: document(),
        },
    );
    assert_eq!(
        serde_json::to_string(&load).unwrap(),
        r#"{"version":1,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":11,"sequence":1,"payload":{"kind":"load_and_run","payload":{"document":{"source_id":21,"revision":9,"display_name":"main.lox","text":"print 1;\n"}}}}"#
    );
    round_trip(&load);

    let commands = vec![
        Command::LoadAndRun {
            document: document(),
        },
        Command::LoadAndDebug {
            document: document(),
        },
        Command::Pause,
        Command::Continue,
        Command::StepInto,
        Command::StepOver,
        Command::StepOut,
        Command::Stop,
        Command::ProvideInput {
            in_reply_to: RequestId(11),
            text: "hello\n".to_string(),
        },
        Command::Shutdown,
    ];
    for command in commands {
        round_trip(&command);
    }

    let paused_snapshot = snapshot(SnapshotReason::Paused(PauseReason::Explicit), 21, 9);
    let terminal_snapshot = snapshot(SnapshotReason::Cancelled, 21, 9);
    let fault_snapshot = snapshot(SnapshotReason::Faulted, 21, 9);
    let location = PauseLocation {
        source_id: SourceId(21),
        revision: RevisionId(9),
        span: span(21, 9),
        debug_point_id: DebugPointId(3),
        activation_id: ActivationId(1),
        dynamic_event: 4,
    };
    let wire_diagnostic = WireDiagnostic::from(diagnostic(21, 9));
    let events = vec![
        WorkerEvent::Hello,
        WorkerEvent::Output {
            text: "hello\n".to_string(),
        },
        WorkerEvent::OutputTruncated,
        WorkerEvent::CommandRejected {
            code: "command.invalid_state".to_string(),
            message: "Continue requires a paused run.".to_string(),
        },
        WorkerEvent::InputRequested {
            prompt: "> ".to_string(),
        },
        WorkerEvent::Diagnostic {
            diagnostic: wire_diagnostic.clone(),
        },
        WorkerEvent::Paused {
            location,
            snapshot: paused_snapshot,
        },
        WorkerEvent::Completed,
        WorkerEvent::Cancelled {
            snapshot: terminal_snapshot,
        },
        WorkerEvent::Faulted {
            diagnostic: wire_diagnostic,
            snapshot: fault_snapshot,
        },
    ];
    for event in events {
        round_trip(&event);
    }
}

#[test]
fn every_command_and_worker_event_has_a_stable_compact_json_shape() {
    let document_json =
        r#"{"source_id":21,"revision":9,"display_name":"main.lox","text":"print 1;\n"}"#;
    let command_goldens = vec![
        (
            Command::LoadAndRun {
                document: document(),
            },
            format!(r#"{{"kind":"load_and_run","payload":{{"document":{document_json}}}}}"#),
        ),
        (
            Command::LoadAndDebug {
                document: document(),
            },
            format!(r#"{{"kind":"load_and_debug","payload":{{"document":{document_json}}}}}"#),
        ),
        (Command::Pause, r#"{"kind":"pause"}"#.to_string()),
        (Command::Continue, r#"{"kind":"continue"}"#.to_string()),
        (Command::StepInto, r#"{"kind":"step_into"}"#.to_string()),
        (Command::StepOver, r#"{"kind":"step_over"}"#.to_string()),
        (Command::StepOut, r#"{"kind":"step_out"}"#.to_string()),
        (Command::Stop, r#"{"kind":"stop"}"#.to_string()),
        (
            Command::ProvideInput {
                in_reply_to: RequestId(12),
                text: "hello\n".to_string(),
            },
            r#"{"kind":"provide_input","payload":{"in_reply_to":12,"text":"hello\n"}}"#.to_string(),
        ),
        (Command::Shutdown, r#"{"kind":"shutdown"}"#.to_string()),
    ];
    for (value, expected) in command_goldens {
        assert_golden(&value, &expected);
    }

    let span_json = r#"{"source_id":21,"revision":9,"start":{"byte_offset":0,"line":1,"column":1},"end":{"byte_offset":5,"line":1,"column":6}}"#;
    let paused_snapshot_json = format!(
        r#"{{"reason":{{"kind":"paused","payload":"explicit"}},"current_span":{span_json},"frames":[],"frames_truncated":false,"globals":[],"globals_truncated":false}}"#
    );
    let cancelled_snapshot_json = format!(
        r#"{{"reason":{{"kind":"cancelled"}},"current_span":{span_json},"frames":[],"frames_truncated":false,"globals":[],"globals_truncated":false}}"#
    );
    let faulted_snapshot_json = format!(
        r#"{{"reason":{{"kind":"faulted"}},"current_span":{span_json},"frames":[],"frames_truncated":false,"globals":[],"globals_truncated":false}}"#
    );
    let diagnostic_json = format!(
        r#"{{"phase":"runtime","severity":"error","code":"runtime.type","code_truncated":false,"message":"Operands must be numbers.","message_truncated":false,"span":{span_json},"frames":[],"frames_truncated":false}}"#
    );
    let empty_paused = VmSnapshot {
        reason: SnapshotReason::Paused(PauseReason::Explicit),
        current_span: span(21, 9),
        frames: Vec::new(),
        frames_truncated: false,
        globals: Vec::new(),
        globals_truncated: false,
    };
    let empty_cancelled = VmSnapshot {
        reason: SnapshotReason::Cancelled,
        ..empty_paused.clone()
    };
    let empty_faulted = VmSnapshot {
        reason: SnapshotReason::Faulted,
        ..empty_paused.clone()
    };
    let empty_diagnostic = WireDiagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "runtime.type".to_string(),
        code_truncated: false,
        message: "Operands must be numbers.".to_string(),
        message_truncated: false,
        span: span(21, 9),
        frames: Vec::new(),
        frames_truncated: false,
    };
    let location = PauseLocation {
        source_id: SourceId(21),
        revision: RevisionId(9),
        span: span(21, 9),
        debug_point_id: DebugPointId(3),
        activation_id: ActivationId(1),
        dynamic_event: 4,
    };

    let event_goldens = vec![
        (WorkerEvent::Hello, r#"{"kind":"hello"}"#.to_string()),
        (
            WorkerEvent::Output {
                text: "hello\n".to_string(),
            },
            r#"{"kind":"output","payload":{"text":"hello\n"}}"#.to_string(),
        ),
        (
            WorkerEvent::OutputTruncated,
            r#"{"kind":"output_truncated"}"#.to_string(),
        ),
        (
            WorkerEvent::CommandRejected {
                code: "command.invalid_state".to_string(),
                message: "Continue requires a paused run.".to_string(),
            },
            r#"{"kind":"command_rejected","payload":{"code":"command.invalid_state","message":"Continue requires a paused run."}}"#.to_string(),
        ),
        (
            WorkerEvent::InputRequested {
                prompt: "> ".to_string(),
            },
            r#"{"kind":"input_requested","payload":{"prompt":"> "}}"#.to_string(),
        ),
        (
            WorkerEvent::Diagnostic {
                diagnostic: empty_diagnostic.clone(),
            },
            format!(r#"{{"kind":"diagnostic","payload":{{"diagnostic":{diagnostic_json}}}}}"#),
        ),
        (
            WorkerEvent::Paused {
                location,
                snapshot: empty_paused.clone(),
            },
            format!(
                r#"{{"kind":"paused","payload":{{"location":{{"source_id":21,"revision":9,"span":{span_json},"debug_point_id":3,"activation_id":1,"dynamic_event":4}},"snapshot":{paused_snapshot_json}}}}}"#
            ),
        ),
        (
            WorkerEvent::Completed,
            r#"{"kind":"completed"}"#.to_string(),
        ),
        (
            WorkerEvent::Cancelled {
                snapshot: empty_cancelled,
            },
            format!(
                r#"{{"kind":"cancelled","payload":{{"snapshot":{cancelled_snapshot_json}}}}}"#
            ),
        ),
        (
            WorkerEvent::Faulted {
                diagnostic: empty_diagnostic,
                snapshot: empty_faulted,
            },
            format!(
                r#"{{"kind":"faulted","payload":{{"diagnostic":{diagnostic_json},"snapshot":{faulted_snapshot_json}}}}}"#
            ),
        ),
    ];
    for (value, expected) in event_goldens {
        assert_golden(&value, &expected);
    }

    let paused_envelope = event_envelope(
        8,
        12,
        WorkerEvent::Paused {
            location,
            snapshot: empty_paused,
        },
    );
    assert_golden(
        &paused_envelope,
        &format!(
            r#"{{"version":1,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":12,"sequence":8,"payload":{{"kind":"paused","payload":{{"location":{{"source_id":21,"revision":9,"span":{span_json},"debug_point_id":3,"activation_id":1,"dynamic_event":4}},"snapshot":{paused_snapshot_json}}}}}}}"#
        ),
    );
}

#[test]
fn strict_tagged_shapes_reject_null_duplicates_and_unknown_fields() {
    for json in [
        r#"{"kind":"pause","payload":null}"#,
        r#"{"kind":"shutdown","payload":{}}"#,
        r#"{"kind":"pause","extra":0}"#,
        r#"{"kind":"pause","kind":"stop"}"#,
        r#"{"kind":"provide_input","payload":{"in_reply_to":1,"text":"x","extra":0}}"#,
    ] {
        assert!(
            serde_json::from_str::<Command>(json).is_err(),
            "accepted {json}"
        );
    }
    for json in [
        r#"{"kind":"hello","payload":null}"#,
        r#"{"kind":"completed","payload":{}}"#,
        r#"{"kind":"output_truncated","extra":0}"#,
        r#"{"kind":"output","payload":{"text":"x","extra":0}}"#,
    ] {
        assert!(
            serde_json::from_str::<WorkerEvent>(json).is_err(),
            "accepted {json}"
        );
    }

    assert_eq!(
        serde_json::from_str::<Command>(
            r#"{"payload":{"in_reply_to":1,"text":"x"},"kind":"provide_input"}"#
        )
        .unwrap(),
        Command::ProvideInput {
            in_reply_to: RequestId(1),
            text: "x".to_string(),
        }
    );
}

#[test]
fn wire_documents_are_normalized_and_never_carry_paths() {
    let valid = document();
    valid.validate().unwrap();
    let source = valid.clone().into_source_document().unwrap();
    assert_eq!(&*source.text, valid.text);
    assert_eq!(source.name, "main.lox");

    let invalid = [
        (
            WireDocument {
                source_id: SourceId(0),
                ..document()
            },
            WireDocumentError::ZeroSourceId,
        ),
        (
            WireDocument {
                revision: RevisionId(0),
                ..document()
            },
            WireDocumentError::ZeroRevision,
        ),
        (
            WireDocument {
                display_name: String::new(),
                ..document()
            },
            WireDocumentError::EmptyDisplayName,
        ),
        (
            WireDocument {
                display_name: "C:\\course\\main.lox".to_string(),
                ..document()
            },
            WireDocumentError::InvalidDisplayName,
        ),
        (
            WireDocument {
                display_name: "folder/main.lox".to_string(),
                ..document()
            },
            WireDocumentError::InvalidDisplayName,
        ),
        (
            WireDocument {
                text: "\u{feff}print 1;\n".to_string(),
                ..document()
            },
            WireDocumentError::NonNormalizedText,
        ),
        (
            WireDocument {
                text: "print 1;\r\n".to_string(),
                ..document()
            },
            WireDocumentError::NonNormalizedText,
        ),
        (
            WireDocument {
                text: "\0".repeat(2 * 1024 * 1024),
                ..document()
            },
            WireDocumentError::SourceTooLarge,
        ),
    ];
    for (value, expected) in invalid {
        assert_eq!(value.validate(), Err(expected));
    }
}

#[test]
fn bounded_diagnostics_truncate_at_utf8_boundaries_and_preserve_order() {
    let function = format!("{}x", "😀".repeat(MAX_DIAGNOSTIC_FUNCTION_BYTES / 4));
    let frames = (0..=MAX_DIAGNOSTIC_FRAMES)
        .map(|index| RuntimeFrame {
            function: format!("{index}:{function}"),
            span: span(21, 9),
        })
        .collect();
    let value = Diagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "INVALID CODE".to_string(),
        message: format!("{}x", "😀".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES / 4)),
        span: span(21, 9),
        frames,
    };
    let wire = WireDiagnostic::from(&value);

    assert_eq!(wire.code, "worker.invalid_diagnostic_code");
    assert!(wire.code_truncated);
    assert_eq!(wire.message.len(), MAX_DIAGNOSTIC_MESSAGE_BYTES);
    assert!(wire.message.is_char_boundary(wire.message.len()));
    assert!(wire.message_truncated);
    assert_eq!(wire.frames.len(), MAX_DIAGNOSTIC_FRAMES);
    assert!(wire.frames_truncated);
    assert!(wire.frames[0].function.starts_with("0:"));
    assert!(wire.frames[0].function.len() <= MAX_DIAGNOSTIC_FUNCTION_BYTES);
    assert!(wire.frames[0].function_truncated);
    wire.validate().unwrap();
}

#[test]
fn codec_round_trips_fragmented_combined_crlf_and_final_eof_frames() {
    let first = command_envelope(1, 1, Command::Pause);
    let second = command_envelope(2, 2, Command::Continue);
    let mut bytes = serde_json::to_vec(&first).unwrap();
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(&serde_json::to_vec(&second).unwrap());
    bytes.push(b'\n');

    let mut reader = BufReader::with_capacity(3, Cursor::new(bytes));
    let mut codec = LineCodec::new();
    assert_eq!(codec.read_command(&mut reader).unwrap(), Some(first));
    assert_eq!(codec.read_command(&mut reader).unwrap(), Some(second));
    assert_eq!(codec.read_command(&mut reader).unwrap(), None);

    let final_frame = command_envelope(1, 1, Command::StepInto);
    let mut final_bytes = serde_json::to_vec(&final_frame).unwrap();
    final_bytes.push(b'\r');
    let mut reader = Cursor::new(final_bytes);
    let mut codec = LineCodec::new();
    assert_eq!(codec.read_command(&mut reader).unwrap(), Some(final_frame));
    assert_eq!(codec.read_command(&mut reader).unwrap(), None);
}

#[test]
fn codec_classifies_framing_schema_version_and_direction_errors() {
    let cases: Vec<(Vec<u8>, DecodeError)> = vec![
        (b"\n".to_vec(), DecodeError::BlankFrame),
        (b"\r\n".to_vec(), DecodeError::BlankFrame),
        (b" \t\n".to_vec(), DecodeError::MalformedJson),
        (b"[]\n".to_vec(), DecodeError::WrongTopLevel),
        (vec![0xff, b'\n'], DecodeError::InvalidUtf8),
        (b"{\"version\":1\n".to_vec(), DecodeError::MalformedJson),
        (b"{\"version\":1".to_vec(), DecodeError::EofMidFrame),
    ];
    for (bytes, expected) in cases {
        let mut codec = LineCodec::new();
        let mut reader = Cursor::new(bytes);
        assert_eq!(codec.read_command(&mut reader), Err(expected));
    }

    let wrong_version = br#"{"version":2,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":{"kind":"pause","payload":null}}
"#;
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(wrong_version)),
        Err(DecodeError::UnsupportedVersion { received: 2 })
    );
    for wrong_version_payload in [
        br#"{"version":2,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":null}
"#.as_slice(),
        br#"{"version":2,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":{"future":true}}
"#.as_slice(),
    ] {
        let mut codec = LineCodec::new();
        assert_eq!(
            codec.read_command(&mut Cursor::new(wrong_version_payload)),
            Err(DecodeError::UnsupportedVersion { received: 2 })
        );
    }

    let mut event = serde_json::to_vec(&hello()).unwrap();
    event.push(b'\n');
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(event)),
        Err(DecodeError::WrongDirection)
    );

    let duplicate = br#"{"version":1,"version":1,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":{"kind":"pause"}}
"#;
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(duplicate)),
        Err(DecodeError::InvalidEnvelope)
    );

    for frame in [
        br#"{"version":1,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":{"kind":"pause","payload":null}}
"#.as_slice(),
        br#"{"version":1,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":{"kind":"unknown"}}
"#.as_slice(),
    ] {
        let mut codec = LineCodec::new();
        assert_eq!(
            codec.read_command(&mut Cursor::new(frame)),
            Err(DecodeError::InvalidEnvelope)
        );
    }

    let trailing = br#"{"version":1,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":{"kind":"pause"}} {}
"#;
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(trailing)),
        Err(DecodeError::MalformedJson)
    );

    let nesting = 160;
    let mut deep = br#"{"version":1,"worker_session_id":7,"run_id":4,"source_revision":9,"request_id":1,"sequence":1,"payload":{"kind":"pause","extra":"#
        .to_vec();
    deep.extend(std::iter::repeat_n(b'[', nesting));
    deep.extend(std::iter::repeat_n(b']', nesting));
    deep.extend_from_slice(b"}}\n");
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(deep)),
        Err(DecodeError::InvalidEnvelope)
    );
}

#[test]
fn codec_drains_invalid_and_oversized_frames_before_recovery() {
    let valid = command_envelope(1, 1, Command::Pause);
    let mut bytes = b"not json\n".to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(&valid).unwrap());
    bytes.push(b'\n');
    let mut reader = Cursor::new(bytes);
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut reader),
        Err(DecodeError::WrongTopLevel)
    );
    assert_eq!(
        codec.read_command(&mut reader).unwrap(),
        Some(valid.clone())
    );

    let mut bytes = vec![b'x'; MAX_PAYLOAD_BYTES + 1];
    bytes.push(b'\n');
    bytes.extend_from_slice(&serde_json::to_vec(&valid).unwrap());
    bytes.push(b'\n');
    let mut reader = Cursor::new(bytes);
    let mut codec = LineCodec::new();
    assert_eq!(codec.read_command(&mut reader), Err(DecodeError::Oversized));
    assert_eq!(codec.read_command(&mut reader).unwrap(), Some(valid));
}

#[test]
fn payload_limit_counts_json_but_not_lf_or_crlf_delimiters() {
    let value = command_envelope(1, 1, Command::Pause);
    let base = serde_json::to_vec(&value).unwrap();

    let mut exact_lf = base.clone();
    exact_lf.resize(MAX_PAYLOAD_BYTES, b' ');
    exact_lf.push(b'\n');
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(exact_lf)).unwrap(),
        Some(value.clone())
    );

    let mut exact_crlf = base.clone();
    exact_crlf.resize(MAX_PAYLOAD_BYTES, b' ');
    exact_crlf.extend_from_slice(b"\r\n");
    let mut codec = LineCodec::new();
    assert_eq!(
        codec
            .read_command(&mut BufReader::with_capacity(1, Cursor::new(exact_crlf)))
            .unwrap(),
        Some(value.clone())
    );

    let mut exact_eof = base.clone();
    exact_eof.resize(MAX_PAYLOAD_BYTES, b' ');
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(exact_eof)).unwrap(),
        Some(value)
    );

    let mut oversized = base;
    oversized.resize(MAX_PAYLOAD_BYTES + 1, b' ');
    oversized.push(b'\n');
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(oversized)),
        Err(DecodeError::Oversized)
    );

    let base = serde_json::to_vec(&command_envelope(1, 1, Command::Pause)).unwrap();
    let mut oversized_crlf = base.clone();
    oversized_crlf.resize(MAX_PAYLOAD_BYTES + 1, b' ');
    oversized_crlf.extend_from_slice(b"\r\n");
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(oversized_crlf)),
        Err(DecodeError::Oversized)
    );

    let mut oversized_bare_cr = base;
    oversized_bare_cr.resize(MAX_PAYLOAD_BYTES, b' ');
    oversized_bare_cr.push(b'\r');
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut Cursor::new(oversized_bare_cr)),
        Err(DecodeError::Oversized)
    );
}

#[test]
fn encode_is_atomic_before_external_io_and_poisoned_after_partial_io() {
    let mut invalid = command_envelope(1, 1, Command::Pause);
    invalid.worker_session_id = WorkerSessionId(0);
    let mut destination = b"prefix".to_vec();
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.write_command(&mut destination, &invalid),
        Err(EncodeError::InvalidEnvelope)
    );
    assert_eq!(destination, b"prefix");

    let oversized = event_envelope(
        1,
        1,
        WorkerEvent::Output {
            text: "x".repeat(MAX_PAYLOAD_BYTES),
        },
    );
    assert_eq!(
        codec.write_worker_event(&mut destination, &oversized),
        Err(EncodeError::InvalidEnvelope)
    );
    assert_eq!(destination, b"prefix");

    let valid = command_envelope(1, 1, Command::Pause);
    let mut writer = FailAfter::new(8);
    assert_eq!(
        codec.write_command(&mut writer, &valid),
        Err(EncodeError::Io(io::ErrorKind::BrokenPipe))
    );
    let written = writer.bytes.len();
    assert!(written > 0);
    assert_eq!(
        codec.write_command(&mut writer, &valid),
        Err(EncodeError::Poisoned)
    );
    assert_eq!(writer.bytes.len(), written);
}

#[test]
fn flush_failure_poisons_future_writes() {
    let valid = command_envelope(1, 1, Command::Pause);
    let mut writer = FlushFails { bytes: Vec::new() };
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.write_command(&mut writer, &valid),
        Err(EncodeError::Io(io::ErrorKind::BrokenPipe))
    );
    let written = writer.bytes.len();
    assert!(written > 0);
    assert_eq!(
        codec.write_command(&mut writer, &valid),
        Err(EncodeError::Poisoned)
    );
    assert_eq!(writer.bytes.len(), written);
}

#[test]
fn read_io_failure_poisons_only_the_decoder() {
    let mut reader = FailingReader;
    let mut codec = LineCodec::new();
    assert_eq!(
        codec.read_command(&mut reader),
        Err(DecodeError::Io(io::ErrorKind::BrokenPipe))
    );
    assert_eq!(codec.read_command(&mut reader), Err(DecodeError::Poisoned));

    let valid = command_envelope(1, 1, Command::Pause);
    let mut output = Vec::new();
    assert!(codec.write_command(&mut output, &valid).is_ok());
}

#[test]
fn command_and_event_sequence_streams_are_independent_and_transactional() {
    let mut commands = CommandStreamValidator::new(WorkerSessionId(7)).unwrap();
    let mut events = WorkerEventStreamValidator::new(WorkerSessionId(7)).unwrap();
    events.validate(&hello()).unwrap();

    let first = command_envelope(1, 1, Command::Pause);
    commands.validate(&first).unwrap();
    assert_eq!(
        commands.validate(&first),
        Err(StreamValidationError::UnexpectedSequence {
            expected: EventSequence(2),
            received: EventSequence(1),
        })
    );

    let stale_request = command_envelope(2, 1, Command::Continue);
    assert_eq!(
        commands.validate(&stale_request),
        Err(StreamValidationError::StaleRequest {
            previous: RequestId(1),
            received: RequestId(1),
        })
    );
    commands
        .validate(&command_envelope(2, 2, Command::Continue))
        .unwrap();

    events
        .validate(&event_envelope(
            2,
            1,
            WorkerEvent::Output {
                text: "ok\n".to_string(),
            },
        ))
        .unwrap();
    events
        .validate(&event_envelope(3, 1, WorkerEvent::Completed))
        .unwrap();
    assert!(events.is_closed());
    assert_eq!(
        events.validate(&event_envelope(4, 2, WorkerEvent::Completed)),
        Err(StreamValidationError::Closed)
    );

    commands
        .validate(&command_envelope(3, 3, Command::Shutdown))
        .unwrap();
    assert!(commands.is_closed());
}

#[test]
fn final_request_id_is_reserved_for_shutdown() {
    let mut rejected = CommandStreamValidator::new(WorkerSessionId(7)).unwrap();
    assert_eq!(
        rejected.validate(&command_envelope(1, u64::MAX, Command::Pause)),
        Err(StreamValidationError::RequestExhausted)
    );
    assert!(rejected.is_closed());

    let mut accepted = CommandStreamValidator::new(WorkerSessionId(7)).unwrap();
    accepted
        .validate(&command_envelope(1, u64::MAX, Command::Shutdown))
        .unwrap();
    assert!(accepted.is_closed());
}

#[test]
fn semantic_validation_rejects_inconsistent_snapshots_without_advancing() {
    let mut events = WorkerEventStreamValidator::new(WorkerSessionId(7)).unwrap();
    events.validate(&hello()).unwrap();

    let invalid = event_envelope(
        2,
        1,
        WorkerEvent::Paused {
            location: PauseLocation {
                source_id: SourceId(21),
                revision: RevisionId(9),
                span: span(21, 9),
                debug_point_id: DebugPointId(1),
                activation_id: ActivationId(1),
                dynamic_event: 1,
            },
            snapshot: snapshot(SnapshotReason::Faulted, 21, 9),
        },
    );
    assert_eq!(
        events.validate(&invalid),
        Err(StreamValidationError::InvalidPayload)
    );

    let mut impossible_span = span(21, 9);
    impossible_span.end.byte_offset = impossible_span.start.byte_offset;
    impossible_span.end.column = impossible_span.start.column + 1;
    let impossible = event_envelope(
        2,
        1,
        WorkerEvent::Paused {
            location: PauseLocation {
                source_id: SourceId(21),
                revision: RevisionId(9),
                span: impossible_span,
                debug_point_id: DebugPointId(1),
                activation_id: ActivationId(1),
                dynamic_event: 1,
            },
            snapshot: VmSnapshot {
                reason: SnapshotReason::Paused(PauseReason::Explicit),
                current_span: impossible_span,
                frames: Vec::new(),
                frames_truncated: false,
                globals: Vec::new(),
                globals_truncated: false,
            },
        },
    );
    assert_eq!(
        events.validate(&impossible),
        Err(StreamValidationError::InvalidPayload)
    );

    let valid = event_envelope(
        2,
        1,
        WorkerEvent::Paused {
            location: PauseLocation {
                source_id: SourceId(21),
                revision: RevisionId(9),
                span: span(21, 9),
                debug_point_id: DebugPointId(1),
                activation_id: ActivationId(1),
                dynamic_event: 1,
            },
            snapshot: snapshot(SnapshotReason::Paused(PauseReason::Explicit), 21, 9),
        },
    );
    events.validate(&valid).unwrap();

    let mut invalid_value = snapshot(SnapshotReason::Faulted, 21, 9);
    invalid_value.frames[0].locals[0].value_kind = ValueKind::Bool;
    let fault = event_envelope(
        3,
        2,
        WorkerEvent::Faulted {
            diagnostic: WireDiagnostic::from(diagnostic(21, 9)),
            snapshot: invalid_value,
        },
    );
    assert_eq!(
        events.validate(&fault),
        Err(StreamValidationError::InvalidPayload)
    );
}

#[test]
fn debug_number_text_accepts_only_the_runtime_canonical_domain() {
    let codec = LineCodec::new();
    for value in [
        "nan",
        "infinity",
        "-infinity",
        "-0",
        "0",
        "42",
        "1.25",
        "0.0000001",
    ] {
        let mut value_snapshot = VmSnapshot {
            reason: SnapshotReason::Faulted,
            current_span: span(21, 9),
            frames: Vec::new(),
            frames_truncated: false,
            globals: Vec::new(),
            globals_truncated: false,
        };
        value_snapshot.globals.push(BindingSnapshot {
            binding_id: None,
            name: "value".to_string(),
            name_truncated: false,
            binding_kind: "global".to_string(),
            value_kind: ValueKind::Number,
            value: DebugValue::Number(value.to_string()),
        });
        let event = event_envelope(
            1,
            1,
            WorkerEvent::Faulted {
                diagnostic: WireDiagnostic::from(diagnostic(21, 9)),
                snapshot: value_snapshot,
            },
        );
        codec.worker_event_payload_len(&event).unwrap();
    }

    for value in ["", "garbage", "NaN", "inf", "+1", "01", "1.0", " 1"] {
        let mut value_snapshot = VmSnapshot {
            reason: SnapshotReason::Faulted,
            current_span: span(21, 9),
            frames: Vec::new(),
            frames_truncated: false,
            globals: Vec::new(),
            globals_truncated: false,
        };
        value_snapshot.globals.push(BindingSnapshot {
            binding_id: None,
            name: "value".to_string(),
            name_truncated: false,
            binding_kind: "global".to_string(),
            value_kind: ValueKind::Number,
            value: DebugValue::Number(value.to_string()),
        });
        let event = event_envelope(
            1,
            1,
            WorkerEvent::Faulted {
                diagnostic: WireDiagnostic::from(diagnostic(21, 9)),
                snapshot: value_snapshot,
            },
        );
        assert_eq!(
            codec.worker_event_payload_len(&event),
            Err(EncodeError::InvalidEnvelope),
            "accepted {value:?}"
        );
    }
}

#[test]
fn full_width_terminal_envelope_stays_inside_frame_budget() {
    let source_id = u64::MAX;
    let revision = u64::MAX;
    let envelope = Envelope {
        version: PROTOCOL_VERSION,
        worker_session_id: WorkerSessionId(u64::MAX),
        run_id: RunId(u64::MAX),
        source_revision: RevisionId(revision),
        request_id: RequestId(u64::MAX),
        sequence: EventSequence(u64::MAX),
        payload: WorkerEvent::Faulted {
            diagnostic: WireDiagnostic::from(diagnostic(source_id, revision)),
            snapshot: snapshot(SnapshotReason::Faulted, source_id, revision),
        },
    };
    let codec = LineCodec::new();
    let size = codec.worker_event_payload_len(&envelope).unwrap();
    assert!(size < MAX_PAYLOAD_BYTES);
}

#[test]
fn near_ceiling_snapshots_and_maximum_diagnostic_fit_every_event_shape() {
    let source_id = u64::MAX;
    let revision = u64::MAX;
    let paused_snapshot = near_ceiling_snapshot(
        SnapshotReason::Paused(PauseReason::Explicit),
        source_id,
        revision,
    );
    let cancelled_snapshot = VmSnapshot {
        reason: SnapshotReason::Cancelled,
        ..paused_snapshot.clone()
    };
    let faulted_snapshot = VmSnapshot {
        reason: SnapshotReason::Faulted,
        ..paused_snapshot.clone()
    };
    let snapshot_size = paused_snapshot.conservative_json_size().unwrap();
    assert!(snapshot_size <= MAX_SNAPSHOT_JSON_BYTES);
    assert!(snapshot_size >= MAX_SNAPSHOT_JSON_BYTES - 1024);

    let diagnostic = maximum_wire_diagnostic(source_id, revision);
    diagnostic.validate().unwrap();
    let codec = LineCodec::new();
    let events = [
        Envelope {
            version: PROTOCOL_VERSION,
            worker_session_id: WorkerSessionId(u64::MAX),
            run_id: RunId(u64::MAX),
            source_revision: RevisionId(revision),
            request_id: RequestId(u64::MAX),
            sequence: EventSequence(u64::MAX - 1),
            payload: WorkerEvent::Paused {
                location: PauseLocation {
                    source_id: SourceId(source_id),
                    revision: RevisionId(revision),
                    span: span(source_id, revision),
                    debug_point_id: DebugPointId(u64::MAX),
                    activation_id: ActivationId(u64::MAX),
                    dynamic_event: u64::MAX,
                },
                snapshot: paused_snapshot,
            },
        },
        Envelope {
            version: PROTOCOL_VERSION,
            worker_session_id: WorkerSessionId(u64::MAX),
            run_id: RunId(u64::MAX),
            source_revision: RevisionId(revision),
            request_id: RequestId(u64::MAX),
            sequence: EventSequence(u64::MAX),
            payload: WorkerEvent::Cancelled {
                snapshot: cancelled_snapshot,
            },
        },
        Envelope {
            version: PROTOCOL_VERSION,
            worker_session_id: WorkerSessionId(u64::MAX),
            run_id: RunId(u64::MAX),
            source_revision: RevisionId(revision),
            request_id: RequestId(u64::MAX),
            sequence: EventSequence(u64::MAX),
            payload: WorkerEvent::Faulted {
                diagnostic,
                snapshot: faulted_snapshot,
            },
        },
    ];

    for event in events {
        let payload_len = codec.worker_event_payload_len(&event).unwrap();
        assert!(payload_len <= MAX_PAYLOAD_BYTES);
    }
}

fn near_ceiling_snapshot(reason: SnapshotReason, source_id: u64, revision: u64) -> VmSnapshot {
    let mut snapshot = VmSnapshot {
        reason,
        current_span: span(source_id, revision),
        frames: Vec::new(),
        frames_truncated: false,
        globals: vec![BindingSnapshot {
            binding_id: None,
            name: "payload".to_string(),
            name_truncated: false,
            binding_kind: "global".to_string(),
            value_kind: ValueKind::String,
            value: DebugValue::String(String::new()),
        }],
        globals_truncated: false,
    };
    let base = snapshot.conservative_json_size().unwrap();
    let count = (MAX_SNAPSHOT_JSON_BYTES - base) / 6;
    snapshot.globals[0].value = DebugValue::String("\0".repeat(count));
    while snapshot.conservative_json_size().unwrap() > MAX_SNAPSHOT_JSON_BYTES {
        let DebugValue::String(value) = &mut snapshot.globals[0].value else {
            unreachable!();
        };
        value.pop();
    }
    snapshot
}

fn maximum_wire_diagnostic(source_id: u64, revision: u64) -> WireDiagnostic {
    WireDiagnostic {
        phase: DiagnosticPhase::Runtime,
        severity: DiagnosticSeverity::Error,
        code: "a".repeat(MAX_DIAGNOSTIC_CODE_BYTES),
        code_truncated: false,
        message: "\0".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES),
        message_truncated: false,
        span: span(source_id, revision),
        frames: (0..MAX_DIAGNOSTIC_FRAMES)
            .map(|_| WireRuntimeFrame {
                function: "\0".repeat(MAX_DIAGNOSTIC_FUNCTION_BYTES),
                function_truncated: false,
                span: span(source_id, revision),
            })
            .collect(),
        frames_truncated: false,
    }
}

struct FailAfter {
    remaining: usize,
    bytes: Vec<u8>,
}

struct FlushFails {
    bytes: Vec<u8>,
}

impl Write for FlushFails {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush failed"))
    }
}

impl FailAfter {
    fn new(limit: usize) -> Self {
        Self {
            remaining: limit,
            bytes: Vec::new(),
        }
    }
}

impl Write for FailAfter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "short writer"));
        }
        let count = self.remaining.min(buffer.len());
        self.bytes.extend_from_slice(&buffer[..count]);
        self.remaining -= count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader failed"))
    }
}

impl BufRead for FailingReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader failed"))
    }

    fn consume(&mut self, _amount: usize) {}
}

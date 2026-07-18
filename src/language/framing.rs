use std::collections::HashSet;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

pub(crate) const MAX_HEADER_BYTES: usize = 8 * 1024;
pub(crate) const MAX_HEADER_COUNT: usize = 32;
pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
#[cfg(test)]
pub(crate) const MAX_DECODE_BATCH_ITEMS: usize = 64;
#[cfg(test)]
pub(crate) const MAX_DECODE_BATCH_BODY_BYTES: usize = MAX_BODY_BYTES;
const MAX_RPC_ID_BYTES: usize = 256;
const MAX_METHOD_BYTES: usize = 256;
const MAX_ERROR_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_ERROR_DATA_BYTES: usize = 1024;
const COMPACT_PREFIX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RpcId {
    Number(i32),
    String(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RpcResponseId {
    Id(RpcId),
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RpcOutcome {
    Result,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RpcEnvelope {
    Request {
        id: RpcId,
        method: String,
    },
    Notification {
        method: String,
    },
    Response {
        id: RpcResponseId,
        outcome: RpcOutcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JsonRpcMessage {
    value: Value,
    envelope: RpcEnvelope,
    body_bytes: usize,
}

impl JsonRpcMessage {
    pub(crate) fn value(&self) -> &Value {
        &self.value
    }

    pub(crate) fn envelope(&self) -> &RpcEnvelope {
        &self.envelope
    }

    pub(crate) fn into_value(self) -> Value {
        self.value
    }

    pub(crate) fn body_bytes(&self) -> usize {
        self.body_bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    HeaderTooLarge,
    TooManyHeaders,
    InvalidHeader,
    MissingContentLength,
    DuplicateContentLength,
    InvalidContentLength,
    BodyTooLarge,
    InvalidUtf8,
    InvalidJson,
    InvalidEnvelope,
    FeedBeforeDrain,
    UnexpectedEof,
    #[cfg(test)]
    DecodeBatchTooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeState {
    Header,
    Body(usize),
    Poisoned(FrameError),
}

pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
    start: usize,
    state: DecodeState,
    drain_required: bool,
    #[cfg(test)]
    compacted_bytes: usize,
}

impl FrameDecoder {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            state: DecodeState::Header,
            drain_required: false,
            #[cfg(test)]
            compacted_bytes: 0,
        }
    }

    pub(crate) fn buffered_bytes(&self) -> usize {
        self.buffer.len() - self.start
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if let DecodeState::Poisoned(error) = self.state {
            return Err(error);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        if self.drain_required {
            self.state = DecodeState::Poisoned(FrameError::FeedBeforeDrain);
            return Err(FrameError::FeedBeforeDrain);
        }
        let retained_limit = MAX_HEADER_BYTES
            .checked_add(MAX_BODY_BYTES)
            .ok_or(FrameError::BodyTooLarge)?;

        let raw_buffer_would_exceed_limit = self
            .buffer
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > retained_limit);
        if self.start == self.buffer.len()
            || self.start >= COMPACT_PREFIX_BYTES
            || raw_buffer_would_exceed_limit
        {
            self.compact();
        }

        if self
            .buffered_bytes()
            .checked_add(bytes.len())
            .is_none_or(|length| length > retained_limit)
        {
            let error = match self.state {
                DecodeState::Header => FrameError::HeaderTooLarge,
                DecodeState::Body(_) => FrameError::BodyTooLarge,
                DecodeState::Poisoned(error) => error,
            };
            self.state = DecodeState::Poisoned(error);
            return Err(error);
        }
        self.buffer.extend_from_slice(bytes);
        self.drain_required = true;

        Ok(())
    }

    pub(crate) fn next_message(&mut self) -> Result<Option<JsonRpcMessage>, FrameError> {
        if let DecodeState::Poisoned(error) = self.state {
            return Err(error);
        }

        let result = self.next_message_unpoisoned();
        match result {
            Ok(None) => self.drain_required = false,
            Err(error) => self.state = DecodeState::Poisoned(error),
            Ok(Some(_)) => {}
        }
        result
    }

    fn next_message_unpoisoned(&mut self) -> Result<Option<JsonRpcMessage>, FrameError> {
        loop {
            match self.state {
                DecodeState::Header => {
                    let available = &self.buffer[self.start..];
                    let Some(end) = header_end(available) else {
                        if available.len() > MAX_HEADER_BYTES {
                            return Err(FrameError::HeaderTooLarge);
                        }
                        return Ok(None);
                    };
                    let consumed = end + 4;
                    if consumed > MAX_HEADER_BYTES {
                        return Err(FrameError::HeaderTooLarge);
                    }
                    let length = parse_content_length(&available[..end])?;
                    if length > MAX_BODY_BYTES {
                        return Err(FrameError::BodyTooLarge);
                    }
                    self.start += consumed;
                    self.state = DecodeState::Body(length);
                }
                DecodeState::Body(length) => {
                    if self.buffered_bytes() < length {
                        return Ok(None);
                    }
                    let end = self
                        .start
                        .checked_add(length)
                        .ok_or(FrameError::BodyTooLarge)?;
                    let body = &self.buffer[self.start..end];
                    let text = std::str::from_utf8(body).map_err(|_| FrameError::InvalidUtf8)?;
                    let value = decode_strict_json(text)?;
                    let envelope = classify_envelope(&value)?;
                    self.start = end;
                    self.state = DecodeState::Header;
                    return Ok(Some(JsonRpcMessage {
                        value,
                        envelope,
                        body_bytes: length,
                    }));
                }
                DecodeState::Poisoned(error) => return Err(error),
            }
        }
    }

    fn compact(&mut self) {
        if self.start == 0 {
            return;
        }
        let remaining = self.buffered_bytes();
        self.buffer.copy_within(self.start.., 0);
        self.buffer.truncate(remaining);
        self.start = 0;
        #[cfg(test)]
        {
            self.compacted_bytes += remaining;
        }
    }

    #[cfg(test)]
    fn compacted_bytes_for_test(&self) -> usize {
        self.compacted_bytes
    }

    #[cfg(test)]
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<JsonRpcMessage>, FrameError> {
        self.feed(bytes)?;
        let mut decoded = Vec::new();
        let mut decoded_body_bytes = 0usize;
        while let Some(message) = self.next_message()? {
            if decoded.len() >= MAX_DECODE_BATCH_ITEMS
                || decoded_body_bytes
                    .checked_add(message.body_bytes())
                    .is_none_or(|body_bytes| body_bytes > MAX_DECODE_BATCH_BODY_BYTES)
            {
                self.state = DecodeState::Poisoned(FrameError::DecodeBatchTooLarge);
                return Err(FrameError::DecodeBatchTooLarge);
            }
            decoded_body_bytes += message.body_bytes();
            decoded.push(message);
        }
        Ok(decoded)
    }

    pub(crate) fn finish(self) -> Result<(), FrameError> {
        match self.state {
            DecodeState::Header if self.buffered_bytes() == 0 => Ok(()),
            DecodeState::Poisoned(error) => Err(error),
            DecodeState::Header | DecodeState::Body(_) => Err(FrameError::UnexpectedEof),
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn encode_message(value: &Value) -> Result<Vec<u8>, FrameError> {
    classify_envelope(value)?;
    let body = serde_json::to_vec(value).map_err(|_| FrameError::InvalidJson)?;
    if body.len() > MAX_BODY_BYTES {
        return Err(FrameError::BodyTooLarge);
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut frame = Vec::with_capacity(header.len() + body.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(header: &[u8]) -> Result<usize, FrameError> {
    let text = std::str::from_utf8(header).map_err(|_| FrameError::InvalidHeader)?;
    let lines: Vec<&str> = text.split("\r\n").collect();
    if lines.len() > MAX_HEADER_COUNT {
        return Err(FrameError::TooManyHeaders);
    }
    let mut content_length = None;
    let mut content_type_seen = false;
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(FrameError::InvalidHeader);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(FrameError::InvalidHeader);
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(FrameError::InvalidHeader);
        }
        if !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            return Err(FrameError::InvalidHeader);
        }
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(FrameError::DuplicateContentLength);
            }
            let value = value.trim_matches([' ', '\t']);
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(FrameError::InvalidContentLength);
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| FrameError::InvalidContentLength)?,
            );
        } else if name.eq_ignore_ascii_case("Content-Type") {
            if content_type_seen {
                return Err(FrameError::InvalidHeader);
            }
            content_type_seen = true;
            validate_content_type(value.trim_matches([' ', '\t']))?;
        }
    }
    content_length.ok_or(FrameError::MissingContentLength)
}

fn classify_envelope(value: &Value) -> Result<RpcEnvelope, FrameError> {
    let object = value.as_object().ok_or(FrameError::InvalidEnvelope)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(FrameError::InvalidEnvelope);
    }

    let method = match object.get("method") {
        Some(Value::String(method)) if !method.is_empty() && method.len() <= MAX_METHOD_BYTES => {
            Some(method.clone())
        }
        Some(_) => return Err(FrameError::InvalidEnvelope),
        None => None,
    };
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");

    if let Some(method) = method {
        if has_result || has_error {
            return Err(FrameError::InvalidEnvelope);
        }
        if object
            .get("params")
            .is_some_and(|params| !params.is_array() && !params.is_object())
        {
            return Err(FrameError::InvalidEnvelope);
        }
        return Ok(match object.get("id") {
            Some(id) => RpcEnvelope::Request {
                id: parse_id(id)?,
                method,
            },
            None => RpcEnvelope::Notification { method },
        });
    }

    let id = object
        .get("id")
        .map(parse_response_id)
        .transpose()?
        .ok_or(FrameError::InvalidEnvelope)?;
    if object.contains_key("params") || has_result == has_error {
        return Err(FrameError::InvalidEnvelope);
    }
    let outcome = if has_result {
        RpcOutcome::Result
    } else {
        validate_error(object.get("error").expect("error field is present"))?;
        RpcOutcome::Error
    };
    Ok(RpcEnvelope::Response { id, outcome })
}

fn parse_id(value: &Value) -> Result<RpcId, FrameError> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .map(RpcId::Number)
            .ok_or(FrameError::InvalidEnvelope),
        Value::String(value) if value.len() <= MAX_RPC_ID_BYTES => Ok(RpcId::String(value.clone())),
        _ => Err(FrameError::InvalidEnvelope),
    }
}

fn parse_response_id(value: &Value) -> Result<RpcResponseId, FrameError> {
    if value.is_null() {
        Ok(RpcResponseId::Null)
    } else {
        parse_id(value).map(RpcResponseId::Id)
    }
}

fn validate_content_type(value: &str) -> Result<(), FrameError> {
    let mut parts = value.split(';');
    let media_type = parts.next().unwrap_or_default().trim();
    if !media_type.eq_ignore_ascii_case("application/vscode-jsonrpc") {
        return Err(FrameError::InvalidHeader);
    }
    let mut charset_seen = false;
    for parameter in parts {
        let Some((name, value)) = parameter.trim().split_once('=') else {
            return Err(FrameError::InvalidHeader);
        };
        if !name.trim().eq_ignore_ascii_case("charset") || charset_seen {
            return Err(FrameError::InvalidHeader);
        }
        charset_seen = true;
        let charset = value.trim();
        if !charset.eq_ignore_ascii_case("utf-8") && !charset.eq_ignore_ascii_case("utf8") {
            return Err(FrameError::InvalidHeader);
        }
    }
    Ok(())
}

fn validate_error(value: &Value) -> Result<(), FrameError> {
    let error = value.as_object().ok_or(FrameError::InvalidEnvelope)?;
    if error
        .get("code")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
        .is_none()
    {
        return Err(FrameError::InvalidEnvelope);
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .ok_or(FrameError::InvalidEnvelope)?;
    if message.len() > MAX_ERROR_MESSAGE_BYTES {
        return Err(FrameError::InvalidEnvelope);
    }
    if let Some(data) = error.get("data") {
        let encoded = serde_json::to_vec(data).map_err(|_| FrameError::InvalidEnvelope)?;
        if encoded.len() > MAX_ERROR_DATA_BYTES {
            return Err(FrameError::InvalidEnvelope);
        }
    }
    Ok(())
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(StrictValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = HashSet::new();
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let value = map.next_value::<StrictValue>()?;
            values.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(values)))
    }
}

fn decode_strict_json(text: &str) -> Result<Value, FrameError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = StrictValue::deserialize(&mut deserializer)
        .map_err(|_| FrameError::InvalidJson)?
        .0;
    deserializer.end().map_err(|_| FrameError::InvalidJson)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn wire(value: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("serialize fixture");
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend(body);
        frame
    }

    fn decode_one(value: Value) -> JsonRpcMessage {
        let mut decoder = FrameDecoder::new();
        let decoded = decoder.push(&wire(&value)).expect("decode fixture");
        assert_eq!(decoded.len(), 1);
        decoder.finish().expect("complete stream");
        decoded.into_iter().next().expect("one message")
    }

    #[test]
    fn decodes_one_byte_fragmentation_and_multiple_frames() {
        let first = json!({"jsonrpc":"2.0","id":1,"result":null});
        let second = json!({"jsonrpc":"2.0","method":"window/logMessage","params":{}});
        let bytes = [wire(&first), wire(&second)].concat();
        let mut decoder = FrameDecoder::new();
        let mut messages = Vec::new();

        for byte in bytes {
            messages.extend(decoder.push(&[byte]).expect("fragment accepted"));
        }

        decoder.finish().expect("complete stream");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].value(), &first);
        assert_eq!(messages[1].value(), &second);
        assert_eq!(
            messages[0].body_bytes(),
            serde_json::to_vec(&first).unwrap().len()
        );
        assert_eq!(
            messages[1].body_bytes(),
            serde_json::to_vec(&second).unwrap().len()
        );
    }

    #[test]
    fn pull_api_offers_one_accounted_message_before_decoding_the_next() {
        let first = json!({"jsonrpc":"2.0","id":1,"result":null});
        let invalid_body = br#"{"jsonrpc":"2.0","id":2,"result":null,"result":null}"#;
        let mut invalid = format!("Content-Length: {}\r\n\r\n", invalid_body.len()).into_bytes();
        invalid.extend_from_slice(invalid_body);

        let first_wire = wire(&first);
        let input = [first_wire, invalid].concat();
        let mut decoder = FrameDecoder::new();
        decoder.feed(&input).expect("bytes fit the retained buffer");

        let decoded = decoder
            .next_message()
            .expect("the first frame is valid")
            .expect("the first frame is complete");
        assert_eq!(decoded.value(), &first);
        assert_eq!(
            decoded.body_bytes(),
            serde_json::to_vec(&first).unwrap().len()
        );
        assert_eq!(decoder.next_message(), Err(FrameError::InvalidJson));
        assert_eq!(decoder.next_message(), Err(FrameError::InvalidJson));
        assert_eq!(decoder.feed(&[]), Err(FrameError::InvalidJson));
        assert_eq!(decoder.finish(), Err(FrameError::InvalidJson));
    }

    #[test]
    fn pull_api_drains_many_tiny_frames_without_forming_a_message_batch() {
        let values = (0..=MAX_DECODE_BATCH_ITEMS)
            .map(|id| json!({"jsonrpc":"2.0","id":id,"result":null}))
            .collect::<Vec<_>>();
        let input = values.iter().flat_map(wire).collect::<Vec<_>>();
        let mut decoder = FrameDecoder::new();
        decoder.feed(&input).expect("tiny frames fit");

        for expected in &values {
            let message = decoder
                .next_message()
                .expect("valid frame")
                .expect("complete frame");
            assert_eq!(message.value(), expected);
        }
        assert_eq!(decoder.next_message().expect("empty buffer"), None);
        decoder.finish().expect("complete stream");
    }

    #[test]
    fn pulling_many_frames_does_not_shift_the_remaining_buffer_per_message() {
        let values = (0..4_096)
            .map(|id| json!({"jsonrpc":"2.0","id":id,"result":null}))
            .collect::<Vec<_>>();
        let input = values.iter().flat_map(wire).collect::<Vec<_>>();
        let mut decoder = FrameDecoder::new();
        decoder.feed(&input).expect("tiny frames fit");

        for expected in &values {
            let message = decoder
                .next_message()
                .expect("valid frame")
                .expect("complete frame");
            assert_eq!(message.value(), expected);
        }

        assert_eq!(decoder.compacted_bytes_for_test(), 0);
        assert_eq!(decoder.buffered_bytes(), 0);
        decoder.finish().expect("complete stream");
    }

    #[test]
    fn a_feed_must_be_drained_to_none_before_another_feed() {
        let first = json!({"jsonrpc":"2.0","id":1,"result":null});
        let second = json!({"jsonrpc":"2.0","id":2,"result":null});
        let mut decoder = FrameDecoder::new();
        decoder.feed(&wire(&first)).expect("first feed");
        assert!(decoder.next_message().expect("valid first").is_some());
        assert_eq!(
            decoder.feed(&wire(&second)),
            Err(FrameError::FeedBeforeDrain)
        );
        assert_eq!(decoder.next_message(), Err(FrameError::FeedBeforeDrain));

        let mut decoder = FrameDecoder::new();
        decoder.feed(&wire(&first)).expect("first feed");
        assert!(decoder.next_message().expect("valid first").is_some());
        assert_eq!(decoder.next_message().expect("drained"), None);
        decoder
            .feed(&wire(&second))
            .expect("second feed after drain");
        assert!(decoder.next_message().expect("valid second").is_some());
        decoder.finish().expect("complete EOF consumes the decoder");
    }

    #[test]
    fn classifies_request_notification_and_response_without_losing_ids() {
        let request = decode_one(json!({
            "jsonrpc":"2.0","id":"server-1","method":"workspace/configuration","params":{}
        }));
        assert!(matches!(
            request.envelope(),
            RpcEnvelope::Request { id: RpcId::String(id), method }
                if id == "server-1" && method == "workspace/configuration"
        ));

        let notification = decode_one(json!({
            "jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{}
        }));
        assert!(matches!(
            notification.envelope(),
            RpcEnvelope::Notification { method }
                if method == "textDocument/publishDiagnostics"
        ));

        let response = decode_one(json!({"jsonrpc":"2.0","id":9,"result":{"ok":true}}));
        assert!(matches!(
            response.envelope(),
            RpcEnvelope::Response {
                id: RpcResponseId::Id(RpcId::Number(9)),
                outcome: RpcOutcome::Result
            }
        ));

        let negative = decode_one(json!({
            "jsonrpc":"2.0","id":-7,"method":"workspace/configuration"
        }));
        assert!(matches!(
            negative.envelope(),
            RpcEnvelope::Request {
                id: RpcId::Number(-7),
                ..
            }
        ));

        let null_response = decode_one(json!({"jsonrpc":"2.0","id":null,"result":null}));
        assert!(matches!(
            null_response.envelope(),
            RpcEnvelope::Response {
                id: RpcResponseId::Null,
                ..
            }
        ));
    }

    #[test]
    fn rejects_invalid_json_rpc_shapes() {
        let invalid = [
            json!([]),
            json!({}),
            json!({"jsonrpc":"1.0","id":1,"result":null}),
            json!({"jsonrpc":"2.0","id":1.5,"result":null}),
            json!({"jsonrpc":"2.0","id":2147483648_i64,"result":null}),
            json!({"jsonrpc":"2.0","method":""}),
            json!({"jsonrpc":"2.0","id":1,"method":"x","result":null}),
            json!({"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-1,"message":"x"}}),
            json!({"jsonrpc":"2.0","id":1}),
            json!({"jsonrpc":"2.0","result":null}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":1.5,"message":"x"}}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":3}}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":2147483648_i64,"message":"x"}}),
            json!({"jsonrpc":"2.0","method":"x","params":7}),
        ];

        for value in invalid {
            let mut decoder = FrameDecoder::new();
            assert!(decoder.push(&wire(&value)).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn rejects_duplicate_json_object_fields_before_value_deserialization() {
        let bodies: &[&[u8]] = &[
            br#"{"jsonrpc":"2.0","jsonrpc":"2.0","id":1,"result":null}"#,
            br#"{"jsonrpc":"2.0","id":1,"id":2,"result":null}"#,
            br#"{"jsonrpc":"2.0","id":1,"result":null,"result":null}"#,
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"code":-2,"message":"x"}}"#,
        ];
        for body in bodies {
            let mut input = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
            input.extend_from_slice(body);
            let mut decoder = FrameDecoder::new();
            assert_eq!(decoder.push(&input), Err(FrameError::InvalidJson));
        }
    }

    #[test]
    fn enforces_header_count_and_header_byte_budgets() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let too_many = format!(
            "{}Content-Length: {}\r\n\r\n",
            "X-Test: a\r\n".repeat(MAX_HEADER_COUNT),
            body.len()
        );
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.push(too_many.as_bytes()),
            Err(FrameError::TooManyHeaders)
        );

        let oversized = vec![b'a'; MAX_HEADER_BYTES + 1];
        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&oversized), Err(FrameError::HeaderTooLarge));
    }

    #[test]
    fn rejects_missing_duplicate_or_invalid_content_length() {
        let headers = [
            "X-Test: x\r\n\r\n",
            "Content-Length: 1\r\nContent-Length: 1\r\n\r\n",
            "Content-Length:\r\n\r\n",
            "Content-Length: -1\r\n\r\n",
            "Content-Length: 1x\r\n\r\n",
            "Content-Length: 184467440737095516160\r\n\r\n",
            "Content-Length: 0\r\n malformed\r\n\r\n",
        ];

        for header in headers {
            let mut decoder = FrameDecoder::new();
            assert!(
                decoder.push(header.as_bytes()).is_err(),
                "accepted {header:?}"
            );
        }
    }

    #[test]
    fn content_type_is_optional_but_if_present_must_be_unique_utf8_lsp_json() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        for content_type in [
            "application/vscode-jsonrpc; charset=utf-8",
            "application/vscode-jsonrpc; charset=utf8",
            "APPLICATION/VSCODE-JSONRPC; CHARSET=UTF-8",
        ] {
            let mut input = format!(
                "Content-Length: {}\r\nContent-Type: {content_type}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            input.extend_from_slice(body);
            let mut decoder = FrameDecoder::new();
            assert_eq!(decoder.push(&input).expect("valid content type").len(), 1);
        }

        for header in [
            "Content-Type: application/json\r\n",
            "Content-Type: application/vscode-jsonrpc; charset=utf-16\r\n",
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n",
        ] {
            let input = format!("Content-Length: {}\r\n{header}\r\n", body.len());
            let mut decoder = FrameDecoder::new();
            assert!(
                decoder.push(input.as_bytes()).is_err(),
                "accepted {header:?}"
            );
        }
    }

    #[test]
    fn rejects_oversized_body_before_allocating_it() {
        let input = format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.push(input.as_bytes()),
            Err(FrameError::BodyTooLarge)
        );
        assert!(decoder.buffered_bytes() <= MAX_HEADER_BYTES);
    }

    #[test]
    fn one_decode_call_has_an_item_and_aggregate_body_budget() {
        let value = json!({"jsonrpc":"2.0","method":"test/tiny"});
        let input = vec![wire(&value); MAX_DECODE_BATCH_ITEMS + 1].concat();
        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&input), Err(FrameError::DecodeBatchTooLarge));

        let chunk = "x".repeat(MAX_DECODE_BATCH_BODY_BYTES / 2);
        let large = json!({"jsonrpc":"2.0","method":"test/large","params":{"x":chunk}});
        let input = [wire(&large), wire(&large)].concat();
        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&input), Err(FrameError::DecodeBatchTooLarge));
    }

    #[test]
    fn rejects_invalid_utf8_and_invalid_json() {
        for body in [vec![0xff], b"{".to_vec()] {
            let mut input = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
            input.extend(body);
            let mut decoder = FrameDecoder::new();
            assert!(decoder.push(&input).is_err());
        }
    }

    #[test]
    fn eof_is_rejected_at_every_incomplete_boundary() {
        let complete = wire(&json!({"jsonrpc":"2.0","id":1,"result":null}));
        for end in 1..complete.len() {
            let mut decoder = FrameDecoder::new();
            let _ = decoder.push(&complete[..end]);
            assert!(decoder.finish().is_err(), "accepted EOF after byte {end}");
        }
        let empty = FrameDecoder::new();
        empty.finish().expect("empty clean EOF");
    }

    #[test]
    fn outbound_frames_are_atomic_bounded_and_include_json_rpc_version() {
        let value = json!({"jsonrpc":"2.0","id":7,"method":"shutdown"});
        let frame = encode_message(&value).expect("encode request");
        assert_eq!(frame, wire(&value));

        let invalid = json!({"id":7,"method":"shutdown"});
        assert_eq!(encode_message(&invalid), Err(FrameError::InvalidEnvelope));

        let oversized = json!({
            "jsonrpc":"2.0",
            "method":"test/oversized",
            "params":{"text":"x".repeat(MAX_BODY_BYTES)}
        });
        assert_eq!(encode_message(&oversized), Err(FrameError::BodyTooLarge));
    }
}

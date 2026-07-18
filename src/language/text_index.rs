use std::collections::HashSet;
use std::ops::Range as StdRange;
use std::sync::Arc;

use lsp_types::{Position, Range};
use serde_json::Value;

use super::snapshot::AnalysisPhase;

pub(crate) const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_SOURCE_LINES: usize = 10_000;
pub(crate) const MAX_RAW_DIAGNOSTICS: usize = 256;
pub(crate) const MAX_ACCEPTED_DIAGNOSTICS: usize = 128;
pub(crate) const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 4 * 1024;
pub(crate) const MAX_DIAGNOSTIC_CODE_BYTES: usize = 256;
pub(crate) const MAX_DIAGNOSTIC_DATA_BYTES: usize = 1024;
pub(crate) const MAX_SEMANTIC_TOKENS: usize = 4_096;
pub(crate) const MAX_DEFINITION_TARGETS: usize = 4_096;
const MAX_URI_BYTES: usize = 4 * 1024;
const MAX_SOURCE_LABEL_BYTES: usize = 256;
const SEMANTIC_FIELDS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexError {
    SourceTooLarge,
    TooManyLines,
    NonCanonicalSource,
    InvalidLine,
    PositionOutOfRange,
    SurrogateInterior,
    ReversedRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PositionPolicy {
    ClampLineEnd,
    Strict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextPoint {
    pub(crate) byte: usize,
    pub(crate) character: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextRange {
    pub(crate) bytes: StdRange<usize>,
    pub(crate) characters: StdRange<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Utf16Boundary {
    utf16: u32,
    byte: usize,
    character: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineIndex {
    byte_start: usize,
    byte_end: usize,
    character_start: usize,
    character_end: usize,
    boundaries: Arc<[Utf16Boundary]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextIndex {
    source: Arc<str>,
    character_boundaries: Arc<[usize]>,
    lines: Arc<[LineIndex]>,
}

impl TextIndex {
    pub(crate) fn new(source: Arc<str>) -> Result<Self, IndexError> {
        if source.len() > MAX_SOURCE_BYTES {
            return Err(IndexError::SourceTooLarge);
        }
        if source.starts_with('\u{feff}') || source.contains('\r') {
            return Err(IndexError::NonCanonicalSource);
        }
        let line_count = source
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            .checked_add(1)
            .ok_or(IndexError::TooManyLines)?;
        if line_count > MAX_SOURCE_LINES {
            return Err(IndexError::TooManyLines);
        }

        let mut character_boundaries: Vec<usize> =
            source.char_indices().map(|(offset, _)| offset).collect();
        character_boundaries.push(source.len());

        let mut line_starts = vec![0usize];
        for (offset, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset + 1);
            }
        }
        let mut lines = Vec::with_capacity(line_starts.len());
        for (line_number, byte_start) in line_starts.iter().copied().enumerate() {
            let byte_end = line_starts
                .get(line_number + 1)
                .map_or(source.len(), |next| next - 1);
            let character_start = character_boundaries
                .binary_search(&byte_start)
                .map_err(|_| IndexError::PositionOutOfRange)?;
            let character_end = character_boundaries
                .binary_search(&byte_end)
                .map_err(|_| IndexError::PositionOutOfRange)?;
            let mut utf16 = 0u32;
            let mut boundaries = Vec::with_capacity(character_end - character_start + 1);
            boundaries.push(Utf16Boundary {
                utf16,
                byte: byte_start,
                character: character_start,
            });
            for (relative, character) in source[byte_start..byte_end].char_indices() {
                utf16 = utf16
                    .checked_add(character.len_utf16() as u32)
                    .ok_or(IndexError::PositionOutOfRange)?;
                boundaries.push(Utf16Boundary {
                    utf16,
                    byte: byte_start + relative + character.len_utf8(),
                    character: character_start + boundaries.len(),
                });
            }
            lines.push(LineIndex {
                byte_start,
                byte_end,
                character_start,
                character_end,
                boundaries: boundaries.into(),
            });
        }

        Ok(Self {
            source,
            character_boundaries: character_boundaries.into(),
            lines: lines.into(),
        })
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn character_count(&self) -> usize {
        self.character_boundaries.len().saturating_sub(1)
    }

    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.source
            .len()
            .saturating_add(
                self.character_boundaries
                    .len()
                    .saturating_mul(std::mem::size_of::<usize>()),
            )
            .saturating_add(
                self.lines
                    .len()
                    .saturating_mul(std::mem::size_of::<LineIndex>()),
            )
            .saturating_add(
                self.lines
                    .iter()
                    .map(|line| {
                        line.boundaries
                            .len()
                            .saturating_mul(std::mem::size_of::<Utf16Boundary>())
                    })
                    .fold(0usize, usize::saturating_add),
            )
    }

    pub(crate) fn position_for_character(&self, character: usize) -> Option<Position> {
        let byte = *self.character_boundaries.get(character)?;
        let line_number = self
            .lines
            .partition_point(|line| line.byte_start <= byte)
            .checked_sub(1)?;
        let line = &self.lines[line_number];
        let utf16 = line
            .boundaries
            .iter()
            .find(|boundary| boundary.byte == byte)?
            .utf16;
        Some(Position::new(u32::try_from(line_number).ok()?, utf16))
    }

    pub(crate) fn resolve_position(
        &self,
        position: Position,
        policy: PositionPolicy,
    ) -> Result<TextPoint, IndexError> {
        let line = self
            .lines
            .get(position.line as usize)
            .ok_or(IndexError::InvalidLine)?;
        let full_length = line
            .boundaries
            .last()
            .expect("line has start boundary")
            .utf16;
        if position.character > full_length {
            return match policy {
                PositionPolicy::ClampLineEnd => {
                    let end = line.boundaries.last().expect("line has end boundary");
                    Ok(TextPoint {
                        byte: end.byte,
                        character: end.character,
                    })
                }
                PositionPolicy::Strict => Err(IndexError::PositionOutOfRange),
            };
        }
        match line
            .boundaries
            .binary_search_by_key(&position.character, |boundary| boundary.utf16)
        {
            Ok(index) => {
                let boundary = line.boundaries[index];
                Ok(TextPoint {
                    byte: boundary.byte,
                    character: boundary.character,
                })
            }
            Err(_) => Err(IndexError::SurrogateInterior),
        }
    }

    pub(crate) fn resolve_range(
        &self,
        range: Range,
        policy: PositionPolicy,
    ) -> Result<TextRange, IndexError> {
        let start = self.resolve_position(range.start, policy)?;
        let end = self.resolve_position(range.end, policy)?;
        if start.byte > end.byte || start.character > end.character {
            return Err(IndexError::ReversedRange);
        }
        Ok(TextRange {
            bytes: start.byte..end.byte,
            characters: start.character..end.character,
        })
    }

    fn semantic_range(&self, line: u32, start: u32, length: u32) -> Result<TextRange, IndexError> {
        if length == 0 {
            return Err(IndexError::PositionOutOfRange);
        }
        let end = start
            .checked_add(length)
            .ok_or(IndexError::PositionOutOfRange)?;
        self.resolve_range(
            Range::new(Position::new(line, start), Position::new(line, end)),
            PositionPolicy::Strict,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SyntaxKind {
    Keyword,
    Comment,
    String,
    Number,
    Variable,
    Operator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyntaxRun {
    pub(crate) range: TextRange,
    pub(crate) kind: SyntaxKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SemanticLegend {
    kinds: Arc<[SyntaxKind]>,
    modifier_count: u32,
}

impl SemanticLegend {
    pub(crate) fn new<T, M, TS, MS>(token_types: T, modifiers: M) -> Result<Self, ValidationError>
    where
        T: IntoIterator<Item = TS>,
        M: IntoIterator<Item = MS>,
        TS: Into<String>,
        MS: Into<String>,
    {
        let token_types: Vec<String> = token_types.into_iter().map(Into::into).collect();
        if token_types.len() != 6 {
            return Err(ValidationError::InvalidLegend);
        }
        let mut seen = HashSet::new();
        let mut kinds = Vec::with_capacity(6);
        for token_type in token_types {
            if !seen.insert(token_type.clone()) {
                return Err(ValidationError::InvalidLegend);
            }
            kinds.push(match token_type.as_str() {
                "keyword" => SyntaxKind::Keyword,
                "comment" => SyntaxKind::Comment,
                "string" => SyntaxKind::String,
                "number" => SyntaxKind::Number,
                "variable" => SyntaxKind::Variable,
                "operator" => SyntaxKind::Operator,
                _ => return Err(ValidationError::InvalidLegend),
            });
        }
        let modifiers: Vec<String> = modifiers.into_iter().map(Into::into).collect();
        if !modifiers.is_empty() {
            return Err(ValidationError::InvalidLegend);
        }
        Ok(Self {
            kinds: kinds.into(),
            modifier_count: 0,
        })
    }

    fn kind(&self, index: u32) -> Result<SyntaxKind, ValidationError> {
        self.kinds
            .get(index as usize)
            .copied()
            .ok_or(ValidationError::InvalidSemanticToken)
    }

    fn modifiers_valid(&self, bits: u32) -> bool {
        if self.modifier_count == 32 {
            true
        } else {
            bits & !((1u32 << self.modifier_count) - 1) == 0
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ValidationError {
    ExpectedArray,
    TooManyRawItems,
    InvalidItem,
    InvalidRange(IndexError),
    StringTooLarge,
    DataTooLarge,
    InvalidLegend,
    InvalidSemanticToken,
    WrongUri,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedDiagnostic {
    pub(crate) range: Option<TextRange>,
    pub(crate) severity: Option<u32>,
    pub(crate) phase: Option<AnalysisPhase>,
    pub(crate) code: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) message: String,
    pub(crate) local_limit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticPlan {
    pub(crate) items: Vec<ValidatedDiagnostic>,
    pub(crate) limited: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefinitionTarget {
    pub(crate) range: TextRange,
}

pub(crate) fn validate_diagnostics(
    index: &TextIndex,
    value: &Value,
) -> Result<DiagnosticPlan, ValidationError> {
    let items = value.as_array().ok_or(ValidationError::ExpectedArray)?;
    if items.len() > MAX_RAW_DIAGNOSTICS {
        return Err(ValidationError::TooManyRawItems);
    }
    let mut validated = Vec::with_capacity(items.len().min(MAX_ACCEPTED_DIAGNOSTICS));
    for item in items {
        let object = item.as_object().ok_or(ValidationError::InvalidItem)?;
        let range = parse_range(object.get("range").ok_or(ValidationError::InvalidItem)?)?;
        let range = index
            .resolve_range(range, PositionPolicy::ClampLineEnd)
            .map_err(ValidationError::InvalidRange)?;
        let message = bounded_required_string(object.get("message"), MAX_DIAGNOSTIC_MESSAGE_BYTES)?;
        let code = match object.get("code") {
            Some(Value::String(code)) if code.len() <= MAX_DIAGNOSTIC_CODE_BYTES => {
                Some(code.clone())
            }
            Some(Value::Number(code)) => {
                let code = code
                    .as_i64()
                    .and_then(|code| i32::try_from(code).ok())
                    .ok_or(ValidationError::InvalidItem)?
                    .to_string();
                if code.len() > MAX_DIAGNOSTIC_CODE_BYTES {
                    return Err(ValidationError::StringTooLarge);
                }
                Some(code)
            }
            Some(_) => return Err(ValidationError::InvalidItem),
            None => None,
        };
        let source = bounded_optional_string(object.get("source"), MAX_SOURCE_LABEL_BYTES)?;
        let severity = match object.get("severity") {
            Some(value) => match value.as_u64() {
                Some(severity @ 1..=4) => Some(severity as u32),
                _ => return Err(ValidationError::InvalidItem),
            },
            None => None,
        };
        if let Some(data) = object.get("data") {
            let encoded = serde_json::to_string(data).map_err(|_| ValidationError::InvalidItem)?;
            if encoded.len() > MAX_DIAGNOSTIC_DATA_BYTES {
                return Err(ValidationError::DataTooLarge);
            }
        }
        let phase = parse_analysis_phase(object.get("data"))?;
        validated.push(ValidatedDiagnostic {
            range: Some(range),
            severity,
            phase,
            code,
            source,
            message,
            local_limit: false,
        });
    }
    let limited = validated.len() > MAX_ACCEPTED_DIAGNOSTICS;
    validated.truncate(MAX_ACCEPTED_DIAGNOSTICS);
    if limited {
        validated.push(ValidatedDiagnostic {
            range: None,
            severity: Some(3),
            phase: None,
            code: None,
            source: Some("oxide".to_owned()),
            message: "analysis results limited".to_owned(),
            local_limit: true,
        });
    }
    Ok(DiagnosticPlan {
        items: validated,
        limited,
    })
}

fn parse_analysis_phase(data: Option<&Value>) -> Result<Option<AnalysisPhase>, ValidationError> {
    let Some(data) = data else {
        return Ok(None);
    };
    let object = data.as_object().ok_or(ValidationError::InvalidItem)?;
    match object.get("phase").and_then(Value::as_str) {
        Some("scanner") => Ok(Some(AnalysisPhase::Scanner)),
        Some("parser") => Ok(Some(AnalysisPhase::Parser)),
        Some("compiler") => Ok(Some(AnalysisPhase::Compiler)),
        Some("runtime") => Ok(Some(AnalysisPhase::Runtime)),
        Some("worker") => Ok(Some(AnalysisPhase::Worker)),
        _ => Err(ValidationError::InvalidItem),
    }
}

pub(crate) fn decode_semantic_tokens(
    index: &TextIndex,
    legend: &SemanticLegend,
    value: &Value,
) -> Result<Vec<SyntaxRun>, ValidationError> {
    let fields = value.as_array().ok_or(ValidationError::ExpectedArray)?;
    if fields.len() > MAX_SEMANTIC_TOKENS * SEMANTIC_FIELDS {
        return Err(ValidationError::TooManyRawItems);
    }
    if fields.len() % SEMANTIC_FIELDS != 0 {
        return Err(ValidationError::InvalidSemanticToken);
    }
    let mut runs = Vec::with_capacity(fields.len() / SEMANTIC_FIELDS);
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;
    for (index_in_stream, token) in fields.chunks_exact(SEMANTIC_FIELDS).enumerate() {
        let field = |offset: usize| -> Result<u32, ValidationError> {
            let raw = token[offset]
                .as_u64()
                .ok_or(ValidationError::InvalidSemanticToken)?;
            u32::try_from(raw).map_err(|_| ValidationError::InvalidSemanticToken)
        };
        let delta_line = field(0)?;
        let delta_start = field(1)?;
        let length = field(2)?;
        let token_type = field(3)?;
        let modifiers = field(4)?;
        let line = if index_in_stream == 0 {
            delta_line
        } else {
            previous_line
                .checked_add(delta_line)
                .ok_or(ValidationError::InvalidSemanticToken)?
        };
        let start = if index_in_stream != 0 && delta_line == 0 {
            previous_start
                .checked_add(delta_start)
                .ok_or(ValidationError::InvalidSemanticToken)?
        } else {
            delta_start
        };
        if !legend.modifiers_valid(modifiers) {
            return Err(ValidationError::InvalidSemanticToken);
        }
        let range = index
            .semantic_range(line, start, length)
            .map_err(ValidationError::InvalidRange)?;
        if runs
            .last()
            .is_some_and(|previous: &SyntaxRun| previous.range.bytes.end > range.bytes.start)
        {
            return Err(ValidationError::InvalidSemanticToken);
        }
        runs.push(SyntaxRun {
            range,
            kind: legend.kind(token_type)?,
        });
        previous_line = line;
        previous_start = start;
    }
    Ok(runs)
}

pub(crate) fn validate_definitions(
    index: &TextIndex,
    expected_uri: &str,
    value: &Value,
) -> Result<Vec<DefinitionTarget>, ValidationError> {
    if expected_uri.len() > MAX_URI_BYTES {
        return Err(ValidationError::WrongUri);
    }
    let owned_single;
    let (items, link_result) = if value.is_null() {
        (&[][..], false)
    } else if let Some(items) = value.as_array() {
        let link_result = items
            .first()
            .and_then(Value::as_object)
            .is_some_and(|object| object.contains_key("targetUri"));
        (items.as_slice(), link_result)
    } else {
        if value
            .as_object()
            .is_some_and(|object| object.contains_key("targetUri"))
        {
            return Err(ValidationError::InvalidItem);
        }
        owned_single = [value.clone()];
        (&owned_single[..], false)
    };
    if items.len() > MAX_DEFINITION_TARGETS {
        return Err(ValidationError::TooManyRawItems);
    }
    let mut targets = Vec::with_capacity(items.len());
    for item in items {
        let object = item.as_object().ok_or(ValidationError::InvalidItem)?;
        if object.contains_key("targetUri") != link_result {
            return Err(ValidationError::InvalidItem);
        }
        let (uri, range) = if link_result {
            let target_range = parse_range(
                object
                    .get("targetRange")
                    .ok_or(ValidationError::InvalidItem)?,
            )?;
            let selection_range = parse_range(
                object
                    .get("targetSelectionRange")
                    .ok_or(ValidationError::InvalidItem)?,
            )?;
            let target_range = index
                .resolve_range(target_range, PositionPolicy::ClampLineEnd)
                .map_err(ValidationError::InvalidRange)?;
            let selection_range = index
                .resolve_range(selection_range, PositionPolicy::ClampLineEnd)
                .map_err(ValidationError::InvalidRange)?;
            if target_range.bytes.start > selection_range.bytes.start
                || target_range.bytes.end < selection_range.bytes.end
            {
                return Err(ValidationError::InvalidRange(IndexError::ReversedRange));
            }
            if let Some(origin) = object.get("originSelectionRange") {
                let origin = parse_range(origin)?;
                index
                    .resolve_range(origin, PositionPolicy::ClampLineEnd)
                    .map_err(ValidationError::InvalidRange)?;
            }
            (
                object.get("targetUri"),
                DefinitionRange::Mapped(selection_range),
            )
        } else {
            (object.get("uri"), DefinitionRange::Raw(object.get("range")))
        };
        let uri = uri
            .and_then(Value::as_str)
            .ok_or(ValidationError::InvalidItem)?;
        if uri != expected_uri {
            return Err(ValidationError::WrongUri);
        }
        let range = match range {
            DefinitionRange::Mapped(range) => range,
            DefinitionRange::Raw(range) => {
                let range = parse_range(range.ok_or(ValidationError::InvalidItem)?)?;
                index
                    .resolve_range(range, PositionPolicy::ClampLineEnd)
                    .map_err(ValidationError::InvalidRange)?
            }
        };
        targets.push(DefinitionTarget { range });
    }
    targets.sort_by(|left, right| {
        left.range
            .bytes
            .start
            .cmp(&right.range.bytes.start)
            .then_with(|| left.range.bytes.end.cmp(&right.range.bytes.end))
    });
    targets.dedup_by(|left, right| left.range == right.range);
    Ok(targets)
}

enum DefinitionRange<'a> {
    Raw(Option<&'a Value>),
    Mapped(TextRange),
}

fn parse_range(value: &Value) -> Result<Range, ValidationError> {
    let object = value.as_object().ok_or(ValidationError::InvalidItem)?;
    Ok(Range::new(
        parse_position(object.get("start").ok_or(ValidationError::InvalidItem)?)?,
        parse_position(object.get("end").ok_or(ValidationError::InvalidItem)?)?,
    ))
}

fn parse_position(value: &Value) -> Result<Position, ValidationError> {
    let object = value.as_object().ok_or(ValidationError::InvalidItem)?;
    let line = object
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ValidationError::InvalidItem)?;
    let character = object
        .get("character")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ValidationError::InvalidItem)?;
    Ok(Position::new(line, character))
}

fn bounded_required_string(
    value: Option<&Value>,
    max_bytes: usize,
) -> Result<String, ValidationError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or(ValidationError::InvalidItem)?;
    if value.len() > max_bytes {
        return Err(ValidationError::StringTooLarge);
    }
    Ok(value.to_owned())
}

fn bounded_optional_string(
    value: Option<&Value>,
    max_bytes: usize,
) -> Result<Option<String>, ValidationError> {
    value
        .map(|value| bounded_required_string(Some(value), max_bytes))
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lsp_types::{Position, Range};
    use serde_json::json;

    use super::*;

    fn range(start_line: u32, start: u32, end_line: u32, end: u32) -> Range {
        Range::new(
            Position::new(start_line, start),
            Position::new(end_line, end),
        )
    }

    #[test]
    fn maps_empty_eof_trailing_line_and_non_bmp_characters() {
        let index = TextIndex::new(Arc::from("a😀\nβ\n")).expect("index source");

        assert_eq!(index.character_count(), 5);
        assert_eq!(index.line_count(), 3);
        assert_eq!(index.position_for_character(0), Some(Position::new(0, 0)));
        assert_eq!(index.position_for_character(1), Some(Position::new(0, 1)));
        assert_eq!(index.position_for_character(2), Some(Position::new(0, 3)));
        assert_eq!(index.position_for_character(3), Some(Position::new(1, 0)));
        assert_eq!(index.position_for_character(4), Some(Position::new(1, 1)));
        assert_eq!(index.position_for_character(5), Some(Position::new(2, 0)));

        let empty = TextIndex::new(Arc::from("")).expect("index empty source");
        assert_eq!(empty.line_count(), 1);
        assert_eq!(empty.position_for_character(0), Some(Position::new(0, 0)));
    }

    #[test]
    fn requires_the_models_bom_free_lf_normalized_text() {
        assert_eq!(
            TextIndex::new(Arc::from("a\r\nb\r")),
            Err(IndexError::NonCanonicalSource)
        );
        assert_eq!(
            TextIndex::new(Arc::from("\u{feff}a")),
            Err(IndexError::NonCanonicalSource)
        );
        let normalized = TextIndex::new(Arc::from("a\nb\n")).expect("index normalized source");
        assert_eq!(normalized.source(), "a\nb\n");
        assert_eq!(
            normalized.position_for_character(4),
            Some(Position::new(2, 0))
        );
    }

    #[test]
    fn ordinary_positions_clamp_past_line_end_but_reject_surrogate_interiors() {
        let index = TextIndex::new(Arc::from("x😀y\n")).expect("index source");

        assert_eq!(
            index.resolve_position(Position::new(0, 99), PositionPolicy::ClampLineEnd),
            Ok(TextPoint {
                byte: "x😀y".len(),
                character: 3,
            })
        );
        assert_eq!(
            index.resolve_position(Position::new(0, 2), PositionPolicy::ClampLineEnd),
            Err(IndexError::SurrogateInterior)
        );
        assert_eq!(
            index.resolve_position(Position::new(1, 1), PositionPolicy::ClampLineEnd),
            Ok(TextPoint {
                byte: "x😀y\n".len(),
                character: 4,
            })
        );
        assert_eq!(
            index.resolve_position(Position::new(2, 0), PositionPolicy::ClampLineEnd),
            Err(IndexError::InvalidLine)
        );
    }

    #[test]
    fn ranges_have_valid_utf8_and_egui_character_boundaries() {
        let index = TextIndex::new(Arc::from("a😀b\nsecond")).expect("index source");
        let mapped = index
            .resolve_range(range(0, 1, 0, 3), PositionPolicy::ClampLineEnd)
            .expect("map emoji");
        assert_eq!(mapped.bytes, 1..5);
        assert_eq!(mapped.characters, 1..2);
        assert_eq!(
            index.resolve_range(range(1, 3, 0, 0), PositionPolicy::ClampLineEnd),
            Err(IndexError::ReversedRange)
        );
    }

    #[test]
    fn diagnostic_validation_preflights_counts_and_bounds_strings() {
        let index = TextIndex::new(Arc::from("alpha\nbeta\n")).expect("index source");
        let mut raw = Vec::new();
        for item in 0..(MAX_ACCEPTED_DIAGNOSTICS + 2) {
            raw.push(json!({
                "range": {
                    "start": {"line": item % 2, "character": 0},
                    "end": {"line": item % 2, "character": 99}
                },
                "severity": 1,
                "code": "E.TEST",
                "source": "parser",
                "message": format!("message-{item}"),
                "data": {"phase":"parser"}
            }));
        }
        let validated = validate_diagnostics(&index, &json!(raw)).expect("valid diagnostics");
        assert_eq!(validated.items.len(), MAX_ACCEPTED_DIAGNOSTICS + 1);
        assert!(validated.limited);
        assert_eq!(validated.items[0].message, "message-0");
        assert_eq!(
            validated.items[MAX_ACCEPTED_DIAGNOSTICS - 1].message,
            format!("message-{}", MAX_ACCEPTED_DIAGNOSTICS - 1)
        );
        let limit = validated.items.last().expect("local limit item");
        assert!(limit.local_limit);
        assert!(limit.range.is_none());
        assert_eq!(limit.message, "analysis results limited");
        assert_eq!(limit.phase, None);
        assert!(
            validated.items[..MAX_ACCEPTED_DIAGNOSTICS]
                .iter()
                .all(|item| item.phase == Some(AnalysisPhase::Parser))
        );

        let phases = [
            ("scanner", AnalysisPhase::Scanner),
            ("parser", AnalysisPhase::Parser),
            ("compiler", AnalysisPhase::Compiler),
            ("runtime", AnalysisPhase::Runtime),
            ("worker", AnalysisPhase::Worker),
        ];
        for (raw_phase, expected) in phases {
            let raw = json!([{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                "message":"typed phase","data":{"phase":raw_phase}
            }]);
            assert_eq!(
                validate_diagnostics(&index, &raw)
                    .expect("known phase")
                    .items[0]
                    .phase,
                Some(expected)
            );
        }
        let absent = json!([{
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
            "message":"limit diagnostic"
        }]);
        assert_eq!(
            validate_diagnostics(&index, &absent)
                .expect("absent phase")
                .items[0]
                .phase,
            None
        );

        let too_many = json!(vec![json!({}); MAX_RAW_DIAGNOSTICS + 1]);
        assert_eq!(
            validate_diagnostics(&index, &too_many),
            Err(ValidationError::TooManyRawItems)
        );

        let oversized_message = json!([{
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
            "message":"x".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES + 1)
        }]);
        assert_eq!(
            validate_diagnostics(&index, &oversized_message),
            Err(ValidationError::StringTooLarge)
        );

        for invalid in [
            json!([{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                "message":"bad code","code":1.5
            }]),
            json!([{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                "message":"bad severity","severity":0
            }]),
            json!([{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                "message":"bad severity","severity":5
            }]),
            json!([{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                "message":"bad phase","data":{"phase":"syntax"}
            }]),
        ] {
            assert_eq!(
                validate_diagnostics(&index, &invalid),
                Err(ValidationError::InvalidItem)
            );
        }
    }

    #[test]
    fn diagnostic_limit_retains_the_first_items_in_input_order() {
        let index = TextIndex::new(Arc::from("abc")).expect("index source");
        let raw = (0..=MAX_ACCEPTED_DIAGNOSTICS)
            .map(|item| {
                let character = if item == MAX_ACCEPTED_DIAGNOSTICS {
                    0
                } else {
                    1
                };
                json!({
                    "range": {
                        "start": {"line": 0, "character": character},
                        "end": {"line": 0, "character": character + 1}
                    },
                    "message": format!("input-{item}")
                })
            })
            .collect::<Vec<_>>();
        let plan = validate_diagnostics(&index, &Value::Array(raw)).expect("diagnostics");
        assert!(plan.limited);
        assert_eq!(plan.items.len(), MAX_ACCEPTED_DIAGNOSTICS + 1);
        for (item, diagnostic) in plan.items[..MAX_ACCEPTED_DIAGNOSTICS].iter().enumerate() {
            assert_eq!(diagnostic.message, format!("input-{item}"));
        }
        let limit = plan.items.last().expect("limit item");
        assert!(limit.local_limit);
        assert_eq!(limit.message, "analysis results limited");
    }

    #[test]
    fn diagnostic_invalid_ranges_fail_the_batch_instead_of_clearing_state() {
        let index = TextIndex::new(Arc::from("😀")).expect("index source");
        let invalid = json!([{
            "range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},
            "message":"bad range"
        }]);
        assert_eq!(
            validate_diagnostics(&index, &invalid),
            Err(ValidationError::InvalidRange(IndexError::SurrogateInterior))
        );
    }

    #[test]
    fn semantic_tokens_decode_checked_deltas_and_all_six_classes() {
        let index =
            TextIndex::new(Arc::from("var name = 42; // hi\n\"s\" + name")).expect("index source");
        let legend = SemanticLegend::new(
            vec![
                "keyword", "comment", "string", "number", "variable", "operator",
            ],
            Vec::<&str>::new(),
        )
        .expect("legend");
        let raw = json!([
            0, 0, 3, 0, 0, 0, 4, 4, 4, 0, 0, 5, 1, 5, 0, 0, 2, 2, 3, 0, 0, 4, 5, 1, 0, 1, 0, 3, 2,
            0, 0, 4, 1, 5, 0, 0, 2, 4, 4, 0
        ]);

        let runs = decode_semantic_tokens(&index, &legend, &raw).expect("decode tokens");
        assert_eq!(runs.len(), 8);
        assert_eq!(runs[0].kind, SyntaxKind::Keyword);
        assert_eq!(runs[4].kind, SyntaxKind::Comment);
        assert_eq!(runs[5].kind, SyntaxKind::String);
        assert_eq!(runs[6].kind, SyntaxKind::Operator);
        assert_eq!(runs[7].kind, SyntaxKind::Variable);
        assert!(
            runs.windows(2)
                .all(|pair| pair[0].range.bytes.end <= pair[1].range.bytes.start)
        );
    }

    #[test]
    fn semantic_tokens_reject_every_delta_and_range_invariant() {
        let index = TextIndex::new(Arc::from("a😀b\nnext")).expect("index source");
        let legend = SemanticLegend::new(
            vec![
                "keyword", "comment", "string", "number", "variable", "operator",
            ],
            Vec::<&str>::new(),
        )
        .expect("legend");
        let invalid = [
            json!([0, 0, 0, 0, 0]),
            json!([0, u32::MAX, 1, 0, 0, 0, 1, 1, 0, 0]),
            json!([u32::MAX, 0, 1, 0, 0, 1, 0, 1, 0, 0]),
            json!([0, 1, 1, 0, 0]),
            json!([0, 0, 99, 0, 0]),
            json!([0, 0, 1, 6, 0]),
            json!([0, 0, 1, 0, 2]),
            json!([0, 0, 2, 0, 0, 0, 1, 2, 1, 0]),
            json!([0, 0, 1, 0]),
        ];
        for data in invalid {
            assert!(
                decode_semantic_tokens(&index, &legend, &data).is_err(),
                "accepted {data}"
            );
        }

        let oversized = json!(vec![0_u32; MAX_SEMANTIC_TOKENS * 5 + 5]);
        assert_eq!(
            decode_semantic_tokens(&index, &legend, &oversized),
            Err(ValidationError::TooManyRawItems)
        );
    }

    #[test]
    fn semantic_nonzero_line_deltas_are_accumulated_exactly_once() {
        let index = TextIndex::new(Arc::from("a\nb\nc")).expect("index source");
        let legend = SemanticLegend::new(
            vec![
                "keyword", "comment", "string", "number", "variable", "operator",
            ],
            Vec::<&str>::new(),
        )
        .expect("legend");
        let runs = decode_semantic_tokens(
            &index,
            &legend,
            &json!([0, 0, 1, 4, 0, 1, 0, 1, 4, 0, 1, 0, 1, 4, 0]),
        )
        .expect("three lines");
        assert_eq!(
            runs.iter()
                .map(|run| run.range.bytes.clone())
                .collect::<Vec<_>>(),
            vec![0..1, 2..3, 4..5]
        );
    }

    #[test]
    fn legend_must_be_unique_and_cover_exactly_the_supported_classes() {
        assert_eq!(
            SemanticLegend::new(
                vec![
                    "keyword", "keyword", "string", "number", "variable", "operator"
                ],
                Vec::<&str>::new()
            ),
            Err(ValidationError::InvalidLegend)
        );
        assert_eq!(
            SemanticLegend::new(
                vec![
                    "keyword", "comment", "string", "number", "variable", "class"
                ],
                Vec::<&str>::new()
            ),
            Err(ValidationError::InvalidLegend)
        );
        assert_eq!(
            SemanticLegend::new(
                vec![
                    "keyword", "comment", "string", "number", "variable", "operator"
                ],
                vec!["declaration"]
            ),
            Err(ValidationError::InvalidLegend)
        );
    }

    #[test]
    fn definitions_validate_uri_sort_deduplicate_and_reject_excess() {
        let index = TextIndex::new(Arc::from("one two three")).expect("index source");
        let uri = "oxide-document://local/7.ox";
        let raw = json!([
            {"uri":uri,"range":{"start":{"line":0,"character":8},"end":{"line":0,"character":13}}},
            {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}},
            {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}}
        ]);
        let targets = validate_definitions(&index, uri, &raw).expect("valid definitions");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].range.bytes, 0..3);
        assert_eq!(targets[1].range.bytes, 8..13);

        let links = json!([{
            "targetUri":uri,
            "targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":7}},
            "targetSelectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
            "originSelectionRange":{"start":{"line":0,"character":8},"end":{"line":0,"character":13}}
        }]);
        assert_eq!(
            validate_definitions(&index, uri, &links).expect("valid links")[0]
                .range
                .bytes,
            0..3
        );

        let mixed = json!([
            {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}},
            {"targetUri":uri,
             "targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":7}},
             "targetSelectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}}
        ]);
        assert_eq!(
            validate_definitions(&index, uri, &mixed),
            Err(ValidationError::InvalidItem)
        );

        let wrong_uri = json!({"uri":"file:///tmp/x","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}});
        assert_eq!(
            validate_definitions(&index, uri, &wrong_uri),
            Err(ValidationError::WrongUri)
        );

        let bad_link = json!({
            "targetUri":uri,
            "targetRange":{"start":{"line":0,"character":4},"end":{"line":0,"character":7}},
            "targetSelectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}
        });
        assert!(validate_definitions(&index, uri, &bad_link).is_err());

        let excess = json!(vec![
            json!({
                "uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
            });
            MAX_DEFINITION_TARGETS + 1
        ]);
        assert_eq!(
            validate_definitions(&index, uri, &excess),
            Err(ValidationError::TooManyRawItems)
        );
    }

    #[test]
    fn source_and_line_budgets_fail_before_retaining_unbounded_indexes() {
        assert_eq!(
            TextIndex::new(Arc::from("x".repeat(MAX_SOURCE_BYTES + 1))),
            Err(IndexError::SourceTooLarge)
        );
        assert_eq!(
            TextIndex::new(Arc::from("\n".repeat(MAX_SOURCE_LINES))),
            Err(IndexError::TooManyLines)
        );
    }

    #[test]
    fn retained_byte_accounting_charges_source_and_exact_index_arrays() {
        let index = TextIndex::new(Arc::from("a😀\nsecond")).expect("index");
        let expected = index.source.len()
            + index.character_boundaries.len() * std::mem::size_of::<usize>()
            + index.lines.len() * std::mem::size_of::<LineIndex>()
            + index
                .lines
                .iter()
                .map(|line| line.boundaries.len() * std::mem::size_of::<Utf16Boundary>())
                .sum::<usize>();
        assert_eq!(index.retained_bytes(), expected);
    }
}

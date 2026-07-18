use std::collections::BTreeMap;
use std::ops::Range;

use eframe::egui::text::{ByteIndex, CCursor, CCursorRange, CharIndex, LayoutJob};
use eframe::egui::{TextBuffer, TextFormat};
use rlox_protocol::{SourceId, SourceSpan, TextPosition};

use crate::{DocumentStamp, MAX_SOURCE_LINES, RunBinding};

pub const MAX_EDITOR_SOURCE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceGrowthLimit {
    Bytes,
    Lines,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceGrowthRejection {
    pub current_bytes: usize,
    pub inserted_bytes: usize,
    pub attempted_bytes: usize,
    pub max_bytes: usize,
    pub attempted_lines: usize,
    pub max_lines: usize,
    pub limit: SourceGrowthLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum UndoOperation {
    Delete { start: CharIndex, characters: usize },
    Insert { at: CharIndex, text: String },
}

pub struct BoundedTextBuffer<'a> {
    text: &'a mut String,
    max_bytes: usize,
    max_lines: usize,
    rejection: Option<SourceGrowthRejection>,
    undo: Vec<UndoOperation>,
}

impl<'a> BoundedTextBuffer<'a> {
    pub fn new(text: &'a mut String, max_bytes: usize) -> Self {
        Self::with_limits(text, max_bytes, MAX_SOURCE_LINES)
    }

    pub fn with_limits(text: &'a mut String, max_bytes: usize, max_lines: usize) -> Self {
        Self {
            text,
            max_bytes,
            max_lines,
            rejection: None,
            undo: Vec::new(),
        }
    }

    pub fn with_default_limit(text: &'a mut String) -> Self {
        Self::new(text, MAX_EDITOR_SOURCE_BYTES)
    }

    pub fn rejection(&self) -> Option<SourceGrowthRejection> {
        self.rejection
    }

    fn rollback(&mut self) {
        while let Some(operation) = self.undo.pop() {
            match operation {
                UndoOperation::Delete { start, characters } => {
                    <String as TextBuffer>::delete_char_range(self.text, start..start + characters);
                }
                UndoOperation::Insert { at, text } => {
                    <String as TextBuffer>::insert_text(self.text, &text, at);
                }
            }
        }
    }
}

impl TextBuffer for BoundedTextBuffer<'_> {
    fn is_mutable(&self) -> bool {
        true
    }

    fn as_str(&self) -> &str {
        self.text
    }

    fn insert_text(&mut self, text: &str, char_index: CharIndex) -> usize {
        if self.rejection.is_some() {
            return 0;
        }
        let current_bytes = self.text.len();
        let attempted_bytes = current_bytes.saturating_add(text.len());
        let insertion_byte = self.byte_index_from_char_index(char_index).0;
        let attempted_lines = normalized_line_count(
            self.text[..insertion_byte]
                .chars()
                .chain(text.chars())
                .chain(self.text[insertion_byte..].chars()),
        );
        let bytes_exceeded = current_bytes
            .checked_add(text.len())
            .is_none_or(|size| size > self.max_bytes);
        let lines_exceeded = attempted_lines > self.max_lines;
        if bytes_exceeded || lines_exceeded {
            self.rejection = Some(SourceGrowthRejection {
                current_bytes,
                inserted_bytes: text.len(),
                attempted_bytes,
                max_bytes: self.max_bytes,
                attempted_lines,
                max_lines: self.max_lines,
                limit: if bytes_exceeded {
                    SourceGrowthLimit::Bytes
                } else {
                    SourceGrowthLimit::Lines
                },
            });
            self.rollback();
            return 0;
        }

        let inserted = <String as TextBuffer>::insert_text(self.text, text, char_index);
        if inserted != 0 {
            self.undo.push(UndoOperation::Delete {
                start: char_index,
                characters: inserted,
            });
        }
        inserted
    }

    fn delete_char_range(&mut self, char_range: Range<CharIndex>) {
        if self.rejection.is_some() {
            return;
        }
        let deleted = <String as TextBuffer>::char_range(self.text, char_range.clone()).to_owned();
        <String as TextBuffer>::delete_char_range(self.text, char_range.clone());
        if !deleted.is_empty() {
            self.undo.push(UndoOperation::Insert {
                at: char_range.start,
                text: deleted,
            });
        }
    }

    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<BoundedTextBuffer<'static>>()
    }
}

fn normalized_line_count(characters: impl Iterator<Item = char>) -> usize {
    let mut lines = 1usize;
    let mut previous_was_carriage_return = false;
    for character in characters {
        match character {
            '\r' => {
                lines = lines.saturating_add(1);
                previous_was_carriage_return = true;
            }
            '\n' if previous_was_carriage_return => {
                previous_was_carriage_return = false;
            }
            '\n' => {
                lines = lines.saturating_add(1);
                previous_was_carriage_return = false;
            }
            _ => previous_was_carriage_return = false,
        }
    }
    lines
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditorSourceKey {
    pub document: DocumentStamp,
    pub run: RunBinding,
}

impl EditorSourceKey {
    pub const fn new(document: DocumentStamp, run: RunBinding) -> Self {
        Self { document, run }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanEndpoint {
    Start,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanMapError {
    StaleDocument {
        expected: DocumentStamp,
        actual: DocumentStamp,
    },
    StaleRun {
        expected: RunBinding,
        actual: RunBinding,
    },
    WrongSource {
        expected: SourceId,
        actual: SourceId,
    },
    WrongRevision {
        expected: rlox_protocol::RevisionId,
        actual: rlox_protocol::RevisionId,
    },
    Reversed,
    OutOfBounds {
        endpoint: SpanEndpoint,
        byte_offset: usize,
        text_bytes: usize,
    },
    NotCharBoundary {
        endpoint: SpanEndpoint,
        byte_offset: usize,
    },
    PositionMismatch {
        endpoint: SpanEndpoint,
        expected: TextPosition,
        actual: TextPosition,
    },
    RunContextRequired,
    ReversedCharacters,
    CharacterRangeMismatch {
        endpoint: SpanEndpoint,
        expected: usize,
        actual: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedPosition {
    pub byte_index: ByteIndex,
    pub char_index: CharIndex,
    pub line_index: usize,
    pub column_index: usize,
}

impl MappedPosition {
    fn source_position(self) -> TextPosition {
        TextPosition {
            byte_offset: self.byte_index.0,
            line: self.line_index + 1,
            column: self.column_index + 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedSpan {
    pub start: MappedPosition,
    pub end: MappedPosition,
}

impl MappedSpan {
    pub fn is_empty(self) -> bool {
        self.start.byte_index == self.end.byte_index
    }

    pub fn egui_cursor_range(self) -> CCursorRange {
        CCursorRange {
            primary: CCursor::new(self.start.char_index),
            secondary: CCursor::new(self.end.char_index),
            h_pos: None,
        }
    }

    fn document_range(self, document: DocumentStamp) -> DocumentRange {
        DocumentRange::new(
            document,
            self.start.byte_index.0..self.end.byte_index.0,
            self.start.char_index..self.end.char_index,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentRange {
    pub document: DocumentStamp,
    pub byte_range: Range<usize>,
    pub char_range: Range<CharIndex>,
}

impl DocumentRange {
    pub const fn new(
        document: DocumentStamp,
        byte_range: Range<usize>,
        char_range: Range<CharIndex>,
    ) -> Self {
        Self {
            document,
            byte_range,
            char_range,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.byte_range.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineStart {
    byte_offset: usize,
    char_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MultibyteScalar {
    start: usize,
    end: usize,
    cumulative_extra_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceMapper {
    document: DocumentStamp,
    run: Option<RunBinding>,
    text_bytes: usize,
    total_chars: usize,
    line_starts: Vec<LineStart>,
    multibyte_scalars: Vec<MultibyteScalar>,
}

impl SourceMapper {
    pub fn new(key: EditorSourceKey, source: &str) -> Self {
        Self::with_source(key.document, Some(key.run), source)
    }

    pub fn for_document(document: DocumentStamp, source: &str) -> Self {
        Self::with_source(document, None, source)
    }

    fn with_source(document: DocumentStamp, run: Option<RunBinding>, source: &str) -> Self {
        let mut line_starts = vec![LineStart {
            byte_offset: 0,
            char_index: 0,
        }];
        let mut multibyte_scalars = Vec::new();
        let mut char_index = 0usize;
        let mut cumulative_extra_bytes = 0usize;

        for (start, character) in source.char_indices() {
            let width = character.len_utf8();
            let end = start + width;
            if width > 1 {
                cumulative_extra_bytes += width - 1;
                multibyte_scalars.push(MultibyteScalar {
                    start,
                    end,
                    cumulative_extra_bytes,
                });
            }
            char_index += 1;
            if character == '\n' {
                line_starts.push(LineStart {
                    byte_offset: end,
                    char_index,
                });
            }
        }

        Self {
            document,
            run,
            text_bytes: source.len(),
            total_chars: char_index,
            line_starts,
            multibyte_scalars,
        }
    }

    pub fn document(&self) -> DocumentStamp {
        self.document
    }

    pub fn run_key(&self) -> Option<EditorSourceKey> {
        self.run.map(|run| EditorSourceKey::new(self.document, run))
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn total_chars(&self) -> usize {
        self.total_chars
    }

    pub fn line_start(&self, line_index: usize) -> Option<MappedPosition> {
        let start = *self.line_starts.get(line_index)?;
        Some(MappedPosition {
            byte_index: ByteIndex(start.byte_offset),
            char_index: CharIndex(start.char_index),
            line_index,
            column_index: 0,
        })
    }

    pub fn position_at(&self, byte_offset: usize) -> Option<MappedPosition> {
        if byte_offset > self.text_bytes {
            return None;
        }

        let completed_scalars = self
            .multibyte_scalars
            .partition_point(|scalar| scalar.end <= byte_offset);
        if self
            .multibyte_scalars
            .get(completed_scalars)
            .is_some_and(|scalar| scalar.start < byte_offset)
        {
            return None;
        }
        let extra_bytes = completed_scalars.checked_sub(1).map_or(0, |index| {
            self.multibyte_scalars[index].cumulative_extra_bytes
        });
        let char_index = byte_offset - extra_bytes;
        let line_index = self
            .line_starts
            .partition_point(|start| start.byte_offset <= byte_offset)
            .checked_sub(1)?;
        let line_start = self.line_starts[line_index];
        Some(MappedPosition {
            byte_index: ByteIndex(byte_offset),
            char_index: CharIndex(char_index),
            line_index,
            column_index: char_index - line_start.char_index,
        })
    }

    pub fn map_span(&self, marker: MarkerSpan) -> Result<MappedSpan, SpanMapError> {
        let Some(run) = self.run else {
            return Err(SpanMapError::RunContextRequired);
        };
        self.map_run_span(EditorSourceKey::new(self.document, run), marker)
    }

    pub fn map_run_span(
        &self,
        expected: EditorSourceKey,
        marker: MarkerSpan,
    ) -> Result<MappedSpan, SpanMapError> {
        if expected.document != self.document {
            return Err(SpanMapError::StaleDocument {
                expected: self.document,
                actual: expected.document,
            });
        }
        if let Some(run) = self.run
            && expected.run != run
        {
            return Err(SpanMapError::StaleRun {
                expected: run,
                actual: expected.run,
            });
        }
        if marker.source.document != expected.document {
            return Err(SpanMapError::StaleDocument {
                expected: expected.document,
                actual: marker.source.document,
            });
        }
        if marker.source.run != expected.run {
            return Err(SpanMapError::StaleRun {
                expected: expected.run,
                actual: marker.source.run,
            });
        }
        if marker.span.source_id != expected.run.source_id {
            return Err(SpanMapError::WrongSource {
                expected: expected.run.source_id,
                actual: marker.span.source_id,
            });
        }
        if marker.span.revision != expected.run.source_revision {
            return Err(SpanMapError::WrongRevision {
                expected: expected.run.source_revision,
                actual: marker.span.revision,
            });
        }
        if marker.span.start.byte_offset > marker.span.end.byte_offset {
            return Err(SpanMapError::Reversed);
        }

        let start = self.map_endpoint(SpanEndpoint::Start, marker.span.start)?;
        let end = self.map_endpoint(SpanEndpoint::End, marker.span.end)?;
        Ok(MappedSpan { start, end })
    }

    pub fn map_run_range(
        &self,
        expected: EditorSourceKey,
        marker: MarkerSpan,
    ) -> Result<DocumentRange, SpanMapError> {
        self.map_run_span(expected, marker)
            .map(|span| span.document_range(self.document))
    }

    pub fn map_document_range(&self, range: &DocumentRange) -> Result<MappedSpan, SpanMapError> {
        if range.document != self.document {
            return Err(SpanMapError::StaleDocument {
                expected: self.document,
                actual: range.document,
            });
        }
        if range.byte_range.start > range.byte_range.end {
            return Err(SpanMapError::Reversed);
        }
        if range.char_range.start > range.char_range.end {
            return Err(SpanMapError::ReversedCharacters);
        }
        let start = self.map_document_endpoint(
            SpanEndpoint::Start,
            range.byte_range.start,
            range.char_range.start,
        )?;
        let end = self.map_document_endpoint(
            SpanEndpoint::End,
            range.byte_range.end,
            range.char_range.end,
        )?;
        Ok(MappedSpan { start, end })
    }

    fn map_document_endpoint(
        &self,
        endpoint: SpanEndpoint,
        byte_offset: usize,
        supplied_character: CharIndex,
    ) -> Result<MappedPosition, SpanMapError> {
        if byte_offset > self.text_bytes {
            return Err(SpanMapError::OutOfBounds {
                endpoint,
                byte_offset,
                text_bytes: self.text_bytes,
            });
        }
        let Some(mapped) = self.position_at(byte_offset) else {
            return Err(SpanMapError::NotCharBoundary {
                endpoint,
                byte_offset,
            });
        };
        if mapped.char_index != supplied_character {
            return Err(SpanMapError::CharacterRangeMismatch {
                endpoint,
                expected: mapped.char_index.0,
                actual: supplied_character.0,
            });
        }
        Ok(mapped)
    }

    fn map_endpoint(
        &self,
        endpoint: SpanEndpoint,
        supplied: TextPosition,
    ) -> Result<MappedPosition, SpanMapError> {
        if supplied.byte_offset > self.text_bytes {
            return Err(SpanMapError::OutOfBounds {
                endpoint,
                byte_offset: supplied.byte_offset,
                text_bytes: self.text_bytes,
            });
        }
        let Some(mapped) = self.position_at(supplied.byte_offset) else {
            return Err(SpanMapError::NotCharBoundary {
                endpoint,
                byte_offset: supplied.byte_offset,
            });
        };
        let expected = mapped.source_position();
        if expected != supplied {
            return Err(SpanMapError::PositionMismatch {
                endpoint,
                expected,
                actual: supplied,
            });
        }
        Ok(mapped)
    }

    pub fn resolve_markers(&self, markers: MarkerInputs) -> MarkerPlan {
        let mut valid = Vec::with_capacity(3);
        let mut rejected = Vec::with_capacity(3);
        for (kind, marker) in [
            (MarkerKind::Current, markers.current),
            (MarkerKind::Fault, markers.fault),
            (MarkerKind::Navigation, markers.navigation),
        ] {
            let Some(marker) = marker else {
                continue;
            };
            match self.map_span(marker) {
                Ok(span) => valid.push(ResolvedMarker { kind, span }),
                Err(error) => rejected.push(RejectedMarker { kind, error }),
            }
        }

        self.build_marker_plan(valid, rejected)
    }

    pub fn resolve_document_markers(&self, markers: DocumentMarkerInputs) -> MarkerPlan {
        let mut valid = Vec::with_capacity(3);
        let mut rejected = Vec::with_capacity(3);
        for (kind, range) in [
            (MarkerKind::Current, markers.current),
            (MarkerKind::Fault, markers.fault),
            (MarkerKind::Navigation, markers.navigation),
        ] {
            let Some(range) = range else {
                continue;
            };
            match self.map_document_range(&range) {
                Ok(span) => valid.push(ResolvedMarker { kind, span }),
                Err(error) => rejected.push(RejectedMarker { kind, error }),
            }
        }

        self.build_marker_plan(valid, rejected)
    }

    fn build_marker_plan(
        &self,
        valid: Vec<ResolvedMarker>,
        rejected: Vec<RejectedMarker>,
    ) -> MarkerPlan {
        let mut endpoints = Vec::with_capacity(2 + valid.len() * 2);
        endpoints.push(
            self.position_at(0)
                .expect("zero is always a source boundary"),
        );
        endpoints.push(
            self.position_at(self.text_bytes)
                .expect("source length is always a source boundary"),
        );
        for marker in &valid {
            if !marker.span.is_empty() {
                endpoints.push(marker.span.start);
                endpoints.push(marker.span.end);
            }
        }
        endpoints.sort_unstable_by_key(|position| position.byte_index);
        endpoints.dedup_by_key(|position| position.byte_index);

        let mut format_runs: Vec<FormatRun> = Vec::with_capacity(endpoints.len());
        for pair in endpoints.windows(2) {
            let start = pair[0];
            let end = pair[1];
            if start.byte_index == end.byte_index {
                continue;
            }
            let mut mask = MarkerMask::NONE;
            for marker in &valid {
                if !marker.span.is_empty()
                    && marker.span.start.byte_index <= start.byte_index
                    && end.byte_index <= marker.span.end.byte_index
                {
                    mask = mask.with(marker.kind);
                }
            }
            if let Some(previous) = format_runs.last_mut()
                && previous.markers == mask
                && previous.byte_range.end == start.byte_index.0
            {
                previous.byte_range.end = end.byte_index.0;
                previous.char_range.end = end.char_index;
            } else {
                format_runs.push(FormatRun {
                    byte_range: start.byte_index.0..end.byte_index.0,
                    char_range: start.char_index..end.char_index,
                    markers: mask,
                });
            }
        }

        let mut points = BTreeMap::<usize, PointMarker>::new();
        let mut gutter = BTreeMap::<usize, MarkerMask>::new();
        for marker in valid {
            gutter
                .entry(marker.span.start.line_index)
                .and_modify(|mask| *mask = mask.with(marker.kind))
                .or_insert_with(|| MarkerMask::from_kind(marker.kind));
            if marker.span.is_empty() {
                points
                    .entry(marker.span.start.byte_index.0)
                    .and_modify(|point| point.markers = point.markers.with(marker.kind))
                    .or_insert(PointMarker {
                        position: marker.span.start,
                        markers: MarkerMask::from_kind(marker.kind),
                        at_eof: marker.span.start.byte_index.0 == self.text_bytes,
                    });
            }
        }

        MarkerPlan {
            document: self.document,
            text_bytes: self.text_bytes,
            format_runs,
            point_markers: points.into_values().collect(),
            gutter_markers: gutter
                .into_iter()
                .map(|(line_index, markers)| GutterMarker {
                    line_index,
                    markers,
                })
                .collect(),
            rejected,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerSpan {
    pub source: EditorSourceKey,
    pub span: SourceSpan,
}

impl MarkerSpan {
    pub const fn new(source: EditorSourceKey, span: SourceSpan) -> Self {
        Self { source, span }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarkerInputs {
    pub current: Option<MarkerSpan>,
    pub fault: Option<MarkerSpan>,
    pub navigation: Option<MarkerSpan>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentMarkerInputs {
    pub current: Option<DocumentRange>,
    pub fault: Option<DocumentRange>,
    pub navigation: Option<DocumentRange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerKind {
    Current,
    Fault,
    Navigation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarkerMask(u8);

impl MarkerMask {
    pub const NONE: Self = Self(0);
    const CURRENT: u8 = 1 << 0;
    const FAULT: u8 = 1 << 1;
    const NAVIGATION: u8 = 1 << 2;

    pub const fn from_kind(kind: MarkerKind) -> Self {
        match kind {
            MarkerKind::Current => Self(Self::CURRENT),
            MarkerKind::Fault => Self(Self::FAULT),
            MarkerKind::Navigation => Self(Self::NAVIGATION),
        }
    }

    pub const fn with(self, kind: MarkerKind) -> Self {
        Self(self.0 | Self::from_kind(kind).0)
    }

    pub const fn contains(self, kind: MarkerKind) -> bool {
        self.0 & Self::from_kind(kind).0 != 0
    }

    pub const fn primary(self) -> Option<MarkerKind> {
        if self.contains(MarkerKind::Fault) {
            Some(MarkerKind::Fault)
        } else if self.contains(MarkerKind::Current) {
            Some(MarkerKind::Current)
        } else if self.contains(MarkerKind::Navigation) {
            Some(MarkerKind::Navigation)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResolvedMarker {
    kind: MarkerKind,
    span: MappedSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatRun {
    pub byte_range: Range<usize>,
    pub char_range: Range<CharIndex>,
    pub markers: MarkerMask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointMarker {
    pub position: MappedPosition,
    pub markers: MarkerMask,
    pub at_eof: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GutterMarker {
    pub line_index: usize,
    pub markers: MarkerMask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RejectedMarker {
    pub kind: MarkerKind,
    pub error: SpanMapError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerPlan {
    document: DocumentStamp,
    text_bytes: usize,
    format_runs: Vec<FormatRun>,
    point_markers: Vec<PointMarker>,
    gutter_markers: Vec<GutterMarker>,
    rejected: Vec<RejectedMarker>,
}

impl MarkerPlan {
    pub fn document(&self) -> DocumentStamp {
        self.document
    }

    pub fn format_runs(&self) -> &[FormatRun] {
        &self.format_runs
    }

    pub fn point_markers(&self) -> &[PointMarker] {
        &self.point_markers
    }

    pub fn gutter_markers(&self) -> &[GutterMarker] {
        &self.gutter_markers
    }

    pub fn rejected(&self) -> &[RejectedMarker] {
        &self.rejected
    }
}

pub const MAX_PREPARED_SYNTAX_RUNS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxClass {
    Keyword,
    Comment,
    String,
    Number,
    Variable,
    Operator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSyntaxRun {
    pub range: DocumentRange,
    pub class: SyntaxClass,
}

impl PreparedSyntaxRun {
    pub const fn new(range: DocumentRange, class: SyntaxClass) -> Self {
        Self { range, class }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxPlanRejection {
    MarkerPlanMismatch,
    TooManyRuns { actual: usize, maximum: usize },
    EmptyRun { index: usize },
    InvalidRun { index: usize, error: SpanMapError },
    UnsortedOrOverlapping { index: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedLayoutRun {
    pub byte_range: Range<usize>,
    pub char_range: Range<CharIndex>,
    pub syntax: Option<SyntaxClass>,
    pub markers: MarkerMask,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedLayoutPlan {
    document: DocumentStamp,
    text_bytes: usize,
    runs: Vec<PreparedLayoutRun>,
    point_markers: Vec<PointMarker>,
    gutter_markers: Vec<GutterMarker>,
    rejected_markers: Vec<RejectedMarker>,
    syntax_rejection: Option<SyntaxPlanRejection>,
    merge_operations: usize,
}

impl PreparedLayoutPlan {
    pub fn compose(
        mapper: &SourceMapper,
        markers: &MarkerPlan,
        syntax: &[PreparedSyntaxRun],
    ) -> Self {
        if !marker_plan_matches(mapper, markers) {
            let empty = mapper.build_marker_plan(Vec::new(), Vec::new());
            return Self::marker_only(&empty, Some(SyntaxPlanRejection::MarkerPlanMismatch));
        }
        if syntax.len() > MAX_PREPARED_SYNTAX_RUNS {
            return Self::marker_only(
                markers,
                Some(SyntaxPlanRejection::TooManyRuns {
                    actual: syntax.len(),
                    maximum: MAX_PREPARED_SYNTAX_RUNS,
                }),
            );
        }

        let mut previous_end = None;
        for (index, run) in syntax.iter().enumerate() {
            let mapped = match mapper.map_document_range(&run.range) {
                Ok(mapped) => mapped,
                Err(error) => {
                    return Self::marker_only(
                        markers,
                        Some(SyntaxPlanRejection::InvalidRun { index, error }),
                    );
                }
            };
            if mapped.is_empty() {
                return Self::marker_only(markers, Some(SyntaxPlanRejection::EmptyRun { index }));
            }
            if previous_end.is_some_and(|end| end > mapped.start.byte_index.0) {
                return Self::marker_only(
                    markers,
                    Some(SyntaxPlanRejection::UnsortedOrOverlapping { index }),
                );
            }
            previous_end = Some(mapped.end.byte_index.0);
        }

        Self::merge(markers, syntax)
    }

    fn marker_only(markers: &MarkerPlan, rejection: Option<SyntaxPlanRejection>) -> Self {
        Self {
            document: markers.document,
            text_bytes: markers.text_bytes,
            runs: markers
                .format_runs
                .iter()
                .map(|run| PreparedLayoutRun {
                    byte_range: run.byte_range.clone(),
                    char_range: run.char_range.clone(),
                    syntax: None,
                    markers: run.markers,
                })
                .collect(),
            point_markers: markers.point_markers.clone(),
            gutter_markers: markers.gutter_markers.clone(),
            rejected_markers: markers.rejected.clone(),
            syntax_rejection: rejection,
            merge_operations: markers.format_runs.len(),
        }
    }

    fn merge(markers: &MarkerPlan, syntax: &[PreparedSyntaxRun]) -> Self {
        let mut runs: Vec<PreparedLayoutRun> =
            Vec::with_capacity(markers.format_runs.len().saturating_add(syntax.len() * 2));
        let mut marker_index = 0usize;
        let mut syntax_index = 0usize;
        let mut byte = 0usize;
        let mut character = CharIndex(0);
        let mut merge_operations = 0usize;

        while let Some(marker) = markers.format_runs.get(marker_index) {
            if byte == marker.byte_range.end {
                marker_index += 1;
                merge_operations = merge_operations.saturating_add(1);
                continue;
            }
            while syntax
                .get(syntax_index)
                .is_some_and(|run| run.range.byte_range.end <= byte)
            {
                syntax_index += 1;
                merge_operations = merge_operations.saturating_add(1);
            }

            let mut syntax_class = None;
            let mut end_byte = marker.byte_range.end;
            let mut end_character = marker.char_range.end;
            if let Some(syntax_run) = syntax.get(syntax_index) {
                if byte < syntax_run.range.byte_range.start {
                    if syntax_run.range.byte_range.start < end_byte {
                        end_byte = syntax_run.range.byte_range.start;
                        end_character = syntax_run.range.char_range.start;
                    }
                } else if byte < syntax_run.range.byte_range.end {
                    syntax_class = Some(syntax_run.class);
                    if syntax_run.range.byte_range.end < end_byte {
                        end_byte = syntax_run.range.byte_range.end;
                        end_character = syntax_run.range.char_range.end;
                    }
                }
            }

            debug_assert!(byte < end_byte, "validated merge must make progress");
            merge_operations = merge_operations.saturating_add(1);
            push_prepared_run(
                &mut runs,
                PreparedLayoutRun {
                    byte_range: byte..end_byte,
                    char_range: character..end_character,
                    syntax: syntax_class,
                    markers: marker.markers,
                },
            );
            byte = end_byte;
            character = end_character;
        }

        Self {
            document: markers.document,
            text_bytes: markers.text_bytes,
            runs,
            point_markers: markers.point_markers.clone(),
            gutter_markers: markers.gutter_markers.clone(),
            rejected_markers: markers.rejected.clone(),
            syntax_rejection: None,
            merge_operations,
        }
    }

    pub fn document(&self) -> DocumentStamp {
        self.document
    }

    pub fn runs(&self) -> &[PreparedLayoutRun] {
        &self.runs
    }

    pub fn point_markers(&self) -> &[PointMarker] {
        &self.point_markers
    }

    pub fn gutter_markers(&self) -> &[GutterMarker] {
        &self.gutter_markers
    }

    pub fn rejected_markers(&self) -> &[RejectedMarker] {
        &self.rejected_markers
    }

    pub fn syntax_rejection(&self) -> Option<SyntaxPlanRejection> {
        self.syntax_rejection
    }

    pub fn merge_operations(&self) -> usize {
        self.merge_operations
    }
}

fn push_prepared_run(runs: &mut Vec<PreparedLayoutRun>, run: PreparedLayoutRun) {
    if let Some(previous) = runs.last_mut()
        && previous.syntax == run.syntax
        && previous.markers == run.markers
        && previous.byte_range.end == run.byte_range.start
        && previous.char_range.end == run.char_range.start
    {
        previous.byte_range.end = run.byte_range.end;
        previous.char_range.end = run.char_range.end;
    } else {
        runs.push(run);
    }
}

fn marker_plan_matches(mapper: &SourceMapper, plan: &MarkerPlan) -> bool {
    if plan.document != mapper.document || plan.text_bytes != mapper.text_bytes {
        return false;
    }
    if mapper.text_bytes == 0 {
        return plan.format_runs.is_empty();
    }
    let mut next_byte = 0usize;
    let mut next_character = CharIndex(0);
    let valid = plan.format_runs.iter().all(|run| {
        let contiguous = run.byte_range.start == next_byte
            && run.char_range.start == next_character
            && run.byte_range.start < run.byte_range.end
            && run.char_range.start < run.char_range.end
            && run.byte_range.end <= mapper.text_bytes
            && mapper
                .position_at(run.byte_range.start)
                .is_some_and(|position| position.char_index == run.char_range.start)
            && mapper
                .position_at(run.byte_range.end)
                .is_some_and(|position| position.char_index == run.char_range.end);
        next_byte = run.byte_range.end;
        next_character = run.char_range.end;
        contiguous
    });
    valid && next_byte == mapper.text_bytes && next_character.0 == mapper.total_chars
}

pub fn build_prepared_layout_job(
    document: DocumentStamp,
    source: &str,
    plan: &PreparedLayoutPlan,
    mut format_for: impl FnMut(Option<SyntaxClass>, MarkerMask) -> TextFormat,
) -> LayoutJob {
    let valid_plan = plan.document == document
        && plan.text_bytes == source.len()
        && if source.is_empty() {
            plan.runs.is_empty()
        } else {
            let mut next_byte = 0usize;
            let mut next_character = CharIndex(0);
            let valid = plan.runs.iter().all(|run| {
                let contiguous = run.byte_range.start == next_byte
                    && run.char_range.start == next_character
                    && run.byte_range.start < run.byte_range.end
                    && run.char_range.start < run.char_range.end
                    && run.byte_range.end <= source.len()
                    && source.is_char_boundary(run.byte_range.start)
                    && source.is_char_boundary(run.byte_range.end)
                    && run.char_range.end.0.checked_sub(run.char_range.start.0)
                        == Some(source[run.byte_range.clone()].chars().count());
                next_byte = run.byte_range.end;
                next_character = run.char_range.end;
                contiguous
            });
            valid && next_byte == source.len() && next_character.0 == source.chars().count()
        };

    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.break_on_newline = true;
    job.keep_trailing_whitespace = true;
    if !valid_plan {
        job.append(source, 0.0, format_for(None, MarkerMask::NONE));
        return job;
    }
    if source.is_empty() {
        job.append("", 0.0, format_for(None, MarkerMask::NONE));
        return job;
    }
    for run in &plan.runs {
        job.append(
            &source[run.byte_range.clone()],
            0.0,
            format_for(run.syntax, run.markers),
        );
    }
    job
}

pub fn build_layout_job(
    source: &str,
    plan: &MarkerPlan,
    mut format_for: impl FnMut(MarkerMask) -> TextFormat,
) -> LayoutJob {
    let valid_plan = plan.text_bytes == source.len()
        && if source.is_empty() {
            plan.format_runs.is_empty()
        } else {
            let mut next = 0usize;
            let valid = plan.format_runs.iter().all(|run| {
                let contiguous = run.byte_range.start == next
                    && run.byte_range.start < run.byte_range.end
                    && run.byte_range.end <= source.len()
                    && source.is_char_boundary(run.byte_range.start)
                    && source.is_char_boundary(run.byte_range.end);
                next = run.byte_range.end;
                contiguous
            });
            valid && next == source.len()
        };

    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.break_on_newline = true;
    job.keep_trailing_whitespace = true;
    if !valid_plan {
        job.append(source, 0.0, format_for(MarkerMask::NONE));
        return job;
    }
    if source.is_empty() {
        job.append("", 0.0, format_for(MarkerMask::NONE));
        return job;
    }
    for run in &plan.format_runs {
        let text = &source[run.byte_range.clone()];
        job.append(text, 0.0, format_for(run.markers));
    }
    job
}

pub fn gutter_digits(line_count: usize) -> usize {
    let mut remaining = line_count.max(1);
    let mut digits = 1;
    while remaining >= 10 {
        remaining /= 10;
        digits += 1;
    }
    digits
}

#[cfg(test)]
mod prepared_layout_tests {
    use super::*;
    use crate::{DocumentId, EditRevision};
    use eframe::egui::{Color32, FontId, Stroke};

    fn document(edit_revision: u64) -> DocumentStamp {
        DocumentStamp {
            document_id: DocumentId::from_raw(91).expect("nonzero document id"),
            edit_revision: EditRevision::from_raw(edit_revision).expect("nonzero edit revision"),
        }
    }

    fn document_range(
        mapper: &SourceMapper,
        document: DocumentStamp,
        bytes: Range<usize>,
    ) -> DocumentRange {
        let start = mapper
            .position_at(bytes.start)
            .expect("test start is a scalar boundary");
        let end = mapper
            .position_at(bytes.end)
            .expect("test end is a scalar boundary");
        DocumentRange::new(document, bytes, start.char_index..end.char_index)
    }

    fn test_format(syntax: Option<SyntaxClass>, markers: MarkerMask) -> TextFormat {
        let color = match syntax {
            Some(SyntaxClass::Keyword) => Color32::from_rgb(1, 2, 3),
            Some(SyntaxClass::Comment) => Color32::from_rgb(4, 5, 6),
            Some(SyntaxClass::String) => Color32::from_rgb(7, 8, 9),
            Some(SyntaxClass::Number) => Color32::from_rgb(10, 11, 12),
            Some(SyntaxClass::Variable) => Color32::from_rgb(13, 14, 15),
            Some(SyntaxClass::Operator) => Color32::from_rgb(16, 17, 18),
            None => Color32::BLACK,
        };
        let mut format = TextFormat {
            font_id: FontId::monospace(14.0),
            color,
            ..Default::default()
        };
        match markers.primary() {
            Some(MarkerKind::Fault) => {
                format.background = Color32::from_rgb(255, 220, 220);
                format.underline = Stroke::new(1.5, Color32::from_rgb(180, 35, 35));
            }
            Some(MarkerKind::Current) => {
                format.background = Color32::from_rgb(255, 242, 184);
            }
            Some(MarkerKind::Navigation) => {
                format.background = Color32::from_rgb(218, 234, 255);
                format.underline = Stroke::new(1.0, Color32::from_rgb(42, 101, 172));
            }
            None => {}
        }
        format
    }

    fn decorations_by_byte(job: &LayoutJob, source_bytes: usize) -> Vec<(Color32, Stroke)> {
        let mut decorations = vec![(Color32::TRANSPARENT, Stroke::NONE); source_bytes];
        for section in &job.sections {
            for decoration in &mut decorations[section.byte_range.start.0..section.byte_range.end.0]
            {
                *decoration = (section.format.background, section.format.underline);
            }
        }
        decorations
    }

    #[test]
    fn prepared_layout_preserves_all_six_syntax_classes() {
        let stamp = document(1);
        let source = "k c s n v o";
        let mapper = SourceMapper::for_document(stamp, source);
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs::default());
        let classes = [
            SyntaxClass::Keyword,
            SyntaxClass::Comment,
            SyntaxClass::String,
            SyntaxClass::Number,
            SyntaxClass::Variable,
            SyntaxClass::Operator,
        ];
        let syntax = classes
            .into_iter()
            .enumerate()
            .map(|(index, class)| {
                let start = index * 2;
                PreparedSyntaxRun::new(document_range(&mapper, stamp, start..start + 1), class)
            })
            .collect::<Vec<_>>();

        let plan = PreparedLayoutPlan::compose(&mapper, &markers, &syntax);

        assert_eq!(plan.syntax_rejection(), None);
        assert_eq!(
            plan.runs()
                .iter()
                .filter_map(|run| run.syntax)
                .collect::<Vec<_>>(),
            classes
        );
    }

    #[test]
    fn document_mapper_validates_run_provenance_before_neutralizing_a_span() {
        let stamp = document(2);
        let source = "print 1;";
        let run = RunBinding {
            worker_session_id: crate::WorkerSessionId(7),
            run_id: crate::RunId(8),
            source_id: SourceId(9),
            source_revision: rlox_protocol::RevisionId(10),
        };
        let source_key = EditorSourceKey::new(stamp, run);
        let mapper = SourceMapper::for_document(stamp, source);
        let marker = MarkerSpan::new(
            source_key,
            SourceSpan {
                source_id: run.source_id,
                revision: run.source_revision,
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
            },
        );

        let neutral = mapper
            .map_run_range(source_key, marker)
            .expect("exact run source is accepted");
        assert_eq!(neutral.byte_range, 0..5);
        assert_eq!(neutral.char_range, CharIndex(0)..CharIndex(5));

        let stale_run = EditorSourceKey::new(
            stamp,
            RunBinding {
                run_id: crate::RunId(11),
                ..run
            },
        );
        assert!(matches!(
            mapper.map_run_span(source_key, MarkerSpan::new(stale_run, marker.span)),
            Err(SpanMapError::StaleRun { .. })
        ));

        let mapper_bound_to_stale_run = SourceMapper::new(stale_run, source);
        assert!(matches!(
            mapper_bound_to_stale_run.map_run_span(source_key, marker),
            Err(SpanMapError::StaleRun { .. })
        ));
    }

    #[test]
    fn adjacent_unicode_syntax_ranges_keep_exact_byte_and_character_boundaries() {
        let stamp = document(3);
        let source = "a🦀語z";
        let mapper = SourceMapper::for_document(stamp, source);
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs::default());
        let syntax = [
            PreparedSyntaxRun::new(document_range(&mapper, stamp, 0..1), SyntaxClass::Keyword),
            PreparedSyntaxRun::new(document_range(&mapper, stamp, 1..5), SyntaxClass::String),
            PreparedSyntaxRun::new(document_range(&mapper, stamp, 5..8), SyntaxClass::Variable),
            PreparedSyntaxRun::new(document_range(&mapper, stamp, 8..9), SyntaxClass::Operator),
        ];

        let plan = PreparedLayoutPlan::compose(&mapper, &markers, &syntax);

        assert_eq!(plan.syntax_rejection(), None);
        assert_eq!(
            plan.runs()
                .iter()
                .map(|run| (run.byte_range.clone(), run.char_range.clone(), run.syntax))
                .collect::<Vec<_>>(),
            vec![
                (0..1, CharIndex(0)..CharIndex(1), Some(SyntaxClass::Keyword)),
                (1..5, CharIndex(1)..CharIndex(2), Some(SyntaxClass::String)),
                (
                    5..8,
                    CharIndex(2)..CharIndex(3),
                    Some(SyntaxClass::Variable),
                ),
                (
                    8..9,
                    CharIndex(3)..CharIndex(4),
                    Some(SyntaxClass::Operator),
                ),
            ]
        );
    }

    #[test]
    fn malformed_syntax_is_discarded_without_removing_valid_markers() {
        let stamp = document(4);
        let source = "a🦀bcdef\nz";
        let mapper = SourceMapper::for_document(stamp, source);
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs {
            current: Some(document_range(&mapper, stamp, 0..5)),
            fault: Some(document_range(&mapper, stamp, 1..7)),
            navigation: Some(document_range(&mapper, stamp, 5..10)),
        });
        let marker_only = PreparedLayoutPlan::compose(&mapper, &markers, &[]);
        let valid = |range| PreparedSyntaxRun::new(range, SyntaxClass::Keyword);

        let stale_same_length = vec![valid(document_range(&mapper, document(3), 0..1))];
        let reversed_start = 5;
        let reversed_end = 1;
        let reversed = vec![valid(DocumentRange::new(
            stamp,
            reversed_start..reversed_end,
            CharIndex(2)..CharIndex(1),
        ))];
        let overlapping = vec![
            valid(document_range(&mapper, stamp, 0..5)),
            valid(document_range(&mapper, stamp, 1..6)),
        ];
        let out_of_bounds = vec![valid(DocumentRange::new(
            stamp,
            source.len()..source.len() + 1,
            CharIndex(mapper.total_chars())..CharIndex(mapper.total_chars() + 1),
        ))];
        let wrong_character_range = vec![valid(DocumentRange::new(
            stamp,
            1..5,
            CharIndex(0)..CharIndex(2),
        ))];
        let over_limit =
            vec![valid(document_range(&mapper, stamp, 0..1)); MAX_PREPARED_SYNTAX_RUNS + 1];

        let plans = [
            stale_same_length,
            reversed,
            overlapping,
            out_of_bounds,
            wrong_character_range,
            over_limit,
        ]
        .iter()
        .map(|malformed| PreparedLayoutPlan::compose(&mapper, &markers, malformed))
        .collect::<Vec<_>>();

        assert!(matches!(
            plans[0].syntax_rejection(),
            Some(SyntaxPlanRejection::InvalidRun {
                error: SpanMapError::StaleDocument { .. },
                ..
            })
        ));
        assert!(matches!(
            plans[1].syntax_rejection(),
            Some(SyntaxPlanRejection::InvalidRun {
                error: SpanMapError::Reversed,
                ..
            })
        ));
        assert!(matches!(
            plans[2].syntax_rejection(),
            Some(SyntaxPlanRejection::UnsortedOrOverlapping { index: 1 })
        ));
        assert!(matches!(
            plans[3].syntax_rejection(),
            Some(SyntaxPlanRejection::InvalidRun {
                error: SpanMapError::OutOfBounds { .. },
                ..
            })
        ));
        assert!(matches!(
            plans[4].syntax_rejection(),
            Some(SyntaxPlanRejection::InvalidRun {
                error: SpanMapError::CharacterRangeMismatch { .. },
                ..
            })
        ));
        assert_eq!(
            plans[5].syntax_rejection(),
            Some(SyntaxPlanRejection::TooManyRuns {
                actual: MAX_PREPARED_SYNTAX_RUNS + 1,
                maximum: MAX_PREPARED_SYNTAX_RUNS,
            })
        );

        for plan in plans {
            assert_eq!(plan.runs(), marker_only.runs());
            assert_eq!(plan.gutter_markers(), marker_only.gutter_markers());
            assert_eq!(plan.point_markers(), marker_only.point_markers());
            assert_eq!(plan.rejected_markers(), marker_only.rejected_markers());
        }
    }

    #[test]
    fn syntax_changes_only_foreground_and_preserves_every_marker_decoration() {
        let stamp = document(5);
        let source = "a🦀bcdef";
        let mapper = SourceMapper::for_document(stamp, source);
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs {
            current: Some(document_range(&mapper, stamp, 0..5)),
            fault: Some(document_range(&mapper, stamp, 1..7)),
            navigation: Some(document_range(&mapper, stamp, 5..10)),
        });
        let marker_only = PreparedLayoutPlan::compose(&mapper, &markers, &[]);
        let with_syntax = PreparedLayoutPlan::compose(
            &mapper,
            &markers,
            &[
                PreparedSyntaxRun::new(document_range(&mapper, stamp, 0..6), SyntaxClass::Keyword),
                PreparedSyntaxRun::new(document_range(&mapper, stamp, 6..10), SyntaxClass::Comment),
            ],
        );

        let plain_job = build_prepared_layout_job(stamp, source, &marker_only, test_format);
        let syntax_job = build_prepared_layout_job(stamp, source, &with_syntax, test_format);

        assert_eq!(plain_job.text, source);
        assert_eq!(syntax_job.text, source);
        assert_eq!(
            decorations_by_byte(&plain_job, source.len()),
            decorations_by_byte(&syntax_job, source.len())
        );
        assert_eq!(marker_only.gutter_markers(), with_syntax.gutter_markers());
        assert_eq!(marker_only.point_markers(), with_syntax.point_markers());
        assert!(
            syntax_job
                .sections
                .iter()
                .any(|section| section.format.color != Color32::BLACK)
        );
    }

    #[test]
    fn maximum_syntax_plan_merges_with_markers_within_a_linear_operation_bound() {
        let stamp = document(6);
        let source = "x".repeat(MAX_PREPARED_SYNTAX_RUNS);
        let mapper = SourceMapper::for_document(stamp, &source);
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs {
            current: Some(document_range(&mapper, stamp, 100..3_000)),
            fault: Some(document_range(&mapper, stamp, 1_000..2_000)),
            navigation: Some(document_range(&mapper, stamp, 2_500..4_000)),
        });
        let syntax = (0..MAX_PREPARED_SYNTAX_RUNS)
            .map(|offset| {
                PreparedSyntaxRun::new(
                    document_range(&mapper, stamp, offset..offset + 1),
                    match offset % 6 {
                        0 => SyntaxClass::Keyword,
                        1 => SyntaxClass::Comment,
                        2 => SyntaxClass::String,
                        3 => SyntaxClass::Number,
                        4 => SyntaxClass::Variable,
                        _ => SyntaxClass::Operator,
                    },
                )
            })
            .collect::<Vec<_>>();

        let plan = PreparedLayoutPlan::compose(&mapper, &markers, &syntax);
        let bound = 4 * (syntax.len() + markers.format_runs().len() + 1);

        assert_eq!(plan.syntax_rejection(), None);
        assert_eq!(plan.runs().len(), MAX_PREPARED_SYNTAX_RUNS);
        assert!(
            plan.merge_operations() > syntax.len(),
            "the counter must charge pointer advancement as well as emitted runs"
        );
        assert!(
            plan.merge_operations() <= bound,
            "{} merge operations exceeded the {bound} operation bound",
            plan.merge_operations()
        );
    }

    #[test]
    fn layouter_rejects_a_same_byte_length_source_with_different_character_identity() {
        let stamp = document(7);
        let mapper = SourceMapper::for_document(stamp, "é");
        let markers = mapper.resolve_document_markers(DocumentMarkerInputs::default());
        let plan = PreparedLayoutPlan::compose(
            &mapper,
            &markers,
            &[PreparedSyntaxRun::new(
                document_range(&mapper, stamp, 0..2),
                SyntaxClass::Keyword,
            )],
        );

        let job = build_prepared_layout_job(stamp, "ab", &plan, test_format);

        assert_eq!(job.text, "ab");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, Color32::BLACK);
        assert_eq!(job.sections[0].format.background, Color32::TRANSPARENT);
        assert_eq!(job.sections[0].format.underline, Stroke::NONE);
    }
}

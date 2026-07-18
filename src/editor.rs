use std::collections::BTreeMap;
use std::ops::Range;

use eframe::egui::text::{ByteIndex, CCursor, CCursorRange, CharIndex, LayoutJob};
use eframe::egui::{TextBuffer, TextFormat};
use rlox::{SourceId, SourceSpan, TextPosition};

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
        expected: rlox::RevisionId,
        actual: rlox::RevisionId,
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
    key: EditorSourceKey,
    text_bytes: usize,
    total_chars: usize,
    line_starts: Vec<LineStart>,
    multibyte_scalars: Vec<MultibyteScalar>,
}

impl SourceMapper {
    pub fn new(key: EditorSourceKey, source: &str) -> Self {
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
            key,
            text_bytes: source.len(),
            total_chars: char_index,
            line_starts,
            multibyte_scalars,
        }
    }

    pub fn key(&self) -> EditorSourceKey {
        self.key
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
        if marker.source.document != self.key.document {
            return Err(SpanMapError::StaleDocument {
                expected: self.key.document,
                actual: marker.source.document,
            });
        }
        if marker.source.run != self.key.run {
            return Err(SpanMapError::StaleRun {
                expected: self.key.run,
                actual: marker.source.run,
            });
        }
        if marker.span.source_id != self.key.run.source_id {
            return Err(SpanMapError::WrongSource {
                expected: self.key.run.source_id,
                actual: marker.span.source_id,
            });
        }
        if marker.span.revision != self.key.run.source_revision {
            return Err(SpanMapError::WrongRevision {
                expected: self.key.run.source_revision,
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
            source: self.key,
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
    source: EditorSourceKey,
    text_bytes: usize,
    format_runs: Vec<FormatRun>,
    point_markers: Vec<PointMarker>,
    gutter_markers: Vec<GutterMarker>,
    rejected: Vec<RejectedMarker>,
}

impl MarkerPlan {
    pub fn source(&self) -> EditorSourceKey {
        self.source
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

pub use oxide_ide::{
    DocumentId, DocumentStamp, EditRevision, MAX_SOURCE_LINES, RunBinding, RunId, WorkerSessionId,
};
use rlox_protocol::{RevisionId, SourceId, SourceSpan, TextPosition};

#[allow(dead_code)]
#[path = "../src/editor.rs"]
mod editor;

use editor::{
    BoundedTextBuffer, EditorSourceKey, MarkerInputs, MarkerKind, MarkerMask, MarkerSpan,
    SourceGrowthLimit, SourceMapper, SpanEndpoint, SpanMapError, build_layout_job, gutter_digits,
};
use eframe::egui::{Color32, FontId, Stroke, TextBuffer as _, TextFormat};

fn key(
    document_revision: u64,
    run_id: u64,
    source_id: u64,
    source_revision: u64,
) -> EditorSourceKey {
    EditorSourceKey::new(
        DocumentStamp {
            document_id: DocumentId::from_raw(41).unwrap(),
            edit_revision: EditRevision::from_raw(document_revision).unwrap(),
        },
        RunBinding {
            worker_session_id: WorkerSessionId(7),
            run_id: RunId(run_id),
            source_id: SourceId(source_id),
            source_revision: RevisionId(source_revision),
        },
    )
}

fn position(source: &str, byte_offset: usize) -> TextPosition {
    assert!(source.is_char_boundary(byte_offset));
    let prefix = &source[..byte_offset];
    let line_start = prefix.rfind('\n').map_or(0, |offset| offset + 1);
    TextPosition {
        byte_offset,
        line: prefix
            .chars()
            .filter(|character| *character == '\n')
            .count()
            + 1,
        column: source[line_start..byte_offset].chars().count() + 1,
    }
}

fn span(source: &str, key: EditorSourceKey, start: usize, end: usize) -> SourceSpan {
    SourceSpan {
        source_id: key.run.source_id,
        revision: key.run.source_revision,
        start: position(source, start),
        end: position(source, end),
    }
}

fn marker(source: &str, key: EditorSourceKey, start: usize, end: usize) -> MarkerSpan {
    MarkerSpan::new(key, span(source, key, start, end))
}

fn mask(kinds: &[MarkerKind]) -> MarkerMask {
    kinds
        .iter()
        .copied()
        .fold(MarkerMask::NONE, MarkerMask::with)
}

#[test]
fn line_count_keeps_empty_and_trailing_source_lines() {
    let source_key = key(1, 1, 10, 1);
    let empty = SourceMapper::new(source_key, "");
    assert_eq!(empty.line_count(), 1);
    assert_eq!(empty.total_chars(), 0);
    assert_eq!(empty.position_at(0).unwrap().line_index, 0);

    let trailing = SourceMapper::new(source_key, "one\n\n");
    assert_eq!(trailing.line_count(), 3);
    assert_eq!(trailing.position_at(5).unwrap().line_index, 2);
    assert_eq!(trailing.position_at(5).unwrap().column_index, 0);
    assert_eq!(trailing.line_start(2).unwrap().byte_index.0, 5);
    assert_eq!(trailing.line_start(2).unwrap().char_index.0, 5);
}

#[test]
fn bounded_text_buffer_counts_bare_carriage_returns_like_model_normalization() {
    let mut source = String::new();
    let mut buffer = BoundedTextBuffer::with_limits(&mut source, 1024, 2);

    assert_eq!(buffer.insert_text("one\rtwo\rthree", 0.into()), 0);

    let rejection = buffer
        .rejection()
        .expect("three normalized lines exceed the cap");
    assert_eq!(rejection.limit, SourceGrowthLimit::Lines);
    assert_eq!(rejection.attempted_lines, 3);
    assert!(source.is_empty(), "rejected text must be rolled back");

    let mut crlf_source = String::new();
    let mut crlf_buffer = BoundedTextBuffer::with_limits(&mut crlf_source, 1024, 2);
    assert_eq!(crlf_buffer.insert_text("one\r\ntwo", 0.into()), 8);
    assert!(crlf_buffer.rejection().is_none());
}

#[test]
fn ascii_offsets_map_to_egui_character_indices_and_scalar_columns() {
    let source = "var answer = 42;\n";
    let source_key = key(2, 2, 11, 3);
    let mapper = SourceMapper::new(source_key, source);
    let mapped = mapper.map_span(marker(source, source_key, 4, 10)).unwrap();

    assert_eq!(mapped.start.byte_index.0, 4);
    assert_eq!(mapped.start.char_index.0, 4);
    assert_eq!((mapped.start.line_index, mapped.start.column_index), (0, 4));
    assert_eq!(mapped.end.byte_index.0, 10);
    assert_eq!(mapped.end.char_index.0, 10);
    let cursor = mapped.egui_cursor_range();
    assert_eq!(cursor.as_sorted_char_range(), 4.into()..10.into());
    assert_eq!(cursor.primary.index.0, 4);
    assert_eq!(cursor.secondary.index.0, 10);
}

#[test]
fn unicode_emoji_combining_marks_and_tabs_map_by_scalar_not_byte() {
    let source = "\té🦀e\u{301}\n語";
    let source_key = key(3, 3, 12, 4);
    let mapper = SourceMapper::new(source_key, source);
    let mapped = mapper.map_span(marker(source, source_key, 3, 10)).unwrap();

    assert_eq!(mapped.start.char_index.0, 2);
    assert_eq!((mapped.start.line_index, mapped.start.column_index), (0, 2));
    assert_eq!(mapped.end.char_index.0, 5);
    assert_eq!((mapped.end.line_index, mapped.end.column_index), (0, 5));

    let second_line = mapper.position_at(11).unwrap();
    assert_eq!(second_line.char_index.0, 6);
    assert_eq!((second_line.line_index, second_line.column_index), (1, 0));
    assert_eq!(mapper.position_at(source.len()).unwrap().char_index.0, 7);
}

#[test]
fn multiline_and_eof_spans_keep_exact_endpoints() {
    let source = "a🦀\n語z\n";
    let source_key = key(4, 4, 13, 5);
    let mapper = SourceMapper::new(source_key, source);
    let multiline = mapper.map_span(marker(source, source_key, 1, 9)).unwrap();
    assert_eq!(multiline.start.char_index.0, 1);
    assert_eq!(multiline.end.char_index.0, 4);
    assert_eq!(
        (multiline.end.line_index, multiline.end.column_index),
        (1, 1)
    );

    let eof = mapper
        .map_span(marker(source, source_key, source.len(), source.len()))
        .unwrap();
    assert!(eof.is_empty());
    assert_eq!(eof.start.char_index.0, 6);
    assert_eq!((eof.start.line_index, eof.start.column_index), (2, 0));
}

#[test]
fn document_and_full_run_identity_are_independent_staleness_gates() {
    let source = "print 1;";
    let displayed = key(8, 20, 30, 40);
    let mapper = SourceMapper::new(displayed, source);

    let stale_document = key(9, 20, 30, 40);
    assert!(matches!(
        mapper.map_span(marker(source, stale_document, 0, 5)),
        Err(SpanMapError::StaleDocument { .. })
    ));

    let stale_run = key(8, 21, 30, 40);
    assert!(matches!(
        mapper.map_span(marker(source, stale_run, 0, 5)),
        Err(SpanMapError::StaleRun { .. })
    ));

    let mut wrong_source = span(source, displayed, 0, 5);
    wrong_source.source_id = SourceId(31);
    assert!(matches!(
        mapper.map_span(MarkerSpan::new(displayed, wrong_source)),
        Err(SpanMapError::WrongSource { .. })
    ));

    let mut wrong_revision = span(source, displayed, 0, 5);
    wrong_revision.revision = RevisionId(41);
    assert!(matches!(
        mapper.map_span(MarkerSpan::new(displayed, wrong_revision)),
        Err(SpanMapError::WrongRevision { .. })
    ));
}

#[test]
fn malformed_offsets_and_noncanonical_positions_are_rejected_not_clamped() {
    let source = "a🦀z";
    let source_key = key(5, 5, 14, 6);
    let mapper = SourceMapper::new(source_key, source);

    let mut reversed = span(source, source_key, 0, source.len());
    std::mem::swap(&mut reversed.start, &mut reversed.end);
    assert_eq!(
        mapper.map_span(MarkerSpan::new(source_key, reversed)),
        Err(SpanMapError::Reversed)
    );

    let mut beyond = span(source, source_key, 0, source.len());
    beyond.end.byte_offset += 1;
    assert!(matches!(
        mapper.map_span(MarkerSpan::new(source_key, beyond)),
        Err(SpanMapError::OutOfBounds {
            endpoint: SpanEndpoint::End,
            ..
        })
    ));

    let mut mid_scalar = span(source, source_key, 0, source.len());
    mid_scalar.start.byte_offset = 2;
    assert_eq!(
        mapper.map_span(MarkerSpan::new(source_key, mid_scalar)),
        Err(SpanMapError::NotCharBoundary {
            endpoint: SpanEndpoint::Start,
            byte_offset: 2,
        })
    );

    let mut wrong_position = span(source, source_key, 0, source.len());
    wrong_position.end.column += 1;
    assert!(matches!(
        mapper.map_span(MarkerSpan::new(source_key, wrong_position)),
        Err(SpanMapError::PositionMismatch {
            endpoint: SpanEndpoint::End,
            ..
        })
    ));
}

#[test]
fn overlapping_markers_become_disjoint_runs_with_explicit_priority() {
    let source = "abcdef!";
    let source_key = key(6, 6, 15, 7);
    let mapper = SourceMapper::new(source_key, source);
    let plan = mapper.resolve_markers(MarkerInputs {
        current: Some(marker(source, source_key, 2, 6)),
        fault: Some(marker(source, source_key, 3, 5)),
        navigation: Some(marker(source, source_key, 0, 4)),
    });

    let actual = plan
        .format_runs()
        .iter()
        .map(|run| (run.byte_range.clone(), run.markers, run.markers.primary()))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            (
                0..2,
                mask(&[MarkerKind::Navigation]),
                Some(MarkerKind::Navigation)
            ),
            (
                2..3,
                mask(&[MarkerKind::Navigation, MarkerKind::Current]),
                Some(MarkerKind::Current),
            ),
            (
                3..4,
                mask(&[
                    MarkerKind::Navigation,
                    MarkerKind::Current,
                    MarkerKind::Fault,
                ]),
                Some(MarkerKind::Fault),
            ),
            (
                4..5,
                mask(&[MarkerKind::Current, MarkerKind::Fault]),
                Some(MarkerKind::Fault),
            ),
            (
                5..6,
                mask(&[MarkerKind::Current]),
                Some(MarkerKind::Current)
            ),
            (6..7, MarkerMask::NONE, None),
        ]
    );
    assert!(plan.rejected().is_empty());
}

#[test]
fn zero_width_eof_markers_remain_visible_and_preserve_all_meanings() {
    let source = "a\n";
    let source_key = key(7, 7, 16, 8);
    let mapper = SourceMapper::new(source_key, source);
    let eof = marker(source, source_key, source.len(), source.len());
    let plan = mapper.resolve_markers(MarkerInputs {
        current: Some(eof),
        fault: Some(eof),
        navigation: Some(eof),
    });

    assert_eq!(plan.point_markers().len(), 1);
    let point = &plan.point_markers()[0];
    assert!(point.at_eof);
    assert_eq!(point.position.char_index.0, 2);
    assert_eq!(point.position.line_index, 1);
    assert_eq!(
        point.markers,
        mask(&[
            MarkerKind::Navigation,
            MarkerKind::Current,
            MarkerKind::Fault,
        ])
    );

    assert_eq!(plan.gutter_markers().len(), 1);
    assert_eq!(plan.gutter_markers()[0].line_index, 1);
    assert_eq!(plan.gutter_markers()[0].markers, point.markers);
}

#[test]
fn one_stale_marker_does_not_hide_other_valid_markers() {
    let source = "abc";
    let displayed = key(10, 10, 50, 60);
    let stale = key(11, 10, 50, 60);
    let mapper = SourceMapper::new(displayed, source);
    let plan = mapper.resolve_markers(MarkerInputs {
        current: Some(marker(source, displayed, 0, 2)),
        fault: None,
        navigation: Some(marker(source, stale, 1, 3)),
    });

    assert_eq!(plan.rejected().len(), 1);
    assert_eq!(plan.rejected()[0].kind, MarkerKind::Navigation);
    assert!(matches!(
        plan.rejected()[0].error,
        SpanMapError::StaleDocument { .. }
    ));
    assert_eq!(plan.format_runs()[0].markers, mask(&[MarkerKind::Current]));
}

#[test]
fn layout_job_preserves_text_and_uses_only_valid_utf8_section_boundaries() {
    let source = "a🦀b\n";
    let source_key = key(12, 12, 70, 80);
    let mapper = SourceMapper::new(source_key, source);
    let plan = mapper.resolve_markers(MarkerInputs {
        current: Some(marker(source, source_key, 1, 5)),
        fault: None,
        navigation: Some(marker(source, source_key, 0, 6)),
    });
    let job = build_layout_job(source, &plan, |markers| {
        let color = match markers.primary() {
            Some(MarkerKind::Fault) => Color32::RED,
            Some(MarkerKind::Current) => Color32::YELLOW,
            Some(MarkerKind::Navigation) => Color32::BLUE,
            None => Color32::BLACK,
        };
        TextFormat {
            font_id: FontId::monospace(14.0),
            color,
            underline: Stroke::NONE,
            ..Default::default()
        }
    });

    assert_eq!(job.text, source);
    assert_eq!(job.wrap.max_width, f32::INFINITY);
    assert!(job.break_on_newline);
    assert!(job.keep_trailing_whitespace);
    let mut next = 0;
    for section in &job.sections {
        assert_eq!(section.byte_range.start.0, next);
        assert!(source.is_char_boundary(section.byte_range.start.0));
        assert!(source.is_char_boundary(section.byte_range.end.0));
        next = section.byte_range.end.0;
    }
    assert_eq!(next, source.len());
}

#[test]
fn mapper_indexes_a_normalized_multibyte_buffer() {
    let source_key = key(13, 13, 90, 100);
    let normalized = "a\n🦀\nb";
    let mapper = SourceMapper::new(source_key, normalized);
    assert_eq!(mapper.line_count(), 3);
    assert_eq!(
        mapper.position_at(normalized.len()).unwrap().char_index.0,
        5
    );
    assert_eq!(mapper.position_at(normalized.len()).unwrap().line_index, 2);
}

#[test]
fn gutter_digit_count_changes_only_at_decimal_boundaries() {
    assert_eq!(gutter_digits(1), 1);
    assert_eq!(gutter_digits(9), 1);
    assert_eq!(gutter_digits(10), 2);
    assert_eq!(gutter_digits(99), 2);
    assert_eq!(gutter_digits(100), 3);
}

#[test]
fn bounded_text_buffer_accepts_growth_at_or_below_the_byte_limit() {
    let mut text = "é".to_owned();
    let mut buffer = BoundedTextBuffer::new(&mut text, 6);
    assert_eq!(buffer.insert_text("🦀", 1.into()), 1);
    assert_eq!(buffer.as_str(), "é🦀");
    assert_eq!(buffer.rejection(), None);
}

#[test]
fn bounded_text_buffer_rejects_multiline_paste_without_partial_insertion() {
    let mut text = "start".to_owned();
    let original = text.clone();
    let mut buffer = BoundedTextBuffer::new(&mut text, 12);
    assert_eq!(buffer.insert_text("\nline two\nline three", 5.into()), 0);
    assert_eq!(buffer.as_str(), original);
    let rejection = buffer.rejection().unwrap();
    assert_eq!(rejection.current_bytes, 5);
    assert_eq!(rejection.inserted_bytes, 20);
    assert_eq!(rejection.max_bytes, 12);
}

#[test]
fn bounded_text_buffer_rejects_excessive_lines_without_partial_insertion() {
    let mut text = "first".to_owned();
    let original = text.clone();
    let mut buffer = BoundedTextBuffer::with_limits(&mut text, 1024, 3);
    assert_eq!(buffer.insert_text("\nsecond\nthird\nfourth", 5.into()), 0);
    assert_eq!(buffer.as_str(), original);
    let rejection = buffer.rejection().unwrap();
    assert_eq!(rejection.limit, SourceGrowthLimit::Lines);
    assert_eq!(rejection.attempted_lines, 4);
    assert_eq!(rejection.max_lines, 3);
}

#[test]
fn oversized_replacement_rolls_back_eguis_delete_then_insert_sequence() {
    let mut text = "abcdef".to_owned();
    let original = text.clone();
    let mut buffer = BoundedTextBuffer::new(&mut text, 8);

    buffer.delete_char_range(1.into()..5.into());
    assert_eq!(buffer.as_str(), "af");
    assert_eq!(buffer.insert_text("0123456789", 1.into()), 0);
    assert_eq!(buffer.as_str(), original);
    assert!(buffer.rejection().is_some());

    buffer.delete_char_range(0.into()..1.into());
    assert_eq!(
        buffer.as_str(),
        original,
        "mutations after rejection must be inert"
    );
}

#[test]
fn deletion_can_reduce_an_existing_oversized_buffer_below_the_cap() {
    let mut text = "0123456789".to_owned();
    let mut buffer = BoundedTextBuffer::new(&mut text, 6);
    buffer.delete_char_range(0.into()..4.into());
    assert_eq!(buffer.as_str(), "456789");
    assert_eq!(buffer.rejection(), None);
}

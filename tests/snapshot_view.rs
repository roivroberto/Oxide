use oxide_ide::{
    EventSequence, RunBinding, RunId, SnapshotKey, SnapshotProvenance, WorkerSessionId,
};
use rlox_protocol::{
    ActivationId, BindingId, BindingSnapshot, DebugValue, FrameSnapshot, RevisionId, SourceId,
    SourceSpan, TextPosition, ValueKind,
};

#[path = "../src/snapshot_view.rs"]
mod snapshot_view;

use snapshot_view::{
    BindingScope, MAX_VALUE_PATH_DEPTH, ValuePath, binding_accessible_name, binding_kind_label,
    frame_accessible_name, present_binding, present_debug_value, snapshot_accessible_name,
    snapshot_provenance_label, value_kind_label,
};

fn snapshot_key() -> SnapshotKey {
    SnapshotKey {
        run: RunBinding {
            worker_session_id: WorkerSessionId(3),
            run_id: RunId(5),
            source_id: SourceId(7),
            source_revision: RevisionId(11),
        },
        sequence: EventSequence(13),
    }
}

fn position(byte_offset: usize, line: usize, column: usize) -> TextPosition {
    TextPosition {
        byte_offset,
        line,
        column,
    }
}

fn span() -> SourceSpan {
    SourceSpan {
        source_id: SourceId(7),
        revision: RevisionId(11),
        start: position(2, 2, 3),
        end: position(4, 2, 5),
    }
}

fn root_path() -> ValuePath {
    ValuePath::frame_binding(
        snapshot_key(),
        ActivationId(17),
        BindingScope::Locals,
        Some(BindingId(19)),
        0,
    )
}

#[test]
fn presents_every_debug_value_without_debug_or_object_addresses() {
    let cases = [
        (DebugValue::Nil, "nil", "nil"),
        (DebugValue::Bool(true), "boolean", "true"),
        (
            DebugValue::Number("-0.125e+9".to_owned()),
            "number",
            "-0.125e+9",
        ),
        (
            DebugValue::String("line\n\0\"\\tail".to_owned()),
            "string",
            "\"line\\n\\u{0}\\\"\\\\tail\"",
        ),
        (
            DebugValue::Function("f\nname".to_owned()),
            "function",
            "function f\\nname",
        ),
        (
            DebugValue::Closure("c\tname".to_owned()),
            "closure",
            "closure c\\tname",
        ),
        (
            DebugValue::Native("clock\r".to_owned()),
            "native function",
            "native function clock\\r",
        ),
        (
            DebugValue::Cycle {
                object_id: 0xdead_beef,
            },
            "cycle",
            "cycle reference",
        ),
        (DebugValue::Truncated, "truncated", "value omitted"),
    ];

    for (value, expected_kind, expected_summary) in cases {
        let presented = present_debug_value(&value, root_path());
        assert_eq!(presented.kind_label, expected_kind);
        assert_eq!(presented.summary, expected_summary);
        assert!(presented.children.is_empty());
        assert!(!presented.summary.contains("DebugValue"));
        assert!(!presented.summary.contains("deadbeef"));
        assert!(!presented.summary.contains("3735928559"));
    }
}

#[test]
fn list_children_have_stable_distinct_paths_and_explicit_omission_labels() {
    let value = DebugValue::List {
        object_id: 4_294_967_295,
        items: vec![DebugValue::Number("1.0".to_owned()), DebugValue::Nil],
        truncated: true,
    };

    let first = present_debug_value(&value, root_path());
    let second = present_debug_value(&value, root_path());

    assert_eq!(first, second);
    assert_eq!(first.kind_label, "list");
    assert_eq!(first.summary, "list, 2 items shown, more items omitted");
    assert!(first.is_expandable());
    assert!(first.is_truncated());
    assert_eq!(first.children[0].path, root_path().child(0).unwrap());
    assert_eq!(first.children[1].path, root_path().child(1).unwrap());
    assert_ne!(first.children[0].path, first.children[1].path);
    assert!(!first.summary.contains("4294967295"));
}

#[test]
fn value_kind_labels_are_exhaustive_and_user_facing() {
    let cases = [
        (ValueKind::Nil, "nil"),
        (ValueKind::Bool, "boolean"),
        (ValueKind::Number, "number"),
        (ValueKind::String, "string"),
        (ValueKind::Function, "function"),
        (ValueKind::Closure, "closure"),
        (ValueKind::Native, "native function"),
        (ValueKind::List, "list"),
        (ValueKind::Cycle, "cycle"),
        (ValueKind::Truncated, "truncated"),
    ];

    for (kind, expected) in cases {
        assert_eq!(value_kind_label(kind), expected);
    }
}

#[test]
fn binding_kinds_and_names_are_explicit_and_control_safe() {
    for (raw, expected) in [
        ("parameter", "parameter"),
        ("local", "local"),
        ("implicit", "implicit local"),
        ("upvalue", "captured variable"),
        ("global", "global"),
        ("future\nkind", "future\\nkind"),
    ] {
        assert_eq!(binding_kind_label(raw), expected);
    }

    let binding = BindingSnapshot {
        binding_id: Some(BindingId(19)),
        name: "answer\nnext".to_owned(),
        name_truncated: true,
        binding_kind: "local".to_owned(),
        value_kind: ValueKind::Number,
        value: DebugValue::Number("42".to_owned()),
    };
    assert_eq!(
        binding_accessible_name(&binding),
        "local answer\\nnext (name truncated), number, 42"
    );

    let presented = present_binding(&binding, root_path());
    assert_eq!(presented.name, "answer\\nnext");
    assert!(presented.name_truncated);
    assert_eq!(presented.kind_label, "local");
    assert_eq!(presented.value_kind_label, "number");
    assert_eq!(presented.value.path, root_path());
    assert_eq!(presented.accessible_name, binding_accessible_name(&binding));
}

#[test]
fn provenance_and_frame_helpers_produce_accessible_context() {
    let provenance_cases = [
        (SnapshotProvenance::Paused, "Current paused snapshot"),
        (SnapshotProvenance::Faulted, "Snapshot at error"),
        (SnapshotProvenance::Cancelled, "Snapshot after stop"),
        (
            SnapshotProvenance::LastSafePause,
            "Last safe paused snapshot",
        ),
    ];
    for (provenance, expected) in provenance_cases {
        assert_eq!(snapshot_provenance_label(provenance), expected);
    }
    assert_eq!(
        snapshot_accessible_name(SnapshotProvenance::Faulted, 2, true, false),
        "Snapshot at error, 2 frames shown, more frames omitted"
    );
    assert_eq!(
        snapshot_accessible_name(SnapshotProvenance::Cancelled, 1, false, true),
        "Snapshot after stop, 1 frame, some globals omitted"
    );

    let frame = FrameSnapshot {
        activation_id: ActivationId(17),
        function: "work\nitem".to_owned(),
        function_truncated: true,
        current_span: span(),
        call_site: None,
        parameters: Vec::new(),
        parameters_truncated: false,
        locals: Vec::new(),
        locals_truncated: false,
        upvalues: Vec::new(),
        upvalues_truncated: false,
    };
    assert_eq!(
        frame_accessible_name(&frame, 0, true),
        "Frame 1, function work\\nitem (name truncated), line 2, column 3, selected"
    );
}

#[test]
fn defensive_presentation_limits_depth_and_nodes() {
    let mut deep = DebugValue::Nil;
    for object_id in 1..=(MAX_VALUE_PATH_DEPTH as u64 + 4) {
        deep = DebugValue::List {
            object_id,
            items: vec![deep],
            truncated: false,
        };
    }
    let presented = present_debug_value(&deep, root_path());
    let mut cursor = &presented;
    let mut edges = 0;
    while let Some(child) = cursor.children.first() {
        edges += 1;
        cursor = child;
    }
    assert_eq!(edges, MAX_VALUE_PATH_DEPTH);
    assert!(cursor.is_truncated());
    assert!(cursor.summary.contains("omitted"));

    let wide = DebugValue::List {
        object_id: 1,
        items: vec![DebugValue::Nil; snapshot_view::MAX_PRESENTED_VALUE_NODES + 100],
        truncated: false,
    };
    let presented = present_debug_value(&wide, root_path());
    assert_eq!(
        presented.children.len(),
        snapshot_view::MAX_PRESENTED_VALUE_NODES - 1
    );
    assert!(presented.is_truncated());
}

#[test]
fn global_and_fallback_binding_paths_are_stable_without_displaying_ids() {
    let global = ValuePath::global_binding(snapshot_key(), 3);
    let first_fallback = ValuePath::frame_binding(
        snapshot_key(),
        ActivationId(17),
        BindingScope::Upvalues,
        None,
        2,
    );
    let second_fallback = ValuePath::frame_binding(
        snapshot_key(),
        ActivationId(17),
        BindingScope::Upvalues,
        None,
        2,
    );

    assert_ne!(global, first_fallback);
    assert_eq!(first_fallback, second_fallback);
    assert_eq!(global.snapshot(), snapshot_key());

    let parameter = ValuePath::frame_binding(
        snapshot_key(),
        ActivationId(17),
        BindingScope::Parameters,
        Some(BindingId(23)),
        0,
    );
    assert_ne!(parameter, first_fallback);
}

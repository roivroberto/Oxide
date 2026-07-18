use std::borrow::Cow;

use rlox_protocol::{
    ActivationId, BindingId, BindingSnapshot, DebugValue, FrameSnapshot, ValueKind,
};

use crate::{SnapshotKey, SnapshotProvenance};

pub const MAX_VALUE_PATH_DEPTH: usize = 16;
pub const MAX_PRESENTED_VALUE_NODES: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BindingScope {
    Parameters,
    Locals,
    Upvalues,
    Globals,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BindingPathKey {
    Id(BindingId),
    Ordinal(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BindingLocation {
    activation_id: Option<ActivationId>,
    scope: BindingScope,
    binding: BindingPathKey,
}

/// A stable, opaque identity for one value tree in a retained snapshot.
///
/// The identity is suitable for an egui collapsing-state key. Runtime object
/// IDs are deliberately excluded: list identity is an implementation detail,
/// while the source-visible binding and item path are stable for the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValuePath {
    snapshot: SnapshotKey,
    location: BindingLocation,
    items: Vec<usize>,
}

impl ValuePath {
    pub fn frame_binding(
        snapshot: SnapshotKey,
        activation_id: ActivationId,
        scope: BindingScope,
        binding_id: Option<BindingId>,
        ordinal: usize,
    ) -> Self {
        debug_assert!(scope != BindingScope::Globals);
        Self {
            snapshot,
            location: BindingLocation {
                activation_id: Some(activation_id),
                scope,
                binding: binding_id
                    .map(BindingPathKey::Id)
                    .unwrap_or(BindingPathKey::Ordinal(ordinal)),
            },
            items: Vec::new(),
        }
    }

    pub fn global_binding(snapshot: SnapshotKey, ordinal: usize) -> Self {
        Self {
            snapshot,
            location: BindingLocation {
                activation_id: None,
                scope: BindingScope::Globals,
                binding: BindingPathKey::Ordinal(ordinal),
            },
            items: Vec::new(),
        }
    }

    pub fn child(&self, index: usize) -> Option<Self> {
        if self.items.len() >= MAX_VALUE_PATH_DEPTH {
            return None;
        }
        let mut child = self.clone();
        child.items.push(index);
        Some(child)
    }

    pub fn depth(&self) -> usize {
        self.items.len()
    }

    pub fn snapshot(&self) -> SnapshotKey {
        self.snapshot
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentedValueState {
    Scalar,
    List { truncated: bool },
    Cycle,
    Truncated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedValue {
    pub path: ValuePath,
    pub kind_label: &'static str,
    pub summary: String,
    pub state: PresentedValueState,
    pub children: Vec<PresentedValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedBinding {
    pub name: String,
    pub name_truncated: bool,
    pub kind_label: String,
    pub value_kind_label: &'static str,
    pub accessible_name: String,
    pub value: PresentedValue,
}

impl PresentedValue {
    pub fn is_expandable(&self) -> bool {
        matches!(self.state, PresentedValueState::List { .. }) && !self.children.is_empty()
    }

    pub fn is_truncated(&self) -> bool {
        matches!(
            self.state,
            PresentedValueState::Truncated | PresentedValueState::List { truncated: true }
        )
    }
}

pub fn present_debug_value(value: &DebugValue, path: ValuePath) -> PresentedValue {
    let mut remaining_nodes = MAX_PRESENTED_VALUE_NODES;
    present_value(value, path, &mut remaining_nodes)
}

pub fn present_binding(binding: &BindingSnapshot, path: ValuePath) -> PresentedBinding {
    PresentedBinding {
        name: escape_display_text(&binding.name),
        name_truncated: binding.name_truncated,
        kind_label: binding_kind_label(&binding.binding_kind).into_owned(),
        value_kind_label: value_kind_label(binding.value_kind),
        accessible_name: binding_accessible_name(binding),
        value: present_debug_value(&binding.value, path),
    }
}

fn present_value(
    value: &DebugValue,
    path: ValuePath,
    remaining_nodes: &mut usize,
) -> PresentedValue {
    *remaining_nodes = remaining_nodes.saturating_sub(1);
    match value {
        DebugValue::Nil => scalar(path, "nil", "nil"),
        DebugValue::Bool(value) => scalar(path, "boolean", value.to_string()),
        DebugValue::Number(value) => scalar(path, "number", escape_display_text(value)),
        DebugValue::String(value) => scalar(
            path,
            "string",
            format!("\"{}\"", escape_display_text(value)),
        ),
        DebugValue::Function(value) => scalar(
            path,
            "function",
            format!("function {}", escape_display_text(value)),
        ),
        DebugValue::Closure(value) => scalar(
            path,
            "closure",
            format!("closure {}", escape_display_text(value)),
        ),
        DebugValue::Native(value) => scalar(
            path,
            "native function",
            format!("native function {}", escape_display_text(value)),
        ),
        DebugValue::List {
            items, truncated, ..
        } => {
            let mut children = Vec::new();
            let depth_limited = path.depth() >= MAX_VALUE_PATH_DEPTH;
            if !depth_limited {
                for (index, item) in items.iter().enumerate() {
                    if *remaining_nodes == 0 {
                        break;
                    }
                    let Some(child_path) = path.child(index) else {
                        break;
                    };
                    children.push(present_value(item, child_path, remaining_nodes));
                }
            }
            let omitted = *truncated || children.len() < items.len();
            PresentedValue {
                path,
                kind_label: "list",
                summary: list_summary(children.len(), omitted),
                state: PresentedValueState::List { truncated: omitted },
                children,
            }
        }
        DebugValue::Cycle { .. } => PresentedValue {
            path,
            kind_label: "cycle",
            summary: "cycle reference".to_owned(),
            state: PresentedValueState::Cycle,
            children: Vec::new(),
        },
        DebugValue::Truncated => PresentedValue {
            path,
            kind_label: "truncated",
            summary: "value omitted".to_owned(),
            state: PresentedValueState::Truncated,
            children: Vec::new(),
        },
    }
}

fn scalar(path: ValuePath, kind_label: &'static str, summary: impl Into<String>) -> PresentedValue {
    PresentedValue {
        path,
        kind_label,
        summary: summary.into(),
        state: PresentedValueState::Scalar,
        children: Vec::new(),
    }
}

fn list_summary(shown: usize, omitted: bool) -> String {
    match (shown, omitted) {
        (0, false) => "empty list".to_owned(),
        (0, true) => "list, items omitted".to_owned(),
        (1, false) => "list, 1 item".to_owned(),
        (1, true) => "list, 1 item shown, more items omitted".to_owned(),
        (count, false) => format!("list, {count} items"),
        (count, true) => format!("list, {count} items shown, more items omitted"),
    }
}

pub fn value_kind_label(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Nil => "nil",
        ValueKind::Bool => "boolean",
        ValueKind::Number => "number",
        ValueKind::String => "string",
        ValueKind::Function => "function",
        ValueKind::Closure => "closure",
        ValueKind::Native => "native function",
        ValueKind::List => "list",
        ValueKind::Cycle => "cycle",
        ValueKind::Truncated => "truncated",
    }
}

pub fn binding_kind_label(kind: &str) -> Cow<'_, str> {
    match kind {
        "parameter" => Cow::Borrowed("parameter"),
        "local" => Cow::Borrowed("local"),
        "implicit" => Cow::Borrowed("implicit local"),
        "upvalue" => Cow::Borrowed("captured variable"),
        "global" => Cow::Borrowed("global"),
        other => Cow::Owned(escape_display_text(other)),
    }
}

pub fn binding_accessible_name(binding: &BindingSnapshot) -> String {
    let mut name = escape_display_text(&binding.name);
    if binding.name_truncated {
        name.push_str(" (name truncated)");
    }
    format!(
        "{} {}, {}, {}",
        binding_kind_label(&binding.binding_kind),
        name,
        value_kind_label(binding.value_kind),
        debug_value_summary(&binding.value),
    )
}

pub fn snapshot_provenance_label(provenance: SnapshotProvenance) -> &'static str {
    match provenance {
        SnapshotProvenance::Paused => "Current paused snapshot",
        SnapshotProvenance::Faulted => "Snapshot at error",
        SnapshotProvenance::Cancelled => "Snapshot after stop",
        SnapshotProvenance::LastSafePause => "Last safe paused snapshot",
    }
}

pub fn snapshot_accessible_name(
    provenance: SnapshotProvenance,
    frame_count: usize,
    frames_truncated: bool,
    globals_truncated: bool,
) -> String {
    let frame_label = if frame_count == 1 {
        "1 frame".to_owned()
    } else {
        format!("{frame_count} frames")
    };
    let mut label = format!("{}, {frame_label}", snapshot_provenance_label(provenance));
    if frames_truncated {
        label.push_str(" shown, more frames omitted");
    }
    if globals_truncated {
        label.push_str(", some globals omitted");
    }
    label
}

pub fn frame_accessible_name(frame: &FrameSnapshot, ordinal: usize, selected: bool) -> String {
    let mut function = escape_display_text(&frame.function);
    if frame.function_truncated {
        function.push_str(" (name truncated)");
    }
    let mut label = format!(
        "Frame {}, function {}, line {}, column {}",
        ordinal.saturating_add(1),
        function,
        frame.current_span.start.line,
        frame.current_span.start.column,
    );
    if frame.parameters_truncated || frame.locals_truncated || frame.upvalues_truncated {
        label.push_str(", some variables omitted");
    }
    if selected {
        label.push_str(", selected");
    }
    label
}

pub fn escape_display_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => escaped.extend(character.escape_default()),
            character => escaped.push(character),
        }
    }
    escaped
}

fn debug_value_summary(value: &DebugValue) -> String {
    match value {
        DebugValue::Nil => "nil".to_owned(),
        DebugValue::Bool(value) => value.to_string(),
        DebugValue::Number(value) => escape_display_text(value),
        DebugValue::String(value) => format!("\"{}\"", escape_display_text(value)),
        DebugValue::Function(value) => format!("function {}", escape_display_text(value)),
        DebugValue::Closure(value) => format!("closure {}", escape_display_text(value)),
        DebugValue::Native(value) => format!("native function {}", escape_display_text(value)),
        DebugValue::List {
            items, truncated, ..
        } => list_summary(items.len(), *truncated),
        DebugValue::Cycle { .. } => "cycle reference".to_owned(),
        DebugValue::Truncated => "value omitted".to_owned(),
    }
}

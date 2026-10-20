//! Typed LSP result model for Faktor's semantic/index consumers.
//!
//! Every wire value is validated and converted into these types: ranges are
//! checked (`u32` positions, ordered start/end), strings are length-bounded,
//! arrays are capped (see the `MAX_*` constants), and unknown symbol kinds or
//! tags are preserved as `Unknown(n)` instead of being silently dropped. A
//! hostile or broken server can therefore never force an unbounded result or
//! panic the consumer.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Hard cap on normalized definition/reference locations per response.
pub const MAX_LOCATIONS: usize = 4096;
/// Hard cap on workspace symbols per response.
pub const MAX_WORKSPACE_SYMBOLS: usize = 4096;
/// Hard cap on call-hierarchy items per response.
pub const MAX_CALL_HIERARCHY_ITEMS: usize = 1024;
/// Hard cap on a workspace-symbol query (UTF-8 bytes) before it is sent.
pub const MAX_QUERY_BYTES: usize = 1024;
/// Hard cap on a document URI retained from the server.
pub const MAX_URI_BYTES: usize = 8192;
/// Hard cap on a symbol/call name retained from the server.
pub const MAX_NAME_BYTES: usize = 4096;
/// Hard cap on a detail/container string retained from the server.
pub const MAX_DETAIL_BYTES: usize = 16 * 1024;
/// Hard cap on an opaque `data` payload retained from the server.
pub const MAX_DATA_BYTES: usize = 64 * 1024;
/// Hard cap on symbol tags retained for one item.
pub const MAX_SYMBOL_TAGS: usize = 32;

/// A validated LSP position (zero-based line, UTF-16 character offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    pub(crate) fn from_value(value: &Value) -> Result<Self, WireError> {
        let object = value
            .as_object()
            .ok_or_else(|| WireError("position is not an object".into()))?;
        Ok(Self {
            line: u32_field(object, "line")?,
            character: u32_field(object, "character")?,
        })
    }
}

/// A validated LSP range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub(crate) fn from_value(value: &Value) -> Result<Self, WireError> {
        let object = value
            .as_object()
            .ok_or_else(|| WireError("range is not an object".into()))?;
        let start = Position::from_value(
            object
                .get("start")
                .ok_or_else(|| WireError("range.start is missing".into()))?,
        )?;
        let end = Position::from_value(
            object
                .get("end")
                .ok_or_else(|| WireError("range.end is missing".into()))?,
        )?;
        if start > end {
            return Err(WireError("range start is after range end".into()));
        }
        Ok(Self { start, end })
    }
}

/// A normalized definition/reference location.
///
/// Plain `Location` results set `selection_range == range`; `LocationLink`
/// results keep the link's target selection range and origin selection range.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
    pub origin_selection_range: Option<Range>,
}

impl Location {
    pub(crate) fn from_definition(value: &Value) -> Result<Self, WireError> {
        if value.get("targetUri").is_some() {
            let uri = bounded_str(value, "targetUri", MAX_URI_BYTES)?;
            let range = Range::from_value(
                value
                    .get("targetRange")
                    .ok_or_else(|| WireError("location link targetRange is missing".into()))?,
            )?;
            let selection_range =
                Range::from_value(value.get("targetSelectionRange").ok_or_else(|| {
                    WireError("location link targetSelectionRange is missing".into())
                })?)?;
            let origin_selection_range = value
                .get("originSelectionRange")
                .map(Range::from_value)
                .transpose()?;
            Ok(Self {
                uri,
                range,
                selection_range,
                origin_selection_range,
            })
        } else {
            let uri = bounded_str(value, "uri", MAX_URI_BYTES)?;
            let range = Range::from_value(
                value
                    .get("range")
                    .ok_or_else(|| WireError("location range is missing".into()))?,
            )?;
            Ok(Self {
                uri,
                range,
                selection_range: range,
                origin_selection_range: None,
            })
        }
    }

    pub(crate) fn from_reference(value: &Value) -> Result<Self, WireError> {
        let uri = bounded_str(value, "uri", MAX_URI_BYTES)?;
        let range = Range::from_value(
            value
                .get("range")
                .ok_or_else(|| WireError("reference location range is missing".into()))?,
        )?;
        Ok(Self {
            uri,
            range,
            selection_range: range,
            origin_selection_range: None,
        })
    }
}

/// LSP `SymbolKind` with the numeric value preserved for unknown kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
    Unknown(u32),
}

impl SymbolKind {
    pub fn as_u32(self) -> u32 {
        match self {
            Self::File => 1,
            Self::Module => 2,
            Self::Namespace => 3,
            Self::Package => 4,
            Self::Class => 5,
            Self::Method => 6,
            Self::Property => 7,
            Self::Field => 8,
            Self::Constructor => 9,
            Self::Enum => 10,
            Self::Interface => 11,
            Self::Function => 12,
            Self::Variable => 13,
            Self::Constant => 14,
            Self::String => 15,
            Self::Number => 16,
            Self::Boolean => 17,
            Self::Array => 18,
            Self::Object => 19,
            Self::Key => 20,
            Self::Null => 21,
            Self::EnumMember => 22,
            Self::Struct => 23,
            Self::Event => 24,
            Self::Operator => 25,
            Self::TypeParameter => 26,
            Self::Unknown(value) => value,
        }
    }

    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::File,
            2 => Self::Module,
            3 => Self::Namespace,
            4 => Self::Package,
            5 => Self::Class,
            6 => Self::Method,
            7 => Self::Property,
            8 => Self::Field,
            9 => Self::Constructor,
            10 => Self::Enum,
            11 => Self::Interface,
            12 => Self::Function,
            13 => Self::Variable,
            14 => Self::Constant,
            15 => Self::String,
            16 => Self::Number,
            17 => Self::Boolean,
            18 => Self::Array,
            19 => Self::Object,
            20 => Self::Key,
            21 => Self::Null,
            22 => Self::EnumMember,
            23 => Self::Struct,
            24 => Self::Event,
            25 => Self::Operator,
            26 => Self::TypeParameter,
            other => Self::Unknown(other),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Module => "module",
            Self::Namespace => "namespace",
            Self::Package => "package",
            Self::Class => "class",
            Self::Method => "method",
            Self::Property => "property",
            Self::Field => "field",
            Self::Constructor => "constructor",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Function => "function",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
            Self::Key => "key",
            Self::Null => "null",
            Self::EnumMember => "enumMember",
            Self::Struct => "struct",
            Self::Event => "event",
            Self::Operator => "operator",
            Self::TypeParameter => "typeParameter",
            Self::Unknown(_) => "unknown",
        }
    }

    pub(crate) fn from_value(value: &Value) -> Result<Self, WireError> {
        let raw = value
            .get("kind")
            .and_then(Value::as_u64)
            .ok_or_else(|| WireError("symbol kind is missing or not an integer".into()))?;
        let raw = u32::try_from(raw)
            .map_err(|_| WireError("symbol kind exceeds the u32 range".into()))?;
        Ok(Self::from_u32(raw))
    }
}

impl Serialize for SymbolKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.as_u32())
    }
}

impl<'de> Deserialize<'de> for SymbolKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_u32(u32::deserialize(deserializer)?))
    }
}

/// LSP `SymbolTag` with unknown values preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolTag {
    Deprecated,
    Unknown(u32),
}

impl SymbolTag {
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Deprecated => 1,
            Self::Unknown(value) => value,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Deprecated => "deprecated",
            Self::Unknown(_) => "unknown",
        }
    }
}

impl From<u32> for SymbolTag {
    fn from(value: u32) -> Self {
        match value {
            1 => Self::Deprecated,
            other => Self::Unknown(other),
        }
    }
}

impl Serialize for SymbolTag {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.as_u32())
    }
}

impl<'de> Deserialize<'de> for SymbolTag {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match u32::deserialize(deserializer)? {
            1 => Self::Deprecated,
            other => Self::Unknown(other),
        })
    }
}

/// A workspace symbol (`workspace/symbol`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub tags: Vec<SymbolTag>,
    pub container_name: Option<String>,
    pub location: WorkspaceSymbolLocation,
    /// Opaque server payload, bounded by [`MAX_DATA_BYTES`].
    pub data: Option<Value>,
}

/// A workspace symbol location; `range` is absent for URI-only results.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceSymbolLocation {
    pub uri: String,
    pub range: Option<Range>,
}

impl WorkspaceSymbol {
    pub(crate) fn from_value(value: &Value) -> Result<Self, WireError> {
        let name = bounded_str(value, "name", MAX_NAME_BYTES)?;
        let kind = SymbolKind::from_value(value)?;
        let deprecated = value.get("deprecated").and_then(Value::as_bool) == Some(true);
        let mut tags = tags_from(value)?;
        if deprecated && !tags.contains(&SymbolTag::Deprecated) {
            tags.push(SymbolTag::Deprecated);
        }
        let container_name = optional_bounded_str(value, "containerName", MAX_DETAIL_BYTES)?;
        let location = value
            .get("location")
            .ok_or_else(|| WireError("workspace symbol location is missing".into()))?;
        let uri = bounded_str(location, "uri", MAX_URI_BYTES)?;
        let range = location.get("range").map(Range::from_value).transpose()?;
        Ok(Self {
            name,
            kind,
            tags,
            container_name,
            location: WorkspaceSymbolLocation { uri, range },
            data: bounded_data(value)?,
        })
    }
}

/// A call-hierarchy item (`prepareCallHierarchy`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub tags: Vec<SymbolTag>,
    pub detail: Option<String>,
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
    /// Opaque server payload that must be passed back on incoming/outgoing
    /// calls; bounded by [`MAX_DATA_BYTES`].
    pub data: Option<Value>,
}

impl CallHierarchyItem {
    pub(crate) fn from_value(value: &Value) -> Result<Self, WireError> {
        let name = bounded_str(value, "name", MAX_NAME_BYTES)?;
        let kind = SymbolKind::from_value(value)?;
        let tags = tags_from(value)?;
        let detail = optional_bounded_str(value, "detail", MAX_DETAIL_BYTES)?;
        let uri = bounded_str(value, "uri", MAX_URI_BYTES)?;
        let range = Range::from_value(
            value
                .get("range")
                .ok_or_else(|| WireError("call hierarchy range is missing".into()))?,
        )?;
        let selection_range = Range::from_value(
            value
                .get("selectionRange")
                .ok_or_else(|| WireError("call hierarchy selectionRange is missing".into()))?,
        )?;
        Ok(Self {
            name,
            kind,
            tags,
            detail,
            uri,
            range,
            selection_range,
            data: bounded_data(value)?,
        })
    }

    /// Recreate the exact wire shape accepted by `callHierarchy/incomingCalls`
    /// and `callHierarchy/outgoingCalls` (including the opaque `data` field).
    pub fn to_wire(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("name".into(), Value::String(self.name.clone()));
        object.insert("kind".into(), Value::from(self.kind.as_u32()));
        if !self.tags.is_empty() {
            object.insert(
                "tags".into(),
                Value::Array(
                    self.tags
                        .iter()
                        .map(|tag| Value::from(tag.as_u32()))
                        .collect(),
                ),
            );
        }
        if let Some(detail) = &self.detail {
            object.insert("detail".into(), Value::String(detail.clone()));
        }
        object.insert("uri".into(), Value::String(self.uri.clone()));
        object.insert("range".into(), range_to_wire(self.range));
        object.insert("selectionRange".into(), range_to_wire(self.selection_range));
        if let Some(data) = &self.data {
            object.insert("data".into(), data.clone());
        }
        Value::Object(object)
    }
}

fn range_to_wire(range: Range) -> Value {
    serde_json::json!({
        "start": { "line": range.start.line, "character": range.start.character },
        "end": { "line": range.end.line, "character": range.end.character },
    })
}

/// One incoming call (`callHierarchy/incomingCalls`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallHierarchyIncomingCall {
    pub from: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

impl CallHierarchyIncomingCall {
    pub(crate) fn from_value(value: &Value) -> Result<Self, WireError> {
        let from = CallHierarchyItem::from_value(
            value
                .get("from")
                .ok_or_else(|| WireError("incoming call `from` item is missing".into()))?,
        )?;
        let from_ranges = ranges_from(value, "fromRanges")?;
        Ok(Self { from, from_ranges })
    }
}

/// One outgoing call (`callHierarchy/outgoingCalls`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallHierarchyOutgoingCall {
    pub to: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

impl CallHierarchyOutgoingCall {
    pub(crate) fn from_value(value: &Value) -> Result<Self, WireError> {
        let to = CallHierarchyItem::from_value(
            value
                .get("to")
                .ok_or_else(|| WireError("outgoing call `to` item is missing".into()))?,
        )?;
        let from_ranges = ranges_from(value, "fromRanges")?;
        Ok(Self { to, from_ranges })
    }
}

/// A bounded result: at most the method's cap is retained, and `truncated`
/// says whether the server returned more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bounded<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}

impl<T> Bounded<T> {
    pub fn new(items: Vec<T>, truncated: bool) -> Self {
        Self { items, truncated }
    }

    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            truncated: false,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn into_items(self) -> Vec<T> {
        self.items
    }
}

/// The tracked document revision used as the semantic cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentRevision {
    pub version: i64,
    pub content_hash: [u8; 32],
}

impl DocumentRevision {
    pub(crate) fn of(version: i64, text: &str) -> Self {
        Self {
            version,
            content_hash: *blake3::hash(text.as_bytes()).as_bytes(),
        }
    }

    pub fn hash_hex(&self) -> String {
        self.content_hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// Validation failure while converting one wire value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn u32_field(object: &serde_json::Map<String, Value>, field: &str) -> Result<u32, WireError> {
    let value = object.get(field).and_then(Value::as_u64).ok_or_else(|| {
        WireError(format!(
            "`{field}` is missing or not a non-negative integer"
        ))
    })?;
    u32::try_from(value).map_err(|_| WireError(format!("`{field}` exceeds the u32 range")))
}

fn bounded_str(value: &Value, field: &str, max: usize) -> Result<String, WireError> {
    let text = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| WireError(format!("`{field}` is missing or not a string")))?;
    if text.len() > max {
        return Err(WireError(format!("`{field}` exceeds the {max}-byte bound")));
    }
    Ok(text.to_string())
}

fn optional_bounded_str(
    value: &Value,
    field: &str,
    max: usize,
) -> Result<Option<String>, WireError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= max => Ok(Some(text.clone())),
        Some(Value::String(_)) => Err(WireError(format!("`{field}` exceeds the {max}-byte bound"))),
        Some(_) => Err(WireError(format!("`{field}` is not a string"))),
    }
}

fn tags_from(value: &Value) -> Result<Vec<SymbolTag>, WireError> {
    match value.get("tags") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut tags = Vec::with_capacity(items.len().min(MAX_SYMBOL_TAGS));
            for item in items.iter().take(MAX_SYMBOL_TAGS) {
                let raw = item
                    .as_u64()
                    .ok_or_else(|| WireError("symbol tag is not an integer".into()))?;
                let raw = u32::try_from(raw)
                    .map_err(|_| WireError("symbol tag exceeds the u32 range".into()))?;
                tags.push(match raw {
                    1 => SymbolTag::Deprecated,
                    other => SymbolTag::Unknown(other),
                });
            }
            Ok(tags)
        }
        Some(_) => Err(WireError("`tags` is not an array".into())),
    }
}

fn bounded_data(value: &Value) -> Result<Option<Value>, WireError> {
    match value.get("data") {
        None | Some(Value::Null) => Ok(None),
        Some(data) => {
            let encoded_len = serde_json::to_vec(data)
                .map_err(|_| WireError("`data` is not serializable".into()))?
                .len();
            if encoded_len > MAX_DATA_BYTES {
                return Err(WireError(format!(
                    "`data` exceeds the {MAX_DATA_BYTES}-byte bound"
                )));
            }
            Ok(Some(data.clone()))
        }
    }
}

fn ranges_from(value: &Value, field: &str) -> Result<Vec<Range>, WireError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut ranges = Vec::with_capacity(items.len().min(MAX_LOCATIONS));
            for item in items.iter().take(MAX_LOCATIONS) {
                ranges.push(Range::from_value(item)?);
            }
            Ok(ranges)
        }
        Some(_) => Err(WireError(format!("`{field}` is not an array"))),
    }
}

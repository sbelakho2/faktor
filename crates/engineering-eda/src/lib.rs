//! Engineering EDA integration surfaces.
//!
//! This crate implements a bounded reader for KiCad PCB s-expression files
//! and a decoder for `kicad-cli pcb drc --format json` reports. It does not
//! invoke tools; the supervised adapters live in `engineering-solvers`.
//!
//! The parser is deliberately strict about depth, node count, token size,
//! non-finite numbers and duplicate net codes. It does not claim to model
//! every KiCad feature: unknown board elements are ignored, while the
//! extracted footprints, pads, nets, tracks and vias are typed and bounded.

use std::collections::BTreeMap;

use engineering_units::{Quantity, UnitRegistry};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_SEXPR_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SEXPR_DEPTH: usize = 256;
pub const MAX_SEXPR_NODES: usize = 2_000_000;
pub const MAX_TOKEN_BYTES: usize = 64 * 1024;
pub const MAX_FOOTPRINTS: usize = 100_000;
pub const MAX_PADS: usize = 1_000_000;
pub const MAX_SEGMENTS: usize = 1_000_000;
pub const MAX_DRC_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DRC_VIOLATIONS: usize = 100_000;

#[derive(Debug, Error)]
pub enum EdaError {
    #[error("input exceeds the {max}-byte bound (got {len})")]
    TooLarge { max: usize, len: usize },
    #[error("input is not valid UTF-8")]
    Utf8,
    #[error("s-expression syntax error at byte {offset}: {message}")]
    Syntax { offset: usize, message: String },
    #[error("s-expression nesting exceeds {max}")]
    DepthExceeded { max: usize },
    #[error("s-expression node count exceeds {max}")]
    NodeLimit { max: usize },
    #[error("token exceeds {max} bytes")]
    TokenTooLong { max: usize },
    #[error("expected a {expected} at {context}")]
    Missing {
        expected: &'static str,
        context: &'static str,
    },
    #[error("invalid number {value:?} in {context}")]
    InvalidNumber { value: String, context: String },
    #[error("duplicate net code {code}")]
    DuplicateNet { code: u32 },
    #[error("duplicate net name {name:?}")]
    DuplicateNetName { name: String },
    #[error("board count {what} exceeds {max}")]
    BoardLimit { what: &'static str, max: usize },
    #[error("DRC report is invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("DRC report exceeds the violation bound {max}")]
    DrcLimit { max: usize },
    #[error("invalid value: {0}")]
    InvalidValue(String),
    #[error("unit error: {0}")]
    Units(#[from] engineering_units::UnitError),
}

pub type EdaResult<T> = Result<T, EdaError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sexp {
    Atom(String),
    Quoted(String),
    List(Vec<Sexp>),
}

impl Sexp {
    pub fn head(&self) -> Option<&str> {
        match self {
            Self::List(items) => match items.first() {
                Some(Self::Atom(head)) | Some(Self::Quoted(head)) => Some(head.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn items(&self) -> &[Sexp] {
        match self {
            Self::List(items) => items,
            _ => &[],
        }
    }

    pub fn atom(&self, index: usize) -> Option<&str> {
        match self.items().get(index) {
            Some(Self::Atom(value)) | Some(Self::Quoted(value)) => Some(value.as_str()),
            _ => None,
        }
    }

    pub fn child(&self, head: &str) -> Option<&Sexp> {
        self.items().iter().find(|item| item.head() == Some(head))
    }

    pub fn children(&self, head: &str) -> impl Iterator<Item = &Sexp> {
        let owned = head.to_string();
        self.items()
            .iter()
            .filter(move |item| item.head() == Some(owned.as_str()))
    }

    fn numbers(&self, start: usize) -> Vec<&str> {
        self.items()
            .iter()
            .skip(start)
            .filter_map(|item| match item {
                Self::Atom(value) => Some(value.as_str()),
                _ => None,
            })
            .collect()
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
    nodes: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            nodes: 0,
        }
    }

    fn count_node(&mut self) -> EdaResult<()> {
        self.nodes += 1;
        if self.nodes > MAX_SEXPR_NODES {
            return Err(EdaError::NodeLimit {
                max: MAX_SEXPR_NODES,
            });
        }
        Ok(())
    }

    fn parse(mut self) -> EdaResult<Sexp> {
        let mut stack: Vec<Vec<Sexp>> = Vec::new();
        let mut root: Option<Sexp> = None;
        while self.position < self.bytes.len() {
            let byte = self.bytes[self.position];
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' => {
                    self.position += 1;
                }
                b'(' => {
                    self.position += 1;
                    if stack.len() >= MAX_SEXPR_DEPTH {
                        return Err(EdaError::DepthExceeded {
                            max: MAX_SEXPR_DEPTH,
                        });
                    }
                    self.count_node()?;
                    stack.push(Vec::new());
                }
                b')' => {
                    let start = self.position;
                    self.position += 1;
                    let completed = stack.pop().ok_or_else(|| EdaError::Syntax {
                        offset: start,
                        message: "unmatched closing parenthesis".to_string(),
                    })?;
                    let list = Sexp::List(completed);
                    if let Some(parent) = stack.last_mut() {
                        parent.push(list);
                    } else if root.is_some() {
                        return Err(EdaError::Syntax {
                            offset: start,
                            message: "multiple root expressions".to_string(),
                        });
                    } else {
                        root = Some(list);
                    }
                }
                b'"' => {
                    let start = self.position;
                    let value = self.parse_quoted()?;
                    self.count_node()?;
                    let atom = Sexp::Quoted(value);
                    if let Some(parent) = stack.last_mut() {
                        parent.push(atom);
                    } else if root.is_some() {
                        return Err(EdaError::Syntax {
                            offset: start,
                            message: "multiple root expressions".to_string(),
                        });
                    } else {
                        root = Some(atom);
                    }
                }
                _ => {
                    let start = self.position;
                    let atom = self.parse_atom()?;
                    self.count_node()?;
                    let atom = Sexp::Atom(atom);
                    if let Some(parent) = stack.last_mut() {
                        parent.push(atom);
                    } else if root.is_some() {
                        return Err(EdaError::Syntax {
                            offset: start,
                            message: "multiple root expressions".to_string(),
                        });
                    } else {
                        root = Some(atom);
                    }
                }
            }
        }
        if let Some(open) = stack.pop() {
            let _ = open;
            return Err(EdaError::Syntax {
                offset: self.position,
                message: "unclosed parenthesis".to_string(),
            });
        }
        root.ok_or_else(|| EdaError::Syntax {
            offset: 0,
            message: "empty input".to_string(),
        })
    }

    fn parse_quoted(&mut self) -> EdaResult<String> {
        let start = self.position;
        self.position += 1;
        let mut out = Vec::new();
        while self.position < self.bytes.len() {
            let byte = self.bytes[self.position];
            self.position += 1;
            match byte {
                b'"' => {
                    return String::from_utf8(out).map_err(|_| EdaError::Utf8);
                }
                b'\\' => {
                    let Some(escaped) = self.bytes.get(self.position).copied() else {
                        return Err(EdaError::Syntax {
                            offset: self.position,
                            message: "trailing escape in quoted string".to_string(),
                        });
                    };
                    self.position += 1;
                    match escaped {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
            if out.len() > MAX_TOKEN_BYTES {
                return Err(EdaError::TokenTooLong {
                    max: MAX_TOKEN_BYTES,
                });
            }
        }
        Err(EdaError::Syntax {
            offset: start,
            message: "unterminated quoted string".to_string(),
        })
    }

    fn parse_atom(&mut self) -> EdaResult<String> {
        let start = self.position;
        while self.position < self.bytes.len() {
            let byte = self.bytes[self.position];
            if byte.is_ascii_whitespace() || byte == b'(' || byte == b')' || byte == b'"' {
                break;
            }
            self.position += 1;
            if self.position - start > MAX_TOKEN_BYTES {
                return Err(EdaError::TokenTooLong {
                    max: MAX_TOKEN_BYTES,
                });
            }
        }
        let bytes = &self.bytes[start..self.position];
        String::from_utf8(bytes.to_vec()).map_err(|_| EdaError::Utf8)
    }
}

pub fn parse_sexpr(input: &str) -> EdaResult<Sexp> {
    Parser::new(input.as_bytes()).parse()
}

fn number(value: Option<&str>, context: &str) -> EdaResult<f64> {
    let raw = value.ok_or(EdaError::Missing {
        expected: "number",
        context: "board element",
    })?;
    let parsed = raw.parse::<f64>().map_err(|_| EdaError::InvalidNumber {
        value: raw.to_string(),
        context: context.to_string(),
    })?;
    if !parsed.is_finite() {
        return Err(EdaError::InvalidNumber {
            value: raw.to_string(),
            context: context.to_string(),
        });
    }
    Ok(parsed)
}

fn point_from(node: &Sexp, context: &str) -> EdaResult<Option<PointMm>> {
    let values = node.numbers(1);
    if values.is_empty() {
        return Ok(None);
    }
    let x = number(values.first().copied(), context)?;
    let y = number(values.get(1).copied(), context)?;
    Ok(Some(PointMm { x, y }))
}

fn point(node: &Sexp, context: &str) -> EdaResult<Option<PointMm>> {
    match node.child("at") {
        Some(at) => point_from(at, context),
        None => Ok(None),
    }
}

fn first_numeric(node: &Sexp) -> EdaResult<Option<f64>> {
    for value in node.numbers(1) {
        if let Ok(parsed) = value.parse::<f64>() {
            if !parsed.is_finite() {
                return Err(EdaError::InvalidNumber {
                    value: value.to_string(),
                    context: "numeric element".to_string(),
                });
            }
            return Ok(Some(parsed));
        }
    }
    Ok(None)
}

fn rotation(node: &Sexp) -> EdaResult<Option<f64>> {
    let Some(at) = node.child("at") else {
        return Ok(None);
    };
    let values = at.numbers(1);
    match values.get(2) {
        Some(value) => Ok(Some(number(Some(value), "rotation")?)),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointMm {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardLayer {
    pub ordinal: i32,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Net {
    pub code: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pad {
    pub number: String,
    pub kind: String,
    pub shape: String,
    pub at: Option<PointMm>,
    pub size: Option<PointMm>,
    pub drill_mm: Option<f64>,
    pub layers: Vec<String>,
    pub net_code: Option<u32>,
    pub net_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    pub library: String,
    pub reference: Option<String>,
    pub layer: Option<String>,
    pub at: Option<PointMm>,
    pub rotation_deg: Option<f64>,
    pub pads: Vec<Pad>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: Option<PointMm>,
    pub end: Option<PointMm>,
    pub width_mm: Option<f64>,
    pub layer: Option<String>,
    pub net_code: Option<u32>,
    pub net_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Via {
    pub at: Option<PointMm>,
    pub size_mm: Option<f64>,
    pub drill_mm: Option<f64>,
    pub layers: Vec<String>,
    pub net_code: Option<u32>,
    pub net_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PcbBoard {
    pub version: Option<u32>,
    pub generator: Option<String>,
    pub thickness: Option<Quantity>,
    pub layers: Vec<BoardLayer>,
    pub copper_layers: Vec<String>,
    pub nets: Vec<Net>,
    pub footprints: Vec<Footprint>,
    pub segments: Vec<Segment>,
    pub vias: Vec<Via>,
    pub edge_cuts_shapes: usize,
}

impl PcbBoard {
    pub fn pad_count(&self) -> usize {
        self.footprints
            .iter()
            .map(|footprint| footprint.pads.len())
            .sum()
    }

    pub fn reference(&self, reference: &str) -> Option<&Footprint> {
        self.footprints
            .iter()
            .find(|footprint| footprint.reference.as_deref() == Some(reference))
    }
}

fn parse_layers(board: &Sexp) -> EdaResult<Vec<BoardLayer>> {
    let Some(layers) = board.child("layers") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for layer in layers.items() {
        let Some(ordinal) = layer.atom(0) else {
            continue;
        };
        let Ok(ordinal) = ordinal.parse::<i32>() else {
            continue;
        };
        let Some(name) = layer.atom(1) else {
            continue;
        };
        let kind = layer.atom(2).unwrap_or("unknown").to_string();
        out.push(BoardLayer {
            ordinal,
            name: name.to_string(),
            kind,
        });
    }
    Ok(out)
}

fn parse_nets(board: &Sexp) -> EdaResult<Vec<Net>> {
    let mut nets: BTreeMap<u32, String> = BTreeMap::new();
    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    for net in board.children("net") {
        let Some(code) = net.atom(1) else {
            continue;
        };
        let code = code.parse::<u32>().map_err(|_| EdaError::InvalidNumber {
            value: code.to_string(),
            context: "net code".to_string(),
        })?;
        let name = net.atom(2).unwrap_or("").to_string();
        if let Some(existing) = nets.insert(code, name.clone()) {
            if existing != name {
                return Err(EdaError::DuplicateNet { code });
            }
        }
        if !name.is_empty() {
            if let Some(existing) = names.insert(name.clone(), code) {
                if existing != code {
                    return Err(EdaError::DuplicateNetName { name });
                }
            }
        }
    }
    Ok(nets
        .into_iter()
        .map(|(code, name)| Net { code, name })
        .collect())
}

fn parse_pad(pad: &Sexp, nets: &BTreeMap<u32, String>) -> EdaResult<Pad> {
    let pad_number = pad.atom(1).unwrap_or("").to_string();
    let kind = pad.atom(2).unwrap_or("unknown").to_string();
    let shape = pad.atom(3).unwrap_or("unknown").to_string();
    let size = if let Some(size) = pad.child("size") {
        let values = size.numbers(1);
        Some(PointMm {
            x: number(values.first().copied(), "pad size")?,
            y: number(values.get(1).copied(), "pad size")?,
        })
    } else {
        None
    };
    let drill_mm = pad.child("drill").map(first_numeric).transpose()?.flatten();
    let layers = pad
        .child("layers")
        .map(|layers| {
            layers
                .items()
                .iter()
                .filter_map(|layer| match layer {
                    Sexp::Quoted(name) | Sexp::Atom(name) => Some(name.clone()),
                    Sexp::List(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let (net_code, net_name) = if let Some(net) = pad.child("net") {
        let code = net.atom(1).and_then(|value| value.parse::<u32>().ok());
        let name = code
            .and_then(|code| nets.get(&code).cloned())
            .or_else(|| net.atom(2).map(str::to_string));
        (code, name)
    } else {
        (None, None)
    };
    Ok(Pad {
        number: pad_number,
        kind,
        shape,
        at: point(pad, "pad position")?,
        size,
        drill_mm,
        layers,
        net_code,
        net_name,
    })
}

fn reference_of(footprint: &Sexp) -> Option<String> {
    for property in footprint.children("property") {
        if property.atom(1) == Some("Reference") {
            return property.atom(2).map(str::to_string);
        }
    }
    for text in footprint.children("fp_text") {
        if text.atom(1) == Some("reference") {
            return text.atom(2).map(str::to_string);
        }
    }
    None
}

fn parse_footprint(footprint: &Sexp, nets: &BTreeMap<u32, String>) -> EdaResult<Footprint> {
    let library = footprint.atom(1).unwrap_or("").to_string();
    let layer = footprint
        .child("layer")
        .and_then(|layer| layer.atom(1))
        .map(str::to_string);
    let mut pads = Vec::new();
    for pad in footprint.children("pad") {
        pads.push(parse_pad(pad, nets)?);
        if pads.len() > MAX_PADS {
            return Err(EdaError::BoardLimit {
                what: "pads",
                max: MAX_PADS,
            });
        }
    }
    Ok(Footprint {
        library,
        reference: reference_of(footprint),
        layer,
        at: point(footprint, "footprint position")?,
        rotation_deg: rotation(footprint)?,
        pads,
    })
}

fn net_pair(node: &Sexp, nets: &BTreeMap<u32, String>) -> (Option<u32>, Option<String>) {
    if let Some(net) = node.child("net") {
        let code = net.atom(1).and_then(|value| value.parse::<u32>().ok());
        let name = code
            .and_then(|code| nets.get(&code).cloned())
            .or_else(|| net.atom(2).map(str::to_string));
        (code, name)
    } else {
        (None, None)
    }
}

fn parse_segment(segment: &Sexp, nets: &BTreeMap<u32, String>) -> EdaResult<Segment> {
    let width_mm = segment
        .child("width")
        .map(first_numeric)
        .transpose()?
        .flatten();
    let (net_code, net_name) = net_pair(segment, nets);
    Ok(Segment {
        start: segment
            .child("start")
            .map(|start| point_from(start, "track start"))
            .transpose()?
            .flatten(),
        end: segment
            .child("end")
            .map(|end| point_from(end, "track end"))
            .transpose()?
            .flatten(),
        width_mm,
        layer: segment
            .child("layer")
            .and_then(|layer| layer.atom(1))
            .map(str::to_string),
        net_code,
        net_name,
    })
}

fn parse_via(via: &Sexp, nets: &BTreeMap<u32, String>) -> EdaResult<Via> {
    let size_mm = via.child("size").map(first_numeric).transpose()?.flatten();
    let drill_mm = via.child("drill").map(first_numeric).transpose()?.flatten();
    let layers = via
        .child("layers")
        .map(|layers| {
            layers
                .items()
                .iter()
                .filter_map(|layer| match layer {
                    Sexp::Quoted(name) | Sexp::Atom(name) => Some(name.clone()),
                    Sexp::List(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let (net_code, net_name) = net_pair(via, nets);
    Ok(Via {
        at: point(via, "via position")?,
        size_mm,
        drill_mm,
        layers,
        net_code,
        net_name,
    })
}

/// Alias with an explicit format name for callers that prefer it.
pub fn parse_kicad_pcb(bytes: &[u8]) -> EdaResult<PcbBoard> {
    parse_board(bytes)
}

pub fn parse_board(bytes: &[u8]) -> EdaResult<PcbBoard> {
    parse_board_limited(bytes, MAX_SEXPR_BYTES)
}

pub fn parse_board_limited(bytes: &[u8], max_bytes: usize) -> EdaResult<PcbBoard> {
    if bytes.len() > max_bytes {
        return Err(EdaError::TooLarge {
            max: max_bytes,
            len: bytes.len(),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| EdaError::Utf8)?;
    let root = parse_sexpr(text)?;
    if root.head() != Some("kicad_pcb") {
        return Err(EdaError::Missing {
            expected: "kicad_pcb root",
            context: "board",
        });
    }
    let version = root
        .child("version")
        .and_then(|version| version.atom(1))
        .and_then(|value| value.parse::<u32>().ok());
    let generator = root
        .child("generator")
        .and_then(|generator| generator.atom(1))
        .map(str::to_string);
    let thickness = root
        .child("general")
        .and_then(|general| general.child("thickness"))
        .and_then(|thickness| thickness.atom(1))
        .map(|value| number(Some(value), "board thickness"))
        .transpose()?
        .map(|value| -> EdaResult<Quantity> {
            Ok(Quantity::new(
                value,
                UnitRegistry::si().require("mm").unwrap(),
            )?)
        })
        .transpose()?;

    let layers = parse_layers(&root)?;
    let copper_layers = layers
        .iter()
        .filter(|layer| matches!(layer.kind.as_str(), "signal" | "power" | "mixed" | "jumper"))
        .map(|layer| layer.name.clone())
        .collect();
    let nets = parse_nets(&root)?;
    let net_map: BTreeMap<u32, String> = nets
        .iter()
        .map(|net| (net.code, net.name.clone()))
        .collect();

    let mut footprints = Vec::new();
    let mut total_pads = 0usize;
    for footprint in root
        .items()
        .iter()
        .filter(|item| matches!(item.head(), Some("footprint") | Some("module")))
    {
        let footprint = parse_footprint(footprint, &net_map)?;
        total_pads = total_pads.saturating_add(footprint.pads.len());
        if total_pads > MAX_PADS {
            return Err(EdaError::BoardLimit {
                what: "pads",
                max: MAX_PADS,
            });
        }
        footprints.push(footprint);
        if footprints.len() > MAX_FOOTPRINTS {
            return Err(EdaError::BoardLimit {
                what: "footprints",
                max: MAX_FOOTPRINTS,
            });
        }
    }

    let mut segments = Vec::new();
    for segment in root.children("segment") {
        segments.push(parse_segment(segment, &net_map)?);
        if segments.len() > MAX_SEGMENTS {
            return Err(EdaError::BoardLimit {
                what: "segments",
                max: MAX_SEGMENTS,
            });
        }
    }
    let mut vias = Vec::new();
    for via in root.children("via") {
        vias.push(parse_via(via, &net_map)?);
        if vias.len() > MAX_SEGMENTS {
            return Err(EdaError::BoardLimit {
                what: "vias",
                max: MAX_SEGMENTS,
            });
        }
    }
    let edge_cuts_shapes = root
        .items()
        .iter()
        .filter(|item| {
            item.head().is_some_and(|head| head.starts_with("gr_"))
                && item.child("layer").and_then(|layer| layer.atom(1)) == Some("Edge.Cuts")
        })
        .count();

    Ok(PcbBoard {
        version,
        generator,
        thickness,
        layers,
        copper_layers,
        nets,
        footprints,
        segments,
        vias,
        edge_cuts_shapes,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrcSeverity {
    Error,
    Warning,
    Exclusion,
    #[serde(other)]
    Unknown,
}

fn default_severity() -> DrcSeverity {
    DrcSeverity::Unknown
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrcItem {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub pos: Option<PointMm>,
    #[serde(default)]
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrcViolation {
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default = "default_severity")]
    pub severity: DrcSeverity,
    #[serde(default)]
    pub items: Vec<DrcItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrcReport {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub coordinate_units: Option<String>,
    #[serde(default)]
    pub kicad_version: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub violations: Vec<DrcViolation>,
    #[serde(default)]
    pub unconnected_items: Vec<DrcViolation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrcCounts {
    pub errors: usize,
    pub warnings: usize,
    pub exclusions: usize,
    pub unknown: usize,
    pub unconnected: usize,
}

impl DrcReport {
    pub fn counts(&self) -> DrcCounts {
        let mut counts = DrcCounts {
            errors: 0,
            warnings: 0,
            exclusions: 0,
            unknown: 0,
            unconnected: self.unconnected_items.len(),
        };
        for violation in &self.violations {
            match violation.severity {
                DrcSeverity::Error => counts.errors += 1,
                DrcSeverity::Warning => counts.warnings += 1,
                DrcSeverity::Exclusion => counts.exclusions += 1,
                DrcSeverity::Unknown => counts.unknown += 1,
            }
        }
        counts
    }

    pub fn has_errors(&self) -> bool {
        self.counts().errors > 0 || self.counts().unconnected > 0
    }
}

pub fn parse_drc_report(bytes: &[u8]) -> EdaResult<DrcReport> {
    parse_drc_json(bytes)
}

pub fn parse_drc_json(bytes: &[u8]) -> EdaResult<DrcReport> {
    if bytes.len() > MAX_DRC_BYTES {
        return Err(EdaError::TooLarge {
            max: MAX_DRC_BYTES,
            len: bytes.len(),
        });
    }
    let report: DrcReport = serde_json::from_slice(bytes)?;
    if report.violations.len() > MAX_DRC_VIOLATIONS
        || report.unconnected_items.len() > MAX_DRC_VIOLATIONS
    {
        return Err(EdaError::DrcLimit {
            max: MAX_DRC_VIOLATIONS,
        });
    }
    for violation in report
        .violations
        .iter()
        .chain(report.unconnected_items.iter())
    {
        if violation.description.len() > 4_096
            || violation
                .items
                .iter()
                .any(|item| item.description.len() > 4_096)
        {
            return Err(EdaError::InvalidValue(
                "DRC description exceeds 4096 bytes".to_string(),
            ));
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = r#"(kicad_pcb
      (version 20241229)
      (generator "pcbnew")
      (general (thickness 1.6))
      (layers
        (0 "F.Cu" signal)
        (2 "B.Cu" signal)
        (25 "Edge.Cuts" user))
      (net 0 "")
      (net 1 "GND")
      (footprint "Resistor_SMD:R_0805"
        (layer "F.Cu")
        (at 110 105 90)
        (property "Reference" "R1")
        (pad "1" smd roundrect (at -0.9 0) (size 1.0 1.4) (layers "F.Cu" "F.Paste" "F.Mask") (net 1 "GND"))
        (pad "2" smd roundrect (at 0.9 0) (size 1.0 1.4) (layers "F.Cu" "F.Paste" "F.Mask")))
      (segment (start 100 100) (end 110 105) (width 0.25) (layer "F.Cu") (net 1))
      (via (at 106 104) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))
      (gr_rect (start 100 100) (end 120 110) (stroke (width 0.1) (type default)) (fill none) (layer "Edge.Cuts"))
      (zone (net 1) (layer "F.Cu") (polygon (pts (xy 0 0) (xy 1 1))))
    )"#;

    #[test]
    fn parses_a_real_kicad_board_shape() {
        let board = parse_board(BOARD.as_bytes()).unwrap();
        assert_eq!(board.version, Some(20241229));
        assert_eq!(board.copper_layers, vec!["F.Cu", "B.Cu"]);
        assert_eq!(board.nets.len(), 2);
        assert_eq!(board.footprints.len(), 1);
        assert_eq!(board.pad_count(), 2);
        assert_eq!(
            board.reference("R1").unwrap().pads[0].net_name.as_deref(),
            Some("GND")
        );
        assert_eq!(board.segments.len(), 1);
        assert_eq!(board.vias.len(), 1);
        assert_eq!(board.edge_cuts_shapes, 1);
        assert!((board.thickness.unwrap().value() - 1.6).abs() < 1e-12);
    }

    #[test]
    fn malformed_and_hostile_sexprs_are_refused() {
        assert!(parse_board(b"(kicad_pcb (version 1)").is_err());
        assert!(parse_board(b"(kicad_pcb \"unterminated)").is_err());
        assert!(parse_board(b"(kicad_pcb) trailing").is_err());
        assert!(parse_board_limited(b"    ", 4).is_err());
        assert!(matches!(
            parse_board_limited(b"(kicad_pcb)", 3),
            Err(EdaError::TooLarge { .. })
        ));
        let deep = format!("{} x {}", "(".repeat(300), ")".repeat(300));
        assert!(matches!(
            parse_sexpr(&deep),
            Err(EdaError::DepthExceeded { .. })
        ));
    }

    #[test]
    fn duplicate_net_codes_and_non_finite_numbers_are_refused() {
        let board = r#"(kicad_pcb (net 1 "A") (net 1 "B"))"#;
        assert!(matches!(
            parse_board(board.as_bytes()),
            Err(EdaError::DuplicateNet { .. })
        ));
        let board = r#"(kicad_pcb (general (thickness nan)))"#;
        assert!(matches!(
            parse_board(board.as_bytes()),
            Err(EdaError::InvalidNumber { .. })
        ));
    }

    #[test]
    fn drc_json_is_decoded_with_typed_severity() {
        let json = br#"{
          "$schema": "https://schemas.kicad.org/drc.v1.json",
          "coordinate_units": "mm",
          "kicad_version": "10.0.6",
          "violations": [
            {"type":"clearance","severity":"error","description":"clearance","items":[]},
            {"type":"text_height","severity":"warning","description":"text","items":[]},
            {"type":"new_rule","severity":"future","description":"future","items":[]}
          ],
          "unconnected_items": []
        }"#;
        let report = parse_drc_json(json).unwrap();
        let counts = report.counts();
        assert_eq!(counts.errors, 1);
        assert_eq!(counts.warnings, 1);
        assert_eq!(counts.unknown, 1);
        assert!(report.has_errors());
    }

    #[test]
    fn drc_json_with_an_unbounded_shape_is_refused() {
        assert!(parse_drc_json(br#"{"violations": "not an array"}"#).is_err());
        let oversized = vec![b' '; MAX_DRC_BYTES + 1];
        assert!(matches!(
            parse_drc_json(&oversized),
            Err(EdaError::TooLarge { .. })
        ));
    }
}

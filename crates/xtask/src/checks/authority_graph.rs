/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Type Authority Graph (#139), ported from `scripts/authority-graph.py`
//! into story #322's gate program: same annotation grammar, same drift
//! classes, same ratchet/artifact-freshness verdict the justfile's
//! `authority` recipe checked (self-test, then ratchet mode with
//! `--check`). Detects PROGRAM-architecture drift: what authority types
//! exist, what invariants they own, what conversions are legal, and what
//! meaning-bearing primitives live outside the graph.
//!
//! Annotation grammar (a contiguous `///` or `//!` doc block):
//!
//!   /// @authority <Name>                      required, unique
//!   /// @layer <value|runtime-catalog|storage|engines|query|record>  required
//!   /// @owns <one-line invariant>              required
//!   /// @constructs <legal constructors, ' | ' separated>
//!   /// @forbids <illegal constructors/escapes, ' | ' separated>
//!   /// @converts <A -> B (context) | ...>      edges of the graph
//!   /// @gate <proof gate>
//!   /// @status <established #NNN | pending #NNN — note>

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

pub const TOOL_VERSION: u32 = 1;

/// layer -> path prefixes (relative to the scan root) that may own it.
const LAYERS: &[(&str, &[&str])] = &[
    ("value", &["crates/kyzo-core/src/data/"]),
    ("runtime-catalog", &["crates/kyzo-core/src/runtime/"]),
    ("storage", &["crates/kyzo-core/src/storage/"]),
    ("engines", &["crates/kyzo-core/src/engines/"]),
    ("query", &["crates/kyzo-core/src/query/"]),
    ("record", &["crates/kyzo-core/src/data/record"]),
];

const LAYER_SPINE: &[(&str, &str)] = &[
    ("value", "Tuple · Domain · ExecRows/ExecDedup · EncodedKey"),
    (
        "runtime-catalog",
        "CatalogGeneration · RelationGeneration · IndexGeneration",
    ),
    (
        "storage",
        "encoded-key / order integrity (consumes EncodedKey)",
    ),
    ("engines", "ResidentIndexKey (rebuildable projections)"),
    ("query", "QueryDomainAdmission (admission into execution)"),
    ("record", "RecordId / KyzoRecord identity"),
];

struct ExpectedNode {
    name: &'static str,
    layer: &'static str,
    story: u32,
    conditional: bool,
}

/// Bootstrap node set from #136. Code annotations are authoritative the
/// moment they exist; this registry only says which nodes MUST eventually
/// exist and which story creates each. `conditional` nodes are planned, not
/// red.
const EXPECTED: &[ExpectedNode] = &[
    ExpectedNode {
        name: "Tuple",
        layer: "value",
        story: 126,
        conditional: false,
    },
    ExpectedNode {
        name: "Domain",
        layer: "value",
        story: 119,
        conditional: false,
    },
    ExpectedNode {
        name: "ExecRows",
        layer: "value",
        story: 119,
        conditional: false,
    },
    ExpectedNode {
        name: "ExecDedup",
        layer: "value",
        story: 119,
        conditional: false,
    },
    ExpectedNode {
        name: "EncodedKey",
        layer: "value",
        story: 119,
        conditional: false,
    },
    ExpectedNode {
        name: "CatalogGeneration",
        layer: "runtime-catalog",
        story: 135,
        conditional: false,
    },
    ExpectedNode {
        name: "RelationGeneration",
        layer: "runtime-catalog",
        story: 135,
        conditional: false,
    },
    ExpectedNode {
        name: "IndexGeneration",
        layer: "runtime-catalog",
        story: 135,
        conditional: false,
    },
    ExpectedNode {
        name: "ResidentIndexKey",
        layer: "engines",
        story: 122,
        conditional: false,
    },
    ExpectedNode {
        name: "QueryDomainAdmission",
        layer: "query",
        story: 122,
        conditional: false,
    },
    ExpectedNode {
        name: "RecordId",
        layer: "record",
        story: 128,
        conditional: true,
    },
];

const REQUIRED_KEYS: &[&str] = &["authority", "layer", "owns"];

/// Paths whose whole PURPOSE is raw-byte / parse boundary work: the blob and
/// string-taxonomy heuristics do not apply there (rule 03's boundary
/// carve-out).
const PARSE_BOUNDARY: &[&str] = &["crates/kyzo-core/src/parse/"];
const BYTE_PLANE: &[&str] = &[
    "crates/kyzo-core/src/data/value/",
    "crates/kyzo-core/src/storage/",
    "crates/kyzo-core/src/data/bitemporal.rs",
];
const SENSITIVE: &[&str] = &[
    "crates/kyzo-core/src/query/",
    "crates/kyzo-core/src/engines/",
    "crates/kyzo-core/src/runtime/",
    "crates/kyzo-core/src/storage/",
];

/// The exact set of bare types a blanket `From<T>` may not target without
/// forging an authority — a literal-string port of the script's
/// `re.fullmatch` alternation (no other regex behavior lives in it).
const BLANKET_FROM_TYPES: &[&str] = &[
    "Vec<DataValue>",
    "Vec<u8>",
    "Vec<u32>",
    "&[u8]",
    "&[u32]",
    "u8",
    "u16",
    "u32",
    "u64",
    "usize",
    "i32",
    "i64",
    "String",
    "&str",
];

static UNCHECKED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bfn\s+(from_raw\w*|\w*new_unchecked\w*|from_bytes_unchecked\w*|forge\w*)\s*[(<]")
        .unwrap()
});
static TUPLE_ALIAS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\btype\s+Tuple\s*=\s*Vec<DataValue>").unwrap());
static GEN_FIELD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:pub(?:\(\w+\))?\s+)?\w*generation\w*\s*:\s*(?:u8|u16|u32|u64|u128|usize|i32|i64|AtomicU(?:32|64))\b",
    )
    .unwrap()
});
static GEN_STRUCT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bstruct\s+(\w+Generation)\b").unwrap());
/// Per-projection freshness twin of the catalog generation authority (#135 /
/// #301 T7): `Watermark` and raw integer freshness/watermark counters.
static FRESHNESS_STRUCT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bstruct\s+Watermark\b").unwrap());
static FRESHNESS_FIELD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:pub(?:\(\w+\))?\s+)?\w*(?:watermark|freshness)\w*\s*:\s*(?:u8|u16|u32|u64|u128|usize|i32|i64|AtomicU(?:32|64))\b",
    )
    .unwrap()
});
/// A second independently-written value encoder: pushing a `Tag::*.byte()`
/// outside the one encoder seat (canonical + Num component).
static ENCODER_TAG_PUSH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^\w])(?:out\.)?push\(\s*Tag::\w+\.byte\(\)").unwrap());
const ENCODER_SEAT: &[&str] = &[
    "crates/kyzo-core/src/data/value/canonical.rs",
    "crates/kyzo-core/src/data/value/number.rs",
];
static RAW_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bfn\s+\w+\s*[(<][^)]*?\b([a-z]\w*_id)\s*:\s*(?:u8|u16|u32|u64|usize)\b").unwrap()
});
static RAW_ID_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub(?:\(\w+\))?\s+)?([a-z]\w*_id)\s*:\s*(?:u8|u16|u32|u64|usize)\s*,?\s*$")
        .unwrap()
});
static STRING_TAX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub(?:\(\w+\))?\s+)?(?:kind|format|variant|taxonomy|type_name)\s*:\s*(?:String|&\s*'?\w*\s*str)\b")
        .unwrap()
});
static BLOB_FIELD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:pub(?:\(\w+\))?\s+)?\w+\s*:\s*Vec<u8>\s*,?\s*$").unwrap());
static CONVERT_EDGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\w+)\s*->\s*(\w+)\s*(?:\((.*?)\))?\s*$").unwrap());
static STATUS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(established|pending)\s+#(\d+)(?:\s*[—-]\s*(.*))?$").unwrap());
static DOC_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:///|//!)\s?(.*)$").unwrap());
static ANNOTATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@(\w[\w-]*)\s+(.*)$").unwrap());
static IMPL_ESCAPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\bimpl(?:<[^>]*>)?\s+(?:std::ops::)?(?:(Deref|DerefMut)|From<(.+?)>)\s+for\s+(\w+)",
    )
    .unwrap()
});
static TYPE_ALIAS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\btype\s+(\w+)\s*=").unwrap());

/// One drift class the graph forbids. Variants are declared in the same
/// order as their `as_str()` string sorts alphabetically, so a derived
/// `Ord` reproduces the Python tool's `findings.sort(key=lambda f: (f.cls,
/// ...))` (a plain string sort) without re-deriving string comparison here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FindingClass {
    BlobMeaning,
    DuplicateAuthority,
    DuplicateAuthorityAlias,
    DuplicateGenerationCounter,
    EncoderTwin,
    FreshnessTwin,
    IllegalEscape,
    LayerMismatch,
    MalformedDeclaration,
    MissingAuthority,
    RawIdCrossing,
    SearchraDecodedTuples,
    StaleAllowlist,
    StringTaxonomy,
    TupleVecAlias,
    UncheckedConstructor,
}

impl FindingClass {
    fn as_str(self) -> &'static str {
        match self {
            FindingClass::BlobMeaning => "blob-meaning",
            FindingClass::DuplicateAuthority => "duplicate-authority",
            FindingClass::DuplicateAuthorityAlias => "duplicate-authority-alias",
            FindingClass::DuplicateGenerationCounter => "duplicate-generation-counter",
            FindingClass::EncoderTwin => "encoder-twin",
            FindingClass::FreshnessTwin => "freshness-twin",
            FindingClass::IllegalEscape => "illegal-escape",
            FindingClass::LayerMismatch => "layer-mismatch",
            FindingClass::MalformedDeclaration => "malformed-declaration",
            FindingClass::MissingAuthority => "missing-authority",
            FindingClass::RawIdCrossing => "raw-id-crossing",
            FindingClass::SearchraDecodedTuples => "searchra-decoded-tuples",
            FindingClass::StaleAllowlist => "stale-allowlist",
            FindingClass::StringTaxonomy => "string-taxonomy",
            FindingClass::TupleVecAlias => "tuple-vec-alias",
            FindingClass::UncheckedConstructor => "unchecked-constructor",
        }
    }
}

#[derive(Clone)]
struct Finding {
    cls: FindingClass,
    path: String,
    lineno: usize,
    text: String,
    note: String,
    allowlisted_by: Option<String>,
}

impl Finding {
    fn new(cls: FindingClass, path: String, lineno: usize, text: String, note: String) -> Self {
        Finding {
            cls,
            path,
            lineno,
            text: text.trim().to_string(),
            note,
            allowlisted_by: None,
        }
    }
}

#[derive(Serialize)]
struct FindingJson {
    class: String,
    file: String,
    line: usize,
    excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowlisted: Option<String>,
}

impl Finding {
    fn as_json(&self) -> FindingJson {
        FindingJson {
            class: self.cls.as_str().to_string(),
            file: self.path.clone(),
            line: self.lineno,
            excerpt: self.text.clone(),
            note: if self.note.is_empty() {
                None
            } else {
                Some(self.note.clone())
            },
            allowlisted: self.allowlisted_by.clone(),
        }
    }
}

#[derive(Default, Clone)]
struct DeclBlock {
    authority: Option<String>,
    layer: Option<String>,
    owns: Option<String>,
    constructs: Option<String>,
    forbids: Option<String>,
    converts: Option<String>,
    gate: Option<String>,
    status: Option<String>,
}

impl DeclBlock {
    fn set(&mut self, key: &str, value: String) -> bool {
        match key {
            "authority" => self.authority = Some(value),
            "layer" => self.layer = Some(value),
            "owns" => self.owns = Some(value),
            "constructs" => self.constructs = Some(value),
            "forbids" => self.forbids = Some(value),
            "converts" => self.converts = Some(value),
            "gate" => self.gate = Some(value),
            "status" => self.status = Some(value),
            _ => return false,
        }
        true
    }
}

#[derive(Clone, Serialize)]
struct StatusInfo {
    kind: String,
    story: Option<u32>,
    note: String,
}

#[derive(Clone, Serialize)]
struct Node {
    name: String,
    layer: String,
    file: String,
    line: usize,
    owns: String,
    constructs: Vec<String>,
    forbids: Vec<String>,
    converts: Vec<String>,
    gate: String,
    status: StatusInfo,
}

#[derive(Clone, Serialize)]
struct Edge {
    from: String,
    to: String,
    context: String,
    declared_by: String,
}

#[derive(Clone, Serialize)]
struct Planned {
    name: String,
    layer: String,
    story: u32,
}

struct AllowlistEntry {
    class: String,
    file: String,
    contains: String,
    reason: String,
    hits: std::cell::Cell<u32>,
}

pub struct ScanOutput {
    nodes: BTreeMap<String, Node>,
    edges: Vec<Edge>,
    planned: Vec<Planned>,
    findings: Vec<Finding>,
    files_scanned: usize,
}

fn strip_line_comment(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0usize;
    let mut in_str = false;
    while i < n {
        let c = chars[i];
        if in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            break;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn split_pipe(s: &str) -> Vec<String> {
    s.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Extract `@authority` doc blocks from one file's raw lines.
fn parse_declarations(
    path: &str,
    raw_lines: &[String],
    findings: &mut Vec<Finding>,
) -> Vec<(DeclBlock, usize)> {
    let mut nodes = Vec::new();
    let mut block: Option<DeclBlock> = None;
    let mut block_start = 0usize;
    for (i, line) in raw_lines.iter().enumerate() {
        let idx = i + 1;
        let stripped = line.trim();
        if let Some(doc_caps) = DOC_LINE_RE.captures(stripped) {
            let body = doc_caps.get(1).map(|m| m.as_str()).unwrap_or("");
            if let Some(km) = ANNOTATION_RE.captures(body) {
                let key = &km[1];
                let value = km[2].trim().to_string();
                if key == "authority" {
                    if let Some(b) = block.take() {
                        nodes.push((b, block_start));
                    }
                    block = Some(DeclBlock {
                        authority: Some(value),
                        ..Default::default()
                    });
                    block_start = idx;
                } else if let Some(b) = block.as_mut()
                    && !b.set(key, value)
                {
                    findings.push(Finding::new(
                        FindingClass::MalformedDeclaration,
                        path.to_string(),
                        idx,
                        stripped.to_string(),
                        format!("unknown annotation key @{key}"),
                    ));
                }
            }
            continue;
        }
        if let Some(b) = block.take() {
            nodes.push((b, block_start));
        }
    }
    if let Some(b) = block.take() {
        nodes.push((b, block_start));
    }
    nodes
}

fn layers_repr() -> String {
    let mut names: Vec<&str> = LAYERS.iter().map(|(l, _)| *l).collect();
    names.sort_unstable();
    format!(
        "[{}]",
        names
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn parse_status(status: &str) -> StatusInfo {
    if let Some(caps) = STATUS_RE.captures(status) {
        StatusInfo {
            kind: caps[1].to_string(),
            story: caps[2].parse().ok(),
            note: caps
                .get(3)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default(),
        }
    } else {
        StatusInfo {
            kind: "unspecified".to_string(),
            story: None,
            note: status.to_string(),
        }
    }
}

/// Validate declaration blocks, build the node table (first declaration of
/// a name wins; later ones are `duplicate-authority` findings), and the
/// `@converts` edges of each accepted node — done in the same
/// decls-in-file-order pass the Python tool's two-pass (check then
/// build_edges over `nodes.values()`, a dict that preserves insertion
/// order) reduces to.
fn check_declarations(
    decls: Vec<(DeclBlock, usize, String)>,
    findings: &mut Vec<Finding>,
) -> (BTreeMap<String, Node>, Vec<Edge>) {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut edges: Vec<Edge> = Vec::new();

    for (block, lineno, path) in decls {
        let name = block.authority.clone().unwrap_or_default();
        let mut missing: Vec<&str> = Vec::new();
        for key in REQUIRED_KEYS {
            let present = match *key {
                "authority" => block.authority.as_deref().is_some_and(|s| !s.is_empty()),
                "layer" => block.layer.as_deref().is_some_and(|s| !s.is_empty()),
                "owns" => block.owns.as_deref().is_some_and(|s| !s.is_empty()),
                _ => unreachable!(),
            };
            if !present {
                missing.push(key);
            }
        }
        if !missing.is_empty() {
            findings.push(Finding::new(
                FindingClass::MalformedDeclaration,
                path.clone(),
                lineno,
                format!("@authority {name}"),
                format!("missing required @{}", missing.join(", @")),
            ));
            continue;
        }
        let layer = block.layer.clone().unwrap();
        let Some((_, prefixes)) = LAYERS.iter().find(|(l, _)| *l == layer) else {
            findings.push(Finding::new(
                FindingClass::MalformedDeclaration,
                path.clone(),
                lineno,
                format!("@authority {name}"),
                format!("unknown layer '{layer}' (want one of {})", layers_repr()),
            ));
            continue;
        };
        if !prefixes.iter().any(|p| path.starts_with(p)) {
            findings.push(Finding::new(
                FindingClass::LayerMismatch,
                path.clone(),
                lineno,
                format!("@authority {name}"),
                format!("declared layer '{layer}' does not own path {path}"),
            ));
        }
        if let Some(existing) = nodes.get(&name) {
            findings.push(Finding::new(
                FindingClass::DuplicateAuthority,
                path.clone(),
                lineno,
                format!("@authority {name}"),
                format!("already declared at {}:{}", existing.file, existing.line),
            ));
            continue;
        }
        if let Some(exp) = EXPECTED.iter().find(|e| e.name == name)
            && exp.layer != layer
        {
            findings.push(Finding::new(
                FindingClass::LayerMismatch,
                path.clone(),
                lineno,
                format!("@authority {name}"),
                format!(
                    "declared layer '{layer}' but the graph places it in '{}'",
                    exp.layer
                ),
            ));
        }
        let status = parse_status(block.status.as_deref().unwrap_or(""));
        let converts = split_pipe(block.converts.as_deref().unwrap_or(""));
        for conv in &converts {
            if let Some(caps) = CONVERT_EDGE_RE.captures(conv) {
                edges.push(Edge {
                    from: caps[1].to_string(),
                    to: caps[2].to_string(),
                    context: caps
                        .get(3)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default(),
                    declared_by: name.clone(),
                });
            } else {
                findings.push(Finding::new(
                    FindingClass::MalformedDeclaration,
                    path.clone(),
                    lineno,
                    format!("@converts {conv}"),
                    "conversion must read 'A -> B (context)'".to_string(),
                ));
            }
        }
        let node = Node {
            name: name.clone(),
            layer: layer.clone(),
            file: path.clone(),
            line: lineno,
            owns: block.owns.clone().unwrap(),
            constructs: split_pipe(block.constructs.as_deref().unwrap_or("")),
            forbids: split_pipe(block.forbids.as_deref().unwrap_or("")),
            converts,
            gate: block.gate.clone().unwrap_or_default(),
            status,
        };
        nodes.insert(name, node);
    }

    (nodes, edges)
}

/// Run the drift checks over one file's code (comments stripped).
fn scan_code(
    path: &str,
    raw_lines: &[String],
    nodes: &BTreeMap<String, Node>,
    findings: &mut Vec<Finding>,
) {
    let in_parse = PARSE_BOUNDARY.iter().any(|p| path.starts_with(p));
    let in_byte_plane = BYTE_PLANE.iter().any(|p| path.starts_with(p));
    let in_sensitive = SENSITIVE.iter().any(|p| path.starts_with(p));
    let is_search_seam = path.ends_with("query/ra/search.rs");

    for (i, raw) in raw_lines.iter().enumerate() {
        let idx = i + 1;
        let s = raw.trim();
        if s.starts_with("//") || s.starts_with("#[") || s.is_empty() {
            continue;
        }
        let code = strip_line_comment(raw);
        let cs = code.trim();
        if cs.is_empty() {
            continue;
        }

        if TUPLE_ALIAS_RE.is_match(cs) {
            if path == "crates/kyzo-core/src/data/value/mod.rs" {
                findings.push(Finding::new(
                    FindingClass::TupleVecAlias,
                    path.to_string(),
                    idx,
                    cs.to_string(),
                    "row authority is a bare Vec<DataValue> alias (newtype owed by #126)"
                        .to_string(),
                ));
            } else {
                findings.push(Finding::new(
                    FindingClass::DuplicateAuthorityAlias,
                    path.to_string(),
                    idx,
                    cs.to_string(),
                    "story-local redefinition of the Tuple row authority".to_string(),
                ));
            }
        }

        if is_search_seam && cs.contains("Vec<Tuple>") {
            findings.push(Finding::new(
                FindingClass::SearchraDecodedTuples,
                path.to_string(),
                idx,
                cs.to_string(),
                "engine hits flow as decoded tuples, not admitted codes (QueryDomainAdmission owed by #122)"
                    .to_string(),
            ));
        }

        if let Some(caps) = UNCHECKED_RE.captures(cs) {
            findings.push(Finding::new(
                FindingClass::UncheckedConstructor,
                path.to_string(),
                idx,
                cs.to_string(),
                format!("raw-door constructor '{}'", &caps[1]),
            ));
        }

        if let Some(caps) = IMPL_ESCAPE_RE.captures(cs) {
            let target = &caps[3];
            if nodes.contains_key(target) {
                if let Some(deref_kind) = caps.get(1) {
                    findings.push(Finding::new(
                        FindingClass::IllegalEscape,
                        path.to_string(),
                        idx,
                        cs.to_string(),
                        format!(
                            "{} dissolves the {target} authority boundary",
                            deref_kind.as_str()
                        ),
                    ));
                } else if let Some(src_m) = caps.get(2) {
                    let src = src_m.as_str().trim();
                    if BLANKET_FROM_TYPES.contains(&src) {
                        findings.push(Finding::new(
                            FindingClass::IllegalEscape,
                            path.to_string(),
                            idx,
                            cs.to_string(),
                            format!("blanket From<{src}> forges the {target} authority"),
                        ));
                    }
                }
            }
        }

        if let Some(caps) = TYPE_ALIAS_RE.captures(cs) {
            let alias_name = &caps[1];
            if alias_name != "Tuple"
                && let Some(n) = nodes.get(alias_name)
                && n.file != path
            {
                findings.push(Finding::new(
                    FindingClass::DuplicateAuthorityAlias,
                    path.to_string(),
                    idx,
                    cs.to_string(),
                    format!("story-local redefinition of the {alias_name} authority"),
                ));
            }
        }

        if GEN_FIELD_RE.is_match(&code) {
            findings.push(Finding::new(
                FindingClass::DuplicateGenerationCounter,
                path.to_string(),
                idx,
                cs.to_string(),
                "raw-integer generation counter (catalog generations are the one validity authority, #135)"
                    .to_string(),
            ));
        }
        if let Some(caps) = GEN_STRUCT_RE.captures(cs) {
            let runtime_catalog_prefixes = LAYERS
                .iter()
                .find(|(l, _)| *l == "runtime-catalog")
                .unwrap()
                .1;
            if !runtime_catalog_prefixes.iter().any(|p| path.starts_with(p)) {
                findings.push(Finding::new(
                    FindingClass::DuplicateGenerationCounter,
                    path.to_string(),
                    idx,
                    cs.to_string(),
                    format!("{} declared outside the runtime catalog seam", &caps[1]),
                ));
            }
        }

        if FRESHNESS_STRUCT_RE.is_match(cs) {
            findings.push(Finding::new(
                FindingClass::FreshnessTwin,
                path.to_string(),
                idx,
                cs.to_string(),
                "per-projection Watermark freshness twin of the catalog generation authority (#135 / #301 T7)"
                    .to_string(),
            ));
        }
        if FRESHNESS_FIELD_RE.is_match(&code) {
            findings.push(Finding::new(
                FindingClass::FreshnessTwin,
                path.to_string(),
                idx,
                cs.to_string(),
                "raw-integer freshness/watermark counter (catalog generations are the one validity authority, #135)"
                    .to_string(),
            ));
        }

        if ENCODER_TAG_PUSH_RE.is_match(cs) && !ENCODER_SEAT.iter().any(|p| path == *p) {
            findings.push(Finding::new(
                FindingClass::EncoderTwin,
                path.to_string(),
                idx,
                cs.to_string(),
                "Tag::*.byte() push outside the one value-encoder seat (canonical.rs + number.rs)"
                    .to_string(),
            ));
        }

        if in_sensitive {
            let raw_id = RAW_ID_RE
                .captures(cs)
                .map(|c| c[1].to_string())
                .or_else(|| RAW_ID_LINE_RE.captures(&code).map(|c| c[1].to_string()));
            if let Some(id_name) = raw_id {
                findings.push(Finding::new(
                    FindingClass::RawIdCrossing,
                    path.to_string(),
                    idx,
                    cs.to_string(),
                    format!("bare-integer identity '{id_name}' crossing an authority-sensitive boundary (newtype it)"),
                ));
            }
        }

        if in_sensitive && !in_parse && STRING_TAX_RE.is_match(&code) {
            findings.push(Finding::new(
                FindingClass::StringTaxonomy,
                path.to_string(),
                idx,
                cs.to_string(),
                "string-typed kind/format field where an enum belongs (rule 03)".to_string(),
            ));
        }

        if in_sensitive && !in_parse && !in_byte_plane && BLOB_FIELD_RE.is_match(&code) {
            findings.push(Finding::new(
                FindingClass::BlobMeaning,
                path.to_string(),
                idx,
                cs.to_string(),
                "generic byte blob carrying meaning outside the byte plane".to_string(),
            ));
        }
    }
}

fn check_missing(nodes: &BTreeMap<String, Node>, findings: &mut Vec<Finding>) -> Vec<Planned> {
    let mut planned = Vec::new();
    let mut expected: Vec<&ExpectedNode> = EXPECTED.iter().collect();
    expected.sort_by_key(|e| e.name);
    for exp in expected {
        if nodes.contains_key(exp.name) {
            continue;
        }
        if exp.conditional {
            planned.push(Planned {
                name: exp.name.to_string(),
                layer: exp.layer.to_string(),
                story: exp.story,
            });
            continue;
        }
        findings.push(Finding::new(
            FindingClass::MissingAuthority,
            "crates/kyzo-core/src".to_string(),
            0,
            exp.name.to_string(),
            format!(
                "expected authority (layer {}) does not exist yet — owed by #{}",
                exp.layer, exp.story
            ),
        ));
    }
    planned
}

fn load_allowlist(path: &Path) -> anyhow::Result<(Vec<AllowlistEntry>, Vec<String>)> {
    if !path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let text = std::fs::read_to_string(path)?;
    let raw: Vec<serde_json::Value> = serde_json::from_str(&text)?;
    let mut ok = Vec::new();
    let mut problems = Vec::new();
    for e in raw {
        let get = |k: &str| e.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let (Some(class), Some(file), Some(contains), Some(reason)) =
            (get("class"), get("file"), get("contains"), get("reason"))
        else {
            problems.push(format!(
                "allowlist entry rejected (must carry exact class/file/contains/reason): {e}"
            ));
            continue;
        };
        if contains.len() < 8 || file.contains(['*', '?', '[']) {
            problems.push(format!("allowlist entry rejected (too broad): {e}"));
            continue;
        }
        ok.push(AllowlistEntry {
            class: class.to_string(),
            file: file.to_string(),
            contains: contains.to_string(),
            reason: reason.to_string(),
            hits: std::cell::Cell::new(0),
        });
    }
    Ok((ok, problems))
}

fn apply_allowlist(findings: &mut [Finding], allow: &[AllowlistEntry]) {
    for f in findings.iter_mut() {
        for e in allow {
            if f.cls.as_str() == e.class
                && f.path.ends_with(e.file.as_str())
                && f.text.contains(e.contains.as_str())
            {
                f.allowlisted_by = Some(e.reason.clone());
                e.hits.set(e.hits.get() + 1);
                break;
            }
        }
    }
}

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if !dir.exists() {
        return;
    }
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path.to_path_buf());
        }
    }
}

/// Scan `crates/kyzo-core/src` under `root`; return the scan plus any
/// allowlist-load warnings.
fn run_scan(root: &Path, allowlist_path: &Path) -> anyhow::Result<(ScanOutput, Vec<String>)> {
    let mut findings: Vec<Finding> = Vec::new();
    let mut decls: Vec<(DeclBlock, usize, String)> = Vec::new();

    let src_root = root.join("crates/kyzo-core/src");
    let mut files: Vec<PathBuf> = Vec::new();
    walk_rs_files(&src_root, &mut files);
    files.sort();

    let mut per_file: Vec<(String, Vec<String>)> = Vec::new();
    for fp in &files {
        let rel = fp
            .strip_prefix(root)
            .unwrap_or(fp)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(fp)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", fp.display()))?;
        let raw_lines: Vec<String> = text.lines().map(str::to_string).collect();
        for (block, lineno) in parse_declarations(&rel, &raw_lines, &mut findings) {
            decls.push((block, lineno, rel.clone()));
        }
        per_file.push((rel, raw_lines));
    }

    let (nodes, edges) = check_declarations(decls, &mut findings);
    for (rel, raw_lines) in &per_file {
        scan_code(rel, raw_lines, &nodes, &mut findings);
    }
    let planned = check_missing(&nodes, &mut findings);

    let (allow, problems) = load_allowlist(allowlist_path)?;
    apply_allowlist(&mut findings, &allow);
    for e in &allow {
        if e.hits.get() == 0 {
            findings.push(Finding::new(
                FindingClass::StaleAllowlist,
                e.file.clone(),
                0,
                e.contains.clone(),
                "allowlist entry matches nothing — delete it".to_string(),
            ));
        }
    }

    findings.sort_by(|a, b| (a.cls, &a.path, a.lineno).cmp(&(b.cls, &b.path, b.lineno)));

    let files_scanned = files.len();
    Ok((
        ScanOutput {
            nodes,
            edges,
            planned,
            findings,
            files_scanned,
        },
        problems,
    ))
}

fn counts_by_class(findings: &[Finding]) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for f in findings {
        if f.allowlisted_by.is_none() {
            *counts.entry(f.cls.as_str().to_string()).or_insert(0) += 1;
        }
    }
    counts
}

#[derive(Serialize)]
struct LayerSpineJson {
    layer: String,
    holds: String,
}

#[derive(Serialize)]
struct MapJson {
    tool: &'static str,
    tool_version: u32,
    scope: &'static str,
    files_scanned: usize,
    layers: Vec<LayerSpineJson>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    planned: Vec<Planned>,
    findings: Vec<FindingJson>,
    counts_by_class: BTreeMap<String, u32>,
}

/// The two artifacts as strings — a pure function of the scan, so the
/// committed copies never drift for volatile reasons (no clock, no commit
/// hash, no mode).
fn render_outputs(scan: &ScanOutput, counts: &BTreeMap<String, u32>) -> (String, String) {
    let live: Vec<&Finding> = scan
        .findings
        .iter()
        .filter(|f| f.allowlisted_by.is_none())
        .collect();
    let allowed: Vec<&Finding> = scan
        .findings
        .iter()
        .filter(|f| f.allowlisted_by.is_some())
        .collect();

    let map_obj = MapJson {
        tool: "authority-graph",
        tool_version: TOOL_VERSION,
        scope: "crates/kyzo-core/src",
        files_scanned: scan.files_scanned,
        layers: LAYER_SPINE
            .iter()
            .map(|(l, h)| LayerSpineJson {
                layer: (*l).to_string(),
                holds: (*h).to_string(),
            })
            .collect(),
        nodes: scan.nodes.values().cloned().collect(),
        edges: scan.edges.clone(),
        planned: scan.planned.clone(),
        findings: scan.findings.iter().map(Finding::as_json).collect(),
        counts_by_class: counts.clone(),
    };
    let map_text = serde_json::to_string_pretty(&map_obj).unwrap_or_default() + "\n";

    let mut l = Vec::new();
    l.push("# Type Authority Graph — drift report".to_string());
    l.push(String::new());
    l.push(format!(
        "Generated by `scripts/authority-graph` (v{TOOL_VERSION}) over `crates/kyzo-core/src` ({} files). Regenerate with `cargo xtask authority --write`; the gate fails if this file is stale (`cargo xtask authority`).",
        scan.files_scanned
    ));
    l.push(String::new());
    l.push("## Layer spine".to_string());
    l.push(String::new());
    l.push("| layer | holds |".to_string());
    l.push("|---|---|".to_string());
    for (layer, holds) in LAYER_SPINE {
        l.push(format!("| `{layer}` | {holds} |"));
    }
    l.push(String::new());
    l.push("## Declared authorities".to_string());
    l.push(String::new());
    if scan.nodes.is_empty() {
        l.push("(none declared)".to_string());
    } else {
        l.push("| authority | layer | status | declared at | owns |".to_string());
        l.push("|---|---|---|---|---|".to_string());
        for n in scan.nodes.values() {
            let stxt = match n.status.story {
                Some(story) => format!("{} #{story}", n.status.kind),
                None => n.status.kind.clone(),
            };
            l.push(format!(
                "| `{}` | `{}` | {stxt} | `{}:{}` | {} |",
                n.name, n.layer, n.file, n.line, n.owns
            ));
        }
    }
    l.push(String::new());
    l.push("## Legal conversions (edges)".to_string());
    l.push(String::new());
    if scan.edges.is_empty() {
        l.push("(none declared)".to_string());
    } else {
        for e in &scan.edges {
            let ctx = if e.context.is_empty() {
                String::new()
            } else {
                format!(" — {}", e.context)
            };
            l.push(format!("- `{}` → `{}`{ctx}", e.from, e.to));
        }
    }
    l.push(String::new());
    if !scan.planned.is_empty() {
        l.push("## Planned (conditional) authorities".to_string());
        l.push(String::new());
        for p in &scan.planned {
            l.push(format!(
                "- `{}` (`{}`) — created by #{} only if it proceeds",
                p.name, p.layer, p.story
            ));
        }
        l.push(String::new());
    }
    l.push(format!(
        "## Findings ({} live, {} allowlisted)",
        live.len(),
        allowed.len()
    ));
    l.push(String::new());
    if !counts.is_empty() {
        l.push("| class | count |".to_string());
        l.push("|---|---|".to_string());
        for (c, n) in counts {
            l.push(format!("| `{c}` | {n} |"));
        }
        l.push(String::new());
        let mut cur: Option<FindingClass> = None;
        for f in &live {
            if cur != Some(f.cls) {
                cur = Some(f.cls);
                l.push(format!("### {}", f.cls.as_str()));
                l.push(String::new());
            }
            let loc = if f.lineno > 0 {
                format!("`{}:{}`", f.path, f.lineno)
            } else {
                format!("`{}`", f.path)
            };
            l.push(format!("- {loc} — {}", f.note));
            if f.lineno > 0 {
                l.push(format!("  `{}`", f.text));
            }
        }
        l.push(String::new());
    } else {
        l.push("No live findings. The tree matches the declared graph.".to_string());
        l.push(String::new());
    }
    if !allowed.is_empty() {
        l.push("## Allowlisted (intentional boundaries)".to_string());
        l.push(String::new());
        for f in &allowed {
            l.push(format!(
                "- `{}:{}` `{}` — {}",
                f.path,
                f.lineno,
                f.cls.as_str(),
                f.allowlisted_by.as_deref().unwrap_or("")
            ));
        }
        l.push(String::new());
    }
    let report_text = l.join("\n") + "\n";

    (map_text, report_text)
}

fn load_baseline(path: &Path) -> anyhow::Result<BTreeMap<String, u32>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn ratchet_regressions(
    counts: &BTreeMap<String, u32>,
    baseline: &BTreeMap<String, u32>,
) -> (Vec<String>, Vec<String>) {
    let mut regressed = Vec::new();
    let mut improved = Vec::new();
    let classes: std::collections::BTreeSet<&String> =
        counts.keys().chain(baseline.keys()).collect();
    for cls in classes {
        let now = counts.get(cls).copied().unwrap_or(0);
        let floor = baseline.get(cls).copied().unwrap_or(0);
        match now.cmp(&floor) {
            std::cmp::Ordering::Greater => regressed.push(format!("{cls}: {floor} -> {now}")),
            std::cmp::Ordering::Less => improved.push(format!("{cls}: {floor} -> {now}")),
            std::cmp::Ordering::Equal => {}
        }
    }
    (regressed, improved)
}

#[derive(Debug)]
pub enum AuthorityError {
    RepoScan(anyhow::Error),
    BaselineLoad(anyhow::Error),
    ArtifactIo(anyhow::Error),
    SelfTestFailed(Vec<String>),
    GateFailed {
        regressed: Vec<String>,
        stale: Vec<String>,
    },
}

impl fmt::Display for AuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthorityError::RepoScan(e) => {
                write!(f, "authority-graph: could not scan the tree: {e:#}")
            }
            AuthorityError::BaselineLoad(e) => {
                write!(f, "authority-graph: could not load baseline: {e:#}")
            }
            AuthorityError::ArtifactIo(e) => {
                write!(f, "authority-graph: could not write artifacts: {e:#}")
            }
            AuthorityError::SelfTestFailed(failures) => {
                writeln!(f, "authority-graph SELF-TEST FAILURE")?;
                for x in failures {
                    writeln!(f, "  - {x}")?;
                }
                Ok(())
            }
            AuthorityError::GateFailed { regressed, stale } => {
                for r in regressed {
                    writeln!(f, "RATCHET FAILURE {r}")?;
                }
                for s in stale {
                    writeln!(
                        f,
                        "STALE: {s} does not match the tree — regenerate with `cargo xtask authority --write` and commit it"
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AuthorityError {}

/// Planted-violation proof: every drift class fires on a fixture tree, and a
/// clean fixture stays clean. Fixtures are a direct port of the Python
/// tool's own self-test (same planted violations, same expected counts).
pub fn self_test() -> Result<String, AuthorityError> {
    let fixtures: &[(&str, &str)] = &[
        (
            "crates/kyzo-core/src/data/value/exec.rs",
            "/// @authority ExecRows\n\
             /// @layer value\n\
             /// @owns admitted execution currency\n\
             /// @constructs ExecRows::admit\n\
             /// @forbids from_raw\n\
             /// @converts ExecRows -> EncodedKey (storage boundary)\n\
             /// @gate no raw-code door\n\
             /// @status established #119\n\
             pub struct ExecRows { codes: Vec<u32> }\n\
             impl ExecRows {\n\
             \u{20}   // fn from_raw(codes: Vec<u32>) -> ExecRows {}   <- commented: must NOT fire\n\
             \u{20}   pub fn from_raw(codes: Vec<u32>) -> ExecRows { ExecRows { codes } }\n\
             }\n",
        ),
        (
            "crates/kyzo-core/src/data/value/mod.rs",
            "/// @authority EncodedKey\n\
             /// @layer value\n\
             /// @owns canonical storage identity\n\
             /// @status established #119\n\
             pub struct EncodedKey(Vec<u8>);\n\
             pub type Tuple = Vec<DataValue>;\n\
             impl Deref for EncodedKey { type Target = Vec<u8>; }\n\
             impl From<Vec<u8>> for EncodedKey { fn from(b: Vec<u8>) -> EncodedKey { EncodedKey(b) } }\n",
        ),
        (
            "crates/kyzo-core/src/query/local_types.rs",
            "type Tuple = Vec<DataValue>;\n\
             type ExecRows = Vec<u32>;\n\
             fn lookup(relation_id: u64) -> bool { relation_id == 0 }\n\
             fn encode_twin(out: &mut Vec<u8>) { out.push(Tag::Null.byte()); }\n\
             struct Carrier {\n\
             \u{20}   payload: Vec<u8>,\n\
             \u{20}   owner_id: u32,\n\
             }\n",
        ),
        (
            "crates/kyzo-core/src/query/ra/search.rs",
            "fn search(row: &[DataValue]) -> Result<Vec<Tuple>> { todo!() }\n",
        ),
        (
            "crates/kyzo-core/src/query/plan_cache.rs",
            "struct PlanCache {\n\
             \u{20}   generation: u64,\n\
             \u{20}   kind: String,\n\
             }\n\
             struct PlanCacheGeneration(u64);\n",
        ),
        (
            "crates/kyzo-core/src/engines/hnsw.rs",
            "/// @authority ResidentIndexKey\n\
             /// @layer value\n\
             /// @owns residency cache identity\n\
             pub struct ResidentIndexKey;\n\
             pub(crate) struct Watermark(u64);\n",
        ),
        (
            "crates/kyzo-core/src/runtime/catalog.rs",
            "/// @authority CatalogGeneration\n\
             /// @layer runtime-catalog\n\
             /// @constructs the catalog authority\n\
             pub struct CatalogGeneration(u64);\n",
        ),
        (
            "crates/kyzo-core/src/parse/lexer.rs",
            "struct Token { kind: String }\n\
             fn eat(input: &str) -> Token { todo!() }\n",
        ),
    ];

    let tmp = tempfile::Builder::new()
        .prefix("authority-selftest-")
        .tempdir()
        .map_err(|e| AuthorityError::RepoScan(e.into()))?;
    let root = tmp.path();
    for (rel, content) in fixtures {
        let fp = root.join(rel);
        if let Some(parent) = fp.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AuthorityError::RepoScan(e.into()))?;
        }
        std::fs::write(&fp, content).map_err(|e| AuthorityError::RepoScan(e.into()))?;
    }

    let (scan, _problems) =
        run_scan(root, &root.join("no-allowlist.json")).map_err(AuthorityError::RepoScan)?;
    let counts = counts_by_class(&scan.findings);

    let non_conditional_expected = EXPECTED.iter().filter(|e| !e.conditional).count() as i64;
    let expect_at_least: &[(FindingClass, i64)] = &[
        (FindingClass::UncheckedConstructor, 1),
        (FindingClass::TupleVecAlias, 1),
        (FindingClass::IllegalEscape, 2),
        (FindingClass::DuplicateAuthorityAlias, 2),
        (FindingClass::RawIdCrossing, 2),
        (FindingClass::BlobMeaning, 1),
        (FindingClass::SearchraDecodedTuples, 1),
        (FindingClass::DuplicateGenerationCounter, 2),
        (FindingClass::EncoderTwin, 1),
        (FindingClass::FreshnessTwin, 1),
        (FindingClass::StringTaxonomy, 1),
        (FindingClass::MalformedDeclaration, 1),
        (FindingClass::MissingAuthority, non_conditional_expected - 3),
    ];

    let mut failures = Vec::new();
    for (cls, want) in expect_at_least {
        let got = counts.get(cls.as_str()).copied().unwrap_or(0) as i64;
        if got < *want {
            failures.push(format!("expected >= {want} '{}', got {got}", cls.as_str()));
        }
    }
    if counts
        .get(FindingClass::UncheckedConstructor.as_str())
        .copied()
        .unwrap_or(0)
        != 1
    {
        failures.push("comment stripping failed: commented from_raw fired".to_string());
    }
    if scan
        .findings
        .iter()
        .any(|f| f.cls == FindingClass::StringTaxonomy && f.path.contains("lexer"))
    {
        failures.push("parse boundary not honored: lexer flagged".to_string());
    }
    if !scan
        .findings
        .iter()
        .any(|f| f.cls == FindingClass::LayerMismatch)
    {
        failures.push("layer-mismatch did not fire".to_string());
    }
    if !scan
        .edges
        .iter()
        .any(|e| e.from == "ExecRows" && e.to == "EncodedKey")
    {
        failures.push("@converts edge not extracted".to_string());
    }

    if !failures.is_empty() {
        return Err(AuthorityError::SelfTestFailed(failures));
    }

    let total: u32 = counts.values().sum();
    Ok(format!(
        "SELF-TEST OK — planted violations all detected (from_raw(Vec<u32>) ExecRows door, plan-cache generation counter, decoded Vec<Tuple> SearchRA path, Deref escape, duplicate Tuple alias, raw id, string taxonomy, blob field, encoder Tag push twin, Watermark freshness twin); clean fixtures stayed clean; {total} findings across {} classes",
        counts.len()
    ))
}

/// The verdict the gate actually checks (justfile's `authority` recipe,
/// preserved 1:1): scan the real tree, ratchet the finding counts against
/// `crates/xtask/authority-baseline.json`, and require the committed
/// `authority/` artifacts to already match what this scan would produce.
pub fn run_gate_check(root: &Path) -> Result<String, AuthorityError> {
    let allowlist_path = root.join("crates/xtask/authority-allowlist.json");
    let baseline_path = root.join("crates/xtask/authority-baseline.json");
    let out_dir = root.join("authority");

    let (scan, problems) = run_scan(root, &allowlist_path).map_err(AuthorityError::RepoScan)?;
    for p in &problems {
        eprintln!("WARNING: {p}");
    }
    let counts = counts_by_class(&scan.findings);
    let (map_text, report_text) = render_outputs(&scan, &counts);

    let mut stale = Vec::new();
    for (name, want) in [
        ("authority-map.json", &map_text),
        ("authority-report.md", &report_text),
    ] {
        let fp = out_dir.join(name);
        let have = std::fs::read_to_string(&fp).unwrap_or_default();
        if &have != want {
            stale.push(fp.display().to_string());
        }
    }

    let baseline = load_baseline(&baseline_path).map_err(AuthorityError::BaselineLoad)?;
    let (regressed, improved) = ratchet_regressions(&counts, &baseline);
    for msg in &improved {
        println!(
            "RATCHET IMPROVED {msg} (tighten the floor: cargo xtask authority --update-baseline)"
        );
    }

    if !regressed.is_empty() || !stale.is_empty() {
        return Err(AuthorityError::GateFailed { regressed, stale });
    }

    let live: u32 = counts.values().sum();
    Ok(format!(
        "authority-graph: {} declared, {} edges, {live} live findings across {} classes -> {}/authority-report.md",
        scan.nodes.len(),
        scan.edges.len(),
        counts.len(),
        out_dir.display()
    ))
}

/// Regenerate the committed `authority/` artifacts from the current tree —
/// the ported tool's report mode, needed to un-stale the freshness check
/// after a legitimate declaration change now that the Python generator is
/// gone.
pub fn write_report(root: &Path) -> Result<String, AuthorityError> {
    let allowlist_path = root.join("crates/xtask/authority-allowlist.json");
    let out_dir = root.join("authority");

    let (scan, problems) = run_scan(root, &allowlist_path).map_err(AuthorityError::RepoScan)?;
    for p in &problems {
        eprintln!("WARNING: {p}");
    }
    let counts = counts_by_class(&scan.findings);
    let (map_text, report_text) = render_outputs(&scan, &counts);

    std::fs::create_dir_all(&out_dir).map_err(|e| AuthorityError::ArtifactIo(e.into()))?;
    std::fs::write(out_dir.join("authority-map.json"), &map_text)
        .map_err(|e| AuthorityError::ArtifactIo(e.into()))?;
    std::fs::write(out_dir.join("authority-report.md"), &report_text)
        .map_err(|e| AuthorityError::ArtifactIo(e.into()))?;

    let live: u32 = counts.values().sum();
    Ok(format!(
        "authority-graph: {} declared, {} edges, {live} live findings across {} classes -> {}/authority-report.md",
        scan.nodes.len(),
        scan.edges.len(),
        counts.len(),
        out_dir.display()
    ))
}

/// Regenerate `crates/xtask/authority-baseline.json` from the current tree's
/// finding counts — the ratchet floor, tightened after a genuine
/// improvement (a class count went down and stays down).
pub fn update_baseline(root: &Path) -> Result<String, AuthorityError> {
    let allowlist_path = root.join("crates/xtask/authority-allowlist.json");
    let baseline_path = root.join("crates/xtask/authority-baseline.json");
    let (scan, _problems) = run_scan(root, &allowlist_path).map_err(AuthorityError::RepoScan)?;
    let counts = counts_by_class(&scan.findings);
    let text = serde_json::to_string_pretty(&counts).unwrap_or_default() + "\n";
    std::fs::write(&baseline_path, &text).map_err(|e| AuthorityError::ArtifactIo(e.into()))?;
    Ok(format!("baseline written: {}", baseline_path.display()))
}

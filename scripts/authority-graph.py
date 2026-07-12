#!/usr/bin/env python3
# Copyright 2026, The KyzoDB Authors. MPL-2.0.
#
# authority-graph — the Type Authority Graph tool (#139).
#
# Detects PROGRAM-architecture drift: what authority types exist, what
# invariants they own, what conversions are legal, and what meaning-bearing
# primitives live outside the graph. Authority is declared NEAR CODE as
# doc-comment metadata; this script extracts it, cross-checks it against the
# expected node set (bootstrapped from #136 — code annotations are the
# authority once they exist), and audits the tree for the drift classes the
# graph forbids.
#
# Annotation grammar (a contiguous /// or //! doc block):
#
#   /// @authority <Name>                      required, unique
#   /// @layer <value|runtime-catalog|storage|engines|query|record>  required
#   /// @owns <one-line invariant>             required
#   /// @constructs <legal constructors, ' | ' separated>
#   /// @forbids <illegal constructors/escapes, ' | ' separated>
#   /// @converts <A -> B (context) | ...>     edges of the graph
#   /// @gate <proof gate>
#   /// @status <established #NNN | pending #NNN — note>
#
# Modes:
#   report   (default) print everything, exit 0
#   ratchet  fail if any finding class count exceeds scripts/authority-baseline.json
#   strict   fail on ANY finding not covered by the narrow allowlist
#
# Outputs (default --out authority/, COMMITTED — they are a pure function of
# the tree: no timestamps, no commit hashes, so regeneration is idempotent):
#   authority-map.json     nodes, edges, findings — machine-readable
#   authority-report.md    the human report
# --check refuses to write and instead fails if the committed copies are
# stale (the gate runs it, so the repo's report can never lie).
#
# Allowlist (scripts/authority-allowlist.json): narrow entries only — exact
# file + line substring + reason. A stale (unmatched) entry is itself flagged.
#
# Usage:
#   scripts/authority-graph                       # report mode
#   scripts/authority-graph --mode ratchet
#   scripts/authority-graph --mode strict
#   scripts/authority-graph --update-baseline     # rewrite the ratchet floor
#   scripts/authority-graph --self-test           # planted-violation proof

import argparse
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

TOOL_VERSION = 1
LAYERS = {
    # layer -> path prefixes (relative to the scan root) that may own it
    "value": ("crates/kyzo-core/src/data/",),
    "runtime-catalog": ("crates/kyzo-core/src/runtime/",),
    "storage": ("crates/kyzo-core/src/storage/",),
    "engines": ("crates/kyzo-core/src/engines/",),
    "query": ("crates/kyzo-core/src/query/",),
    "record": ("crates/kyzo-core/src/data/record",),
}
LAYER_SPINE = [
    ("value", "Tuple · Domain · ExecRows/ExecDedup · EncodedKey"),
    ("runtime-catalog", "CatalogGeneration · RelationGeneration · IndexGeneration"),
    ("storage", "encoded-key / order integrity (consumes EncodedKey)"),
    ("engines", "ResidentIndexKey (rebuildable projections)"),
    ("query", "QueryDomainAdmission (admission into execution)"),
    ("record", "RecordId / KyzoRecord identity"),
]
# Bootstrap node set from #136. Code annotations are authoritative the moment
# they exist; this registry only says which nodes MUST eventually exist and
# which story creates each. `conditional` nodes are planned, not red.
EXPECTED = {
    "Tuple": {"layer": "value", "story": 126},
    "Domain": {"layer": "value", "story": 119},
    "ExecRows": {"layer": "value", "story": 119},
    "ExecDedup": {"layer": "value", "story": 119},
    "EncodedKey": {"layer": "value", "story": 119},
    "CatalogGeneration": {"layer": "runtime-catalog", "story": 135},
    "RelationGeneration": {"layer": "runtime-catalog", "story": 135},
    "IndexGeneration": {"layer": "runtime-catalog", "story": 135},
    "ResidentIndexKey": {"layer": "engines", "story": 122},
    "QueryDomainAdmission": {"layer": "query", "story": 122},
    "RecordId": {"layer": "record", "story": 128, "conditional": True},
}
REQUIRED_KEYS = ("authority", "layer", "owns")
DOC_KEYS = ("authority", "layer", "owns", "constructs", "forbids", "converts",
            "gate", "status")
# Paths whose whole PURPOSE is raw-byte / parse boundary work: the blob and
# string-taxonomy heuristics do not apply there (rule 03's boundary carve-out).
PARSE_BOUNDARY = ("crates/kyzo-core/src/parse/",)
BYTE_PLANE = ("crates/kyzo-core/src/data/value/", "crates/kyzo-core/src/storage/",
              "crates/kyzo-core/src/data/bitemporal.rs")
SENSITIVE = ("crates/kyzo-core/src/query/", "crates/kyzo-core/src/engines/",
             "crates/kyzo-core/src/runtime/", "crates/kyzo-core/src/storage/")

UNCHECKED_RE = re.compile(
    r"\bfn\s+(from_raw\w*|\w*new_unchecked\w*|from_bytes_unchecked\w*|forge\w*)\s*[(<]")
TUPLE_ALIAS_RE = re.compile(r"\btype\s+Tuple\s*=\s*Vec<DataValue>")
GEN_FIELD_RE = re.compile(
    r"^\s*(?:pub(?:\(\w+\))?\s+)?\w*generation\w*\s*:\s*"
    r"(?:u8|u16|u32|u64|u128|usize|i32|i64|AtomicU(?:32|64))\b")
GEN_STRUCT_RE = re.compile(r"\bstruct\s+(\w+Generation)\b")
RAW_ID_RE = re.compile(r"\bfn\s+\w+\s*[(<][^)]*?\b([a-z]\w*_id)\s*:\s*(?:u8|u16|u32|u64|usize)\b")
# Fields and multi-line-signature params on their own line ("relation_id: u64,").
RAW_ID_LINE_RE = re.compile(
    r"^\s*(?:pub(?:\(\w+\))?\s+)?([a-z]\w*_id)\s*:\s*(?:u8|u16|u32|u64|usize)\s*,?\s*$")
STRING_TAX_RE = re.compile(
    r"^\s*(?:pub(?:\(\w+\))?\s+)?(?:kind|format|variant|taxonomy|type_name)\s*:\s*(?:String|&\s*'?\w*\s*str)\b")
BLOB_FIELD_RE = re.compile(r"^\s*(?:pub(?:\(\w+\))?\s+)?\w+\s*:\s*Vec<u8>\s*,?\s*$")
CONVERT_EDGE_RE = re.compile(r"(\w+)\s*->\s*(\w+)\s*(?:\((.*?)\))?\s*$")
STATUS_RE = re.compile(r"^(established|pending)\s+#(\d+)(?:\s*[—-]\s*(.*))?$")


def strip_line_comment(line):
    """Drop a // comment from a line, quote-aware (heuristic scanner)."""
    out, i, n, in_str, in_char = [], 0, len(line), False, False
    while i < n:
        c = line[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif not in_str and c == "/" and i + 1 < n and line[i + 1] == "/":
            break
        out.append(c)
        i += 1
    return "".join(out)


class Finding:
    def __init__(self, cls, path, lineno, text, note=""):
        self.cls = cls
        self.path = path
        self.lineno = lineno
        self.text = text.strip()
        self.note = note
        self.allowlisted_by = None

    def as_dict(self):
        d = {"class": self.cls, "file": self.path, "line": self.lineno,
             "excerpt": self.text}
        if self.note:
            d["note"] = self.note
        if self.allowlisted_by is not None:
            d["allowlisted"] = self.allowlisted_by
        return d


def parse_declarations(path, raw_lines, findings):
    """Extract @authority doc blocks from one file."""
    nodes = []
    block, block_start = None, 0
    for idx, line in enumerate(raw_lines, start=1):
        stripped = line.strip()
        m = re.match(r"^(?:///|//!)\s?(.*)$", stripped)
        if m:
            body = m.group(1)
            km = re.match(r"^@(\w[\w-]*)\s+(.*)$", body)
            if km and km.group(1) == "authority":
                if block is not None:
                    nodes.append((block, block_start))
                block = {"authority": km.group(2).strip()}
                block_start = idx
            elif km and block is not None:
                key = km.group(1)
                if key in DOC_KEYS:
                    block[key] = km.group(2).strip()
                else:
                    findings.append(Finding(
                        "malformed-declaration", path, idx, stripped,
                        f"unknown annotation key @{key}"))
            continue
        if block is not None:
            nodes.append((block, block_start))
            block = None
    if block is not None:
        nodes.append((block, block_start))
    return nodes


def check_declarations(decls, findings):
    """Validate blocks; return name -> node dict for well-formed ones."""
    nodes = {}
    for (block, lineno), path in decls:
        name = block.get("authority", "")
        missing = [k for k in REQUIRED_KEYS if not block.get(k)]
        if missing:
            findings.append(Finding(
                "malformed-declaration", path, lineno,
                f"@authority {name}", f"missing required @{', @'.join(missing)}"))
            continue
        layer = block["layer"]
        if layer not in LAYERS:
            findings.append(Finding(
                "malformed-declaration", path, lineno, f"@authority {name}",
                f"unknown layer '{layer}' (want one of {sorted(LAYERS)})"))
            continue
        if not any(path.startswith(p) for p in LAYERS[layer]):
            findings.append(Finding(
                "layer-mismatch", path, lineno, f"@authority {name}",
                f"declared layer '{layer}' does not own path {path}"))
        if name in nodes:
            findings.append(Finding(
                "duplicate-authority", path, lineno, f"@authority {name}",
                f"already declared at {nodes[name]['file']}:{nodes[name]['line']}"))
            continue
        exp = EXPECTED.get(name)
        if exp and exp["layer"] != layer:
            findings.append(Finding(
                "layer-mismatch", path, lineno, f"@authority {name}",
                f"declared layer '{layer}' but the graph places it in "
                f"'{exp['layer']}'"))
        status = block.get("status", "")
        sm = STATUS_RE.match(status) if status else None
        node = {
            "name": name, "layer": layer, "file": path, "line": lineno,
            "owns": block["owns"],
            "constructs": [s.strip() for s in block.get("constructs", "").split("|") if s.strip()],
            "forbids": [s.strip() for s in block.get("forbids", "").split("|") if s.strip()],
            "converts": [s.strip() for s in block.get("converts", "").split("|") if s.strip()],
            "gate": block.get("gate", ""),
            "status": {"kind": sm.group(1), "story": int(sm.group(2)),
                       "note": (sm.group(3) or "").strip()} if sm
                      else {"kind": "unspecified", "story": None, "note": status},
        }
        nodes[name] = node
    return nodes


def build_edges(nodes, findings):
    edges = []
    for node in nodes.values():
        for conv in node["converts"]:
            m = CONVERT_EDGE_RE.match(conv)
            if not m:
                findings.append(Finding(
                    "malformed-declaration", node["file"], node["line"],
                    f"@converts {conv}",
                    "conversion must read 'A -> B (context)'"))
                continue
            edges.append({"from": m.group(1), "to": m.group(2),
                          "context": (m.group(3) or "").strip(),
                          "declared_by": node["name"]})
    return edges


def scan_code(path, raw_lines, nodes, findings):
    """Run the drift checks over one file's code (comments stripped)."""
    in_parse = any(path.startswith(p) for p in PARSE_BOUNDARY)
    in_byte_plane = any(path.startswith(p) for p in BYTE_PLANE)
    in_sensitive = any(path.startswith(p) for p in SENSITIVE)
    is_search_seam = path.endswith("query/ra/search.rs")
    authority_names = set(nodes)
    for idx, raw in enumerate(raw_lines, start=1):
        s = raw.strip()
        if s.startswith(("//", "///", "//!", "#[")) or not s:
            continue
        code = strip_line_comment(raw)
        cs = code.strip()
        if not cs:
            continue

        m = TUPLE_ALIAS_RE.search(cs)
        if m:
            if path == "crates/kyzo-core/src/data/value/mod.rs":
                findings.append(Finding(
                    "tuple-vec-alias", path, idx, cs,
                    "row authority is a bare Vec<DataValue> alias (newtype owed by #126)"))
            else:
                findings.append(Finding(
                    "duplicate-authority-alias", path, idx, cs,
                    "story-local redefinition of the Tuple row authority"))

        if is_search_seam and "Vec<Tuple>" in cs:
            findings.append(Finding(
                "searchra-decoded-tuples", path, idx, cs,
                "engine hits flow as decoded tuples, not admitted codes "
                "(QueryDomainAdmission owed by #122)"))

        m = UNCHECKED_RE.search(cs)
        if m:
            findings.append(Finding(
                "unchecked-constructor", path, idx, cs,
                f"raw-door constructor '{m.group(1)}'"))

        m = re.search(r"\bimpl(?:<[^>]*>)?\s+(?:std::ops::)?(?:(Deref|DerefMut)|From<(.+?)>)\s+for\s+(\w+)", cs)
        if m and m.group(3) in authority_names:
            if m.group(1):
                findings.append(Finding(
                    "illegal-escape", path, idx, cs,
                    f"{m.group(1)} dissolves the {m.group(3)} authority boundary"))
            else:
                src = m.group(2).strip()
                if re.fullmatch(r"Vec<DataValue>|Vec<u8>|Vec<u32>|&\[u8\]|&\[u32\]|u8|u16|u32|u64|usize|i32|i64|String|&str", src):
                    findings.append(Finding(
                        "illegal-escape", path, idx, cs,
                        f"blanket From<{src}> forges the {m.group(3)} authority"))

        m = re.search(r"\btype\s+(\w+)\s*=", cs)
        if m and m.group(1) in authority_names and m.group(1) != "Tuple" \
                and nodes[m.group(1)]["file"] != path:
            findings.append(Finding(
                "duplicate-authority-alias", path, idx, cs,
                f"story-local redefinition of the {m.group(1)} authority"))

        if GEN_FIELD_RE.match(code):
            findings.append(Finding(
                "duplicate-generation-counter", path, idx, cs,
                "raw-integer generation counter (catalog generations are the "
                "one validity authority, #135)"))
        m = GEN_STRUCT_RE.search(cs)
        if m and not any(path.startswith(p) for p in LAYERS["runtime-catalog"]):
            findings.append(Finding(
                "duplicate-generation-counter", path, idx, cs,
                f"{m.group(1)} declared outside the runtime catalog seam"))

        if in_sensitive:
            m = RAW_ID_RE.search(cs) or RAW_ID_LINE_RE.match(code)
            if m:
                findings.append(Finding(
                    "raw-id-crossing", path, idx, cs,
                    f"bare-integer identity '{m.group(1)}' crossing an "
                    "authority-sensitive boundary (newtype it)"))

        if in_sensitive and not in_parse and STRING_TAX_RE.match(code):
            findings.append(Finding(
                "string-taxonomy", path, idx, cs,
                "string-typed kind/format field where an enum belongs (rule 03)"))

        if in_sensitive and not in_parse and not in_byte_plane \
                and BLOB_FIELD_RE.match(code):
            findings.append(Finding(
                "blob-meaning", path, idx, cs,
                "generic byte blob carrying meaning outside the byte plane"))


def check_missing(nodes, findings):
    planned = []
    for name in sorted(EXPECTED):
        exp = EXPECTED[name]
        if name in nodes:
            continue
        if exp.get("conditional"):
            planned.append({"name": name, "layer": exp["layer"],
                            "story": exp["story"]})
            continue
        findings.append(Finding(
            "missing-authority", "crates/kyzo-core/src", 0, name,
            f"expected authority (layer {exp['layer']}) does not exist yet — "
            f"owed by #{exp['story']}"))
    return planned


def load_allowlist(path, problems):
    if not path.exists():
        return []
    entries = json.loads(path.read_text())
    ok = []
    for e in entries:
        if not all(isinstance(e.get(k), str) and e.get(k)
                   for k in ("class", "file", "contains", "reason")):
            problems.append(f"allowlist entry rejected (must carry exact "
                            f"class/file/contains/reason): {e}")
            continue
        if len(e["contains"]) < 8 or any(ch in e["file"] for ch in "*?["):
            problems.append(f"allowlist entry rejected (too broad): {e}")
            continue
        e["hits"] = 0
        ok.append(e)
    return ok


def apply_allowlist(findings, allowlist):
    for f in findings:
        for e in allowlist:
            if f.cls == e["class"] and f.path.endswith(e["file"]) \
                    and e["contains"] in f.text:
                f.allowlisted_by = e["reason"]
                e["hits"] += 1
                break


def run_scan(root, allowlist_path, problems):
    """Scan crates/kyzo-core/src under root; return (nodes, edges, planned, findings)."""
    findings, decls = [], []
    src = root / "crates" / "kyzo-core" / "src"
    files = sorted(src.rglob("*.rs"))
    per_file = {}
    for fp in files:
        rel = fp.relative_to(root).as_posix()
        raw_lines = fp.read_text(encoding="utf-8").splitlines()
        per_file[rel] = raw_lines
        for block in parse_declarations(rel, raw_lines, findings):
            decls.append((block, rel))
    nodes = check_declarations(decls, findings)
    edges = build_edges(nodes, findings)
    for rel, raw_lines in per_file.items():
        scan_code(rel, raw_lines, nodes, findings)
    planned = check_missing(nodes, findings)
    allowlist = load_allowlist(allowlist_path, problems)
    apply_allowlist(findings, allowlist)
    for e in allowlist:
        if e["hits"] == 0:
            findings.append(Finding(
                "stale-allowlist", e["file"], 0, e["contains"],
                "allowlist entry matches nothing — delete it"))
    findings.sort(key=lambda f: (f.cls, f.path, f.lineno))
    return nodes, edges, planned, findings, len(files)


def counts_by_class(findings):
    counts = {}
    for f in findings:
        if f.allowlisted_by is None:
            counts[f.cls] = counts.get(f.cls, 0) + 1
    return dict(sorted(counts.items()))


def render_outputs(nodes, edges, planned, findings, nfiles):
    """The two artifacts as strings — a pure function of the scan, so the
    committed copies never drift for volatile reasons (no clock, no commit
    hash, no mode)."""
    counts = counts_by_class(findings)
    live = [f for f in findings if f.allowlisted_by is None]
    allowed = [f for f in findings if f.allowlisted_by is not None]
    map_obj = {
        "tool": "authority-graph", "tool_version": TOOL_VERSION,
        "scope": "crates/kyzo-core/src", "files_scanned": nfiles,
        "layers": [{"layer": l, "holds": h} for l, h in LAYER_SPINE],
        "nodes": [nodes[k] for k in sorted(nodes)],
        "edges": edges,
        "planned": planned,
        "findings": [f.as_dict() for f in findings],
        "counts_by_class": counts,
    }
    map_text = json.dumps(map_obj, indent=2) + "\n"

    L = []
    L.append("# Type Authority Graph — drift report")
    L.append("")
    L.append(f"Generated by `scripts/authority-graph` (v{TOOL_VERSION}) over "
             f"`crates/kyzo-core/src` ({nfiles} files). Regenerate with "
             f"`scripts/authority-graph`; the gate fails if this file is "
             f"stale (`--check`).")
    L.append("")
    L.append("## Layer spine")
    L.append("")
    L.append("| layer | holds |")
    L.append("|---|---|")
    for l, h in LAYER_SPINE:
        L.append(f"| `{l}` | {h} |")
    L.append("")
    L.append("## Declared authorities")
    L.append("")
    if nodes:
        L.append("| authority | layer | status | declared at | owns |")
        L.append("|---|---|---|---|---|")
        for k in sorted(nodes):
            n = nodes[k]
            st = n["status"]
            stxt = (f"{st['kind']} #{st['story']}" if st["story"]
                    else st["kind"])
            L.append(f"| `{n['name']}` | `{n['layer']}` | {stxt} | "
                     f"`{n['file']}:{n['line']}` | {n['owns']} |")
    else:
        L.append("(none declared)")
    L.append("")
    L.append("## Legal conversions (edges)")
    L.append("")
    if edges:
        for e in edges:
            ctx = f" — {e['context']}" if e["context"] else ""
            L.append(f"- `{e['from']}` → `{e['to']}`{ctx}")
    else:
        L.append("(none declared)")
    L.append("")
    if planned:
        L.append("## Planned (conditional) authorities")
        L.append("")
        for p in planned:
            L.append(f"- `{p['name']}` (`{p['layer']}`) — created by "
                     f"#{p['story']} only if it proceeds")
        L.append("")
    L.append(f"## Findings ({len(live)} live, {len(allowed)} allowlisted)")
    L.append("")
    if counts:
        L.append("| class | count |")
        L.append("|---|---|")
        for c, n in counts.items():
            L.append(f"| `{c}` | {n} |")
        L.append("")
        cur = None
        for f in live:
            if f.cls != cur:
                cur = f.cls
                L.append(f"### {cur}")
                L.append("")
            loc = f"`{f.path}:{f.lineno}`" if f.lineno else f"`{f.path}`"
            L.append(f"- {loc} — {f.note}")
            if f.lineno:
                L.append(f"  `{f.text}`")
        L.append("")
    else:
        L.append("No live findings. The tree matches the declared graph.")
        L.append("")
    if allowed:
        L.append("## Allowlisted (intentional boundaries)")
        L.append("")
        for f in allowed:
            L.append(f"- `{f.path}:{f.lineno}` `{f.cls}` — {f.allowlisted_by}")
        L.append("")
    return map_obj, map_text, "\n".join(L) + "\n"


def self_test():
    """Planted-violation proof: every drift class fires on a fixture tree,
    and a clean fixture stays clean. This is the story's hardest-obligation
    gate: a raw from_raw(Vec<u32>) ExecRows door, a plan-cache-local
    generation counter, and a decoded Vec<Tuple> SearchRA path MUST all be
    detected."""
    fixtures = {
        # well-formed declaration + clean code: contributes zero findings
        "crates/kyzo-core/src/data/value/exec.rs": """\
/// @authority ExecRows
/// @layer value
/// @owns admitted execution currency
/// @constructs ExecRows::admit
/// @forbids from_raw
/// @converts ExecRows -> EncodedKey (storage boundary)
/// @gate no raw-code door
/// @status established #119
pub struct ExecRows { codes: Vec<u32> }
impl ExecRows {
    // fn from_raw(codes: Vec<u32>) -> ExecRows {}   <- commented: must NOT fire
    pub fn from_raw(codes: Vec<u32>) -> ExecRows { ExecRows { codes } }
}
""",
        # the pre-#126 alias + an illegal escape hatch
        "crates/kyzo-core/src/data/value/mod.rs": """\
/// @authority EncodedKey
/// @layer value
/// @owns canonical storage identity
/// @status established #119
pub struct EncodedKey(Vec<u8>);
pub type Tuple = Vec<DataValue>;
impl Deref for EncodedKey { type Target = Vec<u8>; }
impl From<Vec<u8>> for EncodedKey { fn from(b: Vec<u8>) -> EncodedKey { EncodedKey(b) } }
""",
        # story-local duplicate alias + raw id crossing + blob field;
        # the ExecRows alias exercises the generalized declared-name check
        "crates/kyzo-core/src/query/local_types.rs": """\
type Tuple = Vec<DataValue>;
type ExecRows = Vec<u32>;
fn lookup(relation_id: u64) -> bool { relation_id == 0 }
struct Carrier {
    payload: Vec<u8>,
    owner_id: u32,
}
""",
        # decoded-tuple search seam
        "crates/kyzo-core/src/query/ra/search.rs": """\
fn search(row: &[DataValue]) -> Result<Vec<Tuple>> { todo!() }
""",
        # plan-cache-local generation counter + string taxonomy
        "crates/kyzo-core/src/query/plan_cache.rs": """\
struct PlanCache {
    generation: u64,
    kind: String,
}
struct PlanCacheGeneration(u64);
""",
        # well-formed declaration in the WRONG layer for its path
        "crates/kyzo-core/src/engines/hnsw.rs": """\
/// @authority ResidentIndexKey
/// @layer value
/// @owns residency cache identity
pub struct ResidentIndexKey;
""",
        # malformed declaration: missing @owns entirely
        "crates/kyzo-core/src/runtime/catalog.rs": """\
/// @authority CatalogGeneration
/// @layer runtime-catalog
/// @constructs the catalog authority
pub struct CatalogGeneration(u64);
""",
        # clean boundary file: no findings
        "crates/kyzo-core/src/parse/lexer.rs": """\
struct Token { kind: String }
fn eat(input: &str) -> Token { todo!() }
""",
    }
    expect_at_least = {
        "unchecked-constructor": 1,      # the from_raw(Vec<u32>) door
        "tuple-vec-alias": 1,
        "illegal-escape": 2,             # Deref + nested-generic From<Vec<u8>>
        "duplicate-authority-alias": 2,  # local type Tuple + local type ExecRows
        "raw-id-crossing": 2,            # fn-signature param + own-line field
        "blob-meaning": 1,
        "searchra-decoded-tuples": 1,
        "duplicate-generation-counter": 2,  # raw field + local *Generation struct
        "string-taxonomy": 1,            # plan_cache.rs only; lexer is boundary
        "malformed-declaration": 1,      # missing @owns
        # ExecRows, EncodedKey, ResidentIndexKey are declared above (the
        # CatalogGeneration block is malformed, so it stays missing); every
        # other non-conditional expected node fires.
        "missing-authority": len([n for n, e in EXPECTED.items()
                                  if not e.get("conditional")]) - 3,
    }
    with tempfile.TemporaryDirectory(prefix="authority-selftest-") as td:
        root = Path(td)
        for rel, content in fixtures.items():
            fp = root / rel
            fp.parent.mkdir(parents=True, exist_ok=True)
            fp.write_text(content)
        problems = []
        nodes, edges, planned, findings, _ = run_scan(
            root, root / "no-allowlist.json", problems)
        counts = counts_by_class(findings)
        failures = []
        for cls, want in expect_at_least.items():
            got = counts.get(cls, 0)
            if got < want:
                failures.append(f"expected >= {want} '{cls}', got {got}")
        # the commented-out from_raw must not double-fire
        if counts.get("unchecked-constructor", 0) != 1:
            failures.append("comment stripping failed: commented from_raw fired")
        # the parse-boundary lexer must not fire string-taxonomy
        if any(f.cls == "string-taxonomy" and "lexer" in f.path
               for f in findings):
            failures.append("parse boundary not honored: lexer flagged")
        # the layer mismatch on the engines declaration must fire
        if not any(f.cls == "layer-mismatch" for f in findings):
            failures.append("layer-mismatch did not fire")
        # edges parsed
        if not any(e["from"] == "ExecRows" and e["to"] == "EncodedKey"
                   for e in edges):
            failures.append("@converts edge not extracted")
        if failures:
            print("SELF-TEST FAILURE", file=sys.stderr)
            for f in failures:
                print(f"  - {f}", file=sys.stderr)
            return 1
        planted = ("from_raw(Vec<u32>) ExecRows door, plan-cache generation "
                   "counter, decoded Vec<Tuple> SearchRA path, Deref escape, "
                   "duplicate Tuple alias, raw id, string taxonomy, blob field")
        print(f"SELF-TEST OK — planted violations all detected ({planted}); "
              f"clean fixtures stayed clean; {sum(counts.values())} findings "
              f"across {len(counts)} classes")
        return 0


def main():
    ap = argparse.ArgumentParser(prog="authority-graph")
    ap.add_argument("--mode", choices=["report", "ratchet", "strict"],
                    default="report")
    ap.add_argument("--root", type=Path, default=None)
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--update-baseline", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--check", action="store_true",
                    help="fail if the committed outputs are stale; never write")
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        sys.exit(self_test())

    root = args.root
    if root is None:
        try:
            top = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                                 capture_output=True, text=True).stdout.strip()
        except OSError:
            top = ""
        root = Path(top or ".")
    out_dir = args.out or root / "authority"
    baseline_path = root / "scripts" / "authority-baseline.json"
    allowlist_path = root / "scripts" / "authority-allowlist.json"

    problems = []
    nodes, edges, planned, findings, nfiles = run_scan(
        root, allowlist_path, problems)
    for p in problems:
        print(f"WARNING: {p}", file=sys.stderr)
    map_obj, map_text, report_text = render_outputs(
        nodes, edges, planned, findings, nfiles)
    stale = False
    if args.check:
        for name, want in (("authority-map.json", map_text),
                           ("authority-report.md", report_text)):
            fp = out_dir / name
            have = fp.read_text() if fp.exists() else ""
            if have != want:
                print(f"STALE: {fp} does not match the tree — regenerate "
                      f"with scripts/authority-graph and commit it",
                      file=sys.stderr)
                stale = True
    else:
        out_dir.mkdir(parents=True, exist_ok=True)
        (out_dir / "authority-map.json").write_text(map_text)
        (out_dir / "authority-report.md").write_text(report_text)
    counts = map_obj["counts_by_class"]
    live = sum(counts.values())

    if not args.quiet:
        print(f"authority-graph: {len(nodes)} declared, {len(edges)} edges, "
              f"{live} live findings across {len(counts)} classes "
              f"-> {out_dir}/authority-report.md")

    if args.update_baseline:
        baseline_path.write_text(json.dumps(counts, indent=2) + "\n")
        print(f"baseline written: {baseline_path}")

    if args.mode == "ratchet":
        baseline = (json.loads(baseline_path.read_text())
                    if baseline_path.exists() else {})
        regressed, improved = [], []
        for cls in sorted(set(counts) | set(baseline)):
            now, floor = counts.get(cls, 0), baseline.get(cls, 0)
            if now > floor:
                regressed.append(f"{cls}: {floor} -> {now}")
            elif now < floor:
                improved.append(f"{cls}: {floor} -> {now}")
        for msg in improved:
            print(f"RATCHET IMPROVED {msg} (tighten: --update-baseline)")
        if regressed:
            for msg in regressed:
                print(f"RATCHET FAILURE {msg}", file=sys.stderr)
            sys.exit(1)
        print("ratchet: no drift class grew")
    elif args.mode == "strict":
        if live:
            print(f"STRICT FAILURE: {live} live findings "
                  f"({', '.join(f'{c}:{n}' for c, n in counts.items())})",
                  file=sys.stderr)
            sys.exit(1)
        print("strict: clean — the tree matches the declared graph")
    if stale:
        sys.exit(1)


if __name__ == "__main__":
    main()

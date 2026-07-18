#!/usr/bin/env python3
"""Build repo-root map.json from census L1 + deprecated-construct-map skill arrows."""

from __future__ import annotations

import json
import re
from collections import OrderedDict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# All map paths are repo-relative strings (never absolute).
CORE = Path("crates/kyzo-core")
SRC = CORE / "src"
SEAT_CRATES = {"kyzo-model", "kyzo-trials", "kyzo-oracle"}


def resolve_src(s: str) -> Path:
    s = s.replace(" core", "").strip()
    if s.startswith("crates/"):
        return Path(s)
    if s.startswith(("tests/", "benches/", "examples/")):
        return CORE / s
    return SRC / s


def host_prefix(source: Path) -> Path | None:
    if source.parts[:2] == ("crates", "kyzo-bin"):
        return Path("crates/kyzo-bin/src")
    if source.parts[:2] == ("crates", "kyzo-crashfs"):
        return Path("crates/kyzo-crashfs/src")
    if source.parts[:2] == ("crates", "kyzo-lsp"):
        return Path("crates/kyzo-lsp/src")
    return None


def seat_src(crate: str, rest: str = "") -> str:
    base = Path("crates") / crate / "src"
    if not rest:
        return str(base)
    r = Path(rest)
    if r.parts and r.parts[0] == "src":
        r = Path(*r.parts[1:])
    return str(base / r) if r.parts else str(base)


def norm_dest(token: str, source: Path, relative_base: str | None = None) -> str | None:
    t = token.strip().rstrip("/")
    if not t or t == "-":
        return None
    if t.startswith("::") or " " in t:
        return None

    host = host_prefix(source)

    # Bare crate name → seat crate src/
    if t in SEAT_CRATES:
        return seat_src(t)

    # crates/kyzo-model/... (with or without src/)
    if t.startswith("crates/"):
        p = Path(t)
        if len(p.parts) >= 2 and p.parts[1] in SEAT_CRATES:
            rest = Path(*p.parts[2:]) if len(p.parts) > 2 else Path()
            return seat_src(p.parts[1], str(rest) if rest.parts else "")
        return str(p)

    # kyzo-model/foo or kyzo-model alone already handled
    for crate in SEAT_CRATES:
        if t == crate:
            return seat_src(crate)
        if t.startswith(crate + "/"):
            return seat_src(crate, t[len(crate) + 1 :])

    # Bare filename under last directory base from L1 (e.g. relation.rs under model/schema/)
    if relative_base and "/" not in t and any(
        t.endswith(ext) for ext in (".rs", ".pest", ".md", ".hex")
    ):
        return norm_dest(str(Path(relative_base) / t), source, None)

    if t.startswith(("tests/", "benches/", "examples/")):
        return str(CORE / t)

    if host is not None and not t.startswith("crates/"):
        return str(host / t)

    # Relative to kyzo-core/src
    return str(SRC / t)


def is_pathish(token: str) -> bool:
    t = token.strip()
    if not t or t.startswith("::") or " " in t:
        return False
    if t in SEAT_CRATES or t.startswith(tuple(c + "/" for c in SEAT_CRATES)):
        return True
    if t.startswith("crates/"):
        return True
    if any(t.endswith(ext) for ext in (".rs", ".pest", ".md", ".hex")):
        return True
    if "/" in t:
        return True
    return False


def extract_arrow_parts(l1: str, source: Path) -> list[dict]:
    """Split L1 on → `dest` and resolve relative bare files against last dir path."""
    # Also catch "relocates to `path`" without arrow
    text = l1
    text = re.sub(r"\brelocates to\s+`", "→ `", text)

    segs = re.split(r"→\s*`([^`]+)`", text)
    if len(segs) < 3:
        return []

    relative_base: str | None = None
    parts: list[dict] = []
    for i in range(1, len(segs), 2):
        dest_tok = segs[i].strip()
        prose = segs[i - 1].strip()

        # Update relative base when we see a directory path
        if dest_tok.endswith("/") or (
            "/" in dest_tok
            and not any(dest_tok.endswith(ext) for ext in (".rs", ".pest", ".md", ".hex"))
            and dest_tok not in SEAT_CRATES
        ):
            relative_base = dest_tok.rstrip("/")
            # Directory-only landing: place source basename under it unless more arrows follow
            # Still emit a part so fan-in is visible; later merge may refine.
            dest = norm_dest(dest_tok, source, None)
            if dest and not dest.endswith((".rs", ".pest", ".md", ".hex")):
                dest = str(Path(dest) / source.name)
            constructs = extract_construct_hints(prose)
            parts.append(
                {
                    "constructs": constructs or ["*SEE_L1*"],
                    "dest": dest,
                    "mode": "append",
                    "prose": prose[:240],
                    "_dir_landing": True,
                }
            )
            continue

        dest = norm_dest(dest_tok, source, relative_base)
        if dest is None:
            continue
        if not dest.endswith((".rs", ".pest", ".md", ".hex")):
            dest = str(Path(dest) / source.name)

        constructs = extract_construct_hints(prose)
        # "beside `parse/search.rs`" when arrow lands on a seat-crate root
        look = prose
        if i + 1 < len(segs):
            look = prose + " " + segs[i + 1]
        beside = re.search(r"beside\s+`([^`]+)`", look)
        if beside and (
            dest.endswith("/src")
            or dest.endswith("/src/" + source.name)
            or Path(dest).name in SEAT_CRATES
        ):
            sibling = beside.group(1)
            crate = next(
                (c for c in SEAT_CRATES if dest.startswith(f"crates/{c}/")),
                "kyzo-model",
            )
            dest = seat_src(crate, sibling)

        parts.append(
            {
                "constructs": constructs or ["*SEE_L1*"],
                "dest": dest,
                "mode": "append",
                "prose": prose[:240],
            }
        )

        # If dest was under a directory, that directory is the relative base
        if "/" in dest_tok:
            parent = str(Path(dest_tok).parent)
            if parent != ".":
                relative_base = parent

    # Drop pure directory-landing parts when a later file part shares that directory
    file_dirs = {
        str(Path(p["dest"]).parent)
        for p in parts
        if not p.get("_dir_landing") and p.get("dest")
    }
    cleaned = []
    for p in parts:
        p.pop("_dir_landing", None)
        if p.get("dest") and str(Path(p["dest"]).parent) in file_dirs and p["dest"].endswith(
            "/" + source.name
        ):
            # likely the dir-landing duplicate of later relation.rs/column.rs
            # keep only if no other part lands under same parent with different name
            siblings = [
                q
                for q in parts
                if q is not p
                and q.get("dest")
                and str(Path(q["dest"]).parent) == str(Path(p["dest"]).parent)
            ]
            if siblings:
                continue
        cleaned.append(p)
    return cleaned


def extract_construct_hints(text: str) -> list[str]:
    names = []
    for m in re.finditer(r"`([A-Z][A-Za-z0-9_]+)`", text):
        n = m.group(1)
        if n not in names:
            names.append(n)
    return names


def is_scatter_l1(l1: str) -> bool:
    return bool(
        re.search(
            r"NOT a 1:1|NAMED SPLIT|two seats|three ways|four destinations|"
            r"two destinations|SPLIT,|splits? by |split as the map",
            l1,
            re.I,
        )
    )


def is_whole_l1(l1: str) -> bool:
    if is_scatter_l1(l1):
        return False
    if re.search(r"preserve-and-move whole|preserve-and-move each file whole", l1, re.I):
        return True
    dests = re.findall(r"→\s*`([^`]+)`", l1)
    if len(dests) <= 1 and re.search(r"preserve-and-move|refactor-and-move", l1, re.I):
        return True
    return False


def load_skill_rows(skill_text: str) -> list[tuple[str, str]]:
    section = None
    rows = []
    for line in skill_text.splitlines():
        if line.startswith("## "):
            section = line[3:].split()[0]
            continue
        m = re.match(r"- (.+?) -> (.+)$", line)
        if m and section == "migrated":
            rows.append((m.group(1).strip(), m.group(2).strip()))
    return rows


def load_census(mig_text: str) -> OrderedDict:
    parts = re.split(r"^## ", mig_text, flags=re.M)
    census: OrderedDict = OrderedDict()
    for part in parts[1:]:
        first, _, body = part.partition("\n")
        key = re.split(r"\s+\(", first.strip(), maxsplit=1)[0].strip()
        full = "## " + part
        l1m = re.search(r"- \*\*L1:\*\*(.+?)(?:\n- \*\*L2:\*\*|\n## |\Z)", full, re.S)
        census[key] = {
            "title": first.strip(),
            "l1": (l1m.group(1).strip() if l1m else ""),
        }
    return census


def find_census_key(path: str, census: OrderedDict) -> str | None:
    rel = path
    if path.startswith("crates/kyzo-core/src/"):
        rel = path[len("crates/kyzo-core/src/") :]
    elif path.startswith("crates/kyzo-core/"):
        rel = path[len("crates/kyzo-core/") :]
    best = None
    for key in census:
        k = key.replace(" core", "").rstrip("/")
        if rel == k or rel == key or rel.startswith(k + "/") or path.endswith("/" + k) or path.endswith(k):
            if best is None or len(k) > len(best[0]):
                best = (k, key)
        if "core" in key and key.startswith("data/value/") and rel.startswith("data/value/"):
            fname = Path(rel).name
            specific = f"data/value/{fname}"
            if specific not in census and rel.count("/") == 2:
                if best is None or len(k) > len(best[0]):
                    best = (k, key)
    return best[1] if best else None


def skill_dests_for(path: str, skill_rows: list[tuple[str, str]]) -> list[str]:
    source = Path(path)
    out = []
    for s, d in skill_rows:
        for ss in [x.strip().replace(" core", "") for x in s.split(",")]:
            sp = resolve_src(ss)
            if path == str(sp) or path.startswith(str(sp).rstrip("/") + "/"):
                for part in d.split(";"):
                    part = part.strip()
                    if part == "-":
                        continue
                    if part.endswith("/") or part.endswith("/*"):
                        base = norm_dest(part.rstrip("/*"), source, None)
                        if path.startswith(str(sp).rstrip("/") + "/"):
                            rel = str(Path(path).relative_to(sp))
                            out.append(str(Path(base) / rel))
                        else:
                            out.append(str(Path(base) / Path(path).name))
                    else:
                        nd = norm_dest(part, source, None)
                        if nd:
                            out.append(nd)
                return list(dict.fromkeys(out))
    return out


def merge_parts(parts: list[dict]) -> list[dict]:
    by_dest: OrderedDict = OrderedDict()
    for part in parts:
        d = part["dest"]
        if d not in by_dest:
            by_dest[d] = {
                "constructs": list(part["constructs"]),
                "dest": d,
                "mode": "append",
                "prose": part.get("prose", ""),
            }
        else:
            for c in part["constructs"]:
                if c not in by_dest[d]["constructs"]:
                    by_dest[d]["constructs"].append(c)
    return list(by_dest.values())


# Hand-ruled corrections where L1 prose alone is ambiguous for the machine.
OVERRIDES: dict[str, dict] = {
    "crates/kyzo-core/src/data/relation.rs": {
        "kind": "scatter",
        "parts": [
            {
                "constructs": ["StoredRelationMetadata"],
                "dest": "crates/kyzo-core/src/model/schema/relation.rs",
                "mode": "append",
            },
            {
                "constructs": [
                    "NullableColType",
                    "ColType",
                    "ColumnDef",
                    "VecElementType",
                    "coerce",
                ],
                "dest": "crates/kyzo-core/src/model/schema/column.rs",
                "mode": "append",
            },
        ],
        "authority": "docs/deprecated/deprecated-migrated.md#data/relation.rs",
    },
    "crates/kyzo-core/src/engines/text/ast.rs": {
        "kind": "scatter",
        "parts": [
            {
                "constructs": ["FtsExpr", "FtsLiteral", "FtsNear", "flatten", "is_empty"],
                "dest": "crates/kyzo-model/src/parse/search.rs",
                "mode": "append",
            },
            {
                "constructs": ["tokenize"],
                "dest": "crates/kyzo-core/src/project/text/ast.rs",
                "mode": "append",
            },
        ],
        "authority": "docs/deprecated/deprecated-migrated.md#engines/text/ast.rs",
    },
    "crates/kyzo-core/src/runtime/relation.rs": {
        "kind": "scatter",
        "parts": [
            {
                "constructs": ["SystemKey", "RelationHandle"],
                "dest": "crates/kyzo-core/src/session/catalog.rs",
                "mode": "append",
            },
            {
                "constructs": ["AccessLevel", "InsufficientAccessLevel"],
                "dest": "crates/kyzo-core/src/session/access.rs",
                "mode": "append",
            },
            {
                "constructs": ["IndexPositionUse"],
                "dest": "crates/kyzo-core/src/exec/plan/compile.rs",
                "mode": "append",
            },
        ],
        "authority": "docs/deprecated/deprecated-migrated.md#runtime/relation.rs",
    },
}


def main() -> None:
    mig_text = (ROOT / "docs/deprecated/deprecated-migrated.md").read_text()
    skill_text = (ROOT / ".claude/skills/deprecated-construct-map/SKILL.md").read_text()
    cut_text = (ROOT / "THE-CUT.md").read_text()

    skill_rows = load_skill_rows(skill_text)
    census = load_census(mig_text)
    remain = [
        p
        for p in re.findall(r"^- `([^`]+)`", cut_text, re.M)
        if (ROOT / p).exists()
    ]

    remain_mig: list[str] = []
    for p in remain:
        for s, _d in skill_rows:
            for ss in [x.strip().replace(" core", "") for x in s.split(",")]:
                sp = resolve_src(ss)
                if p == str(sp) or p.startswith(str(sp).rstrip("/") + "/"):
                    remain_mig.append(p)
                    break
            else:
                continue
            break
    remain_mig = list(dict.fromkeys(remain_mig))

    entries = []
    skill_only = []

    for path in remain_mig:
        source = Path(path)
        if path in OVERRIDES:
            e = {"source": path, "mode": "append", **OVERRIDES[path]}
            ckey = find_census_key(path, census)
            if ckey:
                e["l1"] = census[ckey]["l1"][:800]
                e["census_key"] = ckey
            entries.append(e)
            continue

        ckey = find_census_key(path, census)
        skill_ds = skill_dests_for(path, skill_rows)

        if ckey is None:
            skill_only.append(path)
            if len(skill_ds) == 1:
                entries.append(
                    {
                        "source": path,
                        "kind": "whole",
                        "dest": skill_ds[0],
                        "mode": "append",
                        "constructs": ["*"],
                        "authority": ".claude/skills/deprecated-construct-map/SKILL.md",
                    }
                )
            else:
                entries.append(
                    {
                        "source": path,
                        "kind": "scatter",
                        "mode": "append",
                        "parts": [
                            {"constructs": ["*SEE_L1*"], "dest": d, "mode": "append"}
                            for d in skill_ds
                        ],
                        "authority": ".claude/skills/deprecated-construct-map/SKILL.md",
                        "needs_construct_partition": True,
                    }
                )
            continue

        l1 = census[ckey]["l1"]
        parts = extract_arrow_parts(l1, source)

        if is_whole_l1(l1) or (len(parts) <= 1 and not is_scatter_l1(l1)):
            if parts:
                dest = parts[0]["dest"]
            elif skill_ds:
                dest = skill_ds[0]
            else:
                dest = None
            if dest and not dest.endswith((".rs", ".pest", ".md", ".hex")):
                dest = str(Path(dest) / source.name)
            entries.append(
                {
                    "source": path,
                    "kind": "whole",
                    "dest": dest,
                    "mode": "append",
                    "constructs": ["*"],
                    "authority": f"docs/deprecated/deprecated-migrated.md#{ckey}",
                    "l1": l1[:500],
                    "census_key": ckey,
                }
            )
            continue

        if not parts:
            # scatter flagged but no arrows parsed — fall back to skill dests
            parts = [
                {"constructs": ["*SEE_L1*"], "dest": d, "mode": "append", "prose": ""}
                for d in skill_ds
            ]

        parts = merge_parts(parts)
        if len(parts) == 1 and not is_scatter_l1(l1):
            entries.append(
                {
                    "source": path,
                    "kind": "whole",
                    "dest": parts[0]["dest"],
                    "mode": "append",
                    "constructs": ["*"],
                    "authority": f"docs/deprecated/deprecated-migrated.md#{ckey}",
                    "l1": l1[:500],
                    "census_key": ckey,
                }
            )
        else:
            entries.append(
                {
                    "source": path,
                    "kind": "scatter",
                    "mode": "append",
                    "parts": parts,
                    "authority": f"docs/deprecated/deprecated-migrated.md#{ckey}",
                    "l1": l1[:800],
                    "census_key": ckey,
                }
            )

    merge_index: dict[str, list[str]] = {}
    for e in entries:
        if e["kind"] == "whole":
            merge_index.setdefault(e["dest"], []).append(e["source"])
        else:
            for part in e.get("parts", []):
                merge_index.setdefault(part["dest"], []).append(e["source"])

    doc = {
        "version": 1,
        "purpose": (
            "Executable construct/file seating for remaining migrated kill paths. "
            "Script appends only; never overwrite dest content."
        ),
        "mode_default": "append",
        "authority": [
            "docs/deprecated/deprecated-migrated.md",
            ".claude/skills/deprecated-construct-map/SKILL.md",
        ],
        "stats": {
            "remaining_migrated_sources": len(remain_mig),
            "entries": len(entries),
            "whole": sum(1 for e in entries if e["kind"] == "whole"),
            "scatter": sum(1 for e in entries if e["kind"] == "scatter"),
            "skill_only_no_census_prose": len(skill_only),
            "hand_overrides": len(OVERRIDES),
        },
        "entries": entries,
        "dest_fan_in": {d: srcs for d, srcs in merge_index.items() if len(srcs) > 1},
    }

    out = ROOT / "map.json"
    out.write_text(json.dumps(doc, indent=2) + "\n")
    print(json.dumps(doc["stats"], indent=2))
    print("wrote", out, "bytes", out.stat().st_size)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Collapse docs/deprecated/deprecated-*.md into docs/deprecated/map.json."""

from __future__ import annotations

import json
import re
import sys
from collections import OrderedDict
from pathlib import Path

try:
    import yaml
except ImportError:
    sys.exit("PyYAML required")

ROOT = Path(__file__).resolve().parents[1] / "docs" / "deprecated"
FATES = ("absorbed", "migrated", "retired", "sealed", "split")


def normalize_ws(s: str) -> str:
    return re.sub(r"\s+", " ", s).strip()


def split_sections(body: str) -> list[str]:
    matches = list(re.finditer(r"^## ", body, re.M))
    out = []
    for i, m in enumerate(matches):
        end = matches[i + 1].start() if i + 1 < len(matches) else len(body)
        out.append(body[m.start() : end].rstrip())
    return out


def parse_one_heading(part: str) -> dict:
    """Parse a single heading string (no leading ##), ending at closed)."""
    part_n = normalize_ws(part)

    # Paired files sharing one inventory:
    # "format.rs (1107 lines) + format/tests.rs (679 lines; both read whole; inventories: … — closed)"
    both_m = re.match(
        r"^(.+?)\s+\((\d+)\s+lines?\)\s*\+\s*(.+?)\s+\((\d+)\s+lines?;\s*"
        r"both read whole;\s*inventories:\s*(.+)\s*[—-]\s*closed\)\s*(?:and)?\s*$",
        part_n,
    )
    if both_m:
        s1, n1, s2, n2, inventory = (
            both_m.group(1).strip(),
            int(both_m.group(2)),
            both_m.group(3).strip(),
            int(both_m.group(4)),
            both_m.group(5).strip(),
        )
        return {
            "kind": "mapping",
            "key": f"{s1} + {s2}",
            "sources": [s1, s2],
            "lines": [n1, n2],
            "inventory": inventory,
        }

    # Directory / multi-file form (try before group — tokenizer has no "inventory:" label)
    if "each read whole" in part_n or "code files read whole" in part_n:
        dir_m = re.match(
            r"^(.+?)\s+\((.+?)\s*[—-]\s*"
            r"(?:each read whole|code files read whole.*?);\s*"
            r"(?:inventories:\s*)?(.+)\s*[—-]\s*closed\)\s*(?:and)?\s*$",
            part_n,
        )
        if not dir_m:
            raise ValueError(f"directory heading failed: {part_n[:220]!r}")
        key = dir_m.group(1).strip()
        file_bits = dir_m.group(2)
        inventory = dir_m.group(3).strip()
        base = key[: -len(" core")].rstrip() if key.endswith(" core") else key
        sources = []
        lines = []
        for fm in re.finditer(r"([\w./+-]+)\s+(\d+)", file_bits):
            fname = fm.group(1)
            if base.endswith("/"):
                sources.append(f"{base.rstrip('/')}/{fname}")
            elif "/" in base:
                sources.append(f"{base.rstrip('/')}/{fname}")
            else:
                sources.append(fname)
            lines.append(int(fm.group(2)))
        return {
            "kind": "mapping",
            "key": key,
            "sources": sources or [key],
            "lines": lines[0] if len(lines) == 1 else lines,
            "inventory": inventory,
        }

    # Standard: key (N lines; inventory: … — closed)
    if "inventory:" in part_n:
        if not re.search(r"closed\)\s*(?:and)?\s*$", part_n):
            raise ValueError(f"heading missing closed): {part_n[:180]!r}")
        inv_m = re.search(
            r";\s*inventory:\s*(.+?)\s*[—-]\s*closed\)\s*(?:and)?\s*$",
            part_n,
        )
        if not inv_m:
            raise ValueError(f"no inventory: clause: {part_n[:200]!r}")
        inventory = inv_m.group(1).strip()
        pre = part_n[: inv_m.start()].strip()
        if pre.endswith(";"):
            pre = pre[:-1].rstrip()

        sources = []
        line_counts = []
        for sm in re.finditer(r"([^\s+][^()]*?)\s*\((\d+)\s+lines?\)", pre):
            sources.append(sm.group(1).strip().lstrip("+ ").strip())
            line_counts.append(int(sm.group(2)))
        if not sources:
            km = re.match(r"^(.+?)\s*\((\d+)", pre)
            if not km:
                raise ValueError(f"no key/lines: {pre!r}")
            sources = [km.group(1).strip()]
            line_counts = [int(km.group(2))]

        key = " + ".join(sources) if len(sources) > 1 else sources[0]
        return {
            "kind": "mapping",
            "key": key,
            "sources": sources,
            "lines": line_counts[0] if len(line_counts) == 1 else line_counts,
            "inventory": inventory,
        }

    # Group intro: "path/ — title" with no inventory
    em = re.match(r"^(.+?)\s+[—-]\s+(.+)$", part_n)
    if em:
        return {
            "kind": "group",
            "key": em.group(1).strip(),
            "title_rest": em.group(2).strip(),
            "sources": [em.group(1).strip()],
        }
    raise ValueError(f"unparsed heading: {part_n[:200]!r}")


def parse_header_region(header: str) -> tuple[dict, str]:
    """
    Parse the region from ## … up to (but not including) - **L1:**.
    Returns (entry_fields, extra_notes_before_l1).
    """
    header = header.strip()
    # Peel optional non-L1 bullets after the last closed)
    extra = ""
    # Find last closed) — everything after may be notes
    closed_matches = list(re.finditer(r"closed\)", header))
    if closed_matches:
        last = closed_matches[-1]
        after = header[last.end() :].strip()
        # strip trailing "and"
        head_main = header[: last.end()]
        if after.startswith("and"):
            # paired next ## follows outside; after shouldn't include ##
            after = after[3:].strip()
        if after.startswith("- ") or after.startswith("\n- "):
            extra = after.lstrip("\n")
            header = head_main
        else:
            header = head_main + ((" " + after) if after else "")

    # Split paired headings
    parts = re.split(r"\n## ", header)
    parts[0] = re.sub(r"^##\s+", "", parts[0])

    parsed_parts = [parse_one_heading(p) for p in parts]
    if any(p["kind"] == "group" for p in parsed_parts):
        if len(parsed_parts) != 1:
            raise ValueError("group mixed with mapping headings")
        return parsed_parts[0], extra

    # Merge mapping parts (span + symb). Preserve each part's heading key.
    sources = []
    lines: list[int] = []
    inventories = []
    keys = []
    for p in parsed_parts:
        keys.append(p["key"])
        sources.extend(p["sources"])
        lc = p["lines"]
        if isinstance(lc, list):
            lines.extend(lc)
        else:
            lines.append(lc)
        inventories.append(p["inventory"])

    key = " + ".join(keys) if len(keys) > 1 else keys[0]
    entry = {
        "kind": "mapping",
        "key": key,
        "sources": sources,
        "lines": lines[0] if len(lines) == 1 else lines,
        "inventory": " | ".join(inventories) if len(inventories) > 1 else inventories[0],
    }
    if extra:
        entry["pre_l1_notes"] = extra.strip()
    return entry, extra


def merge_raw_sections(raw_sections: list[str]) -> list[str]:
    merged: list[str] = []
    i = 0
    while i < len(raw_sections):
        sec = raw_sections[i]
        if "\n- **L1:**" in sec:
            merged.append(sec)
            i += 1
            continue
        # Pair with next if this ends with closed) and
        nsec = normalize_ws(sec)
        if i + 1 < len(raw_sections) and (
            nsec.endswith("and") or re.search(r"closed\)\s*and\s*$", nsec)
        ):
            merged.append(sec.rstrip() + "\n" + raw_sections[i + 1])
            i += 2
            continue
        # group intro
        merged.append(sec)
        i += 1
    return merged


def parse_fate(fate: str) -> tuple[dict, list[str]]:
    path = ROOT / f"deprecated-{fate}.md"
    text = path.read_text()
    _, fm, body = text.split("---", 2)
    meta = yaml.safe_load(fm) or {}
    paths = meta.get("paths") or []

    title_m = re.search(r"^# (.+)$", body, re.M)
    title = title_m.group(1).strip() if title_m else fate
    doctrine = body[title_m.end() :].partition("\n## ")[0].strip() if title_m else ""

    raw_sections = split_sections(body)
    raw_headers = [
        normalize_ws(sec.split("\n", 1)[0]) for sec in raw_sections
    ]

    entries = []
    for sec in merge_raw_sections(raw_sections):
        if "\n- **L1:**" not in sec:
            lines = sec.split("\n", 1)
            head = re.sub(r"^##\s+", "", lines[0]).strip()
            prose = lines[1].strip() if len(lines) > 1 else ""
            em = re.match(r"^(.+?)\s+[—-]\s+(.+)$", normalize_ws(head))
            key = em.group(1).strip() if em else head
            title_rest = em.group(2).strip() if em else ""
            entries.append(
                {
                    "kind": "group",
                    "key": key,
                    "title_rest": title_rest,
                    "body": prose,
                    "sources": [key],
                }
            )
            continue

        head_end = sec.find("\n- **L1:**")
        header = sec[:head_end]
        try:
            parsed, _extra = parse_header_region(header)
        except ValueError as e:
            raise SystemExit(f"{fate}: {e}\nHEADER={normalize_ws(header)[:300]!r}") from e

        if parsed["kind"] == "group":
            raise SystemExit(f"{fate}: group heading unexpectedly has L1: {parsed['key']}")

        l1_m = re.search(r"- \*\*L1:\*\*\s*(.+?)(?=\n- \*\*L2:\*\*|\Z)", sec, re.S)
        l2_m = re.search(r"- \*\*L2:\*\*\s*(.+?)\Z", sec, re.S)
        if not l1_m or not l2_m:
            raise SystemExit(f"{fate}: missing L1/L2 for {parsed['key']}")

        entry = {
            "kind": "mapping",
            "key": parsed["key"],
            "sources": parsed["sources"],
            "lines": parsed["lines"],
            "inventory": parsed["inventory"],
            "l1": l1_m.group(1).strip(),
            "l2": l2_m.group(1).strip(),
        }
        if "pre_l1_notes" in parsed:
            entry["pre_l1_notes"] = parsed["pre_l1_notes"]
        entries.append(entry)

    return (
        {
            "title": title,
            "doctrine": doctrine,
            "paths": paths,
            "entries": entries,
        },
        raw_headers,
    )


def verify(doc: dict, raw_headers: dict[str, list[str]]) -> list[str]:
    errors: list[str] = []
    for fate in FATES:
        path = ROOT / f"deprecated-{fate}.md"
        text = path.read_text()
        body = text.split("---", 2)[2]
        fate_obj = doc["fates"][fate]
        body_n = normalize_ws(body)

        _, fm, _ = text.split("---", 2)
        meta = yaml.safe_load(fm) or {}
        if meta.get("paths") != fate_obj["paths"]:
            errors.append(f"{fate}: frontmatter paths mismatch")

        title_m = re.search(r"^# (.+)$", body, re.M)
        if not title_m or title_m.group(1).strip() != fate_obj["title"]:
            errors.append(f"{fate}: title mismatch")
        doctrine = body[title_m.end() :].partition("\n## ")[0].strip() if title_m else ""
        if doctrine != fate_obj["doctrine"]:
            errors.append(f"{fate}: doctrine mismatch")

        md_l1 = len(re.findall(r"^- \*\*L1:\*\*", body, re.M))
        json_l1 = sum(1 for e in fate_obj["entries"] if e["kind"] == "mapping")
        if md_l1 != json_l1:
            errors.append(f"{fate}: L1 count MD={md_l1} JSON={json_l1}")
        md_l2 = len(re.findall(r"^- \*\*L2:\*\*", body, re.M))
        if md_l2 != json_l1:
            errors.append(f"{fate}: L2 count MD={md_l2} JSON={json_l1}")

        # ## heading keys
        md_keys = []
        for h in raw_headers[fate]:
            h2 = re.sub(r"^##\s+", "", h)
            km = re.match(r"^(.+?)\s+\(", h2)
            if km:
                md_keys.append(km.group(1).strip())
            else:
                em = re.match(r"^(.+?)\s+[—-]\s+", h2)
                md_keys.append(em.group(1).strip() if em else h2)

        for k in md_keys:
            found = False
            for e in fate_obj["entries"]:
                if e["key"] == k or k in e.get("sources", []):
                    found = True
                    break
                if e["kind"] == "mapping" and (
                    k.rstrip("/") + "/" == e["key"]
                    or e["key"].rstrip("/") == k.rstrip("/")
                    or any(s.startswith(k.rstrip("/") + "/") for s in e.get("sources", []))
                ):
                    found = True
                    break
                if k in e.get("key", ""):
                    found = True
                    break
            if not found:
                errors.append(f"{fate}: MD heading key not in JSON: {k}")

        for e in fate_obj["entries"]:
            if e["kind"] == "group":
                if e["key"] not in body:
                    errors.append(f"{fate}: group key missing: {e['key']}")
                if e["body"] and normalize_ws(e["body"]) not in body_n:
                    # require a distinctive slice
                    slice_ = normalize_ws(e["body"])[:100]
                    if slice_ not in body_n:
                        errors.append(f"{fate}: group body missing for {e['key']}")
                continue
            for field in ("inventory", "l1", "l2"):
                frag = normalize_ws(e[field])
                parts = frag.split(" | ") if field == "inventory" else [frag]
                for part in parts:
                    if not part:
                        continue
                    if part in body_n:
                        continue
                    slice_ = part[:140]
                    if slice_ not in body_n:
                        errors.append(
                            f"{fate}: {e['key']} {field} missing (slice={slice_!r})"
                        )
            if e.get("pre_l1_notes"):
                if normalize_ws(e["pre_l1_notes"])[:100] not in body_n:
                    errors.append(f"{fate}: {e['key']} pre_l1_notes missing")

    return errors


def main() -> int:
    doc: dict = {
        "version": 1,
        "authority": "docs/deprecated/",
        "fates": OrderedDict(),
    }
    raw_headers: dict[str, list[str]] = {}
    for fate in FATES:
        fate_obj, headers = parse_fate(fate)
        doc["fates"][fate] = fate_obj
        raw_headers[fate] = headers
        print(
            f"{fate}: paths={len(fate_obj['paths'])} "
            f"entries={len(fate_obj['entries'])} "
            f"mappings={sum(1 for e in fate_obj['entries'] if e['kind']=='mapping')} "
            f"groups={sum(1 for e in fate_obj['entries'] if e['kind']=='group')}"
        )

    out = ROOT / "map.json"
    out.write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n")
    print("wrote", out, "bytes", out.stat().st_size)

    errors = verify(doc, raw_headers)
    if errors:
        print("VERIFY FAIL", len(errors))
        for e in errors[:50]:
            print(" ", e)
        if len(errors) > 50:
            print(f"  … +{len(errors)-50} more")
        return 1
    print("VERIFY OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

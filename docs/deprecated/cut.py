#!/usr/bin/env python3
"""Remove completed move H2 blocks from deprecated-migrated.md only."""
import re
from pathlib import Path

MIG = Path(__file__).with_name("deprecated-migrated.md")

DONE = [
    "lib.rs",
    "tests/common/mod.rs",
    "tests/unified_scenario.rs",
    "tests/relational_core.rs",
    "tests/recursion_and_negation.rs",
    "tests/aggregation.rs",
    "tests/data_types.rs",
    "tests/errors_and_refusals.rs",
    "tests/adversarial_robustness.rs",
    "tests/system_ops.rs",
    "tests/vector_and_fts.rs",
    "tests/time_travel.rs",
    "tests/standing_queries.rs",
    "tests/storage_allocation_law.rs",
    "tests/public_api_surface.rs",
    "benches/db_scan.rs",
    "benches/ra_exec.rs",
    "benches/storage.rs",
    "benches/string_eq.rs",
    "examples/language_tour.rs",
    "crates/kyzo-bin/src/main.rs",
    "crates/kyzo-bin/src/engine.rs",
    "crates/kyzo-bin/src/repl/mod.rs",
    "crates/kyzo-bin/src/repl/editor.rs",
    "crates/kyzo-bin/src/repl/commands.rs",
    "crates/kyzo-bin/src/server/auth.rs",
    "crates/kyzo-bin/src/server/query.rs",
    "crates/kyzo-bin/src/server/bulk.rs",
    "crates/kyzo-bin/src/server/rules.rs",
    "crates/kyzo-bin/tests/repl_smoke.rs",
    "crates/kyzo-crashfs/src/lib.rs",
    "crates/kyzo-crashfs/src/fault.rs",
    "crates/kyzo-crashfs/src/passthrough.rs",
    "crates/kyzo-crashfs/src/harness.rs",
    "crates/kyzo-crashfs/tests/standalone_mount.rs",
    "crates/kyzo-arrow-interop/src/lib.rs",
    "crates/kyzo-arrow-interop/tests/decode_kyzo_stream.rs",
]

text = MIG.read_text()
assert len(text) > 1000, "migrated.md looks empty — abort"

missing = [p for p in DONE if not re.search(rf"^## {re.escape(p)}(?:\s|\()", text, re.M)]
assert not missing, f"H2 not found for: {missing}"

for path in DONE:
    text, n = re.subn(
        rf"(?ms)^## {re.escape(path)}(?:\s|\().*?(?=^## |\Z)",
        "",
        text,
        count=1,
    )
    assert n == 1, f"failed to cut: {path}"

still = [p for p in DONE if re.search(rf"^## {re.escape(p)}(?:\s|\()", text, re.M)]
assert not still, f"still present: {still}"
assert len(text) > 1000, "cut wiped the file — abort"

MIG.write_text(text)
print(f"cut {len(DONE)} blocks from {MIG.name}; {text.count(chr(10))+1} lines remain")

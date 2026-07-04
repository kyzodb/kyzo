/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Generative fuzzing for the parse tier: the caller is a fuzzer with
//! intent, so we fuzz before the callers arrive.
//!
//! Three layers:
//!
//! 1. **A grammar-aware generator** ([`script_strategy`]) producing
//!    structurally-plausible KyzoScript — rules with heads and bodies,
//!    aggregations from the real registry ([`parse_aggr`]'s names), fixed
//!    rules from the real registry ([`DEFAULT_FIXED_RULES`]), options,
//!    sigils, literal edge values, validity specs, imperative blocks, sys
//!    ops, and shapes that approach the landed ceilings
//!    ([`NESTING_CEILING`], [`FTS_OPS_CEILING`]). Plausible-but-possibly-
//!    invalid text stresses far deeper paths than random bytes.
//! 2. **A mutation layer** ([`mutate`]) over generated scripts: truncate
//!    anywhere, splice two scripts, duplicate a slice, flip brackets,
//!    inject hostile byte payloads (NUL, RTL override, zalgo, multibyte,
//!    invalid UTF-8) at arbitrary byte offsets. Byte-level splices may
//!    produce invalid UTF-8; the result is fed through
//!    [`String::from_utf8_lossy`], because `&str` is the real API surface.
//! 3. **The laws** ([`check_script_laws`]): [`parse_script`] (empty params,
//!    the real default fixed-rule registry) must return `Ok` or a *spanned*
//!    error whose labels lie within the input, must never panic
//!    (`catch_unwind` is the harness backstop, and any caught panic fails
//!    the test naming the input), and must never abort (the nesting/ops
//!    ceilings must trip first — the generator deliberately includes shapes
//!    at and past them). It must also not blow up in time: a per-case
//!    wall-clock bound ([`CASE_TIME_BOUND`]) fails any case that *returns*
//!    too slowly. That bound is measured after the parse returns, so it
//!    catches terminating slowness, not true non-termination — a parse that
//!    never returns is bounded only by the harness-level external timeout,
//!    which kills the whole run rather than naming one input (the parser has
//!    no cooperative cancellation to interrupt a single case cleanly). See
//!    [`check_laws_with`].
//!
//! Spanned-ness is enforced *unconditionally*: the fix wave that spanned
//! FINDING-1..8 retired the findings ledger, so any label-less parser error
//! is now a hard failure (their fix sites are pinned by
//! [`former_findings_now_carry_spans`], their inputs replayed by
//! [`regression_corpus`]). A *new* span-less error fails the test: minimize
//! it, add it to the corpus, and — only to tolerate it briefly — key an
//! exception on its `Diagnostic::code()`, never a rendered-string substring.
//! Do not fix the parser from here.
//!
//! Tuning: the checked-in defaults keep the whole module inside a ~1–2 s
//! budget (see [`cases`]). For nightly/big runs escalate with
//! `PROPTEST_CASES=10000 cargo test -p kyzo parse::fuzz`.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use miette::Diagnostic;
use proptest::prelude::*;
use proptest::sample::Index;

use super::*;
use crate::fixed_rule::DEFAULT_FIXED_RULES;
use crate::parse::fts::parse_fts_query;

fn vld() -> ValidityTs {
    ValidityTs(Reverse(0))
}

/// A terminating case that *returns* slower than this is a blowup FINDING.
/// Measured after the parse returns (see [`check_laws_with`]), so it bounds
/// terminating slowness only; a non-terminating parse never reaches the
/// check and is caught instead by the harness-level external timeout, which
/// kills the run rather than naming the input. Generous because CI runs
/// debug builds and the corpus carries multi-hundred-KB entries.
const CASE_TIME_BOUND: Duration = Duration::from_secs(5);

/// The checked-in case count, overridable for nightly big runs with
/// `PROPTEST_CASES=10000 cargo test -p kyzo parse::fuzz`. (We read the
/// variable ourselves because a literal `cases:` in `proptest_config`
/// would otherwise shadow the standard knob.)
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ─────────────────────────────────────────────────────────────────────────
// The laws
// ─────────────────────────────────────────────────────────────────────────

/// Walk a diagnostic tree (labels, then `related` and `diagnostic_source`)
/// recording whether any label exists and whether any lies out of bounds.
fn walk_labels(diag: &dyn Diagnostic, len: usize, any: &mut bool, oob: &mut Option<String>) {
    if let Some(labels) = diag.labels() {
        for label in labels {
            *any = true;
            if label.offset() + label.len() > len {
                *oob = Some(format!(
                    "label at {}..{} exceeds input length {len}",
                    label.offset(),
                    label.offset() + label.len(),
                ));
            }
        }
    }
    if let Some(related) = diag.related() {
        for r in related {
            walk_labels(r, len, any, oob);
        }
    }
    if let Some(src) = diag.diagnostic_source() {
        walk_labels(src, len, any, oob);
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Assert the parse-tier laws for one input. `Err` carries a description
/// naming the violation and the input; any `Err` is a FINDING.
fn check_laws_with<F, T>(src: &str, parse: F) -> Result<(), String>
where
    F: FnOnce(&str) -> Result<T>,
{
    let started = Instant::now();
    let outcome = catch_unwind(AssertUnwindSafe(|| parse(src)));
    let elapsed = started.elapsed();
    let result = match outcome {
        Err(payload) => {
            return Err(format!(
                "LAW VIOLATION (panic): {:?} on input {src:?}",
                panic_message(payload)
            ));
        }
        Ok(r) => r,
    };
    if elapsed > CASE_TIME_BOUND {
        return Err(format!(
            "LAW VIOLATION (time): took {elapsed:?} (bound {CASE_TIME_BOUND:?}) on input {src:?}"
        ));
    }
    if let Err(report) = result {
        let mut any_label = false;
        let mut out_of_bounds = None;
        walk_labels(&*report, src.len(), &mut any_label, &mut out_of_bounds);
        if let Some(oob) = out_of_bounds {
            return Err(format!(
                "LAW VIOLATION (label out of bounds): {oob}; error {report:?} on input {src:?}"
            ));
        }
        if !any_label {
            // Spanned-ness is enforced unconditionally: the fix wave retired
            // the ledger of excused shapes, so every label-less error is a
            // violation. A future finding is recorded by adding its minimized
            // input to the corpus and, if it must be tolerated briefly, an
            // exception keyed on `Diagnostic::code()` (or a full-message
            // equality) — never a rendered-string *substring*, which silently
            // excuses off-target errors that merely share a fragment.
            return Err(format!(
                "LAW VIOLATION (span-less error): {report:?} on input {src:?}"
            ));
        }
        if let Some(generic) = matches_banned_generic_message(&report.to_string()) {
            // Designed-ness is enforced unconditionally too (story #73): the
            // diagnostics redesign retired every bare "unexpected token"-
            // style message in the parser, so a top-level error whose
            // `Display` still equals one of the retired placeholders is a
            // regression, not a new shape to excuse.
            return Err(format!(
                "LAW VIOLATION (undesigned message, matches retired placeholder {generic:?}): \
                 {report:?} on input {src:?}"
            ));
        }
    }
    Ok(())
}

/// The exact, non-parameterized placeholder messages the diagnostics
/// redesign (story #73) retired — every one of these used to be a
/// complete error message on its own, with no offending name or value
/// interpolated in, so an exact-equality check can catch a regression to
/// any of them without ever risking a false hit on a legitimately designed
/// message that merely shares a word (the parameterized retired messages —
/// `"Query option {0} is not constant"` and its kin — can't regress to
/// their old *exact* text at all, since the interpolated name makes every
/// rendering different from the fixed string that never existed on its
/// own; those are instead covered by the `#[help]`-presence and
/// SQL-refugee checks elsewhere in this module).
const BANNED_GENERIC_MESSAGES: &[&str] = &[
    "Invalid expression encountered",
    "Cannot parse integer",
    "Cannot parse float",
    "unexpected token",
    "unexpected input",
];

/// The retired `ParseError` message was `"The query parser has encountered
/// unexpected input / end of input at {span}"` — parameterized by the span,
/// so never an exact match, but the sentence *before* the span is fixed and
/// distinctive enough that no legitimate designed message would start with
/// it by coincidence.
const BANNED_MESSAGE_PREFIX: &str = "The query parser has encountered unexpected input";

/// `Some(the banned phrase)` if `message` exactly equals one of
/// [`BANNED_GENERIC_MESSAGES`] or starts with [`BANNED_MESSAGE_PREFIX`].
fn matches_banned_generic_message(message: &str) -> Option<&'static str> {
    if message.starts_with(BANNED_MESSAGE_PREFIX) {
        return Some(BANNED_MESSAGE_PREFIX);
    }
    BANNED_GENERIC_MESSAGES
        .iter()
        .find(|&&banned| message == banned)
        .copied()
}

/// The laws for `parse_script` with empty params and the real default
/// fixed-rule registry.
fn check_script_laws(src: &str) -> Result<(), String> {
    check_laws_with(src, |s| {
        parse_script(s, &BTreeMap::new(), &DEFAULT_FIXED_RULES, vld())
    })
}

/// The laws for the FTS query parser (its own entry point; `fts_booster`
/// and the flat-chain ops ceiling live here).
fn check_fts_laws(src: &str) -> Result<(), String> {
    check_laws_with(src, parse_fts_query)
}

// ─────────────────────────────────────────────────────────────────────────
// Layer 1: the grammar-aware generator
// ─────────────────────────────────────────────────────────────────────────

/// Aggregation names exactly as `data/aggr.rs::parse_aggr` accepts them,
/// plus one deliberate stranger.
const AGGR_NAMES: &[&str] = &[
    "and",
    "or",
    "unique",
    "group_count",
    "union",
    "intersection",
    "count",
    "count_unique",
    "variance",
    "std_dev",
    "sum",
    "product",
    "min",
    "max",
    "mean",
    "choice",
    "collect",
    "shortest",
    "min_cost",
    "bit_and",
    "bit_or",
    "bit_xor",
    "latest_by",
    "smallest_by",
    "choice_rand",
    "no_such_aggr",
];

const VAR_NAMES: &[&str] = &[
    "a", "b", "x", "y", "xs", "score", "变量", "élan", "_hidden", "a_b1", "Ω",
];

const IDENT_NAMES: &[&str] = &[
    "r",
    "rel",
    "t",
    "idx",
    "f",
    "g",
    "col",
    "データ",
    "aaaaaaaaaaaa",
];

const REL_NAMES: &[&str] = &["*rel", "*a.b", "*_tmp", "*stored.rel:idx", "*变量"];

/// Number literal edge values: every radix, separators, `i64` bounds and
/// past them (the hex-int overflow is a corpus regression), float edges.
const NUMBERS: &[&str] = &[
    "0",
    "1",
    "42",
    "9223372036854775807",
    "9223372036854775808",
    "123456789012345678901234567890",
    "0xdead_beef",
    "0x7fff_ffff_ffff_ffff",
    "0xffff_ffff_ffff_ffff_ffff",
    "0o777",
    "0b1_010",
    "1_000_000",
    "3.5",
    "0.0",
    "1.",
    "2.5e10",
    "1e-999",
    "1E308",
    "9007199254740993.0",
];

/// String literal edge values: escapes (including `\u`), raw strings with
/// sigils, embedded comment openers, RTL, zalgo, wide multibyte, a lone
/// UTF-16 surrogate escape, and an unterminated string.
const STRINGS: &[&str] = &[
    r#""hello""#,
    r#""a#b /* not a comment */""#,
    r#""tab\t nl\n quote\" uA""#,
    r#"'single \' quoted'"#,
    r#"___"raw " content"___"#,
    "\"\u{202E}right-to-left\u{202D}\"",
    "\"z\u{0334}\u{0322}a\u{0300}l\u{0336}g\u{0335}o\u{0338}\"",
    "\"日本語🦀\"",
    r#""\ud800""#,
    r#""unterminated"#,
];

const PARAMS: &[&str] = &["$p", "$q", "$_x", "$p.q", "$0", "$漢"];

/// Validity specs for `@`-clauses: symbolic, RFC 3339, numeric, garbage.
const VALIDITIES: &[&str] = &[
    "'NOW'",
    "'END'",
    "'2020-01-01T00:00:00Z'",
    "123",
    "9223372036854775807",
    "'garbage'",
    "$v",
];

const FUNCTION_NAMES: &[&str] = &[
    "length",
    "concat",
    "to_string",
    "abs",
    "coalesce",
    "if",
    "cond",
    "regex_matches",
    "not_a_function",
];

const BINARY_OPS: &[&str] = &[
    "+", "-", "*", "/", "%", "^", "==", "!=", ">", "<", ">=", "<=", "&&", "||", "++", "->", "~",
];

const COL_TYPES: &[&str] = &[
    "Int",
    "Float",
    "String",
    "Bytes",
    "Uuid",
    "Bool",
    "Json",
    "Validity",
    "Any",
    "Any?",
    "Int?",
    "[Int]",
    "[Float; 3]",
    "[Int; 0x10]",
    "(Int, Float,)",
    "()",
    "<F32; 3>",
    "<F64; 1024>",
    "<F32; 99999999999999999999999>",
];

fn select(pool: &'static [&'static str]) -> impl Strategy<Value = String> {
    proptest::sample::select(pool).prop_map(str::to_string)
}

/// An expression: literals at the leaves, then lists, objects, function
/// application, unary/binary operator chains and grouping, recursively.
fn expr_strategy() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        4 => select(NUMBERS),
        3 => select(STRINGS),
        1 => Just("true".to_string()),
        1 => Just("false".to_string()),
        1 => Just("null".to_string()),
        2 => select(VAR_NAMES),
        2 => select(PARAMS),
    ];
    leaf.prop_recursive(4, 24, 4, |inner| {
        prop_oneof![
            // list, sometimes with a trailing comma
            (prop::collection::vec(inner.clone(), 0..4), any::<bool>()).prop_map(
                |(items, trailing)| {
                    let tail = if trailing && !items.is_empty() {
                        ","
                    } else {
                        ""
                    };
                    format!("[{}{tail}]", items.join(", "))
                }
            ),
            // object
            (
                prop::collection::vec((inner.clone(), inner.clone()), 0..3),
                any::<bool>()
            )
                .prop_map(|(pairs, trailing)| {
                    let body = pairs
                        .iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let tail = if trailing && !pairs.is_empty() {
                        ","
                    } else {
                        ""
                    };
                    format!("{{{body}{tail}}}")
                }),
            // function application
            (
                select(FUNCTION_NAMES),
                prop::collection::vec(inner.clone(), 0..3)
            )
                .prop_map(|(f, args)| format!("{f}({})", args.join(", "))),
            // binary operator chain
            (inner.clone(), select(BINARY_OPS), inner.clone())
                .prop_map(|(a, op, b)| format!("{a} {op} {b}")),
            // unary prefixes
            ("[-!]{1,3}", inner.clone()).prop_map(|(u, e)| format!("{u}{e}")),
            // grouping
            inner.prop_map(|e| format!("({e})")),
        ]
    })
}

/// One body atom: bare expressions, unification, membership, stored /
/// rule / search applies (with sigils and validity specs), negation and
/// bracketed sub-bodies, recursively.
fn atom_strategy() -> impl Strategy<Value = String> {
    let expr = expr_strategy();
    let leaf = prop_oneof![
        2 => expr_strategy(),
        2 => (select(VAR_NAMES), expr_strategy()).prop_map(|(v, e)| format!("{v} = {e}")),
        2 => (select(VAR_NAMES), expr_strategy()).prop_map(|(v, e)| format!("{v} in {e}")),
        // positional stored-relation apply, with optional validity
        2 => (
            select(REL_NAMES),
            prop::collection::vec(select(VAR_NAMES), 0..3),
            proptest::option::of(select(VALIDITIES)),
        )
            .prop_map(|(rel, args, v)| {
                let vspec = v.map(|v| format!(" @ {v}")).unwrap_or_default();
                format!("{rel}[{}{vspec}]", args.join(", "))
            }),
        // named stored-relation apply
        2 => (
            select(REL_NAMES),
            prop::collection::vec((select(IDENT_NAMES), proptest::option::of(expr)), 0..3),
            proptest::option::of(select(VALIDITIES)),
        )
            .prop_map(|(rel, pairs, v)| {
                let body = pairs
                    .iter()
                    .map(|(k, e)| match e {
                        Some(e) => format!("{k}: {e}"),
                        None => k.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let vspec = v.map(|v| format!(" @ {v}")).unwrap_or_default();
                format!("{rel}{{{body}{vspec}}}")
            }),
        // search-index apply with option fields (`~rel:idx{a: q | k: 10}`)
        1 => (
            select(IDENT_NAMES),
            select(VAR_NAMES),
            prop::collection::vec((select(IDENT_NAMES), expr_strategy()), 0..3),
        )
            .prop_map(|(rel, q, opts)| {
                let opts = opts
                    .iter()
                    .map(|(k, e)| format!("{k}: {e}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("~{rel}:idx{{a: {q} | {opts}}}")
            }),
        // in-program rule apply
        1 => (
            select(IDENT_NAMES),
            prop::collection::vec(expr_strategy(), 0..3)
        )
            .prop_map(|(name, args)| format!("{name}[{}]", args.join(", "))),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            ("(not ){1,3}", inner.clone()).prop_map(|(nots, a)| format!("{nots}{a}")),
            prop::collection::vec(inner, 1..3).prop_map(|atoms| format!("({})", atoms.join(", "))),
        ]
    })
}

/// A rule head: the entry `?` (usually) or a named rule, args either plain
/// vars or aggregations drawn from the real registry names — including
/// aggregations with extra arguments (`collect(b, 5)`).
fn rule_head_strategy() -> impl Strategy<Value = String> {
    let head_arg = prop_oneof![
        3 => select(VAR_NAMES),
        2 => (select(AGGR_NAMES), select(VAR_NAMES)).prop_map(|(a, v)| format!("{a}({v})")),
        1 => (select(AGGR_NAMES), select(VAR_NAMES), expr_strategy())
            .prop_map(|(a, v, e)| format!("{a}({v}, {e})")),
    ];
    let name = prop_oneof![
        3 => Just("?".to_string()),
        1 => select(IDENT_NAMES),
    ];
    (name, prop::collection::vec(head_arg, 0..4), any::<bool>()).prop_map(
        |(name, args, trailing)| {
            let tail = if trailing && !args.is_empty() {
                ","
            } else {
                ""
            };
            format!("{name}[{}{tail}]", args.join(", "))
        },
    )
}

/// One argument to a fixed rule: an option pair, an in-program relation,
/// or a stored relation (positional/named, optionally with validity).
fn fixed_arg_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        (select(IDENT_NAMES), expr_strategy()).prop_map(|(k, e)| format!("{k}: {e}")),
        (
            select(IDENT_NAMES),
            prop::collection::vec(select(VAR_NAMES), 0..3)
        )
            .prop_map(|(r, vs)| format!("{r}[{}]", vs.join(", "))),
        (
            select(REL_NAMES),
            prop::collection::vec(select(VAR_NAMES), 0..3),
            proptest::option::of(select(VALIDITIES)),
        )
            .prop_map(|(r, vs, v)| {
                let vspec = v.map(|v| format!(" @ {v}")).unwrap_or_default();
                format!("{r}[{}{vspec}]", vs.join(", "))
            }),
        // The named-field shape that panicked upstream: `*rel{f: x}`.
        (select(REL_NAMES), select(IDENT_NAMES), select(VAR_NAMES))
            .prop_map(|(r, f, x)| format!("{r}{{{f}: {x}}}")),
    ]
}

/// A fixed-rule name: drawn from the *real* default registry, or a
/// stranger to exercise the not-found path.
fn fixed_rule_name_strategy() -> impl Strategy<Value = String> {
    let names: Vec<String> = DEFAULT_FIXED_RULES
        .keys()
        .cloned()
        .chain(["NoSuchRule".to_string()])
        .collect();
    proptest::sample::select(names)
}

/// One rule of a query program: Datalog (`:=`), constant (`<-`), or fixed
/// (`<~`), with an optional terminating `;`.
fn rule_strategy() -> impl Strategy<Value = String> {
    let body = prop::collection::vec(
        (
            atom_strategy(),
            prop_oneof![3 => Just(", "), 1 => Just(" or ")],
        ),
        1..4,
    )
    .prop_map(|parts| {
        let mut out = String::new();
        for (i, (atom, sep)) in parts.iter().enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            out.push_str(atom);
        }
        out
    });
    (
        rule_head_strategy(),
        prop_oneof![
            4 => body.prop_map(|b| format!(":= {b}")),
            2 => expr_strategy().prop_map(|e| format!("<- {e}")),
            2 => (
                fixed_rule_name_strategy(),
                prop::collection::vec(fixed_arg_strategy(), 0..3)
            )
                .prop_map(|(name, args)| format!("<~ {name}({})", args.join(", "))),
        ],
        any::<bool>(),
    )
        .prop_map(|(head, tail, semi)| {
            let semi = if semi { ";" } else { "" };
            format!("{head} {tail}{semi}")
        })
}

/// One column of a stored-relation schema clause.
fn schema_col_strategy() -> impl Strategy<Value = String> {
    (
        select(IDENT_NAMES),
        proptest::option::of(select(COL_TYPES)),
        proptest::option::of(prop_oneof![
            expr_strategy().prop_map(|e| format!("default {e}")),
            select(VAR_NAMES).prop_map(|v| format!("= {v}")),
        ]),
    )
        .prop_map(|(name, ty, extra)| {
            let ty = ty.map(|t| format!(": {t}")).unwrap_or_default();
            let extra = extra.map(|e| format!(" {e}")).unwrap_or_default();
            format!("{name}{ty}{extra}")
        })
}

/// A stored-relation schema clause (`{a: Int => b: Float default 0}`).
fn schema_strategy() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(schema_col_strategy(), 0..3),
        proptest::option::of(prop::collection::vec(schema_col_strategy(), 0..3)),
    )
        .prop_map(|(keys, vals)| {
            let keys = keys.join(", ");
            match vals {
                Some(vals) => format!("{{{keys} => {}}}", vals.join(", ")),
                None => format!("{{{keys}}}"),
            }
        })
}

/// One query option (`:limit`, `:sort`, relation writes with schemas, …).
fn option_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        expr_strategy().prop_map(|e| format!(":limit {e}")),
        expr_strategy().prop_map(|e| format!(":offset {e}")),
        expr_strategy().prop_map(|e| format!(":timeout {e}")),
        expr_strategy().prop_map(|e| format!(":sleep {e}")),
        expr_strategy().prop_map(|e| format!(":disable_magic_rewrite {e}")),
        Just(":returning".to_string()),
        Just(":assert none".to_string()),
        Just(":assert some".to_string()),
        prop::collection::vec(
            ("[-+]?", select(VAR_NAMES)).prop_map(|(d, v)| format!("{d}{v}")),
            1..3
        )
        .prop_map(|args| format!(":sort {}", args.join(", "))),
        (
            proptest::sample::select(
                &[
                    ":create",
                    ":replace",
                    ":insert",
                    ":put",
                    ":update",
                    ":rm",
                    ":delete",
                    ":ensure",
                    ":ensure_not",
                ][..]
            ),
            select(IDENT_NAMES),
            proptest::option::of(schema_strategy()),
            // The write-time `@` clause: mostly the legal one-coordinate
            // form (sometimes a real validity spec, sometimes one of this
            // mutation's own output vars — the per-row case), but also the
            // two-coordinate form and `:ensure`/`:ensure_not` (via the `op`
            // draw above) so the refusal paths get fuzzed too, never just
            // the happy path.
            proptest::option::of(prop_oneof![
                3 => select(VALIDITIES).prop_map(|v| format!(" @ {v}")),
                2 => select(VAR_NAMES).prop_map(|v| format!(" @ {v}")),
                1 => (select(VALIDITIES), select(VALIDITIES))
                    .prop_map(|(a, b)| format!(" @ {a}, {b}")),
            ]),
        )
            .prop_map(|(op, rel, schema, at)| {
                let schema = schema.map(|s| format!(" {s}")).unwrap_or_default();
                let at = at.unwrap_or_default();
                format!("{op} {rel}{schema}{at}")
            }),
    ]
}

/// A whole query script: rules then options.
fn query_script_strategy() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(rule_strategy(), 1..4),
        prop::collection::vec(option_strategy(), 0..3),
    )
        .prop_map(|(rules, options)| {
            let mut parts = rules;
            parts.extend(options);
            parts.join("\n")
        })
}

/// A sys script: every `::` operation, with expression and inner-query
/// holes filled by the generators above (index DDL includes hnsw/fts/lsh
/// option fields).
fn sys_script_strategy() -> impl Strategy<Value = String> {
    let simple = proptest::sample::select(
        &[
            "::relations",
            "::running",
            "::compact",
            "::fixed_rules",
            "::index drop r:idx",
            "::hnsw drop r:idx",
            "::fts drop r:idx",
            "::lsh drop r:idx",
        ][..],
    )
    .prop_map(str::to_string);
    prop_oneof![
        2 => simple,
        1 => select(IDENT_NAMES).prop_map(|r| format!("::columns {r}")),
        1 => select(IDENT_NAMES).prop_map(|r| format!("::indices {r}")),
        1 => prop::collection::vec(select(IDENT_NAMES), 1..3)
            .prop_map(|rs| format!("::remove {}", rs.join(", "))),
        1 => (select(IDENT_NAMES), select(IDENT_NAMES))
            .prop_map(|(a, b)| format!("::rename {a} -> {b}")),
        1 => (
            proptest::sample::select(&["normal", "protected", "read_only", "hidden"][..]),
            prop::collection::vec(select(IDENT_NAMES), 1..3),
        )
            .prop_map(|(lvl, rs)| format!("::access_level {lvl} {}", rs.join(", "))),
        1 => expr_strategy().prop_map(|e| format!("::kill {e}")),
        1 => query_script_strategy().prop_map(|q| format!("::explain {{ {q} }}")),
        1 => (select(IDENT_NAMES), prop::collection::vec(select(IDENT_NAMES), 0..3))
            .prop_map(|(r, cols)| format!("::index create {r}:idx {{{}}}", cols.join(", "))),
        1 => (
            proptest::sample::select(&["hnsw", "fts", "lsh"][..]),
            select(IDENT_NAMES),
            prop::collection::vec((select(IDENT_NAMES), expr_strategy()), 0..4),
        )
            .prop_map(|(kind, r, opts)| {
                let opts = opts
                    .iter()
                    .map(|(k, e)| format!("{k}: {e}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("::{kind} create {r}:i {{{opts}}}")
            }),
        1 => (
            select(IDENT_NAMES),
            proptest::sample::select(&["put", "rm", "replace"][..]),
            query_script_strategy(),
        )
            .prop_map(|(r, ev, q)| format!("::set_triggers {r} on {ev} {{ {q} }}")),
    ]
}

/// An imperative script: query clauses with `as _temp`, `%if`/`%if_not`
/// with `%then`/`%else`, `%loop`/`%mark`, `%return`, `%swap`, `%debug`,
/// `%ignore_error`, and embedded sys ops — nested blocks included.
fn imperative_script_strategy() -> impl Strategy<Value = String> {
    let leaf_stmt = prop_oneof![
        3 => (query_script_strategy(), proptest::option::of("_[a-z]{1,3}"))
            .prop_map(|(q, store)| {
                let store = store.map(|s| format!(" as {s}")).unwrap_or_default();
                format!("{{ {q} }}{store}")
            }),
        1 => Just("%break".to_string()),
        1 => Just("%continue".to_string()),
        1 => Just("%return _t".to_string()),
        1 => Just("%return".to_string()),
        1 => Just("%swap _a _b".to_string()),
        1 => Just("%debug _t".to_string()),
        1 => query_script_strategy().prop_map(|q| format!("%ignore_error {{ {q} }}")),
        1 => sys_script_strategy()
            .prop_map(|s| format!("{{ {s} }} as _res")),
    ];
    let stmts = leaf_stmt.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            (
                any::<bool>(),
                prop_oneof![
                    Just("_cond".to_string()),
                    query_script_strategy().prop_map(|q| format!("{{ {q} }}")),
                ],
                any::<bool>(),
                prop::collection::vec(inner.clone(), 1..3),
                proptest::option::of(prop::collection::vec(inner.clone(), 1..2)),
            )
                .prop_map(|(negated, cond, then_kw, then_b, else_b)| {
                    let kw = if negated { "%if_not" } else { "%if" };
                    let then_kw = if then_kw { " %then" } else { "" };
                    let else_b = else_b
                        .map(|b| format!(" %else {}", b.join(" ")))
                        .unwrap_or_default();
                    format!("{kw} {cond}{then_kw} {}{else_b} %end", then_b.join(" "))
                }),
            (
                proptest::option::of(select(IDENT_NAMES)),
                prop::collection::vec(inner, 1..3)
            )
                .prop_map(|(mark, body)| {
                    let mark = mark.map(|m| format!("%mark {m} ")).unwrap_or_default();
                    format!("{mark}%loop {} %end", body.join(" "))
                }),
        ]
    });
    prop::collection::vec(stmts, 1..4).prop_map(|stmts| stmts.join("\n"))
}

/// Shapes that deliberately approach — and cross — the landed ceilings:
/// bracket nesting, `not` chains, `%loop` chains, unary-minus chains,
/// power chains, nested block comments, and long flat conjunctions. The
/// laws require these to parse or be *refused with a span*, never abort.
fn ceiling_shape_strategy() -> impl Strategy<Value = String> {
    (50usize..=72, 0u8..7).prop_map(|(depth, shape)| match shape {
        0 => format!("?[x] := x = {}1{}", "[".repeat(depth), "]".repeat(depth)),
        1 => format!("?[x] := {}x == 1", "not ".repeat(depth)),
        2 => format!(
            "{}{{ ?[a] <- [[1]] }}{}",
            "%loop ".repeat(depth),
            " %end".repeat(depth)
        ),
        3 => format!("?[x] := x = {}1", "-".repeat(depth)),
        4 => format!("?[x] := x = 1{}", "^1".repeat(depth)),
        5 => format!("{}?[a] <- [[1]]{}", "/*".repeat(depth), "*/".repeat(depth)),
        _ => format!("?[a] := a in [1]{}", ", a == 1".repeat(depth * 4)),
    })
}

/// The top-level generator: any species, weighted toward query scripts,
/// with ceiling-approaching shapes in the mix.
fn script_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => query_script_strategy(),
        2 => sys_script_strategy(),
        2 => imperative_script_strategy(),
        1 => ceiling_shape_strategy(),
    ]
}

/// An FTS query: phrases (plain, quoted, raw), prefix markers, boosters
/// (float and integer, including overflowing ones), `NEAR`, groups, and
/// operator chains.
fn fts_query_strategy() -> impl Strategy<Value = String> {
    let booster = proptest::sample::select(
        &[
            "^2",
            "^1.5",
            "^0.0",
            "^22",
            "^99999999999999999999999",
            "^9223372036854775808",
            "^0.000000001",
        ][..],
    );
    let phrase = (
        prop_oneof![
            3 => proptest::sample::select(&["word", "héllo", "日本語", "z\u{0334}a\u{0300}"][..])
                .prop_map(str::to_string),
            1 => select(STRINGS),
        ],
        proptest::option::of("\\*"),
        proptest::option::of(booster),
    )
        .prop_map(|(w, star, boost)| {
            format!(
                "{w}{}{}",
                star.map(|_| "*").unwrap_or_default(),
                boost.unwrap_or_default()
            )
        });
    let term = phrase.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            (
                proptest::option::of(0u32..20),
                prop::collection::vec(inner.clone(), 1..3)
            )
                .prop_map(|(n, ps)| {
                    let n = n.map(|n| format!("/{n}")).unwrap_or_default();
                    format!("NEAR{n}({})", ps.join(" "))
                }),
            prop::collection::vec(inner, 1..3).prop_map(|ts| format!("({})", ts.join(" "))),
        ]
    });
    prop::collection::vec(
        (
            term,
            proptest::sample::select(&[" AND ", " OR ", " NOT ", ", ", "; ", " "][..]),
        ),
        1..5,
    )
    .prop_map(|parts| {
        let mut out = String::new();
        for (i, (t, sep)) in parts.iter().enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            out.push_str(t);
        }
        out
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Layer 2: the mutation layer
// ─────────────────────────────────────────────────────────────────────────

/// Hostile injection payloads: NUL, RTL override, zalgo combining marks,
/// wide multibyte, invalid UTF-8 (bare continuation, surrogate encoding,
/// overlong), a BOM, and a zero-width joiner.
const PAYLOADS: &[&[u8]] = &[
    b"\x00",
    "\u{202E}".as_bytes(),
    "\u{0334}\u{0322}\u{0300}".as_bytes(),
    "🦀".as_bytes(),
    b"\xFF\xFE",
    b"\xED\xA0\x80",
    b"\xC0\xAF",
    "\u{FEFF}".as_bytes(),
    "\u{200D}".as_bytes(),
    b"\x80",
];

const BRACKETS: &[u8] = b"()[]{}";

/// One byte-level mutation. `Index` fields resolve against whatever the
/// buffer's length is when the op runs, so ops compose in any order.
#[derive(Debug, Clone)]
enum MutOp {
    /// Cut the buffer at an arbitrary byte offset.
    Truncate(Index),
    /// Replace the tail from `at` with the other script's tail from
    /// `other_at`: a crossover splice.
    Splice(Index, Index),
    /// Duplicate the slice between two offsets, inserting it at the
    /// slice's end: token/region duplication.
    DupSlice(Index, Index),
    /// Replace one bracket byte with a different bracket.
    FlipBracket(Index, Index),
    /// Insert a hostile payload at an arbitrary byte offset.
    Inject(Index, Index),
}

fn mut_op_strategy() -> impl Strategy<Value = MutOp> {
    prop_oneof![
        any::<Index>().prop_map(MutOp::Truncate),
        (any::<Index>(), any::<Index>()).prop_map(|(a, b)| MutOp::Splice(a, b)),
        (any::<Index>(), any::<Index>()).prop_map(|(a, b)| MutOp::DupSlice(a, b)),
        (any::<Index>(), any::<Index>()).prop_map(|(a, b)| MutOp::FlipBracket(a, b)),
        (any::<Index>(), any::<Index>()).prop_map(|(a, b)| MutOp::Inject(a, b)),
    ]
}

/// Apply byte-level mutations to `base` (with `other` as splice donor).
/// The result may be invalid UTF-8 mid-stream; it is materialized through
/// `from_utf8_lossy`, because `&str` is the real API surface.
fn mutate(base: &str, other: &str, ops: &[MutOp]) -> String {
    let mut bytes = base.as_bytes().to_vec();
    for op in ops {
        match op {
            MutOp::Truncate(at) => {
                let cut = at.index(bytes.len() + 1);
                bytes.truncate(cut);
            }
            MutOp::Splice(at, other_at) => {
                let cut = at.index(bytes.len() + 1);
                let from = other_at.index(other.len() + 1);
                bytes.truncate(cut);
                bytes.extend_from_slice(&other.as_bytes()[from..]);
            }
            MutOp::DupSlice(a, b) => {
                if bytes.is_empty() {
                    continue;
                }
                let (x, y) = (a.index(bytes.len() + 1), b.index(bytes.len() + 1));
                let (start, end) = (x.min(y), x.max(y));
                let slice = bytes[start..end].to_vec();
                bytes.splice(end..end, slice);
            }
            MutOp::FlipBracket(which, replacement) => {
                let positions: Vec<usize> = bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| BRACKETS.contains(b))
                    .map(|(i, _)| i)
                    .collect();
                if positions.is_empty() {
                    continue;
                }
                let pos = positions[which.index(positions.len())];
                bytes[pos] = BRACKETS[replacement.index(BRACKETS.len())];
            }
            MutOp::Inject(at, payload) => {
                let pos = at.index(bytes.len() + 1);
                let payload = PAYLOADS[payload.index(PAYLOADS.len())];
                bytes.splice(pos..pos, payload.iter().copied());
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

// ─────────────────────────────────────────────────────────────────────────
// Layer 3: the law tests
// ─────────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(96),
        // Failures persist to proptest-regressions/ for replay.
        ..ProptestConfig::default()
    })]

    /// Grammar-aware scripts, unmutated: the laws hold on every
    /// structurally-plausible input.
    #[test]
    fn generated_scripts_uphold_laws(src in script_strategy()) {
        if let Err(violation) = check_script_laws(&src) {
            prop_assert!(false, "{violation}");
        }
    }

    /// Generated scripts through the byte-mutation layer: the laws hold on
    /// arbitrarily damaged (and lossily re-decoded) text.
    #[test]
    fn mutated_scripts_uphold_laws(
        base in script_strategy(),
        donor in script_strategy(),
        ops in prop::collection::vec(mut_op_strategy(), 1..5),
    ) {
        let src = mutate(&base, &donor, &ops);
        if let Err(violation) = check_script_laws(&src) {
            prop_assert!(false, "{violation}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(48),
        ..ProptestConfig::default()
    })]

    /// The FTS query parser under the same laws, generated and mutated
    /// (boosters, NEAR, groups, operator chains).
    #[test]
    fn fts_queries_uphold_laws(
        base in fts_query_strategy(),
        donor in fts_query_strategy(),
        ops in prop::collection::vec(mut_op_strategy(), 0..4),
    ) {
        let src = mutate(&base, &donor, &ops);
        if let Err(violation) = check_fts_laws(&src) {
            prop_assert!(false, "{violation}");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Layer 4: the regression corpus
// ─────────────────────────────────────────────────────────────────────────

/// Every hostile input this project's reviews and fuzzing sessions have
/// produced, replayed deterministically on every run. Entries still tagged
/// `FINDING-n` in their names are the minimized inputs the fix wave spanned
/// (FINDING-1..8); they are kept because they exercise those paths, and the
/// laws now hold on them fully — including the spanned-error law, since each
/// carries an in-bounds label (asserted directly in
/// [`former_findings_now_carry_spans`]).
fn regression_corpus() -> Vec<(&'static str, String)> {
    let mut corpus: Vec<(&'static str, String)> = vec![
        // The hex-int overflow (upstream panicked in from_str_radix).
        (
            "hex-int overflow",
            "?[x] := x = 0xffff_ffff_ffff_ffff_ffff".into(),
        ),
        (
            "hex-int overflow, minimal",
            "?[x] := x = 0x1_0000_0000_0000_0000".into(),
        ),
        (
            "decimal i64 overflow",
            "?[x] := x = 9223372036854775808".into(),
        ),
        (
            "octal overflow",
            "?[x] := x = 0o7777777777777777777777777".into(),
        ),
        (
            "binary overflow",
            format!("?[x] := x = 0b{}", "1".repeat(65)),
        ),
        // The fixed-rule named-field shape (upstream panicked stripping `:`).
        (
            "fixed-rule *rel{f:x} shape",
            "?[a, b] <~ ReorderSort(*rel{f: x})".into(),
        ),
        ("fixed-rule unknown name", "?[a] <~ NoSuchRule()".into()),
        // Deep nesting at and past the ceiling.
        (
            "nesting at the ceiling",
            format!("?[x] := x = {}1{}", "[".repeat(60), "]".repeat(60)),
        ),
        (
            "nesting past the ceiling",
            format!("?[x] := x = {}1", "[".repeat(300)),
        ),
        (
            "deep not chain",
            format!("?[x] := {}x == 1", "not ".repeat(5000)),
        ),
        ("deep %loop chain", "%loop ".repeat(300)),
        ("deep block comments", "/*".repeat(300)),
        (
            "deep unary minus",
            format!("?[x] := x = {}1", "-".repeat(5000)),
        ),
        (
            "deep power chain",
            format!("?[x] := x = 1{}", "^1".repeat(5000)),
        ),
        // 300 KB flat AND chain (both comma-atoms and `&&` operators).
        (
            "300KB flat comma chain",
            format!("?[a] := a in [1]{}", ", a == 1".repeat(37_500)),
        ),
        (
            "300KB flat && chain",
            format!("?[a] := a = true{}", " && a".repeat(50_000)),
        ),
        // Hostile bytes and encodings.
        ("NUL byte in script", "?[a] := a = \0 in [1]".into()),
        (
            "RTL override in ident position",
            "?[a] := \u{202E}a == 1".into(),
        ),
        (
            "zalgo rule",
            "?[z\u{0334}\u{0322}] := z\u{0334}\u{0322} in [1]".into(),
        ),
        ("lone surrogate escape", r#"?[a] := a = "\ud800""#.into()),
        (
            "lossy replacement chars",
            "?[a] := a \u{FFFD}\u{FFFD} 1".into(),
        ),
        ("empty input", String::new()),
        ("whitespace only", "  \t\r\n ".into()),
        ("lone question mark", "?".into()),
        ("unterminated string", "?[a] := a = \"open".into()),
        ("unterminated raw string", "?[a] := a = ___\"open".into()),
        ("unterminated block comment", "?[a] := a = 1 /* open".into()),
        // Params, validity, options.
        ("missing param", "?[a] := a = $missing".into()),
        ("validity garbage", "?[a] := *rel[a @ 'garbage']".into()),
        (
            "validity huge int",
            "?[a] := *rel[a @ 9223372036854775807]".into(),
        ),
        ("option not const", "?[a] := a in [1] :limit a".into()),
        ("negative limit", "?[a] := a in [1] :limit -1".into()),
        ("aggr in const rule", "?[count(a)] <- [[1]]".into()),
        (
            "unknown aggregation",
            "?[no_such_aggr(a)] := a in [1]".into(),
        ),
        ("empty rule head", "?[] := a in [1]".into()),
        ("no entry rule (FINDING-7)", "r[a] := a in [1]".into()),
        ("const rule bad row (FINDING-8)", "?[a] <- [1]".into()),
        ("const rule non-list (FINDING-8)", "?[a] <- 1".into()),
        // Schema edges. FINDING-3: the overflowing vec dimension's error
        // carries no span.
        (
            "vec dim usize overflow (FINDING-3)",
            "?[a] <- [[1]] :create t {v: <F32; 99999999999999999999999>}".into(),
        ),
        (
            "list len not const (FINDING-2)",
            "?[a] <- [[1]] :create t {v: [Int; x]}".into(),
        ),
        (
            "negative list len",
            "?[a] <- [[1]] :create t {v: [Int; -1]}".into(),
        ),
        ("duplicate columns", "?[a] <- [[1]] :create t {a, a}".into()),
        // Sys-op edges. FINDING-1/2: `::kill` errors carry no span.
        ("kill with string (FINDING-1)", "::kill 'pid'".into()),
        ("kill with variable (FINDING-2)", "::kill x".into()),
        ("kill with field access (FINDING-5)", "::kill 1 -> 2".into()),
        ("empty index columns", "::index create r:idx {}".into()),
        ("explain of imperative", "::explain { %return }".into()),
        // FINDING-6 family: index-DDL option validation is span-less.
        (
            "hnsw with no options (FINDING-6)",
            "::hnsw create r:i {}".into(),
        ),
        (
            "hnsw bad dtype (FINDING-6)",
            "::hnsw create r:i {dtype: X}".into(),
        ),
        (
            "hnsw bad fields (FINDING-5)",
            "::hnsw create r:i {fields: 1}".into(),
        ),
        (
            "lsh non-integer n_gram (FINDING-6)",
            "::lsh create r:i {n_gram: 0.5}".into(),
        ),
        (
            "lsh unknown option (FINDING-6)",
            "::lsh create r:i {bogus: 1}".into(),
        ),
        (
            "fts bad tokenizer (FINDING-6)",
            "::fts create r:i {tokenizer: 1}".into(),
        ),
        (
            "fts bad filters (FINDING-6)",
            "::fts create r:i {extractor: v, tokenizer: Simple, filters: 3}".into(),
        ),
        // Imperative edges.
        ("swap of undeclared temps", "%swap _a _b".into()),
        ("bare break", "%break".into()),
        (
            "if with missing end",
            "%if _c %then { ?[a] <- [[1]] }".into(),
        ),
        (
            "return of query",
            "{ ?[a] <- [[1]] } %return { ?[b] <- [[2]] }".into(),
        ),
    ];
    // Mismatched-bracket sweeps: every bracket flipped to every other.
    for &open in BRACKETS {
        for &close in BRACKETS {
            corpus.push((
                "bracket flip sweep",
                format!("?[a] := a in {}1{}", open as char, close as char),
            ));
        }
    }
    corpus
}

/// FTS-specific hostile corpus (its own entry point and ceilings).
fn fts_regression_corpus() -> Vec<(&'static str, String)> {
    vec![
        // Boosters, including the integer-booster shape that panicked
        // upstream and overflowing values.
        ("integer booster", "word^22".into()),
        ("float booster", "word^1.5".into()),
        ("booster overflow", "word^99999999999999999999999".into()),
        ("booster i64::MAX+1", "word^9223372036854775808".into()),
        ("booster on quoted phrase", "\"a phrase\"^2".into()),
        ("booster on prefix", "wor*^3".into()),
        ("bare booster (FINDING-4)", "^2".into()),
        ("empty booster (FINDING-4)", "word^".into()),
        // Ceilings.
        (
            "flat AND chain past ops ceiling",
            format!("w{}", " AND w".repeat(20_000)),
        ),
        ("NOT chain", format!("{}w", "NOT ".repeat(2_000))),
        (
            "deep groups",
            format!("{}w{}", "(".repeat(300), ")".repeat(300)),
        ),
        // Structure and encoding hostility.
        ("empty (FINDING-4)", String::new()),
        ("only operator (FINDING-4)", "AND".into()),
        ("NEAR without operand (FINDING-4)", "NEAR/3()".into()),
        (
            "NEAR huge distance",
            format!("NEAR/{}(a b)", "9".repeat(30)),
        ),
        ("RTL phrase", "\u{202E}drow".into()),
        ("NUL phrase (FINDING-4)", "\0".into()),
    ]
}

/// Replay the whole regression corpus against the laws on every test run.
#[test]
fn regression_corpus_upholds_laws() {
    for (what, src) in regression_corpus() {
        if let Err(violation) = check_script_laws(&src) {
            panic!("corpus entry {what:?}: {violation}");
        }
    }
    for (what, src) in fts_regression_corpus() {
        if let Err(violation) = check_fts_laws(&src) {
            panic!("fts corpus entry {what:?}: {violation}");
        }
    }
}

/// One script per SQL keyword [`crate::parse::SQL_KEYWORD_HINTS`] maps to a
/// KyzoScript idiom — a plausible SQL-shaped mistake a Datalog newcomer
/// would actually type, one clause at a time so each script's failure
/// implicates a single keyword's hint. Not exhaustive over every possible
/// SQL sentence (that's the newcomer's problem to invent, not this
/// corpus's); exhaustive over every keyword the hint table knows about.
fn sql_refugee_corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("select", "SELECT name, age FROM person"),
        ("from", "?[name] := FROM person"),
        ("where", "?[x] := *person{x} WHERE x > 1"),
        ("join", "SELECT * FROM a JOIN b ON a.id = b.id"),
        ("group", "?[dept] := *emp{dept} GROUP BY dept"),
        ("having", "?[dept] := *emp{dept} HAVING count(dept) > 1"),
        ("order", "?[x] := *t{x} ORDER BY x"),
        ("insert", "INSERT INTO person VALUES (1, 'a')"),
        ("update", "UPDATE person SET name = 'a'"),
        ("delete", "DELETE FROM person WHERE x = 1"),
        ("values", "INSERT INTO person VALUES (1, 'a')"),
        ("create", "CREATE TABLE person (id INT)"),
    ]
}

/// The diagnostics law test the redesign's DoD names explicitly: every
/// entry in [`sql_refugee_corpus`] must fail to parse (none of these are
/// legal KyzoScript) AND carry a `#[help]` naming KyzoScript's real idiom —
/// not just a refusal, a *designed* refusal. Each script also still passes
/// through [`check_script_laws`] (spanned, no banned placeholder), so this
/// test is additive to the general laws, not a carve-out from them.
#[test]
fn sql_refugee_mistakes_get_designed_help() {
    for (keyword, src) in sql_refugee_corpus() {
        if let Err(violation) = check_script_laws(src) {
            panic!("SQL-refugee corpus entry {keyword:?}: {violation}");
        }
        let err = parse_script(src, &BTreeMap::new(), &DEFAULT_FIXED_RULES, vld())
            .expect_err("SQL syntax is not legal KyzoScript");
        let help = err.help().map(|h| h.to_string());
        assert!(
            help.as_deref().is_some_and(|h| h.contains("KyzoScript")),
            "keyword {keyword:?} ({src:?}) should get a designed KyzoScript-idiom hint, \
             got: {err:?}"
        );
    }
}

/// The fix wave's proof, and the inverse of the retired
/// `findings_ledger_is_current`: every former finding (FINDING-1..8) still
/// errors on its minimized input, and that error now carries at least one
/// in-bounds label. A fix regressing to a span-less error fails here by name
/// (the fuzz laws catch it too, now that the ledger is empty); a fix whose
/// span points outside the input fails on the out-of-bounds check.
#[test]
fn former_findings_now_carry_spans() {
    fn assert_spanned(finding: &str, src: &str, err: &dyn Diagnostic) {
        let mut any_label = false;
        let mut oob = None;
        walk_labels(err, src.len(), &mut any_label, &mut oob);
        assert!(
            any_label,
            "{finding} ({src:?}) still errors without a span: {err:?}"
        );
        assert!(
            oob.is_none(),
            "{finding} ({src:?}) labels out of bounds: {}",
            oob.unwrap()
        );
    }

    // Script-parser findings, each at its minimized input.
    for (finding, src) in [
        ("FINDING-1", "::kill 'pid'"),
        ("FINDING-2", "::kill x"),
        (
            "FINDING-3",
            "?[a] <- [[1]] :create t {v: <F32; 99999999999999999999999>}",
        ),
        ("FINDING-5", "::kill 1 -> 2"),
        ("FINDING-6", "::hnsw create r:i {}"),
        ("FINDING-7", "r[a] := a in [1]"),
        ("FINDING-8", "?[a] <- [1]"),
    ] {
        let err = parse_script(src, &BTreeMap::new(), &DEFAULT_FIXED_RULES, vld())
            .expect_err("former-finding inputs must still error");
        assert_spanned(finding, src, &*err);
    }

    // FINDING-4 lives in the FTS parser (its own entry point).
    let src = "AND";
    let err = parse_fts_query(src).expect_err("bare operator must error");
    assert_spanned("FINDING-4", src, &*err);
}

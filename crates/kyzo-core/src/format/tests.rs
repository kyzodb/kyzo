/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Property tests: idempotence (`fmt(fmt(x)) == fmt(x)`) and meaning-
//! preserving round-trip (`parse(fmt(x))` is the same tree as `x`), over
//! generated KyzoScript source text — parsed by the real parser, exactly
//! as `query::gauntlet`'s SQLancer-class oracle generates programs (issue
//! #29). The seeded RNG here is a fresh splitmix64 transcription rather
//! than reaching into that module's private harness — the same call its
//! own doc makes for not reaching into `query::trials`: a fourth
//! independent transcription of a primitive this tree already writes out
//! by hand in `storage/sim.rs`, `query/trials.rs`, and `query/gauntlet.rs`.
//!
//! Equality is checked against the parser's own derived
//! `Debug`/`Display` chain (`InputProgram`/`Expr`/… — never anything this
//! module writes), which is an independent oracle for "same tree": if two
//! parses produce byte-identical debug text, they built the same AST.
//!
//! `Str` constants are generated with quotes, backslashes, and control
//! characters deliberately included — `write_str_literal`'s whole reason
//! to exist is escaping those. This relies on issue #93 (`raw_string`'s
//! zero-underscore fence shadowing `quoted_string` in the grammar's
//! ordered choice, so an escaped double-quoted string failed to reparse
//! regardless of what any escaper emitted) being fixed; it has landed
//! (`raw_string`'s fence is now `"_"+`, at least one underscore).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::rules::contract::{EmptyNamedRowsBody, FixedRule, SimpleFixedRule};
use kyzo_model::program::Comment;
use kyzo_model::value::DataValue;
use crate::parse::{Script, parse_expressions, parse_script};
use crate::session::current_validity;

use super::{format_expr, format_program, format_program_with_comments};

// ─────────────────────────────────────────────────────────────────────────
// splitmix64 (see module doc: independently transcribed, not reused)
// ─────────────────────────────────────────────────────────────────────────

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Rng { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

fn no_fixed_rules() -> BTreeMap<String, Arc<dyn FixedRule>> {
    BTreeMap::new()
}

// ─────────────────────────────────────────────────────────────────────────
// Round-trip assertion helpers, shared by the regression list and the
// property tests below.
// ─────────────────────────────────────────────────────────────────────────

/// A derived `Debug` string with every `span: N..M` blinded to `span: _`.
/// `SourceSpan`s are provenance (which source bytes produced this node),
/// not meaning: this formatter freely reorders top-level rules (entry
/// first, see [`super::format_program`]'s doc) and always re-renders
/// canonically, so a byte offset into the reformatted text is expected to
/// differ from a byte offset into the original — the derived `Debug` this
/// suite uses as its independent "same tree" oracle needs to compare
/// everything BUT that one field.
fn debug_no_spans(s: &str) -> String {
    let mut result = String::new();
    let mut rest = s;
    while let Some(idx) = rest.find("span: ") {
        result.push_str(&rest[..idx]);
        result.push_str("span: _");
        rest = &rest[idx + "span: ".len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}

fn assert_expr_round_trips(src: &str) -> String {
    let parsed = parse_expressions(src, &no_params())
        .unwrap_or_else(|e| panic!("fixture `{src}` must itself parse: {e}"));
    let formatted = format_expr(&parsed);
    let reparsed = parse_expressions(&formatted, &no_params()).unwrap_or_else(|e| {
        panic!("format produced unparseable text for `{src}` -> `{formatted}`: {e}")
    });
    assert_eq!(
        debug_no_spans(&format!("{parsed:?}")),
        debug_no_spans(&format!("{reparsed:?}")),
        "round-trip changed meaning: `{src}` -> `{formatted}`"
    );
    let formatted_again = format_expr(&reparsed);
    assert_eq!(
        formatted, formatted_again,
        "not idempotent: `{src}` -> `{formatted}` -> `{formatted_again}`"
    );
    formatted
}

fn assert_program_round_trips(src: &str) -> String {
    let cur_vld = current_validity().expect("current validity");
    let prog = match parse_script(src, &no_params(), &no_fixed_rules(), cur_vld) {
        Ok(Script::Single(p)) => *p,
        Ok(_) => panic!("fixture `{src}` is not a single-query script"),
        Err(e) => panic!("fixture must itself parse: {src}\n{e}"),
    };
    let formatted = format_program(&prog);
    let reparsed = match parse_script(&formatted, &no_params(), &no_fixed_rules(), cur_vld) {
        Ok(Script::Single(p)) => *p,
        Ok(_) => panic!("formatter changed script species:\n{formatted}"),
        Err(e) => panic!(
            "format produced unparseable text for:\n{src}\n---formatted to--->\n{formatted}\n{e}"
        ),
    };
    assert_eq!(
        debug_no_spans(&format!("{prog:?}")),
        debug_no_spans(&format!("{reparsed:?}")),
        "round-trip changed meaning:\n{src}\n---formatted to--->\n{formatted}"
    );
    let formatted_again = format_program(&reparsed);
    assert_eq!(
        formatted, formatted_again,
        "not idempotent:\n{formatted}\n---formatted again to--->\n{formatted_again}"
    );
    formatted
}

fn comment_texts(comments: &[Comment]) -> Vec<&str> {
    comments.iter().map(|c| c.text.as_str()).collect()
}

/// Same shape as [`assert_program_round_trips`], but through
/// [`format_program_with_comments`]. The derived `Debug` chain includes
/// every `InputInlineRule`'s `trivia` field and the whole-program
/// `leading_trivia`/`trailing_trivia`, so the same span-blind comparison
/// that proves "same tree" for the bare formatter also proves "the same
/// comments are attached to the same nodes" here — no separate trivia
/// comparator needed. (`FixedRuleApply`'s hand-written `Debug` omits
/// `trivia`, so that one node kind is NOT covered by this oracle; see
/// `fixed_rule_trivia_round_trips` below, which checks it directly.)
fn assert_program_with_comments_round_trips(src: &str) -> String {
    let cur_vld = current_validity().expect("current validity");
    let prog = match parse_script(src, &no_params(), &no_fixed_rules(), cur_vld) {
        Ok(Script::Single(p)) => *p,
        Ok(_) => panic!("fixture `{src}` is not a single-query script"),
        Err(e) => panic!("fixture must itself parse: {src}\n{e}"),
    };
    let formatted = format_program_with_comments(&prog);
    let reparsed = match parse_script(&formatted, &no_params(), &no_fixed_rules(), cur_vld) {
        Ok(Script::Single(p)) => *p,
        Ok(_) => panic!("formatter changed script species:\n{formatted}"),
        Err(e) => panic!(
            "format produced unparseable text for:\n{src}\n---formatted to--->\n{formatted}\n{e}"
        ),
    };
    assert_eq!(
        debug_no_spans(&format!("{prog:?}")),
        debug_no_spans(&format!("{reparsed:?}")),
        "round-trip changed meaning or lost/misattached a comment:\n{src}\n---formatted to--->\n{formatted}"
    );
    let formatted_again = format_program_with_comments(&reparsed);
    assert_eq!(
        formatted, formatted_again,
        "not idempotent:\n{formatted}\n---formatted again to--->\n{formatted_again}"
    );
    formatted
}

// ─────────────────────────────────────────────────────────────────────────
// Precedence regressions — the exact cases empirically walked against
// `parse/expr.rs`'s `PRATT_PARSER` while building this table (see this
// module's doc): each one locks in a fact about the grammar's precedence
// that is easy to get backwards (`%` looser than `+`, `~` tighter than
// `^`, `->` tighter than the unary prefixes, `-`/`!` chain with no parens).
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn precedence_regressions() {
    for src in [
        "-a->b",
        "a->-b",
        "a^b~c",
        "a*b^c",
        "a+b%c",
        "a==b&&c",
        "a||b&&c",
        "a~b~c",
        "a-b-c",
        "a^b^c",
        "!a->b",
        "-!a",
        "!-a",
        "a<b==c<d",
        "a+b*c-d/e%f^g~h",
        "(a+b)*c",
        "a*(b+c)",
        "-(a+b)",
        "-(a~b)",
        "(a-b)-c",
        "a-(b-c)",
    ] {
        assert_expr_round_trips(src);
    }
}

/// Multiple surface spellings of the same op collapse to ONE rendering:
/// the canonicalization law this whole module exists to enforce.
#[test]
fn surface_sugar_collapses_to_one_form() {
    assert_eq!(format_expr_of("a + b"), format_expr_of("add(a, b)"));
    assert_eq!(format_expr_of("a && b"), format_expr_of("and(a, b)"));
    assert_eq!(format_expr_of("-a"), format_expr_of("minus(a)"));
    assert_eq!(
        format_expr_of("if(a > 0, 1, 2)"),
        format_expr_of("cond(a > 0, 1, true, 2)")
    );
}

fn format_expr_of(src: &str) -> String {
    format_expr(&parse_expressions(src, &no_params()).expect("fixture parses"))
}

/// A whole-number float must round-trip as a float, not silently become an
/// int (`5.0.to_string() == "5"` in Rust, which would reparse as `pos_int`).
#[test]
fn whole_number_float_keeps_its_decimal_point() {
    let out = assert_expr_round_trips("to_float(\"NAN\")");
    assert_eq!(out, "to_float(\"NAN\")");
    let formatted = format_expr_of("5.0 + 1.5");
    assert!(formatted.contains("5.0"), "got: {formatted}");
}

/// `regex_matches(x, p)` hides an extra `OP_REGEX` wrap around `p` at
/// parse time (`Op::post_process_args`); the formatter must print `p`
/// alone, not the wrapper, or emit an unbound `regex(...)` call that fails
/// to resolve back to the same op on re-parse.
#[test]
fn hidden_regex_wrap_reverses_on_format() {
    let out = assert_expr_round_trips(r#"regex_matches(a, "x.*y")"#);
    assert_eq!(out, r#"regex_matches(a, "x.*y")"#);
}

#[test]
fn list_and_object_literals_round_trip() {
    assert_expr_round_trips("[1, 2, 3]");
    assert_expr_round_trips("[]");
    assert_expr_round_trips("{a: 1, b: 2}");
    assert_expr_round_trips("{}");
    assert_expr_round_trips("[a + b, [1, 2], {x: 1}]");
}

/// Every quoting style decodes to the same content and re-emits
/// double-quoted; a literal embedded quote/backslash/control character
/// round-trips through `write_str_literal`'s escaping (issue #93 fixed:
/// `raw_string` no longer shadows `quoted_string` for a fenceless `"..."`).
#[test]
fn string_escaping_round_trips() {
    assert_eq!(assert_expr_round_trips(r#""hello""#), r#""hello""#);
    assert_eq!(assert_expr_round_trips(r#""a\"b\\c""#), r#""a\"b\\c""#);
    assert_eq!(assert_expr_round_trips(r#""tab\ttab""#), r#""tab\ttab""#);
    assert_eq!(
        assert_expr_round_trips(r#""line\nbreak""#),
        r#""line\nbreak""#
    );
    // single-quoted and raw-string spellings decode to the same content
    // and re-emit double-quoted (one canonical quoting style).
    assert_eq!(assert_expr_round_trips(r#"'hello'"#), r#""hello""#);
    assert_eq!(assert_expr_round_trips(r##"_"hello"_"##), r#""hello""#);
}

// ─────────────────────────────────────────────────────────────────────────
// Whole-program regressions: rule heads with aggregation, options,
// negation/conjunction/disjunction nesting, `:create`.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn program_regressions() {
    for src in [
        "?[a] := *e[a, b];",
        "?[count(a)] := *e[a, b];",
        "?[a, b] := *e[a, b] or *n[a];",
        "?[a, b] := (*e[a, b], *n[a]) or *n[b];",
        "?[a] := not *n[a];",
        "?[a] := not not *n[a];",
        "?[a] := *n[a], a > 0, a != 5;\n:limit 10;\n:offset 2;\n",
        "?[a] := *n[a];\n:order -a;\n",
        "?[a] := *n[a];\n:disable_magic_rewrite true;\n",
        "h[a] := *n[a];\n?[a] := h[a];",
    ] {
        assert_program_round_trips(src);
    }
}

#[test]
fn create_relation_option_round_trips() {
    assert_program_round_trips(
        "?[a, b] := *n[a], b = a + 1;\n:create out {a: Int => b: Int = b};\n",
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Comment preservation (story #92's non-waivable DoD bullet, unblocked by
// story #30's grammar+AST trivia capture). Each case here mirrors one of
// `parse::mod`'s own trivia-attachment tests, checked from the OTHER end:
// not "did the parser attach the comment correctly" (that's proven there)
// but "does the formatter put it back exactly where it round-trips".
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn leading_comment_round_trips() {
    let out = assert_program_with_comments_round_trips("# a leading comment\n?[a] := *n[a];\n");
    assert!(out.contains("# a leading comment"));
}

#[test]
fn multiple_leading_comments_round_trip_in_order() {
    let out = assert_program_with_comments_round_trips(
        "# first\n# second\n/* third */\n?[a] := *n[a];\n",
    );
    let first = out.find("# first").expect("first comment present");
    let second = out.find("# second").expect("second comment present");
    let third = out.find("/* third */").expect("third comment present");
    assert!(
        first < second && second < third,
        "comments out of order:\n{out}"
    );
}

#[test]
fn trailing_comment_round_trips_on_its_own_line() {
    let out = assert_program_with_comments_round_trips("?[a] := *n[a]; # trailing\n");
    assert!(out.contains("*n[a]; # trailing"), "got:\n{out}");
}

/// Two rules, each keeps only its own neighboring comments — the BTreeMap's
/// alphabetical key order must not scramble which comment goes with which
/// rule (the same law `parse::mod`'s `each_rule_gets_its_own_neighboring_
/// comments` proves on the parse side).
#[test]
fn each_rule_keeps_its_own_comments_through_a_format_round_trip() {
    let out = assert_program_with_comments_round_trips(
        "# leads helper\nhelper[a] := *n[a]; # trails helper\n# leads entry\n?[a] := helper[a];\n",
    );
    assert!(out.contains("# leads helper"));
    assert!(out.contains("# trails helper"));
    assert!(out.contains("# leads entry"));
    // "leads helper" must still precede helper's own rule DEFINITION (not
    // the entry's "helper[a]" reference to it, which also contains the
    // substring "helper[" and — since this formatter renders the entry
    // FIRST — sits earlier in the output), and "leads entry" must still
    // precede the entry: even though the formatter reorders rules, a
    // misattachment that swapped the two comments would fail this.
    let leads_helper = out.find("# leads helper").unwrap();
    let helper_rule = out.find("helper[a] := *n[a]").unwrap();
    let leads_entry = out.find("# leads entry").unwrap();
    let entry_rule = out.find("?[a] := helper[a]").unwrap();
    assert!(leads_helper < helper_rule, "got:\n{out}");
    assert!(leads_entry < entry_rule, "got:\n{out}");
}

#[test]
fn whole_program_trailing_comment_round_trips() {
    let out = assert_program_with_comments_round_trips("?[a] := *n[a];\n# a footer comment\n");
    assert!(
        out.trim_end().ends_with("# a footer comment"),
        "got:\n{out}"
    );
}

/// The guardrail `parse::mod`'s own test pins from the parser side
/// (`comments_do_not_change_a_program_s_meaning`): the SAME program, with
/// and without comments, must format to byte-identical text through the
/// trivia-BLIND [`format_program`] — this is that guarantee's mirror,
/// checked from the formatter's own test file rather than only trusted
/// from the parser's.
#[test]
fn bare_format_is_unaffected_by_comments() {
    let bare = "h[a] := *n[a];\n?[a] := h[a];\n";
    let commented = "# header\nh[a] := *n[a]; # trails h\n# leads entry\n?[a] := h[a];\n# footer\n";
    let cur_vld = current_validity().expect("current validity");
    let bare_prog = match parse_script(bare, &no_params(), &no_fixed_rules(), cur_vld) {
        Ok(Script::Single(p)) => *p,
        other => panic!("fixture must parse: {other:?}"),
    };
    let commented_prog = match parse_script(commented, &no_params(), &no_fixed_rules(), cur_vld) {
        Ok(Script::Single(p)) => *p,
        other => panic!("fixture must parse: {other:?}"),
    };
    assert_eq!(format_program(&bare_prog), format_program(&commented_prog));
}

/// `FixedRuleApply`'s hand-written `Debug` (in `data/program.rs`) omits
/// `trivia`, so `assert_program_with_comments_round_trips`'s derived-Debug
/// oracle cannot see this node kind at all — checked directly here
/// instead, against a trivially-registered `SimpleFixedRule` (never
/// actually run; parsing only needs its name and arity to resolve).
#[test]
fn fixed_rule_trivia_round_trips() {
    let mut fixed_rules: BTreeMap<String, Arc<dyn FixedRule>> = BTreeMap::new();
    fixed_rules.insert(
        "algo".to_string(),
        // Named body — never run; parse/format only needs name + arity (P083).
        Arc::new(SimpleFixedRule::new(
            1,
            EmptyNamedRowsBody,
        )),
    );
    let src = "# leads algo\nh[a] <~ algo(); # trails algo\n?[a] := h[a];\n";
    let cur_vld = current_validity().expect("current validity");
    let prog = match parse_script(src, &no_params(), &fixed_rules, cur_vld) {
        Ok(Script::Single(p)) => *p,
        other => panic!("fixture must parse: {other:?}"),
    };
    let formatted = format_program_with_comments(&prog);
    assert!(formatted.contains("# leads algo"), "got:\n{formatted}");
    assert!(formatted.contains("# trails algo"), "got:\n{formatted}");
    let reparsed = match parse_script(&formatted, &no_params(), &fixed_rules, cur_vld) {
        Ok(Script::Single(p)) => *p,
        other => panic!("format produced unparseable text:\n{formatted}\n{other:?}"),
    };
    let trivia = |p: &kyzo_model::program::InputProgram| match p
        .rules()
        .get(&kyzo_model::program::symbol::Symbol::new(
            "h",
            kyzo_model::SourceSpan(0, 0),
        ))
        .expect("rule `h` present")
    {
        kyzo_model::program::InputInlineRulesOrFixed::Fixed { fixed } => fixed.trivia.clone(),
        other => panic!("expected `h` as a fixed rule, got {other:?}"),
    };
    let orig = trivia(&prog);
    let again = trivia(&reparsed);
    assert_eq!(comment_texts(&orig.leading), comment_texts(&again.leading));
    assert_eq!(
        comment_texts(&orig.trailing),
        comment_texts(&again.trailing)
    );
    let formatted_again = format_program_with_comments(&reparsed);
    assert_eq!(formatted, formatted_again, "not idempotent");
}

// ─────────────────────────────────────────────────────────────────────────
// Property tests: random expressions, and random small programs.
// ─────────────────────────────────────────────────────────────────────────

const VARS: [&str; 4] = ["a", "b", "c", "d"];
const BINOPS: [&str; 17] = [
    "+", "-", "*", "/", "%", "^", "==", "!=", ">", "<", ">=", "<=", "&&", "||", "~", "++", "->",
];
const UNOPS: [&str; 2] = ["-", "!"];

fn gen_expr(rng: &mut Rng, depth: u32) -> String {
    if depth == 0 || rng.chance(1, 3) {
        return gen_leaf(rng);
    }
    if rng.chance(1, 6) {
        let op = *rng.pick(&UNOPS);
        return format!("{op}{}", gen_expr(rng, depth - 1));
    }
    if rng.chance(1, 8) {
        return match rng.below(3) {
            0 => format!("abs({})", gen_expr(rng, depth - 1)),
            1 => format!(
                "min({}, {})",
                gen_expr(rng, depth - 1),
                gen_expr(rng, depth - 1)
            ),
            _ => format!(
                "atan2({}, {})",
                gen_expr(rng, depth - 1),
                gen_expr(rng, depth - 1)
            ),
        };
    }
    let op = *rng.pick(&BINOPS);
    format!(
        "{} {op} {}",
        gen_expr(rng, depth - 1),
        gen_expr(rng, depth - 1)
    )
}

/// String content deliberately including characters `write_str_literal`
/// must escape (`"`, `\`, newline, tab) — encoded here through that same
/// function, so the GENERATED source text is itself valid KyzoScript
/// (the initial parse must succeed before there is anything to round-trip).
const STRING_POOL: [&str; 7] = [
    "hello",
    "wor\"ld",
    "back\\slash",
    "line\nbreak",
    "tab\ttab",
    "kyzo",
    "",
];

fn gen_leaf(rng: &mut Rng) -> String {
    match rng.below(5) {
        0 => rng.pick(&VARS).to_string(),
        1 => rng.below(1000).to_string(),
        2 => format!("{}.5", rng.below(100)),
        3 => {
            let content = rng.pick(&STRING_POOL);
            let mut lit = String::new();
            super::write_str_literal(content, &mut lit);
            lit
        }
        _ => {
            let n = rng.below(3);
            let items: Vec<String> = (0..n).map(|_| gen_leaf(rng)).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

#[test]
fn expr_property_round_trips() {
    let mut rng = Rng::new(0xC0FFEE_u64);
    let mut checked = 0;
    for _ in 0..500 {
        let src = gen_expr(&mut rng, 5);
        // The generator can occasionally build a shape the grammar itself
        // refuses (e.g. the nesting-depth ceiling on a deep unary chain);
        // that is a generator artifact to skip past, not a formatter
        // concern — this suite is about what the parser DID accept.
        if parse_expressions(&src, &no_params()).is_err() {
            continue;
        }
        assert_expr_round_trips(&src);
        checked += 1;
    }
    assert!(
        checked > 400,
        "generator produced too few parseable expressions: {checked}"
    );
}

fn gen_body(rng: &mut Rng, helpers: &[(String, u64)]) -> String {
    let n_atoms = 1 + rng.below(3);
    let atoms: Vec<String> = (0..n_atoms).map(|_| gen_atom(rng, helpers)).collect();
    atoms.join(", ")
}

fn gen_atom(rng: &mut Rng, helpers: &[(String, u64)]) -> String {
    let negate = rng.chance(1, 5);
    let base = match rng.below(if helpers.is_empty() { 4 } else { 5 }) {
        0 => format!("*e[{}, {}]", rng.pick(&VARS), rng.pick(&VARS)),
        1 => format!("*n[{}]", rng.pick(&VARS)),
        2 => format!("{} = {}", rng.pick(&VARS), gen_expr(rng, 3)),
        3 => gen_expr(rng, 3),
        _ => {
            let (name, arity) = rng.pick(helpers).clone();
            let args: Vec<&str> = (0..arity).map(|_| *rng.pick(&VARS)).collect();
            format!("{name}[{}]", args.join(", "))
        }
    };
    if negate {
        format!("not {base}")
    } else if rng.chance(1, 6) {
        // an "or" with a second atom, exercising disjunction rendering
        format!("{base} or {}", gen_atom(rng, helpers))
    } else {
        base
    }
}

/// `with_comments`: when true, sprinkles a leading comment before each
/// rule/entry, a trailing comment on its own line, and a whole-program
/// footer comment — each independently at 1-in-2 odds, so a given
/// iteration exercises anywhere from zero comments up to every seat trivia
/// can attach to.
fn gen_program_text(rng: &mut Rng, with_comments: bool) -> String {
    let mut src = String::new();
    let n_helpers = rng.below(3);
    let mut helpers: Vec<(String, u64)> = Vec::new();
    for i in 0..n_helpers {
        let name = format!("h{i}");
        let arity = 1 + rng.below(2);
        let head: Vec<&str> = (0..arity).map(|j| VARS[j as usize]).collect();
        if with_comments && rng.chance(1, 2) {
            src.push_str(&format!("# leads {name}\n"));
        }
        src.push_str(&format!(
            "{name}[{}] := {}",
            head.join(", "),
            gen_body(rng, &helpers)
        ));
        src.push(';');
        if with_comments && rng.chance(1, 2) {
            src.push_str(&format!(" # trails {name}"));
        }
        src.push('\n');
        helpers.push((name, arity));
    }
    let entry_arity = 1 + rng.below(2);
    let head: Vec<&str> = (0..entry_arity).map(|j| VARS[j as usize]).collect();
    if with_comments && rng.chance(1, 2) {
        src.push_str("# leads entry\n");
    }
    src.push_str(&format!(
        "?[{}] := {}",
        head.join(", "),
        gen_body(rng, &helpers)
    ));
    src.push(';');
    if with_comments && rng.chance(1, 2) {
        src.push_str(" # trails entry");
    }
    src.push('\n');
    if rng.chance(1, 2) {
        src.push_str(&format!(":limit {};\n", rng.below(50) + 1));
    }
    if rng.chance(1, 3) {
        src.push_str(&format!(":offset {};\n", rng.below(10)));
    }
    if rng.chance(1, 3) {
        let dir = if rng.chance(1, 2) { "-" } else { "" };
        src.push_str(&format!(":order {dir}{};\n", VARS[0]));
    }
    if with_comments && rng.chance(1, 2) {
        src.push_str("# a footer comment\n");
    }
    src
}

#[test]
fn program_property_round_trips() {
    let mut rng = Rng::new(0xFEED_FACE_u64);
    for i in 0..300 {
        let src = gen_program_text(&mut rng, false);
        // `assert_program_round_trips` panics with the source/formatted
        // text on any failure; the iteration count is folded into that
        // panic by generating fresh (seed-derived) text each time, so a
        // failing seed is exactly the printed `src`.
        let _ = i;
        assert_program_round_trips(&src);
    }
}

#[test]
fn program_with_comments_property_round_trips() {
    let mut rng = Rng::new(0xC0DE_CAFE_u64);
    for i in 0..300 {
        let src = gen_program_text(&mut rng, true);
        let _ = i;
        assert_program_with_comments_round_trips(&src);
    }
}

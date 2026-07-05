/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The formatter: `InputProgram` → one canonical KyzoScript source text.
//!
//! `parse` is text-becomes-proof; this is proof-becomes-one-true-text — the
//! same determinism law the rest of the engine holds (one memcomparable
//! byte order, one fixpoint admission order) restated at the language
//! surface. Every surface spelling that means the same tree collapses to
//! ONE spelling: `add(a, b)` and `a + b` both print as `a + b`; `if(c, t)`
//! and `cond(c, t, true, null)` both print as `cond(c, t, true, null)`;
//! every string literal — however it was quoted — prints double-quoted.
//! [`format_program`] is total over any [`InputProgram`] a real parse can
//! produce; it never inspects source text or spans.
//!
//! **Two laws, proven by [`tests`]'s property suite over generated
//! programs**: idempotence (`fmt(fmt(x)) == fmt(x)`) and meaning-preserving
//! round-trip (`parse(fmt(x))` is the same tree as `x`, checked against the
//! parser's own derived `Debug` — an oracle this module never touches).
//!
//! **Expression precedence is the hard part**, because this grammar's
//! table is not the arithmetic-textbook one: `%` binds *looser* than `+`/
//! `-`/`++`, `~` (coalesce) binds *tighter* than `^`, and `->` (field
//! access) binds tighter than the unary prefixes. The table below
//! ([`infix_form`]/[`prefix_form`]) is transcribed from `parse/expr.rs`'s
//! `PRATT_PARSER` and empirically confirmed against it (see that module's
//! history) — a grammar precedence change must edit both tables, or this
//! formatter will parenthesize wrongly (still safe, just no longer
//! minimal) or, if the tables disagree about *associativity*, will emit
//! wrong-meaning text. Only [`std::fmt`]-free hand-written string building
//! is used, precisely so this module never depends on any other type's own
//! `Display` (several of those — `Expr`'s included — are Debug-oriented
//! prefix-call dumps, or use Rust's `\u{..}` string escaping, neither of
//! which is valid or round-trippable KyzoScript source).
//!
//! **One hidden AST rewrite to know about** ([`Op::post_process_args`]):
//! every `OP_REGEX_*` op's pattern argument is wrapped in an invisible
//! `OP_REGEX` application at parse time (`regex_matches(x, p)` parses to
//! `regex_matches(x, regex(p))` in the tree, though `regex(...)` is not
//! itself callable — `OP_REGEX` has no `get_op` entry). This module
//! reverses exactly that one rewrite when printing ([`unwrap_hidden_regex_arg`]),
//! so the pattern prints as the user wrote it and re-parsing re-applies the
//! identical hidden wrap.
//!
//! **Named limitation: comments are not preserved.** `kyzoscript.pest`
//! marks `COMMENT` silent (`BLOCK_COMMENT`/`LINE_COMMENT` under the
//! implicit `WHITESPACE`/`COMMENT` rule pest skips automatically) — no
//! parse-tree node is ever created for one, so by the time an
//! [`InputProgram`] exists, every comment in the original source is
//! already gone. A formatter operating on `InputProgram` (this module's
//! whole contract) cannot recover what the parser never kept; preserving
//! comments needs the grammar itself to capture them as trivia attached to
//! AST nodes — a `parse`-tier and `data::program`-tier design change, not a
//! rendering one. Stated here rather than silently unmet.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::data::aggr::Aggregation;
use crate::data::expr::{Expr, LazyOp};
use crate::data::program::{
    Comment, DeltaAxis, FixedRuleApply, FixedRuleArg, InputAtom, InputInlineRule,
    InputInlineRulesOrFixed, InputNamedFieldRelationApplyAtom, InputProgram,
    InputRelationApplyAtom, InputRelationHandle, InputRuleApplyAtom, QueryAssertion,
    QueryOutOptions, RelationOp, ReturnMutation, SearchInput, Unification, ValidityClause,
    WriteValidity,
};
use crate::data::relation::ColumnDef;
use crate::data::symb::Symbol;
use crate::data::value::{AsOf, DataValue, MAX_VALIDITY_TS, Num, ValidityTs, Vector};

// ─────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────

/// Whether a render also emits comment trivia. `Bare` is what
/// [`format_program`] always uses: the trivia-blind canonical form
/// `parse::mod`'s own guardrail test
/// (`comments_do_not_change_a_program_s_meaning`) depends on staying
/// byte-identical whether or not the source had comments in it — this
/// mode must never read a `trivia` field. `WithComments`
/// ([`format_program_with_comments`]) is the same rendering with trivia
/// placed back where it was captured from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TriviaMode {
    Bare,
    WithComments,
}

/// Render one program to its canonical KyzoScript source text — comments
/// dropped (see [`format_program_with_comments`] for the trivia-preserving
/// twin; both share every non-trivia rendering decision below, so they can
/// never disagree about what the *meaning* prints as). The entry rule
/// prints first (every hand-written query in this tree leads with `?` —
/// the existing `Display for InputProgram` in `data/program.rs` prints it
/// last, but that ordering is documented there as "cosmetic, nothing
/// re-parses it"; this module is free to, and does, choose the more
/// idiomatic order), then every other rule sorted by name (the `BTreeMap`
/// this program already stores them in), then the `:options`.
pub(crate) fn format_program(prog: &InputProgram) -> String {
    format_program_inner(prog, TriviaMode::Bare)
}

/// [`format_program`]'s canonical text, with every comment the parser
/// captured ([`crate::data::program::Trivia`]) rendered back: leading
/// comments each on their own line immediately before the rule/fixed-rule
/// application they attached to, trailing comments appended (space-
/// separated) on that construct's own output line, and the whole
/// program's leading/trailing overflow trivia at the very start/end.
pub(crate) fn format_program_with_comments(prog: &InputProgram) -> String {
    format_program_inner(prog, TriviaMode::WithComments)
}

fn format_program_inner(prog: &InputProgram, mode: TriviaMode) -> String {
    let mut out = String::new();
    if mode == TriviaMode::WithComments {
        write_comment_lines(&prog.leading_trivia, &mut out);
    }
    write_ruleset(prog.entry_name(), prog.entry(), mode, &mut out);
    for (name, ruleset) in prog.rules() {
        write_ruleset(name, ruleset, mode, &mut out);
    }
    write_out_opts(prog.out_opts(), prog.disable_magic_rewrite(), &mut out);
    if mode == TriviaMode::WithComments {
        write_comment_lines(&prog.trailing_trivia, &mut out);
    }
    out
}

/// Every comment, verbatim (delimiters already in `Comment::text`), one
/// per line, in source order.
fn write_comment_lines(comments: &[Comment], out: &mut String) {
    for c in comments {
        out.push_str(&c.text);
        out.push('\n');
    }
}

/// Render one expression alone (no program around it) — the unit the
/// property tests attack directly, and generally useful wherever a single
/// canonical expression's text is wanted independent of a whole program.
pub(crate) fn format_expr(e: &Expr) -> String {
    let mut out = String::new();
    write_expr(e, &mut out);
    out
}

fn write_ruleset(
    name: &Symbol,
    ruleset: &InputInlineRulesOrFixed,
    mode: TriviaMode,
    out: &mut String,
) {
    match ruleset {
        InputInlineRulesOrFixed::Rules { rules } => {
            for rule in rules {
                if mode == TriviaMode::WithComments {
                    write_comment_lines(&rule.trivia.leading, out);
                }
                write_inline_rule(name, rule, out);
                finish_construct_line(mode, &rule.trivia.trailing, out);
            }
        }
        InputInlineRulesOrFixed::Fixed { fixed } => {
            if mode == TriviaMode::WithComments {
                write_comment_lines(&fixed.trivia.leading, out);
            }
            write_fixed_rule(name, fixed, out);
            finish_construct_line(mode, &fixed.trivia.trailing, out);
        }
    }
}

/// Closes one rule/fixed-rule's output line: its trailing comments
/// (space-separated, in `WithComments` mode only), then the newline every
/// construct ends on regardless of mode.
fn finish_construct_line(mode: TriviaMode, trailing: &[Comment], out: &mut String) {
    if mode == TriviaMode::WithComments {
        for c in trailing {
            out.push(' ');
            out.push_str(&c.text);
        }
    }
    out.push('\n');
}

fn write_inline_rule(name: &Symbol, rule: &InputInlineRule, out: &mut String) {
    out.push_str(&name.name);
    out.push('[');
    for (i, (head, aggr)) in rule.head.iter().zip(rule.aggr.iter()).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_head_arg(head, aggr.as_ref(), out);
    }
    out.push_str("] := ");
    for (i, atom) in rule.body.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_conjunct_member(atom, out);
    }
    out.push(';');
}

fn write_head_arg(head: &Symbol, aggr: Option<&(Aggregation, Vec<DataValue>)>, out: &mut String) {
    match aggr {
        None => out.push_str(&head.name),
        Some((aggr, args)) => {
            out.push_str(aggr.name);
            out.push('(');
            out.push_str(&head.name);
            for a in args {
                out.push_str(", ");
                write_const(a, out);
            }
            out.push(')');
        }
    }
}

fn write_fixed_rule(name: &Symbol, fixed: &FixedRuleApply, out: &mut String) {
    out.push_str(&name.name);
    out.push('[');
    for (i, h) in fixed.head.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&h.name);
    }
    out.push_str("] <~ ");
    out.push_str(&fixed.fixed_handle.name.name);
    out.push('(');
    let mut first = true;
    for arg in &fixed.rule_args {
        if !first {
            out.push_str(", ");
        }
        first = false;
        write_fixed_rule_arg(arg, out);
    }
    for (k, v) in fixed.options.as_ref() {
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push_str(k);
        out.push_str(": ");
        write_expr(v, out);
    }
    out.push_str(");");
}

fn write_fixed_rule_arg(arg: &FixedRuleArg, out: &mut String) {
    match arg {
        FixedRuleArg::InMem { name, bindings, .. } => {
            out.push_str(&name.name);
            write_var_bracket_list(bindings, out);
        }
        FixedRuleArg::Stored {
            name,
            bindings,
            as_of,
            ..
        } => {
            out.push('*');
            out.push_str(&name.name);
            out.push('[');
            for (i, b) in bindings.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&b.name);
            }
            if let Some(as_of) = as_of {
                out.push_str(", ");
                write_at_clause(as_of, out);
            }
            out.push(']');
        }
        FixedRuleArg::NamedStored {
            name,
            bindings,
            as_of,
            ..
        } => {
            out.push('*');
            out.push_str(&name.name);
            out.push('{');
            for (i, (k, v)) in bindings.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(k);
                out.push_str(": ");
                out.push_str(&v.name);
            }
            if let Some(as_of) = as_of {
                out.push_str(", ");
                write_at_clause(as_of, out);
            }
            out.push('}');
        }
    }
}

fn write_var_bracket_list(vars: &[Symbol], out: &mut String) {
    out.push('[');
    for (i, v) in vars.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&v.name);
    }
    out.push(']');
}

// ─────────────────────────────────────────────────────────────────────────
// Atoms: a rule body is a `Vec<InputAtom>`, comma-joined (the grammar's
// implicit outer conjunction); this section renders one member.
//
// The AND/OR nesting law (from `rule_body`/`disjunction` in
// `kyzoscript.pest`): a comma-separated slot IS already a `disjunction`
// production, so an `InputAtom::Disjunction` prints bare, no parens, at
// this level or nested one level inside a `Conjunction`. An
// `InputAtom::Conjunction` used as ONE member of a `Disjunction`'s "or"
// chain has no bare spelling — the grammar reaches it only through
// `grouped` (`"(" rule_body ")"`) — so it always needs explicit parens
// there. Same-kind nesting (a `Conjunction` inside a `Conjunction`, an
// `Disjunction` inside a `Disjunction` — reachable only via a user's own
// redundant parens) is flattened rather than re-parenthesized: it is
// associative, so flattening is meaning-preserving and strictly more
// canonical (no redundant parens survive a format pass).
// ─────────────────────────────────────────────────────────────────────────

/// One member of an implicit top-level (or already-flattened) conjunction:
/// a bare atom, or an "or" chain, printed without wrapping parens (both are
/// valid `disjunction` productions directly).
fn write_conjunct_member(atom: &InputAtom, out: &mut String) {
    match atom {
        InputAtom::Conjunction { inner, .. } => {
            let mut members = Vec::new();
            flatten_conjunction(inner, &mut members);
            for (i, m) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_conjunct_member(m, out);
            }
        }
        InputAtom::Disjunction { inner, .. } => {
            let mut members = Vec::new();
            flatten_disjunction(inner, &mut members);
            for (i, m) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(" or ");
                }
                write_disjunct_member(m, out);
            }
        }
        _ => write_plain_atom(atom, out),
    }
}

/// One member of an "or" chain: a bare atom, or a parenthesized AND-group
/// (the only way the grammar admits one there).
fn write_disjunct_member(atom: &InputAtom, out: &mut String) {
    match atom {
        InputAtom::Conjunction { inner, .. } => {
            let mut members = Vec::new();
            flatten_conjunction(inner, &mut members);
            out.push('(');
            for (i, m) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_conjunct_member(m, out);
            }
            out.push(')');
        }
        _ => write_plain_atom(atom, out),
    }
}

fn flatten_conjunction<'a>(members: &'a [InputAtom], out: &mut Vec<&'a InputAtom>) {
    for m in members {
        match m {
            InputAtom::Conjunction { inner, .. } => flatten_conjunction(inner, out),
            other => out.push(other),
        }
    }
}

fn flatten_disjunction<'a>(members: &'a [InputAtom], out: &mut Vec<&'a InputAtom>) {
    for m in members {
        match m {
            InputAtom::Disjunction { inner, .. } => flatten_disjunction(inner, out),
            other => out.push(other),
        }
    }
}

/// An atom that is never itself a `Conjunction`/`Disjunction`: a rule/
/// relation application, a predicate, a unification, a search, or a
/// negation of one of those (recursively — `negation = {not_op ~ atom}`,
/// and `atom`'s own alternatives include `negation`, so `not not x` needs
/// no parens either).
fn write_plain_atom(atom: &InputAtom, out: &mut String) {
    match atom {
        InputAtom::Negation { inner, .. } => {
            out.push_str("not ");
            match inner.as_ref() {
                // `not` accepts one `atom` production directly; a
                // Conjunction/Disjunction inner needs the same `grouped`
                // parens a disjunction member would.
                InputAtom::Conjunction { .. } | InputAtom::Disjunction { .. } => {
                    write_disjunct_member(inner, out);
                }
                _ => write_plain_atom(inner, out),
            }
        }
        InputAtom::Rule {
            inner: InputRuleApplyAtom { name, args, .. },
        } => {
            out.push_str(&name.name);
            out.push('[');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(a, out);
            }
            out.push(']');
        }
        InputAtom::NamedFieldRelation {
            inner:
                InputNamedFieldRelationApplyAtom {
                    name,
                    args,
                    validity,
                    ..
                },
        } => {
            out.push('*');
            out.push_str(&name.name);
            out.push('{');
            for (i, (k, v)) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(k);
                out.push_str(": ");
                write_expr(v, out);
            }
            out.push('}');
            write_read_validity(validity.as_ref(), out);
        }
        InputAtom::Relation {
            inner:
                InputRelationApplyAtom {
                    name,
                    args,
                    validity,
                    ..
                },
        } => {
            out.push('*');
            out.push_str(&name.name);
            out.push('[');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(a, out);
            }
            out.push(']');
            write_read_validity(validity.as_ref(), out);
        }
        InputAtom::Predicate { inner } => write_expr(inner, out),
        InputAtom::Unification { inner } => write_unification(inner, out),
        InputAtom::Search { inner } => write_search(inner, out),
        InputAtom::Conjunction { .. } | InputAtom::Disjunction { .. } => {
            // Only reachable if a caller forgets to flatten first; every
            // call site above goes through `write_conjunct_member`/
            // `write_disjunct_member` instead, so this stays unreached in
            // practice. Still total: render it exactly the way a
            // disjunction member would.
            write_disjunct_member(atom, out);
        }
    }
}

fn write_unification(u: &Unification, out: &mut String) {
    out.push_str(&u.binding.name);
    out.push_str(if u.one_many_unif { " in " } else { " = " });
    write_expr(&u.expr, out);
}

fn write_search(s: &SearchInput, out: &mut String) {
    out.push('~');
    out.push_str(&s.relation.name);
    out.push(':');
    out.push_str(&s.index.name);
    out.push('{');
    for (i, (k, v)) in s.bindings.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(k);
        out.push_str(": ");
        write_expr(v, out);
    }
    out.push_str(" | ");
    for (i, (k, v)) in s.parameters.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(k);
        out.push_str(": ");
        write_expr(v, out);
    }
    out.push('}');
}

// ─────────────────────────────────────────────────────────────────────────
// Validity clauses (`@ …` / `@spans …` / `@delta(…) …` / `@delta_sys(…) …`)
// ─────────────────────────────────────────────────────────────────────────

fn write_read_validity(validity: Option<&ValidityClause>, out: &mut String) {
    let Some(validity) = validity else { return };
    out.push(' ');
    match validity {
        ValidityClause::At(as_of) => write_at_clause(as_of, out),
        ValidityClause::Spans { sys, var } => {
            out.push_str("@spans ");
            out.push_str(&var.name);
            if *sys != MAX_VALIDITY_TS {
                out.push_str(", ");
                write_vld(*sys, out);
            }
        }
        ValidityClause::Delta {
            axis,
            from,
            to,
            var,
        } => {
            out.push_str(match axis {
                DeltaAxis::Valid => "@delta(",
                DeltaAxis::Sys => "@delta_sys(",
            });
            write_vld(*from, out);
            out.push_str(", ");
            write_vld(*to, out);
            out.push_str(") ");
            out.push_str(&var.name);
        }
    }
}

/// `@ valid` (sys defaults to the record's current belief) or
/// `@ sys, valid` — matches `parse_at_expr_clause`'s coordinate order
/// exactly (`@ system, valid` when two are given).
fn write_at_clause(as_of: &AsOf, out: &mut String) {
    out.push('@');
    out.push(' ');
    if as_of.sys == MAX_VALIDITY_TS {
        write_vld(as_of.valid, out);
    } else {
        write_vld(as_of.sys, out);
        out.push_str(", ");
        write_vld(as_of.valid, out);
    }
}

/// A validity coordinate, canonically the raw microsecond integer — the
/// same choice the pre-existing `WriteValidity::Fixed` rendering in
/// `data/program.rs` makes (`ts.0.0`), which `data_value_to_vld_spec`
/// accepts directly as `DataValue::Num`.
fn write_vld(ts: ValidityTs, out: &mut String) {
    out.push_str(&ts.0.0.to_string());
}

// ─────────────────────────────────────────────────────────────────────────
// Query options
// ─────────────────────────────────────────────────────────────────────────

fn write_out_opts(opts: &QueryOutOptions, disable_magic_rewrite: bool, out: &mut String) {
    if disable_magic_rewrite {
        out.push_str(":disable_magic_rewrite true;\n");
    }
    if let Some(l) = opts.limit {
        out.push_str(":limit ");
        out.push_str(&l.to_string());
        out.push_str(";\n");
    }
    if let Some(o) = opts.offset {
        out.push_str(":offset ");
        out.push_str(&o.to_string());
        out.push_str(";\n");
    }
    if let Some(t) = opts.timeout {
        out.push_str(":timeout ");
        write_float(t, out);
        out.push_str(";\n");
    }
    if let Some(s) = opts.sleep {
        out.push_str(":sleep ");
        write_float(s, out);
        out.push_str(";\n");
    }
    if !opts.sorters.is_empty() {
        out.push_str(":order ");
        for (i, (symb, dir)) in opts.sorters.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            if *dir == crate::data::program::SortDir::Dsc {
                out.push('-');
            }
            out.push_str(&symb.name);
        }
        out.push_str(";\n");
    }
    if let Some((handle, op, return_mutation, write_vld)) = &opts.store_relation {
        if *return_mutation == ReturnMutation::Returning {
            out.push_str(":returning;\n");
        }
        write_relation_option(handle, *op, write_vld, out);
    }
    if let Some(a) = &opts.assertion {
        out.push_str(match a {
            QueryAssertion::AssertNone(_) => ":assert none;\n",
            QueryAssertion::AssertSome(_) => ":assert some;\n",
        });
    }
}

fn write_relation_option(
    handle: &InputRelationHandle,
    op: RelationOp,
    write_vld: &WriteValidity,
    out: &mut String,
) {
    out.push_str(match op {
        RelationOp::Create => ":create ",
        RelationOp::Replace => ":replace ",
        RelationOp::Put => ":put ",
        RelationOp::Insert => ":insert ",
        RelationOp::Update => ":update ",
        RelationOp::Rm => ":rm ",
        RelationOp::Delete => ":delete ",
        RelationOp::Ensure => ":ensure ",
        RelationOp::EnsureNot => ":ensure_not ",
    });
    out.push_str(&handle.name.name);
    out.push_str(" {");
    write_table_cols(&handle.metadata.keys, &handle.key_bindings, out);
    out.push_str(" => ");
    write_table_cols(&handle.metadata.non_keys, &handle.dep_bindings, out);
    out.push('}');
    match write_vld {
        WriteValidity::Now => {}
        WriteValidity::Fixed(ts) => {
            out.push_str(" @ ");
            write_vld_ts(*ts, out);
        }
        WriteValidity::PerRow(expr) => {
            out.push_str(" @ ");
            write_expr(expr, out);
        }
    }
    out.push_str(";\n");
}

fn write_vld_ts(ts: ValidityTs, out: &mut String) {
    out.push_str(&ts.0.0.to_string());
}

fn write_table_cols(cols: &[ColumnDef], bindings: &[Symbol], out: &mut String) {
    for (i, (col, bind)) in cols.iter().zip(bindings).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&col.name);
        out.push_str(": ");
        out.push_str(&col.typing.to_string());
        match &col.default_gen {
            Some(generator) => {
                out.push_str(" default ");
                write_expr(generator, out);
            }
            None => {
                out.push_str(" = ");
                out.push_str(&bind.name);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Expressions — the precedence-driven part.
// ─────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Assoc {
    Left,
    Right,
}

/// One past the tightest real precedence level (`->`, at 12): every
/// "atomic" rendering (literals, bindings, function/list/object calls,
/// `cond`) never needs parens as anyone's operand.
const ATOMIC: u8 = 100;

/// `(precedence, associativity, surface symbol)` for a 2-argument
/// [`Expr::Apply`]/[`Expr::Lazy`] this grammar also accepts as an infix
/// operator — `None` for anything that must render as a function call
/// (wrong arity for its op, or no infix spelling at all). Numbers match
/// `parse/expr.rs`'s `PRATT_PARSER` table exactly, confirmed empirically
/// against it (higher binds tighter): `||`1 `&&`2 cmp3 `==`/`!=`4 `%`5
/// `+`/`-`/`++`6 `*`/`/`7 `^`8 `~`9 — then the unary prefixes at 10/11 (see
/// [`prefix_form`]) and `->` at 12 tighter still.
fn infix_form(e: &Expr) -> Option<(u8, Assoc, &'static str)> {
    match e {
        Expr::Apply { op, args, .. } if args.len() == 2 => Some(match op.name {
            "OP_GT" => (3, Assoc::Left, ">"),
            "OP_LT" => (3, Assoc::Left, "<"),
            "OP_GE" => (3, Assoc::Left, ">="),
            "OP_LE" => (3, Assoc::Left, "<="),
            "OP_EQ" => (4, Assoc::Left, "=="),
            "OP_NEQ" => (4, Assoc::Left, "!="),
            "OP_MOD" => (5, Assoc::Left, "%"),
            "OP_ADD" => (6, Assoc::Left, "+"),
            "OP_SUB" => (6, Assoc::Left, "-"),
            "OP_CONCAT" => (6, Assoc::Left, "++"),
            "OP_MUL" => (7, Assoc::Left, "*"),
            "OP_DIV" => (7, Assoc::Left, "/"),
            "OP_POW" => (8, Assoc::Right, "^"),
            "OP_MAYBE_GET" => (12, Assoc::Left, "->"),
            _ => return None,
        }),
        Expr::Lazy { op, args, .. } if args.len() == 2 => Some(match op {
            LazyOp::Or => (1, Assoc::Left, "||"),
            LazyOp::And => (2, Assoc::Left, "&&"),
            LazyOp::Coalesce => (9, Assoc::Left, "~"),
        }),
        _ => None,
    }
}

/// `(precedence, prefix symbol)` for a 1-argument [`Expr::Apply`] this
/// grammar also accepts as a unary prefix. A prefix's own child, when it
/// is itself another prefix application, never needs parens regardless of
/// these numbers (see [`write_prefix_operand`]): `unary_op*` in the
/// grammar is a plain repetition glued to one term, not a recursive
/// precedence climb, so a straight prefix chain always re-parses to the
/// same nesting order it was written in.
fn prefix_form(e: &Expr) -> Option<(u8, &'static str)> {
    match e {
        Expr::Apply { op, args, .. } if args.len() == 1 => match op.name {
            "OP_MINUS" => Some((10, "-")),
            "OP_NEGATE" => Some((11, "!")),
            _ => None,
        },
        _ => None,
    }
}

fn precedence(e: &Expr) -> u8 {
    if let Some((p, ..)) = infix_form(e) {
        return p;
    }
    if let Some((p, _)) = prefix_form(e) {
        return p;
    }
    ATOMIC
}

fn write_expr(e: &Expr, out: &mut String) {
    if let Some((prec, assoc, sym)) = infix_form(e) {
        let args: &[Expr] = match e {
            Expr::Apply { args, .. } => args,
            Expr::Lazy { args, .. } => args,
            _ => unreachable!("infix_form only returns Some for Apply/Lazy"),
        };
        write_operand(&args[0], prec, assoc == Assoc::Left, out);
        out.push(' ');
        out.push_str(sym);
        out.push(' ');
        write_operand(&args[1], prec, assoc == Assoc::Right, out);
        return;
    }
    if let Some((prec, sym)) = prefix_form(e) {
        out.push_str(sym);
        let inner = match e {
            Expr::Apply { args, .. } => &args[0],
            _ => unreachable!("prefix_form only returns Some for Apply"),
        };
        write_prefix_operand(inner, prec, out);
        return;
    }
    write_atom_expr(e, out);
}

/// `child` on one side of a binary operator at `parent_prec`. `keeps_equal`
/// is whether THIS side may print an equal-precedence child bare — true
/// for the side matching the operator's own associativity (the left side
/// of a left-associative op, the right side of a right-associative one) —
/// since that is exactly how re-parsing would re-nest it; the other side
/// needs parens at equal precedence or it would silently re-associate.
fn write_operand(child: &Expr, parent_prec: u8, keeps_equal: bool, out: &mut String) {
    let child_prec = precedence(child);
    let needs_parens = if keeps_equal {
        child_prec < parent_prec
    } else {
        child_prec <= parent_prec
    };
    if needs_parens {
        out.push('(');
        write_expr(child, out);
        out.push(')');
    } else {
        write_expr(child, out);
    }
}

/// A prefix operator's own operand. Another prefix chains with no parens,
/// ever (see [`prefix_form`]'s doc); anything else follows the ordinary
/// operand rule with no equal-precedence exception (no infix op shares a
/// prefix's precedence number, so this only ever matters for `<`).
fn write_prefix_operand(child: &Expr, parent_prec: u8, out: &mut String) {
    if prefix_form(child).is_some() {
        write_expr(child, out);
    } else {
        write_operand(child, parent_prec, false, out);
    }
}

fn write_atom_expr(e: &Expr, out: &mut String) {
    match e {
        Expr::Binding { var, .. } => out.push_str(&var.name),
        Expr::Const { val, .. } => write_const(val, out),
        Expr::Apply { op, args, .. } => match op.name {
            "OP_LIST" => write_bracketed(args, out),
            "OP_JSON_OBJECT" => write_object(args, out),
            _ => write_apply_call(op.name, args, out),
        },
        Expr::UnboundApply { op, args, .. } => write_call(op, args, out),
        Expr::Lazy { op, args, .. } => {
            let name = match op {
                LazyOp::And => "and",
                LazyOp::Or => "or",
                LazyOp::Coalesce => "coalesce",
            };
            write_call(name, args, out);
        }
        Expr::Cond { clauses, .. } => {
            out.push_str("cond(");
            for (i, (c, v)) in clauses.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(c, out);
                out.push_str(", ");
                write_expr(v, out);
            }
            out.push(')');
        }
    }
}

fn write_call(name: &str, args: &[Expr], out: &mut String) {
    out.push_str(name);
    out.push('(');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_expr(a, out);
    }
    out.push(')');
}

/// A named-op function call, reversing the one hidden AST rewrite in this
/// tree ([`Op::post_process_args`]): every `OP_REGEX_*` op's second
/// argument is wrapped in an invisible `OP_REGEX` application at parse
/// time, so the pattern prints as the user wrote it (and re-parsing
/// re-applies the identical wrap unconditionally).
fn write_apply_call(op_name: &'static str, args: &[Expr], out: &mut String) {
    let name = crate::data::expr::op_display_name(op_name);
    out.push_str(&name);
    out.push('(');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if i == 1
            && let Some(pattern) = unwrap_hidden_regex_arg(op_name, a)
        {
            write_expr(pattern, out);
            continue;
        }
        write_expr(a, out);
    }
    out.push(')');
}

fn unwrap_hidden_regex_arg<'a>(op_name: &str, arg: &'a Expr) -> Option<&'a Expr> {
    if !op_name.starts_with("OP_REGEX_") {
        return None;
    }
    match arg {
        Expr::Apply { op, args, .. } if op.name == "OP_REGEX" && args.len() == 1 => Some(&args[0]),
        _ => None,
    }
}

fn write_bracketed(args: &[Expr], out: &mut String) {
    out.push('[');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_expr(a, out);
    }
    out.push(']');
}

fn write_object(args: &[Expr], out: &mut String) {
    out.push('{');
    for (i, pair) in args.chunks_exact(2).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_expr(&pair[0], out);
        out.push_str(": ");
        write_expr(&pair[1], out);
    }
    out.push('}');
}

// ─────────────────────────────────────────────────────────────────────────
// Constants: any `DataValue` as a KyzoScript expression.
// ─────────────────────────────────────────────────────────────────────────

/// Any `DataValue` as KyzoScript source. Only `Null`/`Bool`/`Num`/`Str` have
/// direct literal syntax (`literal = null | boolean | number | string` in
/// the grammar); everything else round-trips through the constructor
/// function that builds it (`decode_base64`, `to_uuid`, `regex`,
/// `make_interval`, `vec`, `parse_json`) — the same functions a user would
/// call, chosen so re-parsing produces the identical `DataValue`. `Set` and
/// `Validity`/`Bot` have no KyzoScript constructor at all (a `Set` comes
/// only from an aggregation result, never from an expression a parser
/// builds); a `Const` holding one is reachable only via a `$param` of that
/// exact `DataValue` shape, and this rendering, honestly, does not
/// round-trip it — there is no source text that would.
fn write_const(val: &DataValue, out: &mut String) {
    match val {
        DataValue::Null | DataValue::Bot => out.push_str("null"),
        DataValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        DataValue::Num(Num::Int(i)) => out.push_str(&i.to_string()),
        DataValue::Num(Num::Float(f)) => write_float(*f, out),
        DataValue::Str(s) => write_str_literal(s, out),
        DataValue::Bytes(b) => {
            out.push_str("decode_base64(");
            write_str_literal(&STANDARD.encode(b), out);
            out.push(')');
        }
        DataValue::Uuid(u) => {
            out.push_str("to_uuid(");
            write_str_literal(&u.0.to_string(), out);
            out.push(')');
        }
        DataValue::Regex(rx) => {
            out.push_str("regex(");
            write_str_literal(rx.0.as_str(), out);
            out.push(')');
        }
        DataValue::List(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_const(v, out);
            }
            out.push(']');
        }
        DataValue::Set(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_const(v, out);
            }
            out.push(']');
        }
        DataValue::Validity(v) => {
            out.push_str("validity(");
            out.push_str(&v.timestamp.0.0.to_string());
            out.push_str(", ");
            out.push_str(if v.is_assert.0 { "true" } else { "false" });
            out.push(')');
        }
        DataValue::Interval(iv) => {
            out.push_str("make_interval(");
            out.push_str(&iv.start().to_string());
            out.push_str(", ");
            out.push_str(&iv.end().to_string());
            out.push(')');
        }
        DataValue::Vec(Vector::F32(a)) => {
            out.push_str("vec([");
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_float(*x as f64, out);
            }
            out.push_str("])");
        }
        DataValue::Vec(Vector::F64(a)) => {
            out.push_str("vec([");
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_float(*x, out);
            }
            out.push_str("], \"F64\")");
        }
        DataValue::Json(j) => {
            out.push_str("parse_json(");
            write_str_literal(&j.0.to_string(), out);
            out.push(')');
        }
    }
}

/// An `f64` as a `dot_float`/`sci_float` token — never as a bare `pos_int`.
/// Rust's own `Display` for a whole-numbered float (`5.0.to_string() ==
/// "5"`) would silently reparse as `Num::Int`, not `Num::Float`: force a
/// decimal point onto any output that came out looking like a bare
/// integer. NaN/±Infinity have no literal at all (same grammar gap as
/// `Set`) and go through `to_float("NAN"|"INF"|"NEG_INF")`, this crate's
/// one existing convention for them (`data::value::Num`'s own `Display`).
fn write_float(f: f64, out: &mut String) {
    if f.is_nan() {
        out.push_str(r#"to_float("NAN")"#);
        return;
    }
    if f.is_infinite() {
        out.push_str(if f.is_sign_negative() {
            r#"to_float("NEG_INF")"#
        } else {
            r#"to_float("INF")"#
        });
        return;
    }
    let start = out.len();
    out.push_str(&f.to_string());
    if !out[start..].contains(['.', 'e', 'E']) {
        out.push_str(".0");
    }
}

/// A string literal, always double-quoted (whichever of the three quoting
/// styles the source used, `DataValue::Str` keeps only the decoded
/// content — quoting style is not part of the tree, so double-quoted is
/// the one canonical choice). Escapes exactly what `quoted_string`'s
/// `char` production requires unescaped (`"`, `\`) plus the named
/// short escapes and a `\uXXXX` fallback for other control characters;
/// anything else — including astral-plane characters, which `\uXXXX` (4
/// hex digits) cannot represent at all — is written out literally, which
/// the grammar's `ANY` in `char` already permits unescaped.
fn write_str_literal(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests;

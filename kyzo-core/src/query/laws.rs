/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The reference semantics of stratified Datalog, as executable law.
//!
//! Everything here is deliberately naive: no indexes, no deltas, no rewrites
//! — just the textbook fixpoint, written to be *obviously* correct. The real
//! engine's optimized evaluation must produce byte-identical answer sets to
//! this oracle on every program the differential tests generate. The oracle
//! is judge, never production code (`cfg(test)` only).
//!
//! The abstract program form is minimal on purpose — relation symbols,
//! variables, `DataValue` constants, optional negation, optional head
//! aggregations, opaque fixed rules — so it can outlive any concrete AST
//! the engine uses. The aggregations themselves are the *real* landed
//! [`Aggregation`] values from `data/aggr.rs`: the oracle folds through
//! exactly the code users get, so a bug in an aggregation cannot hide
//! behind a parallel test-only reimplementation.
//!
//! ## Aggregation semantics, as law
//!
//! - A **normal aggregation** head is evaluated once, at the fixpoint of
//!   everything beneath it: group the rule set's derived rows by the
//!   non-aggregated head positions, fold each group through the normal
//!   form, one output row per group. Rows are counted per distinct binding
//!   of the body's variables (the bodies join sets, so that multiset is
//!   well-defined without any plan-dependent notion of duplicates).
//! - A **meet aggregation** head whose rules are *all* meet forms may be
//!   self-recursive: each derived row meets into an accumulator keyed by
//!   the non-aggregated positions, *during* the fixpoint, and the
//!   accumulated rows are what the recursive body reads back. Naive
//!   iteration simply re-derives everything until no accumulated value
//!   changes.
//! - An aggregation head with **every position aggregated** always has a
//!   row. For normal forms, no input rows yield the single empty-fold
//!   row. For meet forms, if the first round — where the recursive reads
//!   see the empty store — derives nothing, the identity row
//!   (`init_val`s) is inserted as a real fact the rest of the recursion
//!   builds on; if anything was derived, the identity row never exists
//!   (exposing it alongside real rows would let its value join into rule
//!   bodies and derive facts outside the least fixpoint). With a grouping
//!   position, no rows yield no rows.
//! - A **fixed rule** is an opaque function from complete input relations
//!   to an output relation; it always sits on a stratum boundary (inputs
//!   strictly below, readers strictly above), never inside recursion.
//!
//! Three deliberate divergences from upstream cozo, all in the oracle's
//! favor: upstream `compile.rs::aggr_kind` silently demoted a meet
//! signature whose aggregated positions were not a suffix to a *normal*
//! aggregation — which its evaluator then froze after epoch 0, silently
//! dropping recursive derivations — while the oracle groups by position
//! and evaluates meets inside recursion wherever they appear; the
//! order-dependent aggregations (`choice`, `collect`, `min_cost`/
//! `shortest`/`latest_by`/`smallest_by` ties, `choice_rand`) are
//! deterministic here (sorted-set derivation order) but their tie-breaks
//! are arrival-order artifacts, so differential harnesses must avoid or
//! canonicalize them; and the abstract [`Program`] has no entry symbol,
//! so the oracle judges the *whole* program — upstream prunes rules
//! unreachable from the entry before both checking and evaluation (dead
//! rules are neither refused nor computed), while the oracle checks and
//! evaluates everything, so differential harnesses must feed
//! entry-reachable programs.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::data::aggr::{Aggregation, MeetAggrObj, NormalAggrObj};
use crate::data::bitemporal::ClaimPolarity;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;

pub(crate) type Rel = &'static str;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Term {
    Var(&'static str),
    Const(DataValue),
}

/// A bitemporal read coordinate, mirroring `data::value::AsOf`'s `(sys,
/// valid)` shape and "newest at or before governs" semantics — but in
/// plain ascending `i64` (larger means later) rather than `ValidityTs`'s
/// `Reverse`-wrapped descending order. The oracle stays in this module's
/// own idiom (plain values, no wrapper cleverness, obviously correct by
/// inspection); the two bitemporal test harnesses this oracle unifies
/// (`query/time_travel_trials.rs`, `query/time_travel_script_laws.rs`)
/// already work in plain ascending timestamps throughout, so this is also
/// the coordinate their generated histories bridge against directly.
///
/// **The exact correspondence.** `laws::AsOf { valid: v, sys: s }` is
/// `data::value::AsOf { valid: ValidityTs(Reverse(v)), sys:
/// ValidityTs(Reverse(s)) }` — wrap each field in `ValidityTs(Reverse(_))`
/// to go from this type to the real one. Because `Reverse` inverts
/// comparison, this module's ascending `t <= v` ("instant `t` is at or
/// before coordinate `v`") is the real type's DESCENDING `ValidityTs(t) >=
/// ValidityTs(v)` — the two types encode the identical total order
/// through inverted representations, never a different one.
/// [`asof_mirror_matches_bitemporal_kernel_on_a_shared_fixture`] proves
/// the two pick the same governing version on shared rows, rather than
/// leaving that "the same order" claim as an assertion in a doc comment.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AsOf {
    /// The valid-time coordinate: among believed claims, the newest valid
    /// instant at or before this one governs.
    pub valid: i64,
    /// The system-time coordinate: resolve by the newest system version
    /// at or before this instant.
    pub sys: i64,
}

impl AsOf {
    /// The record's current belief: every stored instant is visible, at
    /// its newest recorded system version.
    pub(crate) const fn current() -> Self {
        AsOf {
            valid: i64::MAX,
            sys: i64::MAX,
        }
    }

    /// The record's current belief about the world at `valid` — mirrors
    /// `data::value::AsOf::current`.
    pub(crate) const fn current_at(valid: i64) -> Self {
        AsOf {
            valid,
            sys: i64::MAX,
        }
    }
}

/// One stored point-event in a fact's bitemporal history: the fact's
/// identifying key columns, the non-key payload it claims (populated only
/// for [`ClaimPolarity::Assert`] — empty for `Retract`/`Erase`, mirroring
/// the stored format where polarity lives in the value and a
/// retract/erase carries no payload, `data/bitemporal.rs`), the valid
/// instant, the system version, and the polarity.
///
/// **The untimed embedding.** A plain (non-historical) fact tuple `t` in
/// `Program::facts` is sugar for exactly one canonical `Event`: assert
/// `t` at the canonical instant `(valid = 0, sys = 0)`. [`Event::untimed`]
/// makes this embedding a real, callable function rather than a comment —
/// used by the bridge differentials proving the unified resolution
/// algebra reproduces the disjoint oracles it replaces — but
/// `Program::facts` itself is untouched: an untimed program's facts are
/// never routed through event history at all (no `Program::histories`
/// entry), so every existing untimed differential stays byte-identical
/// with zero code-path change. A relation is EITHER plain (`facts`) or
/// historical (`histories`), never both (`check_wellformed` refuses the
/// overlap) — the two worlds are cleanly disjoint, unified only through
/// the one evaluator that reads either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub key: Tuple,
    pub payload: Tuple,
    pub valid: i64,
    pub sys: i64,
    pub polarity: ClaimPolarity,
}

impl Event {
    /// The valid instant `i64::MAX` is RESERVED for the `@ 'END'`
    /// write-side sentinel (`parse/query.rs`'s end-sentinel resolution,
    /// `put_at_now_and_end_sentinels_resolve`) — never a storable event
    /// coordinate. Refusing it here, at construction, is what keeps a
    /// zero-width `[i64::MAX, i64::MAX)` derived interval unrepresentable:
    /// `OPEN_END` reuses that same value for "unbounded," so an assert
    /// claiming the terminal tick itself would have nowhere left to end.
    /// (Hostile-review ruling, issue #62 comment 4882951801: the write
    /// path's own refusal of the same instant is a separate, later
    /// change; this is the oracle's side of the reservation.)
    fn check_valid_not_reserved(valid: i64) -> miette::Result<()> {
        if valid == i64::MAX {
            miette::bail!(
                "valid instant i64::MAX is reserved for the `@ 'END'` write-side \
                 sentinel; no event may claim it as its own coordinate"
            );
        }
        Ok(())
    }
    pub(crate) fn assert(key: Tuple, payload: Tuple, valid: i64, sys: i64) -> miette::Result<Self> {
        Self::check_valid_not_reserved(valid)?;
        Ok(Event {
            key,
            payload,
            valid,
            sys,
            polarity: ClaimPolarity::Assert,
        })
    }
    pub(crate) fn retract(key: Tuple, valid: i64, sys: i64) -> miette::Result<Self> {
        Self::check_valid_not_reserved(valid)?;
        Ok(Event {
            key,
            payload: Vec::new(),
            valid,
            sys,
            polarity: ClaimPolarity::Retract,
        })
    }
    pub(crate) fn erase(key: Tuple, valid: i64, sys: i64) -> miette::Result<Self> {
        Self::check_valid_not_reserved(valid)?;
        Ok(Event {
            key,
            payload: Vec::new(),
            valid,
            sys,
            polarity: ClaimPolarity::Erase,
        })
    }
    /// The untimed embedding: sugar for "this fact has always held, as
    /// far as any historical read can see." See the type doc. Bypasses
    /// the reserved-tick check entirely (not merely passes it): `valid =
    /// 0` is a fixed internal constant, never user input, so there is no
    /// coordinate here to validate.
    pub(crate) fn untimed(tuple: Tuple) -> Self {
        Event {
            key: tuple,
            payload: Vec::new(),
            valid: 0,
            sys: 0,
            polarity: ClaimPolarity::Assert,
        }
    }
}

/// The governing tuple for one fact — all events sharing a key — at `at`.
/// The brute-force twin of the governing-version sweep already pinned in
/// miniature at `data/bitemporal.rs:305-346`
/// (`check_key_for_bitemporal`/its own test oracle): among instants at or
/// before `at.valid`, newest first, the newest system version at or
/// before `at.sys` governs that instant; `Assert` holds (`key ++
/// payload`), `Retract` settles absent (no fall-through), `Erase` is
/// transparent — resolution falls through to the fact's next older
/// instant.
fn resolve_events(events: &[&Event], at: AsOf) -> Option<Tuple> {
    let mut instants: Vec<i64> = events
        .iter()
        .map(|e| e.valid)
        .filter(|v| *v <= at.valid)
        .collect();
    instants.sort_unstable();
    instants.dedup();
    for instant in instants.into_iter().rev() {
        let governing = events
            .iter()
            .filter(|e| e.valid == instant && e.sys <= at.sys)
            .max_by_key(|e| e.sys);
        match governing.map(|e| e.polarity) {
            Some(ClaimPolarity::Assert) => {
                let e = governing.expect("just matched Some");
                let mut tuple = e.key.clone();
                tuple.extend(e.payload.iter().cloned());
                return Some(tuple);
            }
            Some(ClaimPolarity::Retract) => return None,
            Some(ClaimPolarity::Erase) | None => {}
        }
    }
    None
}

/// [`resolve_events`] for one named fact key within a relation's whole
/// event history.
pub(crate) fn resolve(history: &[Event], key: &Tuple, at: AsOf) -> Option<Tuple> {
    let events: Vec<&Event> = history.iter().filter(|e| &e.key == key).collect();
    resolve_events(&events, at)
}

/// The relation-wide snapshot at `at`: every fact key with a governing
/// tuple.
pub(crate) fn resolve_relation(history: &[Event], at: AsOf) -> BTreeSet<Tuple> {
    let mut by_key: BTreeMap<&Tuple, Vec<&Event>> = BTreeMap::new();
    for e in history {
        by_key.entry(&e.key).or_default().push(e);
    }
    by_key
        .into_values()
        .filter_map(|events| resolve_events(&events, at))
        .collect()
}

/// Which axis a derived-interval sweep varies, the other held `fixed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Axis {
    /// Valid-time intervals at a fixed system snapshot — "what held, and
    /// when, as believed as of `fixed`."
    Valid,
    /// System-time intervals at a fixed valid instant — "what the record
    /// said about this one instant, over the record's own history,"
    /// `[stamp, next-version-stamp)`.
    Sys,
}

/// One maximal half-open run `[start, end)` of a fact's step function
/// along `Axis`, holding the governing tuple (`key ++ payload`, [`resolve`]'s
/// return convention) throughout. `end == `[`OPEN_END`] means the run is
/// still open (nothing later supersedes it in this history).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Interval {
    pub start: i64,
    pub end: i64,
    pub tuple: Tuple,
}

/// The sentinel meaning "no later coordinate closes this interval" — the
/// maximum representable instant, never a real stored coordinate.
pub(crate) const OPEN_END: i64 = i64::MAX;

/// Derived intervals are never stored, only computed: at fixed `fixed`,
/// the step function `v ↦ resolve(history, key, coordinate(v))` — for
/// `Axis::Valid`, `coordinate(v) = AsOf { valid: v, sys: fixed }`; for
/// `Axis::Sys`, `coordinate(v) = AsOf { valid: fixed, sys: v }` —
/// decomposed into maximal constant half-open runs. One interval per
/// maximal run of *equal* payload; coalescing is definitional, so
/// un-coalesced output is unrepresentable by construction (the loop below
/// only closes a run when the next breakpoint's payload differs).
pub(crate) fn derive_intervals(
    history: &[Event],
    key: &Tuple,
    axis: Axis,
    fixed: i64,
) -> Vec<Interval> {
    let events: Vec<&Event> = history.iter().filter(|e| &e.key == key).collect();
    let mut breaks: Vec<i64> = match axis {
        // Every stored valid instant of this fact is a candidate
        // breakpoint at the fixed system snapshot.
        Axis::Valid => events.iter().map(|e| e.valid).collect(),
        // Only versions recorded at or before the fixed valid instant can
        // ever govern it (fall-through only reaches OLDER instants, never
        // newer ones), so later instants' system stamps are irrelevant
        // breakpoints here.
        Axis::Sys => events
            .iter()
            .filter(|e| e.valid <= fixed)
            .map(|e| e.sys)
            .collect(),
    };
    breaks.sort_unstable();
    breaks.dedup();
    let coordinate = |pt: i64| -> AsOf {
        match axis {
            Axis::Valid => AsOf {
                valid: pt,
                sys: fixed,
            },
            Axis::Sys => AsOf {
                valid: fixed,
                sys: pt,
            },
        }
    };

    let mut out = Vec::new();
    let mut i = 0;
    while i < breaks.len() {
        let start = breaks[i];
        let Some(tuple) = resolve_events(&events, coordinate(start)) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < breaks.len()
            && resolve_events(&events, coordinate(breaks[j + 1])).as_ref() == Some(&tuple)
        {
            j += 1;
        }
        let end = if j + 1 < breaks.len() {
            breaks[j + 1]
        } else {
            OPEN_END
        };
        out.push(Interval { start, end, tuple });
        i = j + 1;
    }
    out
}

/// A net snapshot difference: a signed fact, never a "modified" kind — a
/// payload change at one key falls out of a plain set difference as a
/// `Minus`/`Plus` pair (the old and new tuples differ in full, so each
/// appears on its own side).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SignedFact {
    Plus(Tuple),
    Minus(Tuple),
}

/// The reference diff: axis-parameterized only in that `from`/`to` may
/// differ along either coordinate (or both) — the same computation either
/// way, since it operates on the two resolved snapshots directly, never
/// on the intervals.
pub(crate) fn diff(history: &[Event], from: AsOf, to: AsOf) -> BTreeSet<SignedFact> {
    let a = resolve_relation(history, from);
    let b = resolve_relation(history, to);
    let mut out = BTreeSet::new();
    for t in a.difference(&b) {
        out.insert(SignedFact::Minus(t.clone()));
    }
    for t in b.difference(&a) {
        out.insert(SignedFact::Plus(t.clone()));
    }
    out
}

/// Patch composition with cancellation: tally each tuple's net polarity
/// (`Plus` = +1, `Minus` = -1) across both patches, in order; a tuple
/// whose net is zero cancels out of the result entirely (e.g. a payload
/// that changes and changes back within the composed window). The
/// executable form of the compositionality law
/// `diff(a,c) == diff(a,b) ⊕ diff(b,c)`.
pub(crate) fn compose(
    first: &BTreeSet<SignedFact>,
    second: &BTreeSet<SignedFact>,
) -> BTreeSet<SignedFact> {
    let mut tally: BTreeMap<&Tuple, i32> = BTreeMap::new();
    for patch in [first, second] {
        for fact in patch {
            let (t, delta) = match fact {
                SignedFact::Plus(t) => (t, 1),
                SignedFact::Minus(t) => (t, -1),
            };
            *tally.entry(t).or_insert(0) += delta;
        }
    }
    tally
        .into_iter()
        .filter_map(|(t, net)| match net {
            0 => None,
            n if n > 0 => Some(SignedFact::Plus(t.clone())),
            _ => Some(SignedFact::Minus(t.clone())),
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct Literal {
    pub rel: Rel,
    pub args: Vec<Term>,
    pub negated: bool,
    /// The literal's own bitemporal read coordinate, overriding the
    /// query-level default (`naive_eval_at`'s parameter) when present.
    /// Meaningful only on a literal reading a relation with an entry in
    /// [`Program::histories`]; `check_wellformed` refuses it elsewhere.
    /// Negating a literal that carries one is refused
    /// (`Rejection::NegationOverTimeTravel`), mirroring the engine's
    /// `NegationOverTimeTravelError` (`query/ra/mod.rs:260`).
    pub as_of: Option<AsOf>,
}

impl Literal {
    /// A positive body literal, current/untimed (no explicit as-of) — the
    /// one seam every call site should construct through, so a future
    /// field on `Literal` fans out from here instead of from every file's
    /// own hand-written struct literal (the lesson of story #62's
    /// compiler-forced fallout across five files).
    pub(crate) fn pos(rel: Rel, args: Vec<Term>) -> Self {
        Literal {
            rel,
            args,
            negated: false,
            as_of: None,
        }
    }
    /// A negated body literal, current/untimed.
    pub(crate) fn neg(rel: Rel, args: Vec<Term>) -> Self {
        Literal {
            rel,
            args,
            negated: true,
            as_of: None,
        }
    }
    /// A positive body literal at its own bitemporal coordinate.
    pub(crate) fn pos_at(rel: Rel, args: Vec<Term>, at: AsOf) -> Self {
        Literal {
            rel,
            args,
            negated: false,
            as_of: Some(at),
        }
    }
    /// A negated body literal at its own bitemporal coordinate — refused
    /// by [`check_time_travel_negation`], but constructible so the
    /// refusal corpus (and this file's own refusal test) can build the
    /// exact shape that must be rejected.
    pub(crate) fn neg_at(rel: Rel, args: Vec<Term>, at: AsOf) -> Self {
        Literal {
            rel,
            args,
            negated: true,
            as_of: Some(at),
        }
    }
}

/// One head position's aggregation, if any: the real landed [`Aggregation`]
/// plus its compile-time arguments (only `collect` takes one today).
pub(crate) type HeadAggr = Option<(Aggregation, Vec<DataValue>)>;

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub head_rel: Rel,
    pub head_args: Vec<Term>,
    /// Per-head-position aggregations, same length as `head_args`.
    pub aggr: Vec<HeadAggr>,
    pub body: Vec<Literal>,
}

impl Rule {
    /// A rule with no aggregations.
    pub(crate) fn plain(head_rel: Rel, head_args: Vec<Term>, body: Vec<Literal>) -> Self {
        let aggr = vec![None; head_args.len()];
        Self {
            head_rel,
            head_args,
            aggr,
            body,
        }
    }

    /// A rule with per-position head aggregations.
    pub(crate) fn aggregated(
        head_rel: Rel,
        head_args: Vec<Term>,
        aggr: Vec<HeadAggr>,
        body: Vec<Literal>,
    ) -> Self {
        Self {
            head_rel,
            head_args,
            aggr,
            body,
        }
    }
}

/// A fixed rule, modeled abstractly: an opaque function from its complete
/// input relations to an output relation. Stratification always puts it on
/// a stratum boundary — inputs strictly below, readers strictly above — so
/// it can never sit inside recursion; evaluation runs it exactly once.
#[derive(Clone, Debug)]
pub(crate) struct FixedRule {
    pub head_rel: Rel,
    pub inputs: Vec<Rel>,
    /// Receives the input relations in `inputs` order.
    pub eval: fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Program {
    pub rules: Vec<Rule>,
    pub fixed: Vec<FixedRule>,
    pub facts: BTreeMap<Rel, BTreeSet<Tuple>>,
    /// Bitemporal EDBs: a relation lives here XOR in `facts`, never both
    /// (`check_wellformed` refuses the overlap). A historical relation's
    /// current snapshot at any coordinate is [`resolve_relation`], never a
    /// precomputed set — literals reading it may each carry their own
    /// [`AsOf`], so the same relation can be read at different coordinates
    /// within one program.
    pub histories: BTreeMap<Rel, Vec<Event>>,
}

impl Program {
    /// An untimed program: no historical relations at all. The one seam
    /// call sites that never touch time should build through, instead of
    /// each hand-spelling `histories: Default::default()` (or, worse,
    /// enumerating every field and silently drifting when a new one is
    /// added — the exact fallout story #62 caused across five files).
    pub(crate) fn untimed(
        rules: Vec<Rule>,
        fixed: Vec<FixedRule>,
        facts: BTreeMap<Rel, BTreeSet<Tuple>>,
    ) -> Self {
        Program {
            rules,
            fixed,
            facts,
            histories: BTreeMap::new(),
        }
    }
}

/// Why a program is refused, or an evaluation failed. The real compiler
/// must refuse the same programs, for the same reasons; evaluation errors
/// are values (law 5), never panics.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// A head variable is not bound by any positive body literal, or a
    /// negated literal uses a variable no positive literal binds.
    Unsafe(&'static str),
    /// A stratum-forcing dependency (negation, non-meet aggregation, a
    /// read of a meet-aggregated or fixed relation) occurs inside a
    /// recursive cycle.
    Unstratifiable(&'static str),
    /// The program shape is ill-formed: an aggregation vector whose length
    /// differs from the head's, rules of one head disagreeing on their
    /// aggregation signature (upstream refuses this at parse as
    /// `parser::head_aggr_mismatch`), a fixed head that is also a rule
    /// head, duplicated, or seeded with facts, facts under an aggregated
    /// head, or a relation used at two different arities.
    Malformed(&'static str),
    /// An aggregation failed at evaluation time (e.g. a type error inside
    /// a fold); carried as a value, never a panic.
    AggrError(String),
    /// A negated literal carries its own as-of coordinate — mirrors the
    /// engine's `NegationOverTimeTravelError` (`query/ra/mod.rs:260`):
    /// negation over a time-travel scan is refused, not silently wrong.
    NegationOverTimeTravel(&'static str),
}

fn literal_vars(l: &Literal) -> HashSet<&'static str> {
    l.args
        .iter()
        .filter_map(|t| match t {
            Term::Var(v) => Some(*v),
            Term::Const(_) => None,
        })
        .collect()
}

/// Law 4 (rule safety), reference form.
pub(crate) fn check_safety(program: &Program) -> Result<(), Rejection> {
    for rule in &program.rules {
        let positive_vars: HashSet<&str> = rule
            .body
            .iter()
            .filter(|l| !l.negated)
            .flat_map(literal_vars)
            .collect();
        for t in &rule.head_args {
            if let Term::Var(v) = t
                && !positive_vars.contains(v)
            {
                return Err(Rejection::Unsafe(rule.head_rel));
            }
        }
        for l in rule.body.iter().filter(|l| l.negated) {
            if !literal_vars(l).is_subset(&positive_vars) {
                return Err(Rejection::Unsafe(rule.head_rel));
            }
        }
    }
    Ok(())
}

/// How a head relation aggregates, across *all* of its rules — the
/// classification upstream `stratify.rs` derives per rule set.
#[derive(Clone, Copy)]
struct HeadClass {
    /// Some rule of this head aggregates some position.
    has_aggr: bool,
    /// It aggregates, and every aggregated position of every rule is a
    /// meet form — the only class allowed to recurse through itself.
    is_meet: bool,
}

fn head_classes(program: &Program) -> HashMap<Rel, HeadClass> {
    let mut per_head: HashMap<Rel, Vec<&Rule>> = HashMap::new();
    for rule in &program.rules {
        per_head.entry(rule.head_rel).or_default().push(rule);
    }
    per_head
        .into_iter()
        .map(|(rel, rules)| {
            let has_aggr = rules.iter().any(|r| r.aggr.iter().any(|a| a.is_some()));
            let is_meet = has_aggr
                && rules.iter().all(|r| {
                    r.aggr.iter().all(|a| match a {
                        None => true,
                        Some((aggr, _)) => aggr.is_meet(),
                    })
                });
            (rel, HeadClass { has_aggr, is_meet })
        })
        .collect()
}

/// The dependency graph, one edge per body literal or fixed-rule input:
/// head → dependency, with `forcing` true when the dependency must be
/// complete strictly below the head. Mirrors the "poisoned" edges of
/// upstream `stratify.rs` (`convert_normal_form_program_to_graph`):
///
/// - an aggregating head's only non-forcing dependency is a meet head
///   reading *itself*, positively — every other dependency of an
///   aggregating head forces a stratum;
/// - a non-aggregating rule forces a stratum on negated dependencies and
///   on any read of a meet-aggregated or fixed relation;
/// - a fixed rule forces a stratum on every input.
fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
    let classes = head_classes(program);
    let fixed_heads: HashSet<Rel> = program.fixed.iter().map(|f| f.head_rel).collect();
    let is_meet = |rel: Rel| classes.get(rel).is_some_and(|c| c.is_meet);
    let mut edges = Vec::new();
    for rule in &program.rules {
        let head = rule.head_rel;
        let class = classes[&head];
        for lit in &rule.body {
            let dep = lit.rel;
            let forcing = if class.has_aggr {
                if class.is_meet && dep == head {
                    // The one legal aggregation inside recursion: a meet
                    // head folding its own positive derivations.
                    lit.negated
                } else {
                    true
                }
            } else {
                lit.negated || fixed_heads.contains(dep) || is_meet(dep)
            };
            edges.push((head, dep, forcing));
        }
    }
    for f in &program.fixed {
        for dep in &f.inputs {
            edges.push((f.head_rel, *dep, true));
        }
    }
    edges
}

/// Law 2 (stratification), reference form: a program is unstratifiable iff
/// some dependency cycle contains a stratum-forcing edge. With aggregation
/// this is exactly upstream `stratify.rs`'s rule: self-recursion is legal
/// only when all rules of the head aggregate with meet forms; normal
/// aggregation over any dependency, negation in a cycle, and fixed rules
/// in a cycle are refused.
pub(crate) fn check_stratifiable(program: &Program) -> Result<(), Rejection> {
    let edges = dependency_edges(program);
    let mut adjacency: HashMap<Rel, HashSet<Rel>> = HashMap::new();
    for (head, dep, _) in &edges {
        adjacency.entry(*head).or_default().insert(*dep);
    }
    let reaches = |from: Rel, to: Rel| -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        while let Some(r) = stack.pop() {
            if r == to {
                return true;
            }
            if seen.insert(r) {
                stack.extend(adjacency.get(r).into_iter().flatten().copied());
            }
        }
        false
    };
    for (head, dep, forcing) in &edges {
        if *forcing && reaches(dep, head) {
            return Err(Rejection::Unstratifiable(head));
        }
    }
    Ok(())
}

fn aggr_err(e: miette::Report) -> Rejection {
    Rejection::AggrError(e.to_string())
}

/// Program-shape validation the real compiler performs at parse/compile
/// time; see [`Rejection::Malformed`] for the refused shapes.
pub(crate) fn check_wellformed(program: &Program) -> Result<(), Rejection> {
    let mut signatures: BTreeMap<Rel, &[HeadAggr]> = BTreeMap::new();
    for rule in &program.rules {
        if rule.aggr.len() != rule.head_args.len() {
            return Err(Rejection::Malformed(rule.head_rel));
        }
        match signatures.entry(rule.head_rel) {
            Entry::Occupied(prev) if *prev.get() != rule.aggr.as_slice() => {
                return Err(Rejection::Malformed(rule.head_rel));
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(e) => {
                e.insert(&rule.aggr);
            }
        }
    }
    let mut fixed_heads = HashSet::new();
    for f in &program.fixed {
        if !fixed_heads.insert(f.head_rel) || program.facts.contains_key(f.head_rel) {
            return Err(Rejection::Malformed(f.head_rel));
        }
    }
    for rule in &program.rules {
        if fixed_heads.contains(rule.head_rel) {
            return Err(Rejection::Malformed(rule.head_rel));
        }
    }
    for (rel, class) in head_classes(program) {
        if class.has_aggr && program.facts.contains_key(rel) {
            return Err(Rejection::Malformed(rel));
        }
    }
    // A relation lives in `facts` XOR `histories`, never both — the two
    // worlds (always-current EDB, bitemporal EDB) stay disjoint, unified
    // only through the one evaluator that reads either.
    for rel in program.histories.keys() {
        if program.facts.contains_key(rel) {
            return Err(Rejection::Malformed(rel));
        }
    }
    // A historical relation is a stored EDB leaf, never a derivable head:
    // a rule or fixed rule sharing its name would derive into `db` while
    // every reader still resolves the SAME name through `histories`
    // (`literal_rows` prefers a historical entry unconditionally, ahead
    // of `db`) — the derivation would exist and never be seen by
    // anything that reads it. Refused alongside the facts∩histories
    // check above (hostile-review finding, issue #62 comment
    // 4882951801).
    for rule in &program.rules {
        if program.histories.contains_key(rule.head_rel) {
            return Err(Rejection::Malformed(rule.head_rel));
        }
    }
    for f in &program.fixed {
        if program.histories.contains_key(f.head_rel) {
            return Err(Rejection::Malformed(f.head_rel));
        }
    }
    // Every event of one historical relation shares one key arity, and
    // every ASSERT shares one payload arity (retract/erase carry none, by
    // construction) — a relation with inconsistent shapes across its own
    // history is ill-formed the same way a fact tuple at the wrong arity
    // is.
    for (rel, history) in &program.histories {
        let key_arity = history.first().map(|e| e.key.len());
        for e in history {
            if Some(e.key.len()) != key_arity {
                return Err(Rejection::Malformed(rel));
            }
        }
        let payload_arity = history
            .iter()
            .find(|e| e.polarity == ClaimPolarity::Assert)
            .map(|e| e.payload.len());
        for e in history
            .iter()
            .filter(|e| e.polarity == ClaimPolarity::Assert)
        {
            if Some(e.payload.len()) != payload_arity {
                return Err(Rejection::Malformed(rel));
            }
        }
    }
    // A literal's `as_of` is meaningful only on a historical relation —
    // time is a read coordinate resolved at stored leaves, and a plain
    // fact/derived (IDB) relation has no leaf to resolve it against.
    for rule in &program.rules {
        for lit in &rule.body {
            if lit.as_of.is_some() && !program.histories.contains_key(lit.rel) {
                return Err(Rejection::Malformed(lit.rel));
            }
        }
    }
    // One arity per relation, across facts, rule heads, and body literals
    // (the real compiler refuses arity clashes at compile time). A fixed
    // head's *output* arity is opaque to the model — its `eval` may emit
    // tuples of any length — but its readers must at least agree among
    // themselves, and they are its only arity sources here (fixed heads
    // can be neither rule heads nor fact relations, checked above).
    let mut arities: HashMap<Rel, usize> = HashMap::new();
    let mut check_arity = |rel: Rel, arity: usize| -> Result<(), Rejection> {
        match arities.get(rel) {
            Some(known) if *known != arity => Err(Rejection::Malformed(rel)),
            Some(_) => Ok(()),
            None => {
                arities.insert(rel, arity);
                Ok(())
            }
        }
    };
    for (rel, tuples) in &program.facts {
        for t in tuples {
            check_arity(rel, t.len())?;
        }
    }
    for (rel, history) in &program.histories {
        if let (Some(k), Some(v)) = (
            history.first().map(|e| e.key.len()),
            history
                .iter()
                .find(|e| e.polarity == ClaimPolarity::Assert)
                .map(|e| e.payload.len()),
        ) {
            check_arity(rel, k + v)?;
        }
    }
    for rule in &program.rules {
        check_arity(rule.head_rel, rule.head_args.len())?;
        for l in &rule.body {
            check_arity(l.rel, l.args.len())?;
        }
    }
    Ok(())
}

/// Assign strata: a relation sits strictly above every stratum-forcing
/// dependency, and at least as high as its other dependencies. Assumes
/// `check_stratifiable` passed.
fn strata(program: &Program) -> HashMap<Rel, usize> {
    let edges = dependency_edges(program);
    let mut s: HashMap<Rel, usize> = HashMap::new();
    let rels: HashSet<Rel> = program
        .rules
        .iter()
        .flat_map(|r| std::iter::once(r.head_rel).chain(r.body.iter().map(|l| l.rel)))
        .chain(program.facts.keys().copied())
        .chain(program.histories.keys().copied())
        .chain(
            program
                .fixed
                .iter()
                .flat_map(|f| std::iter::once(f.head_rel).chain(f.inputs.iter().copied())),
        )
        .collect();
    for r in &rels {
        s.insert(r, 0);
    }
    // Bellman-Ford over ≤ |rels| levels: any simple dependency path has
    // fewer than |rels| edges, so |rels| passes settle every level and one
    // more observes no change.
    let bound = rels.len() + 1;
    for _ in 0..bound {
        let mut changed = false;
        for (head, dep, forcing) in &edges {
            let need = s[dep] + usize::from(*forcing);
            if s[head] < need {
                s.insert(*head, need);
                changed = true;
            }
        }
        if !changed {
            return s;
        }
    }
    unreachable!("stratum assignment must converge on stratifiable programs");
}

type Bindings = HashMap<&'static str, DataValue>;

fn unify(args: &[Term], tuple: &Tuple, bound: &Bindings) -> Option<Bindings> {
    if args.len() != tuple.len() {
        return None;
    }
    let mut out = bound.clone();
    for (t, v) in args.iter().zip(tuple) {
        match t {
            Term::Const(c) => {
                if c != v {
                    return None;
                }
            }
            Term::Var(name) => match out.get(name) {
                Some(existing) if existing != v => return None,
                Some(_) => {}
                None => {
                    out.insert(name, v.clone());
                }
            },
        }
    }
    Some(out)
}

fn ground(args: &[Term], bound: &Bindings) -> Tuple {
    args.iter()
        .map(|t| match t {
            Term::Const(c) => c.clone(),
            Term::Var(v) => bound[v].clone(),
        })
        .collect()
}

/// The rows a literal reading `lit.rel` sees. A plain fact/derived
/// relation reads `db` exactly as before this module grew a time axis —
/// zero behavior change for every untimed program, no `Program::histories`
/// lookup even attempted. A historical relation is never a precomputed
/// snapshot in `db`: it is resolved fresh, here, at the literal's own
/// coordinate if it carries one, else `default_as_of` — so two literals
/// reading the SAME historical relation at different coordinates within
/// one program each see their own snapshot (`AsOf` pushed down to the
/// stored leaf the literal names, never precomputed above it).
fn literal_rows(
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    lit: &Literal,
    default_as_of: AsOf,
) -> BTreeSet<Tuple> {
    match program.histories.get(lit.rel) {
        Some(history) => resolve_relation(history, lit.as_of.unwrap_or(default_as_of)),
        None => db.get(lit.rel).cloned().unwrap_or_default(),
    }
}

/// All satisfying bindings of a rule body against the current database,
/// one per distinct binding of the body's variables. Positives first, so
/// safety guarantees negated literals are fully bound when probed.
fn body_bindings(
    rule: &Rule,
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
) -> Vec<Bindings> {
    let mut ordered: Vec<&Literal> = rule.body.iter().filter(|l| !l.negated).collect();
    ordered.extend(rule.body.iter().filter(|l| l.negated));

    let mut frontier: Vec<Bindings> = vec![Bindings::new()];
    for lit in ordered {
        let rows = literal_rows(program, db, lit, default_as_of);
        let mut next = Vec::new();
        for bound in &frontier {
            if lit.negated {
                let probe = ground(&lit.args, bound);
                if !rows.contains(&probe) {
                    next.push(bound.clone());
                }
            } else {
                for tuple in &rows {
                    if let Some(b) = unify(&lit.args, tuple, bound) {
                        next.push(b);
                    }
                }
            }
        }
        frontier = next;
    }
    frontier
}

/// The rule's derived head rows, one per body binding. Distinct bindings
/// can ground to the same row; the multiplicity is what normal
/// aggregations fold over, so it is preserved.
fn derived_rows(
    rule: &Rule,
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
) -> Vec<Tuple> {
    body_bindings(rule, program, db, default_as_of)
        .iter()
        .map(|b| ground(&rule.head_args, b))
        .collect()
}

/// Evaluate one normal-aggregation head, once, over the fixpoint of
/// everything beneath it (stratification guarantees all its dependencies
/// are complete): group every rule's derived rows by the non-aggregated
/// head positions, fold each group through the normal forms — matching
/// upstream `eval.rs::initial_rule_aggr_eval`, shared groups across the
/// head's rules included. No rows with every position aggregated yields
/// the single empty-fold row.
fn eval_normal_aggr_head(
    rules: &[&Rule],
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
) -> Result<BTreeSet<Tuple>, Rejection> {
    // Well-formedness guarantees every rule of the head shares this
    // signature.
    let signature = &rules[0].aggr;
    let key_positions: Vec<usize> = signature
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_none())
        .map(|(i, _)| i)
        .collect();
    let val_positions: Vec<(usize, &Aggregation, &[DataValue])> = signature
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.as_ref().map(|(aggr, args)| (i, aggr, args.as_slice())))
        .collect();
    let fresh_ops = || -> Result<Vec<Box<dyn NormalAggrObj>>, Rejection> {
        val_positions
            .iter()
            .map(|(_, aggr, args)| aggr.normal_op(args).map_err(aggr_err))
            .collect()
    };

    let mut groups: BTreeMap<Tuple, Vec<Box<dyn NormalAggrObj>>> = BTreeMap::new();
    for rule in rules {
        for row in derived_rows(rule, program, db, default_as_of) {
            let key: Tuple = key_positions.iter().map(|i| row[*i].clone()).collect();
            let ops = match groups.entry(key) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(fresh_ops()?),
            };
            for (op, (i, _, _)) in ops.iter_mut().zip(&val_positions) {
                op.set(&row[*i]).map_err(aggr_err)?;
            }
        }
    }

    let mut out = BTreeSet::new();
    if groups.is_empty() && key_positions.is_empty() && !val_positions.is_empty() {
        let mut row = Vec::with_capacity(val_positions.len());
        for op in fresh_ops()? {
            row.push(op.get().map_err(aggr_err)?);
        }
        out.insert(row);
    }
    for (key, ops) in groups {
        let mut row = vec![DataValue::Null; signature.len()];
        for (slot, i) in key_positions.iter().enumerate() {
            row[*i] = key[slot].clone();
        }
        for (op, (i, _, _)) in ops.iter().zip(&val_positions) {
            row[*i] = op.get().map_err(aggr_err)?;
        }
        out.insert(row);
    }
    Ok(out)
}

/// The running state of one meet-aggregated head during its stratum's
/// fixpoint: an accumulator keyed by the non-aggregated head positions,
/// updated in place through the real landed meet ops.
struct MeetState {
    key_positions: Vec<usize>,
    val_positions: Vec<usize>,
    ops: Vec<Box<dyn MeetAggrObj>>,
    arity: usize,
    acc: BTreeMap<Tuple, Tuple>,
}

impl MeetState {
    fn new(signature: &[HeadAggr]) -> Result<Self, Rejection> {
        let key_positions = signature
            .iter()
            .enumerate()
            .filter(|(_, a)| a.is_none())
            .map(|(i, _)| i)
            .collect();
        let mut val_positions = Vec::new();
        let mut ops = Vec::new();
        for (i, a) in signature.iter().enumerate() {
            if let Some((aggr, _)) = a {
                // Total by classification (`is_meet` heads only), never a
                // panic: a non-meet form here is a malformed program.
                let op = aggr
                    .meet_op()
                    .ok_or(Rejection::Malformed("non-meet aggregation on a meet head"))?;
                val_positions.push(i);
                ops.push(op);
            }
        }
        Ok(Self {
            key_positions,
            val_positions,
            ops,
            arity: signature.len(),
            acc: BTreeMap::new(),
        })
    }

    /// Meet one derived row into the accumulator; true iff any accumulated
    /// value changed (a fresh key always counts).
    fn meet_row(&mut self, row: &Tuple) -> Result<bool, Rejection> {
        let key: Tuple = self.key_positions.iter().map(|i| row[*i].clone()).collect();
        let vals: Tuple = self.val_positions.iter().map(|i| row[*i].clone()).collect();
        match self.acc.entry(key) {
            Entry::Vacant(e) => {
                e.insert(vals);
                Ok(true)
            }
            Entry::Occupied(mut e) => {
                let stored = e.get_mut();
                let mut changed = false;
                for (slot, op) in self.ops.iter().enumerate() {
                    changed |= op
                        .update(&mut stored[slot], &vals[slot])
                        .map_err(aggr_err)?;
                }
                Ok(changed)
            }
        }
    }

    /// The accumulated rows, re-interleaved into head-position order —
    /// this is the relation the recursive body (and everything above)
    /// reads.
    fn materialize(&self) -> BTreeSet<Tuple> {
        self.acc
            .iter()
            .map(|(key, vals)| {
                let mut row = vec![DataValue::Null; self.arity];
                for (slot, i) in self.key_positions.iter().enumerate() {
                    row[*i] = key[slot].clone();
                }
                for (slot, i) in self.val_positions.iter().enumerate() {
                    row[*i] = vals[slot].clone();
                }
                row
            })
            .collect()
    }
}

/// Every negated literal must resolve at a single, statically-known
/// coordinate — never a moving target it could itself perturb. A literal
/// carrying its own as-of coordinate is refused when negated, mirroring
/// the engine's `NegationOverTimeTravelError` (`query/ra/mod.rs:260`):
/// lifting this refusal (negation over a FIXED as-of snapshot is
/// well-defined) is a later task, named but not done here.
fn check_time_travel_negation(program: &Program) -> Result<(), Rejection> {
    for rule in &program.rules {
        for lit in rule.body.iter().filter(|l| l.negated) {
            if lit.as_of.is_some() {
                return Err(Rejection::NegationOverTimeTravel(rule.head_rel));
            }
        }
    }
    Ok(())
}

/// Naive stratified fixpoint evaluation: the textbook algorithm extended
/// with the aggregation and fixed-rule semantics in the module docs — the
/// oracle for Laws 1 and 3. Validates shape, safety, and stratifiability
/// first. The untimed entry point: every historical literal without its
/// own coordinate reads its relation's current belief
/// ([`AsOf::current`]).
pub(crate) fn naive_eval(program: &Program) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    naive_eval_at(program, AsOf::current())
}

/// [`naive_eval`], with an explicit query-level default coordinate: every
/// literal reading a historical relation without its own `as_of` resolves
/// at `default_as_of` instead of "current." Untimed programs (no
/// `Program::histories` entries at all) are unaffected by `default_as_of`
/// — no code path in this function ever consults it unless a literal
/// actually reads a historical relation.
pub(crate) fn naive_eval_at(
    program: &Program,
    default_as_of: AsOf,
) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    check_wellformed(program)?;
    check_safety(program)?;
    check_stratifiable(program)?;
    check_time_travel_negation(program)?;
    let classes = head_classes(program);
    let strata_of = strata(program);
    let max_stratum = strata_of.values().copied().max().unwrap_or(0);

    let mut db = program.facts.clone();

    for stratum in 0..=max_stratum {
        // Fixed rules run first and exactly once: stratification forces
        // their inputs strictly below (complete) and their readers
        // strictly above.
        for f in program
            .fixed
            .iter()
            .filter(|f| strata_of[f.head_rel] == stratum)
        {
            let inputs: Vec<BTreeSet<Tuple>> = f
                .inputs
                .iter()
                .map(|r| db.get(r).cloned().unwrap_or_default())
                .collect();
            db.insert(f.head_rel, (f.eval)(&inputs));
        }

        // Normal-aggregation heads run once, next: stratification forces
        // every dependency strictly below, so the rows they fold are
        // already the fixpoint of the strata beneath.
        let normal_heads: BTreeSet<Rel> = program
            .rules
            .iter()
            .filter(|r| strata_of[r.head_rel] == stratum)
            .map(|r| r.head_rel)
            .filter(|rel| {
                let c = classes[rel];
                c.has_aggr && !c.is_meet
            })
            .collect();
        for head in &normal_heads {
            let head_rules: Vec<&Rule> = program
                .rules
                .iter()
                .filter(|r| r.head_rel == *head)
                .collect();
            let out = eval_normal_aggr_head(&head_rules, program, &db, default_as_of)?;
            db.insert(head, out);
        }

        // Meet-aggregation heads of this stratum accumulate during the
        // fixpoint below; plain heads insert as ever.
        let mut meets: BTreeMap<Rel, MeetState> = BTreeMap::new();
        for rule in program
            .rules
            .iter()
            .filter(|r| strata_of[r.head_rel] == stratum && classes[r.head_rel].is_meet)
        {
            if !meets.contains_key(rule.head_rel) {
                meets.insert(rule.head_rel, MeetState::new(&rule.aggr)?);
            }
        }
        // Law 3's embodiment: over finite data with no invented values the
        // fixpoint is reached in finitely many rounds; the generous bound
        // turns non-termination into a loud test failure.
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            assert!(
                rounds <= 100_000,
                "fixpoint bound exceeded: non-termination"
            );
            let mut changed = false;
            for rule in program
                .rules
                .iter()
                .filter(|r| strata_of[r.head_rel] == stratum && !normal_heads.contains(r.head_rel))
            {
                let rows = derived_rows(rule, program, &db, default_as_of);
                if let Some(state) = meets.get_mut(rule.head_rel) {
                    for row in &rows {
                        changed |= state.meet_row(row)?;
                    }
                } else {
                    for row in rows {
                        changed |= db.entry(rule.head_rel).or_default().insert(row);
                    }
                }
            }
            // Upstream's epoch-0 identity rule, transcribed
            // (`eval.rs::initial_rule_meet_eval`): an all-aggregated meet
            // head whose first round — where the recursive reads saw the
            // empty store, exactly epoch 0 — derived nothing gets the
            // identity row, a real fact the rest of the recursion builds
            // on. Once any row exists the identity is never inserted:
            // exposing it alongside real derivations would let its value
            // (e.g. `min`'s Null) join into rule bodies and derive facts
            // outside the least fixpoint.
            if rounds == 1 {
                for state in meets.values_mut() {
                    if state.acc.is_empty()
                        && state.key_positions.is_empty()
                        && !state.ops.is_empty()
                    {
                        let identity: Tuple = state.ops.iter().map(|op| op.init_val()).collect();
                        state.acc.insert(Vec::new(), identity);
                        changed = true;
                    }
                }
            }
            // Republish the accumulated meet relations so the next round's
            // derivations (the recursive reads) see this round's meets.
            for (head, state) in &meets {
                db.insert(head, state.materialize());
            }
            if !changed {
                break;
            }
        }
    }
    Ok(db)
}

/// The corpus of programs the compiler must refuse — shared between the
/// reference checker's self-tests and (as they land) the real compiler's.
pub(crate) fn unstratifiable_corpus() -> Vec<(&'static str, Program)> {
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        if negated {
            Literal::neg(rel, args)
        } else {
            Literal::pos(rel, args)
        }
    }
    fn named(name: &'static str) -> (Aggregation, Vec<DataValue>) {
        let aggr = crate::data::aggr::parse_aggr(name)
            .unwrap_or_else(|| panic!("corpus uses only real aggregations, missing: {name}"));
        (aggr, vec![])
    }
    let x = || Term::Var("X");
    let y = || Term::Var("Y");
    vec![
        (
            "direct self-negation: p(X) :- d(X), not p(X)",
            Program {
                rules: vec![Rule::plain(
                    "p",
                    vec![x()],
                    vec![lit("d", vec![x()], false), lit("p", vec![x()], true)],
                )],
                ..Program::default()
            },
        ),
        (
            "mutual negation: p :- d, not q; q :- d, not p",
            Program {
                rules: vec![
                    Rule::plain(
                        "p",
                        vec![x()],
                        vec![lit("d", vec![x()], false), lit("q", vec![x()], true)],
                    ),
                    Rule::plain(
                        "q",
                        vec![x()],
                        vec![lit("d", vec![x()], false), lit("p", vec![x()], true)],
                    ),
                ],
                ..Program::default()
            },
        ),
        (
            "win-move game: win(X) :- move(X,Y), not win(Y)",
            Program {
                rules: vec![Rule::plain(
                    "win",
                    vec![x()],
                    vec![
                        lit("move", vec![x(), y()], false),
                        lit("win", vec![y()], true),
                    ],
                )],
                ..Program::default()
            },
        ),
        (
            "negation through a positive cycle: a :- d, not b; b :- a",
            Program {
                rules: vec![
                    Rule::plain(
                        "a",
                        vec![x()],
                        vec![lit("d", vec![x()], false), lit("b", vec![x()], true)],
                    ),
                    Rule::plain("b", vec![x()], vec![lit("a", vec![x()], false)]),
                ],
                ..Program::default()
            },
        ),
        (
            "recursive normal aggregation: p(X, count(Y)) :- d(X,Y); p(X, count(Y)) :- p(X,Y)",
            Program {
                rules: vec![
                    Rule::aggregated(
                        "p",
                        vec![x(), y()],
                        vec![None, Some(named("count"))],
                        vec![lit("d", vec![x(), y()], false)],
                    ),
                    Rule::aggregated(
                        "p",
                        vec![x(), y()],
                        vec![None, Some(named("count"))],
                        vec![lit("p", vec![x(), y()], false)],
                    ),
                ],
                ..Program::default()
            },
        ),
        (
            "mixed meet+normal aggregation on a recursive head: \
             q(X, min(Y), count(Z)) :- q(X,Y,Z)",
            Program {
                rules: vec![Rule::aggregated(
                    "q",
                    vec![x(), y(), Term::Var("Z")],
                    vec![None, Some(named("min")), Some(named("count"))],
                    vec![lit("q", vec![x(), y(), Term::Var("Z")], false)],
                )],
                ..Program::default()
            },
        ),
        (
            "meet aggregation negating its own head: m(X, min(Y)) :- d(X,Y), not m(X,Y)",
            Program {
                rules: vec![Rule::aggregated(
                    "m",
                    vec![x(), y()],
                    vec![None, Some(named("min"))],
                    vec![
                        lit("d", vec![x(), y()], false),
                        lit("m", vec![x(), y()], true),
                    ],
                )],
                ..Program::default()
            },
        ),
        (
            "fixed rule inside recursion: r(X) :- f(X), with fixed f over input r",
            Program {
                rules: vec![Rule::plain(
                    "r",
                    vec![x()],
                    vec![lit("f", vec![x()], false)],
                )],
                fixed: vec![FixedRule {
                    head_rel: "f",
                    inputs: vec!["r"],
                    eval: |_| BTreeSet::new(),
                }],
                ..Program::default()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::data::aggr::parse_aggr;

    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn edge_facts(edges: &[(i64, i64)]) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
        facts.insert(
            "edge",
            edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
        );
        facts
    }
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        if negated {
            Literal::neg(rel, args)
        } else {
            Literal::pos(rel, args)
        }
    }
    /// A body literal reading a historical relation at its own coordinate.
    fn lit_at(rel: Rel, args: Vec<Term>, negated: bool, at: AsOf) -> Literal {
        if negated {
            Literal::neg_at(rel, args, at)
        } else {
            Literal::pos_at(rel, args, at)
        }
    }
    fn x() -> Term {
        Term::Var("X")
    }
    fn y() -> Term {
        Term::Var("Y")
    }
    fn z() -> Term {
        Term::Var("Z")
    }
    /// A real landed aggregation by name, with no arguments.
    fn named(name: &str) -> HeadAggr {
        Some((
            parse_aggr(name).unwrap_or_else(|| panic!("real aggregation exists: {name}")),
            vec![],
        ))
    }

    /// path(X,Y) :- edge(X,Y); path(X,Y) :- edge(X,Z), path(Z,Y).
    fn transitive_closure() -> Vec<Rule> {
        vec![
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![
                    lit("edge", vec![x(), z()], false),
                    lit("path", vec![z(), y()], false),
                ],
            ),
        ]
    }

    /// The meet-reachability shape shared by the recursion tests and the
    /// property/differential harnesses:
    ///   m(X, aggr(V)) :- seed(X, V).
    ///   m(Y, aggr(V)) :- edge(X, Y), m(X, V).
    fn meet_reach_rules(aggr_name: &str) -> Vec<Rule> {
        vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![None, named(aggr_name)],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y(), z()],
                vec![None, named(aggr_name)],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x(), z()], false),
                ],
            ),
        ]
    }

    #[test]
    fn law1_transitive_closure_exact() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 2), (2, 3), (3, 4), (1, 3), (2, 4), (1, 4)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .collect();
        assert_eq!(db["path"], want);
    }

    #[test]
    fn law3_recursion_terminates_on_cyclic_data() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 1)]),
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        // Full 3×3 closure on a cycle.
        assert_eq!(db["path"].len(), 9);
    }

    #[test]
    fn law2_stratified_negation_evaluates_correctly() {
        // unreachable(X,Y) :- node(X), node(Y), not path(X,Y).
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert("node", (1..=3).map(|i| vec![v(i)]).collect());
        let mut rules = transitive_closure();
        rules.push(Rule::plain(
            "unreachable",
            vec![x(), y()],
            vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                lit("path", vec![x(), y()], true),
            ],
        ));
        let db = naive_eval(&Program {
            rules,
            facts,
            ..Program::default()
        })
        .unwrap();
        let want: BTreeSet<Tuple> = [(1, 1), (2, 1), (2, 2), (3, 1), (3, 2), (3, 3)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .collect();
        assert_eq!(db["unreachable"], want);
    }

    #[test]
    fn law2_unstratifiable_corpus_is_refused() {
        for (name, program) in unstratifiable_corpus() {
            assert!(
                matches!(
                    check_stratifiable(&program),
                    Err(Rejection::Unstratifiable(_))
                ),
                "must refuse: {name}"
            );
            assert!(naive_eval(&program).is_err(), "eval must refuse: {name}");
        }
    }

    #[test]
    fn law4_unsafe_rules_are_refused() {
        // Head variable unbound by any positive literal.
        let unbound_head = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("q", vec![y()], false)],
            )],
            ..Program::default()
        };
        assert_eq!(check_safety(&unbound_head), Err(Rejection::Unsafe("p")));

        // Negated literal over a variable no positive literal binds.
        let unbound_negation = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("q", vec![x()], false), lit("r", vec![z()], true)],
            )],
            ..Program::default()
        };
        assert_eq!(check_safety(&unbound_negation), Err(Rejection::Unsafe("p")));
    }

    #[test]
    fn constants_and_repeated_variables_unify_exactly() {
        // same(X) :- edge(X, X).  eq3(X) :- edge(3, X).
        let mut facts = edge_facts(&[(1, 1), (1, 2), (3, 5)]);
        facts.get_mut("edge").unwrap().insert(vec![v(4), v(4)]);
        let program = Program {
            rules: vec![
                Rule::plain("same", vec![x()], vec![lit("edge", vec![x(), x()], false)]),
                Rule::plain(
                    "eq3",
                    vec![x()],
                    vec![lit("edge", vec![Term::Const(v(3)), x()], false)],
                ),
            ],
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["same"], [vec![v(1)], vec![v(4)]].into_iter().collect());
        assert_eq!(db["eq3"], [vec![v(5)]].into_iter().collect());
    }

    /// Normal aggregation: group by the non-aggregated head positions and
    /// fold each group — groups shared across every rule of the head, sums
    /// exact `Int`s (the landed semantics, not upstream's f64 fold).
    #[test]
    fn normal_aggregation_groups_and_folds() {
        // total(D, sum(A), count(A)) :- sale(D, A); ... :- bonus(D, A).
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "sale",
            [(1, 10), (1, 20), (2, 5)]
                .iter()
                .map(|(d, a)| vec![v(*d), v(*a)])
                .collect(),
        );
        facts.insert("bonus", [vec![v(1), v(40)]].into_iter().collect());
        let rule = |rel| {
            Rule::aggregated(
                "total",
                vec![x(), y(), y()],
                vec![None, named("sum"), named("count")],
                vec![lit(rel, vec![x(), y()], false)],
            )
        };
        let program = Program {
            rules: vec![rule("sale"), rule("bonus")],
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 70, 3), (2, 5, 1)]
            .into_iter()
            .map(|(d, s, c)| vec![v(d), v(s), v(c)])
            .collect();
        assert_eq!(db["total"], want);
    }

    /// Aggregation over no rows: every position aggregated yields the
    /// single empty-fold row; a grouping position yields no rows at all.
    #[test]
    fn normal_aggregation_over_no_rows() {
        let all_aggregated = Program {
            rules: vec![Rule::aggregated(
                "c",
                vec![x(), x()],
                vec![named("count"), named("sum")],
                vec![lit("nothing", vec![x()], false)],
            )],
            ..Program::default()
        };
        let db = naive_eval(&all_aggregated).unwrap();
        assert_eq!(db["c"], [vec![v(0), v(0)]].into_iter().collect());

        let keyed = Program {
            rules: vec![Rule::aggregated(
                "t",
                vec![x(), y()],
                vec![None, named("count")],
                vec![lit("nothing", vec![x(), y()], false)],
            )],
            ..Program::default()
        };
        let db = naive_eval(&keyed).unwrap();
        assert!(db.get("t").is_none_or(|s| s.is_empty()));
    }

    /// Normal aggregation runs at the fixpoint: it folds the *complete*
    /// transitive closure computed in the stratum beneath it.
    #[test]
    fn normal_aggregation_folds_the_fixpoint_of_recursion() {
        // reach_count(X, count(Y)) :- path(X, Y).
        let mut rules = transitive_closure();
        rules.push(Rule::aggregated(
            "reach_count",
            vec![x(), y()],
            vec![None, named("count")],
            vec![lit("path", vec![x(), y()], false)],
        ));
        let program = Program {
            rules,
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 3), (2, 2), (3, 1)]
            .into_iter()
            .map(|(n, c)| vec![v(n), v(c)])
            .collect();
        assert_eq!(db["reach_count"], want);
    }

    /// The corpus counterpart: a self-recursive all-meet head is accepted
    /// and evaluated *inside* the fixpoint — here `min` labels flowing
    /// through a graph with a cycle, so termination is the meet's doing.
    #[test]
    fn meet_aggregation_evaluates_inside_recursion() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]);
        facts.insert(
            "seed",
            [(1, 5), (4, 1)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        let program = Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        };
        assert_eq!(check_stratifiable(&program), Ok(()));
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 5), (2, 5), (3, 5), (4, 1)]
            .into_iter()
            .map(|(n, l)| vec![v(n), v(l)])
            .collect();
        assert_eq!(db["m"], want);
    }

    /// A meet head with every position aggregated and no derivations
    /// yields the single identity row of its meets.
    #[test]
    fn meet_aggregation_over_no_rows_yields_the_identity_row() {
        let program = Program {
            rules: vec![Rule::aggregated(
                "g",
                vec![x(), y()],
                vec![named("min"), named("or")],
                vec![lit("nothing", vec![x(), y()], false)],
            )],
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(
            db["g"],
            [vec![DataValue::Null, DataValue::from(false)]]
                .into_iter()
                .collect()
        );
    }

    /// Review finding 1 (fix wave): the identity row of an all-aggregated
    /// meet head is a *real fact during recursion* — upstream meets it
    /// into the store at epoch 0, and derivations build on it. Here
    /// `m(or(W)) :- seed(W); m(or(W)) :- edge(V, W), m(V)` with no seeds:
    /// the identity `false` matches `edge(false, true)` and derives
    /// `true`; an oracle that only appended the identity after the
    /// fixpoint would answer `false`.
    #[test]
    fn meet_identity_row_feeds_recursion() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "edge",
            [vec![DataValue::from(false), DataValue::from(true)]]
                .into_iter()
                .collect(),
        );
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x()],
                vec![named("or")],
                vec![lit("seed", vec![x()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y()],
                vec![named("or")],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x()], false),
                ],
            ),
        ];
        let program = Program {
            rules,
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["m"], [vec![DataValue::from(true)]].into_iter().collect());
    }

    /// Review finding 1, second wave: the identity row must be *invisible*
    /// when derivations exist — upstream inserts it only when epoch 0
    /// derives nothing. `and`/`or` cannot tell (two-point lattices where
    /// the identity absorbs), but any larger lattice can: here `min`'s
    /// `Null` identity, if leaked into round-one recursion, would join
    /// `edge(Null, 1)` and derive a spurious 1, answering {1} instead of
    /// the least fixpoint {5}.
    #[test]
    fn meet_identity_row_is_invisible_when_derivations_exist() {
        // m(min(W)) :- seed(W);  m(min(W)) :- edge(V, W), m(V).
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("seed", [vec![v(5)]].into_iter().collect());
        facts.insert("edge", [vec![DataValue::Null, v(1)]].into_iter().collect());
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x()],
                vec![named("min")],
                vec![lit("seed", vec![x()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y()],
                vec![named("min")],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x()], false),
                ],
            ),
        ];
        let program = Program {
            rules,
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["m"], [vec![v(5)]].into_iter().collect());
    }

    /// Negation over a meet-aggregated relation forces a stratum, so the
    /// negating rule reads the *completed* accumulated relation.
    #[test]
    fn negation_reads_the_completed_meet_relation() {
        // unseeded(X) :- node(X), not m(X, true).
        let mut facts = edge_facts(&[(1, 2)]);
        facts.insert(
            "seed",
            [vec![v(1), DataValue::from(true)]].into_iter().collect(),
        );
        facts.insert("node", (1..=3).map(|i| vec![v(i)]).collect());
        let mut rules = meet_reach_rules("or");
        rules.push(Rule::plain(
            "unseeded",
            vec![x()],
            vec![
                lit("node", vec![x()], false),
                lit("m", vec![x(), Term::Const(DataValue::from(true))], true),
            ],
        ));
        let program = Program {
            rules,
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        // m accumulates {(1,true),(2,true)}; node 3 has no m row at all.
        assert_eq!(db["unseeded"], [vec![v(3)]].into_iter().collect());
    }

    /// Fixed rules are opaque relation transformers on stratum boundaries:
    /// a constant one feeds recursion from below, a projecting one
    /// consumes the completed closure from above, and plain rules read its
    /// output one stratum higher still.
    #[test]
    fn fixed_rules_sit_on_stratum_boundaries() {
        let constant_edges = FixedRule {
            head_rel: "edge",
            inputs: vec![],
            eval: |_| {
                [(1, 2), (2, 3)]
                    .iter()
                    .map(|(a, b)| vec![v(*a), v(*b)])
                    .collect()
            },
        };
        let path_sources = FixedRule {
            head_rel: "sources",
            inputs: vec!["path"],
            eval: |inputs| inputs[0].iter().map(|t| vec![t[0].clone()]).collect(),
        };
        let mut rules = transitive_closure();
        rules.push(Rule::plain(
            "out",
            vec![x()],
            vec![lit("sources", vec![x()], false)],
        ));
        let program = Program {
            rules,
            fixed: vec![constant_edges, path_sources],
            ..Program::default()
        };
        let s = strata(&program);
        assert!(
            s["path"] > s["edge"],
            "readers sit strictly above a fixed rule"
        );
        assert!(
            s["sources"] > s["path"],
            "a fixed rule sits strictly above its inputs"
        );
        assert!(s["out"] > s["sources"]);
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["path"].len(), 3);
        let want: BTreeSet<Tuple> = [vec![v(1)], vec![v(2)]].into_iter().collect();
        assert_eq!(db["sources"], want);
        assert_eq!(db["out"], want);
    }

    /// Law 5 at the oracle: aggregation type errors surface as values,
    /// through both the meet path and the normal path.
    #[test]
    fn aggregation_type_errors_are_values_not_panics() {
        // min meeting a Bool into a Bool.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "seed",
            [
                vec![v(1), DataValue::from(false)],
                vec![v(1), DataValue::from(true)],
            ]
            .into_iter()
            .collect(),
        );
        facts.insert("edge", BTreeSet::new());
        let program = Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        };
        assert!(matches!(naive_eval(&program), Err(Rejection::AggrError(_))));

        // sum folding a Bool.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "d",
            [vec![v(1), DataValue::from(true)]].into_iter().collect(),
        );
        let program = Program {
            rules: vec![Rule::aggregated(
                "t",
                vec![x(), y()],
                vec![None, named("sum")],
                vec![lit("d", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        assert!(matches!(naive_eval(&program), Err(Rejection::AggrError(_))));
    }

    /// The ill-formed shapes the real compiler refuses at parse/compile
    /// time (upstream `parser::head_aggr_mismatch` among them) are refused
    /// here as values.
    #[test]
    fn malformed_programs_are_refused_not_evaluated() {
        // Aggregation vector shorter than the head.
        let short = Program {
            rules: vec![Rule::aggregated(
                "p",
                vec![x(), y()],
                vec![named("min")],
                vec![lit("d", vec![x(), y()], false)],
            )],
            ..Program::default()
        };
        assert!(matches!(naive_eval(&short), Err(Rejection::Malformed("p"))));

        // Rules of one head disagreeing on the aggregation signature.
        let mismatch = Program {
            rules: vec![
                Rule::aggregated(
                    "p",
                    vec![x(), y()],
                    vec![None, named("min")],
                    vec![lit("d", vec![x(), y()], false)],
                ),
                Rule::aggregated(
                    "p",
                    vec![x(), y()],
                    vec![None, named("count")],
                    vec![lit("d", vec![x(), y()], false)],
                ),
            ],
            ..Program::default()
        };
        assert!(matches!(
            naive_eval(&mismatch),
            Err(Rejection::Malformed("p"))
        ));

        // A fixed head that is also a rule head.
        let clash = Program {
            rules: vec![Rule::plain(
                "f",
                vec![x()],
                vec![lit("d", vec![x()], false)],
            )],
            fixed: vec![FixedRule {
                head_rel: "f",
                inputs: vec![],
                eval: |_| BTreeSet::new(),
            }],
            ..Program::default()
        };
        assert!(matches!(naive_eval(&clash), Err(Rejection::Malformed("f"))));

        // Facts under an aggregated head.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("m", [vec![v(1), v(1)]].into_iter().collect());
        let seeded = Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        };
        assert!(matches!(
            naive_eval(&seeded),
            Err(Rejection::Malformed("m"))
        ));

        // Duplicate fixed heads.
        let dup = Program {
            fixed: vec![
                FixedRule {
                    head_rel: "f",
                    inputs: vec![],
                    eval: |_| BTreeSet::new(),
                },
                FixedRule {
                    head_rel: "f",
                    inputs: vec![],
                    eval: |_| BTreeSet::new(),
                },
            ],
            ..Program::default()
        };
        assert!(matches!(naive_eval(&dup), Err(Rejection::Malformed("f"))));

        // A relation used at two different arities.
        let clash = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("edge", vec![x()], false)],
            )],
            facts: edge_facts(&[(1, 2)]),
            ..Program::default()
        };
        assert!(matches!(
            naive_eval(&clash),
            Err(Rejection::Malformed("edge"))
        ));
    }

    /// Which changed-flag the delta machinery believes.
    #[derive(Clone, Copy)]
    enum FlagMode {
        /// The landed contract: true iff the stored value changed.
        Landed,
        /// Upstream's inverted `and`/`or` flag (`old == *l`): believe the
        /// opposite of what happened.
        UpstreamInverted,
    }

    /// A transcription of upstream's semi-naive meet evaluation for the
    /// [`meet_reach_rules`] shape (`eval.rs::initial_rule_meet_eval` /
    /// `incremental_rule_meet_eval` joining against the delta, plus
    /// `temp_store.rs::MeetAggrStore::merge_in`'s flag-gated delta): per
    /// epoch, the recursive rule derives only from the previous delta,
    /// rows meet into the running total, and a key re-enters the delta
    /// only when the changed-flag says its accumulated value moved. The
    /// flag is therefore load-bearing: lie once and propagation stops.
    fn semi_naive_meet_reach(
        edges: &BTreeSet<(i64, i64)>,
        seeds: &BTreeMap<i64, DataValue>,
        op: &dyn MeetAggrObj,
        mode: FlagMode,
    ) -> BTreeMap<i64, DataValue> {
        let mut total: BTreeMap<i64, DataValue> = BTreeMap::new();
        // Epoch 0: only the seed rule fires — the recursive store is empty.
        let mut epoch_rows: Vec<(i64, DataValue)> =
            seeds.iter().map(|(k, val)| (*k, val.clone())).collect();
        for _epoch in 0..100_000 {
            // The epoch's own meet store: rows meet together before merging.
            let mut fresh: BTreeMap<i64, DataValue> = BTreeMap::new();
            for (k, val) in epoch_rows {
                match fresh.entry(k) {
                    Entry::Vacant(e) => {
                        e.insert(val);
                    }
                    Entry::Occupied(mut e) => {
                        op.update(e.get_mut(), &val).expect("meet update");
                    }
                }
            }
            // merge_in: flag-gated delta discovery.
            let mut delta: BTreeMap<i64, DataValue> = BTreeMap::new();
            for (k, val) in fresh {
                match total.entry(k) {
                    Entry::Vacant(e) => {
                        delta.insert(k, val.clone());
                        e.insert(val);
                    }
                    Entry::Occupied(mut e) => {
                        let really_changed = op.update(e.get_mut(), &val).expect("meet update");
                        let believed = match mode {
                            FlagMode::Landed => really_changed,
                            FlagMode::UpstreamInverted => !really_changed,
                        };
                        if believed {
                            delta.insert(k, e.get().clone());
                        }
                    }
                }
            }
            if delta.is_empty() {
                return total;
            }
            // Next epoch: the recursive rule joined against the delta only.
            let mut next = Vec::new();
            for (from, val) in &delta {
                for (a, b) in edges {
                    if a == from {
                        next.push((*b, val.clone()));
                    }
                }
            }
            epoch_rows = next;
        }
        panic!("semi-naive simulator failed to converge");
    }

    /// The upstream `and`/`or` premature-fixpoint bug, as a differential.
    /// Upstream's `MeetAggrAnd`/`MeetAggrOr` returned `old == *l` — the
    /// inversion of the changed-flag contract — so the one update that
    /// flips an accumulated value announced "unchanged", the key never
    /// re-entered the delta, and recursion stopped one hop short. The
    /// naive oracle computes the correct fixpoint; the same semi-naive
    /// machinery run with the inverted flag reproduces exactly what
    /// upstream would have returned, and the two must differ.
    #[test]
    fn and_or_inverted_flag_reaches_a_premature_fixpoint() {
        let edges: BTreeSet<(i64, i64)> = [(1, 2), (2, 3)].into_iter().collect();
        // or: truth must propagate 1 → 2 → 3; and: falsity must.
        for (name, seed_of, fixpoint) in [
            ("or", [true, false, false], true),
            ("and", [false, true, true], false),
        ] {
            let seeds: BTreeMap<i64, DataValue> = (1..=3)
                .map(|k| (k, DataValue::from(seed_of[(k - 1) as usize])))
                .collect();
            let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
            facts.insert(
                "edge",
                edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
            );
            facts.insert(
                "seed",
                seeds
                    .iter()
                    .map(|(k, val)| vec![v(*k), val.clone()])
                    .collect(),
            );
            let program = Program {
                rules: meet_reach_rules(name),
                facts,
                ..Program::default()
            };
            let db = naive_eval(&program).unwrap();
            let correct: BTreeMap<i64, DataValue> =
                (1..=3).map(|k| (k, DataValue::from(fixpoint))).collect();
            let oracle: BTreeMap<i64, DataValue> = db["m"]
                .iter()
                .map(|t| (t[0].get_int().expect("int key"), t[1].clone()))
                .collect();
            assert_eq!(oracle, correct, "oracle fixpoint for {name}");

            let op = parse_aggr(name)
                .expect("real aggregation")
                .meet_op()
                .expect("meet form");
            // The honest flag reaches the oracle's fixpoint...
            let honest = semi_naive_meet_reach(&edges, &seeds, op.as_ref(), FlagMode::Landed);
            assert_eq!(
                honest, oracle,
                "honest semi-naive equals the oracle for {name}"
            );
            // ...the inverted flag stops early: node 2's flip is applied
            // to the store but never re-enters the delta, so node 3 keeps
            // its seed value.
            let buggy =
                semi_naive_meet_reach(&edges, &seeds, op.as_ref(), FlagMode::UpstreamInverted);
            assert_ne!(
                buggy, oracle,
                "the upstream inversion must be observable for {name}"
            );
            assert_eq!(
                buggy[&2],
                DataValue::from(fixpoint),
                "node 2's stored value did move"
            );
            assert_eq!(
                buggy[&3],
                DataValue::from(!fixpoint),
                "node 3 is stranded at its seed: the premature fixpoint for {name}"
            );
        }
    }

    #[derive(Clone, Debug)]
    struct MeetCase {
        aggr_name: &'static str,
        edges: BTreeSet<(i64, i64)>,
        seeds: BTreeMap<i64, DataValue>,
    }

    fn case_for(name: &'static str, value: BoxedStrategy<DataValue>) -> BoxedStrategy<MeetCase> {
        (1i64..=5)
            .prop_flat_map(move |n| {
                let value = value.clone();
                (
                    prop::collection::btree_set((0..n, 0..n), 0..8),
                    prop::collection::btree_map(0..n, value, 0..=(n as usize)),
                )
            })
            .prop_map(move |(edges, seeds)| MeetCase {
                aggr_name: name,
                edges,
                seeds,
            })
            .boxed()
    }

    /// Random small meet-recursive programs over the commutative meets;
    /// values are typed per aggregation (`union` seeds are `Set`s, the
    /// canonical accumulator representation).
    fn arb_meet_case() -> BoxedStrategy<MeetCase> {
        let bool_val = || any::<bool>().prop_map(DataValue::from).boxed();
        let int_val = || (-10i64..10).prop_map(DataValue::from).boxed();
        let set_val = prop::collection::btree_set((0i64..4).prop_map(DataValue::from), 0..3)
            .prop_map(DataValue::Set)
            .boxed();
        prop_oneof![
            case_for("or", bool_val()),
            case_for("and", bool_val()),
            case_for("min", int_val()),
            case_for("max", int_val()),
            case_for("union", set_val),
        ]
        .boxed()
    }

    proptest! {
        /// Oracle self-consistency: on randomly generated meet-recursive
        /// programs, naive re-derivation-to-fixpoint equals the
        /// upstream-shaped semi-naive strategy driven by the landed
        /// changed-flags, and a plain rule one stratum up reads exactly
        /// the accumulated meet relation.
        #[test]
        fn naive_meet_fixpoint_matches_semi_naive(case in arb_meet_case()) {
            let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
            facts.insert(
                "edge",
                case.edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
            );
            facts.insert(
                "seed",
                case.seeds.iter().map(|(k, val)| vec![v(*k), val.clone()]).collect(),
            );
            let mut rules = meet_reach_rules(case.aggr_name);
            rules.push(Rule::plain(
                "out",
                vec![x(), y()],
                vec![lit("m", vec![x(), y()], false)],
            ));
            let program = Program { rules, facts, ..Program::default() };
            let db = naive_eval(&program).expect("stratifiable meet program");
            let m = db.get("m").cloned().unwrap_or_default();

            let op = parse_aggr(case.aggr_name)
                .expect("real aggregation")
                .meet_op()
                .expect("meet form");
            let semi_naive: BTreeSet<Tuple> =
                semi_naive_meet_reach(&case.edges, &case.seeds, op.as_ref(), FlagMode::Landed)
                    .into_iter()
                    .map(|(k, val)| vec![v(k), val])
                    .collect();
            prop_assert_eq!(&m, &semi_naive);
            prop_assert_eq!(db.get("out").cloned().unwrap_or_default(), m);
        }
    }

    // ═════════════════════════════════════════════════════════════════
    // The unified temporal oracle: resolution, derived intervals, diff —
    // unifying this module's naive_eval with the two bespoke test-oracle
    // families it replaces (`time_travel_trials.rs::naive_asof`,
    // `time_travel_script_laws.rs::oracle_at`).
    // ═════════════════════════════════════════════════════════════════

    fn k(i: i64) -> Tuple {
        vec![v(i)]
    }
    fn pay(i: i64) -> Tuple {
        vec![v(i)]
    }
    /// The full governing tuple `key ++ payload` for key `i`, payload `p`.
    fn kv(i: i64, p: i64) -> Tuple {
        vec![v(i), v(p)]
    }

    /// `Event` construction is fallible only for the reserved terminal
    /// tick (`i64::MAX`); no fixture below ever uses it (the dedicated
    /// tests for that reservation construct it explicitly, without these
    /// helpers). Panicking here is `expect`, not a swallowed error.
    fn ev_assert(key: Tuple, payload: Tuple, valid: i64, sys: i64) -> Event {
        Event::assert(key, payload, valid, sys)
            .expect("valid instant is never the reserved terminal tick in these fixtures")
    }
    fn ev_retract(key: Tuple, valid: i64, sys: i64) -> Event {
        Event::retract(key, valid, sys)
            .expect("valid instant is never the reserved terminal tick in these fixtures")
    }
    fn ev_erase(key: Tuple, valid: i64, sys: i64) -> Event {
        Event::erase(key, valid, sys)
            .expect("valid instant is never the reserved terminal tick in these fixtures")
    }

    // ── Degenerate-case table: each ruled case pinned as its own test ───

    #[test]
    fn retract_clips_start_to_retract_exclusive() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(100), 10, 0),
            ev_retract(key.clone(), 30, 0),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![Interval {
                start: 10,
                end: 30,
                tuple: kv(1, 100)
            }]
        );
        assert_eq!(
            resolve(&history, &key, AsOf { valid: 29, sys: 0 }),
            Some(kv(1, 100))
        );
        assert_eq!(
            resolve(&history, &key, AsOf { valid: 30, sys: 0 }),
            None,
            "the retract's own instant is excluded from the prior interval"
        );
    }

    #[test]
    fn dangling_retract_blocks_erase_fall_through() {
        // An older instant asserts; a newer, terminal instant retracts:
        // the retract settles absence definitively — nothing may fall
        // through to the older claim, unlike an Erase in the same shape.
        let key = k(1);
        let retracted = vec![
            ev_assert(key.clone(), pay(1), 10, 0),
            ev_retract(key.clone(), 20, 0),
        ];
        assert_eq!(resolve(&retracted, &key, AsOf::current()), None);

        let erased = vec![
            ev_assert(key.clone(), pay(1), 10, 0),
            ev_erase(key.clone(), 20, 0),
        ];
        assert_eq!(
            resolve(&erased, &key, AsOf::current()),
            Some(kv(1, 1)),
            "erase is transparent; a dangling retract is not"
        );
    }

    #[test]
    fn double_assert_same_payload_is_idempotent_one_interval() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(9), 10, 0),
            ev_assert(key.clone(), pay(9), 20, 0),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![Interval {
                start: 10,
                end: OPEN_END,
                tuple: kv(1, 9)
            }],
            "identical re-asserts coalesce into one interval"
        );
    }

    #[test]
    fn double_assert_different_payload_splits_at_the_second_assert() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(9), 10, 0),
            ev_assert(key.clone(), pay(8), 20, 0),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 10,
                    end: 20,
                    tuple: kv(1, 9)
                },
                Interval {
                    start: 20,
                    end: OPEN_END,
                    tuple: kv(1, 8)
                },
            ]
        );
    }

    #[test]
    fn assert_after_retract_opens_a_new_interval() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(1), 10, 0),
            ev_retract(key.clone(), 20, 0),
            ev_assert(key.clone(), pay(2), 30, 0),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 10,
                    end: 20,
                    tuple: kv(1, 1)
                },
                Interval {
                    start: 30,
                    end: OPEN_END,
                    tuple: kv(1, 2)
                },
            ]
        );
    }

    #[test]
    fn assert_then_retract_same_instant_newer_sys_holds_nowhere() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(1), 10, 0),
            ev_retract(key.clone(), 10, 1),
        ];
        assert_eq!(resolve(&history, &key, AsOf::current()), None);
        assert!(
            derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys).is_empty(),
            "the fact holds at no instant"
        );
        // Before the correction's own stamp, the assert still governed.
        assert_eq!(
            resolve(&history, &key, AsOf { valid: 10, sys: 0 }),
            Some(kv(1, 1))
        );
    }

    #[test]
    fn erase_is_transparent_to_intervals() {
        // Assert at 10; a system correction erases the instant at 20 (no
        // claim was ever really made there); assert again at 30 with a
        // DIFFERENT payload. The derived interval must show the original
        // claim continuing straight through the erased instant.
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(1), 10, 0),
            ev_erase(key.clone(), 20, 0),
            ev_assert(key.clone(), pay(2), 30, 0),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 10,
                    end: 30,
                    tuple: kv(1, 1)
                },
                Interval {
                    start: 30,
                    end: OPEN_END,
                    tuple: kv(1, 2)
                },
            ],
            "the erased instant contributes no breakpoint of its own"
        );
    }

    #[test]
    fn instants_are_one_tick_no_zero_width_intervals() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(1), 10, 0),
            ev_assert(key.clone(), pay(2), 11, 0),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 10,
                    end: 11,
                    tuple: kv(1, 1)
                },
                Interval {
                    start: 11,
                    end: OPEN_END,
                    tuple: kv(1, 2)
                },
            ]
        );
        for iv in &ivs {
            assert!(iv.end > iv.start, "no zero-width interval: {iv:?}");
        }
    }

    #[test]
    fn system_axis_interval_of_a_version_is_stamp_to_next_version_stamp() {
        // One valid instant, three system corrections: [0,5) the first
        // claim, [5,9) the second, [9, OPEN_END) the third and current.
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(1), 100, 0),
            ev_assert(key.clone(), pay(2), 100, 5),
            ev_assert(key.clone(), pay(3), 100, 9),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Sys, 100);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 0,
                    end: 5,
                    tuple: kv(1, 1)
                },
                Interval {
                    start: 5,
                    end: 9,
                    tuple: kv(1, 2)
                },
                Interval {
                    start: 9,
                    end: OPEN_END,
                    tuple: kv(1, 3)
                },
            ]
        );
    }

    // ── The reserved terminal tick (hostile-review ruling) ──────────────

    /// `Event::assert`/`retract`/`erase` all refuse `valid == i64::MAX` —
    /// the terminal tick is reserved for the `@ 'END'` write-side
    /// sentinel, never a storable event coordinate.
    #[test]
    fn terminal_tick_is_reserved_and_refused_at_construction() {
        assert!(Event::assert(k(1), pay(1), i64::MAX, 0).is_err());
        assert!(Event::retract(k(1), i64::MAX, 0).is_err());
        assert!(Event::erase(k(1), i64::MAX, 0).is_err());
        // Every other instant, including the one just short of it, is fine.
        assert!(Event::assert(k(1), pay(1), i64::MAX - 1, 0).is_ok());
    }

    /// The reviewer's reproducer: an assert at the terminal tick is
    /// refused at construction, so `derive_intervals` never even sees it
    /// — the zero-width `[i64::MAX, i64::MAX)` interval the old
    /// "unreachable" waiver would have let through is unrepresentable,
    /// not merely rare.
    #[test]
    fn assert_at_terminal_tick_never_produces_a_zero_width_interval() {
        let key = k(1);
        let history = vec![ev_assert(key.clone(), pay(1), 10, 0)];
        let err = Event::assert(key.clone(), pay(2), i64::MAX, 1)
            .expect_err("the terminal tick must be refused, not silently accepted");
        assert!(
            err.to_string().contains("reserved"),
            "expected a reservation error, got: {err}"
        );
        // The history therefore never contains a terminal-tick event, and
        // every derived interval is non-zero-width.
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        for iv in &ivs {
            assert!(iv.end > iv.start, "no zero-width interval: {iv:?}");
        }
    }

    // ── Cross-check against the real kernel ─────────────────────────────

    /// The plain-ascending `laws::AsOf` mirror and the real, Reverse-
    /// wrapped `data::bitemporal` kernel (`check_key_for_bitemporal`) pick
    /// the SAME governing version on shared rows — the two reference
    /// models are provably the same algebra, not merely similarly worded.
    /// The kernel side replicates `data/bitemporal.rs`'s own
    /// `skip_walk`/`bikey` test helpers (private to that module, so
    /// reconstructed here rather than imported) against the real, public
    /// `check_key_for_bitemporal`; the mirror side calls this module's
    /// `resolve_relation` on the identical rows, translated per the exact
    /// correspondence documented on [`AsOf`].
    #[test]
    fn asof_mirror_matches_bitemporal_kernel_on_a_shared_fixture() {
        use crate::data::bitemporal::check_key_for_bitemporal;
        use crate::data::tuple::{RelationId, TupleT};
        use crate::data::value::{Validity, ValidityTs};
        use std::cmp::Reverse;

        fn vts(t: i64) -> ValidityTs {
            ValidityTs(Reverse(t))
        }
        fn slot(t: i64) -> Validity {
            Validity {
                timestamp: vts(t),
                is_assert: Reverse(true),
            }
        }
        fn bikey(fact: i64, valid_ts: i64, sys_ts: i64) -> Vec<u8> {
            [
                DataValue::from(fact),
                DataValue::Validity(slot(valid_ts)),
                DataValue::Validity(slot(sys_ts)),
            ]
            .encode_as_key(RelationId(7))
            .into_vec()
        }
        /// A from-scratch skip-walk over the real kernel — the same shape
        /// as `data/bitemporal.rs`'s private `skip_walk` test helper.
        fn kernel_resolves(
            store: &BTreeMap<Vec<u8>, ClaimPolarity>,
            sys_at: i64,
            valid_at: i64,
        ) -> BTreeSet<i64> {
            let mut out = BTreeSet::new();
            let mut bound = vec![];
            let mut steps = 0usize;
            loop {
                steps += 1;
                assert!(
                    steps <= 4 * store.len() + 4,
                    "kernel walk failed to terminate"
                );
                let Some((k, polarity)) = store.range(bound..).next() else {
                    break;
                };
                let (ret, nxt) = check_key_for_bitemporal(
                    k,
                    *polarity,
                    crate::data::value::AsOf {
                        sys: vts(sys_at),
                        valid: vts(valid_at),
                    },
                    None,
                )
                .expect("well-formed test key");
                bound = if nxt.as_slice() > k.as_slice() {
                    nxt
                } else {
                    let mut succ = k.clone();
                    succ.push(0);
                    succ
                };
                if let Some(t) = ret {
                    out.insert(t[0].get_int().expect("int fact column"));
                }
            }
            out
        }

        // Two facts; asserts, a retraction, and a system-time erasure
        // interleaved across instants and corrections — the same
        // ingredients `data/bitemporal.rs`'s own fixtures exercise, with
        // negative valid AND negative sys coordinates folded into the
        // STORED rows themselves (hostile-review pin: sign-boundary
        // coverage belongs in the fixture, not only in the probe grid).
        let rows: Vec<(i64, i64, i64, ClaimPolarity)> = vec![
            (1, -20, -20, ClaimPolarity::Assert),
            (1, -3, -10, ClaimPolarity::Assert),
            (1, -3, -5, ClaimPolarity::Retract),
            (1, 10, -5, ClaimPolarity::Assert),
            (1, 10, 15, ClaimPolarity::Erase),
            (1, 20, 5, ClaimPolarity::Assert),
            (2, 15, -25, ClaimPolarity::Assert),
        ];
        let store: BTreeMap<Vec<u8>, ClaimPolarity> = rows
            .iter()
            .map(|(f, valid, sys, p)| (bikey(*f, *valid, *sys), *p))
            .collect();
        let events: Vec<Event> = rows
            .iter()
            .map(|(f, valid, sys, polarity)| Event {
                key: vec![v(*f)],
                payload: vec![],
                valid: *valid,
                sys: *sys,
                polarity: *polarity,
            })
            .collect();

        for sys_at in [-25i64, -5, 0, 5, 15, 25] {
            for valid_at in [-30i64, -10, -3, 0, 10, 20, 30] {
                let kernel = kernel_resolves(&store, sys_at, valid_at);
                let mirror: BTreeSet<i64> = resolve_relation(
                    &events,
                    AsOf {
                        valid: valid_at,
                        sys: sys_at,
                    },
                )
                .into_iter()
                .map(|t| t[0].get_int().expect("int fact"))
                .collect();
                assert_eq!(
                    mirror, kernel,
                    "sys_at={sys_at} valid_at={valid_at}: the laws::AsOf mirror \
                     disagrees with the real bitemporal kernel"
                );
            }
        }
    }

    /// The typed refusal, not a panic: a literal's `as_of` naming a
    /// relation entirely absent from `Program::histories` (never mind
    /// present-with-zero-rows) is `Rejection::Malformed` at
    /// `check_wellformed`, before evaluation ever runs.
    #[test]
    fn as_of_naming_a_relation_absent_from_histories_is_refused() {
        let program = Program {
            rules: vec![Rule::plain(
                "out",
                vec![x()],
                vec![lit_at("ghost", vec![x()], false, AsOf::current_at(10))],
            )],
            ..Program::default()
        };
        assert_eq!(
            check_wellformed(&program),
            Err(Rejection::Malformed("ghost"))
        );
        assert!(matches!(
            naive_eval(&program),
            Err(Rejection::Malformed("ghost"))
        ));
    }

    /// A rule head sharing a name with a historical relation is refused:
    /// its derivation would land in `db` under that name while every
    /// reader (`literal_rows`) still resolves the SAME name through
    /// `histories` first — the derived rows would exist and never be
    /// seen. Pinned as its own test alongside the facts∩histories check
    /// it sits beside (hostile-review finding, issue #62 comment
    /// 4882951801).
    #[test]
    fn rule_head_sharing_a_name_with_a_historical_relation_is_refused() {
        // Arity-consistent on purpose (`h`'s historical rows are key
        // arity 1 + payload arity 1 = 2, matching the rule's own head and
        // body here): this isolates the NEW rule-head∩histories refusal
        // from the pre-existing arity-mismatch refusal, which would
        // otherwise return the same `Malformed("h")` value for an
        // unrelated reason and mask a broken check.
        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert("h", vec![ev_assert(k(1), pay(1), 5, 0)]);
        let program = Program {
            rules: vec![Rule::plain(
                "h",
                vec![x(), y()],
                vec![lit("h", vec![x(), y()], false)],
            )],
            histories,
            ..Program::default()
        };
        assert_eq!(check_wellformed(&program), Err(Rejection::Malformed("h")));
        assert!(matches!(
            naive_eval(&program),
            Err(Rejection::Malformed("h"))
        ));
    }

    /// The fixed-rule twin of the above: a fixed rule's head sharing a
    /// name with a historical relation is refused the same way.
    #[test]
    fn fixed_head_sharing_a_name_with_a_historical_relation_is_refused() {
        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert("h", vec![ev_assert(k(1), pay(1), 5, 0)]);
        let program = Program {
            fixed: vec![FixedRule {
                head_rel: "h",
                inputs: vec![],
                eval: |_| BTreeSet::new(),
            }],
            histories,
            ..Program::default()
        };
        assert_eq!(check_wellformed(&program), Err(Rejection::Malformed("h")));
    }

    // ── The untimed embedding ────────────────────────────────────────

    #[test]
    fn untimed_event_embedding_matches_a_plain_fact_byte_identically() {
        // path(X,Y) :- edge(X,Y); path(X,Y) :- edge(X,Z), path(Z,Y), run
        // once with `edge` as a plain fact set, once with `edge` as a
        // historical relation whose events are the untimed embedding of
        // the SAME tuples ([`Event::untimed`]) — the two must agree
        // byte-for-byte on the derived `path` relation.
        let edges = edge_facts(&[(1, 2), (2, 3), (3, 4)]);
        let plain = Program {
            rules: transitive_closure(),
            facts: edges.clone(),
            ..Program::default()
        };
        let plain_db = naive_eval(&plain).unwrap();

        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert(
            "edge",
            edges["edge"].iter().cloned().map(Event::untimed).collect(),
        );
        let historical = Program {
            rules: transitive_closure(),
            histories,
            ..Program::default()
        };
        let historical_db = naive_eval(&historical).unwrap();
        assert_eq!(historical_db["path"], plain_db["path"]);
    }

    // ── Per-literal resolution inside naive_eval ────────────────────────

    #[test]
    fn naive_eval_resolves_historical_literals_at_their_own_coordinate() {
        // both(X) :- edge{X,_}@5, edge{X,_}@15 — a key present in only
        // one of the two snapshots must never join.
        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert(
            "edge",
            vec![
                ev_assert(k(1), pay(0), 0, 0),  // 1 present from t=0
                ev_retract(k(1), 10, 0),        // 1 gone from t=10
                ev_assert(k(2), pay(0), 10, 0), // 2 present from t=10
            ],
        );
        let program = Program {
            rules: vec![Rule::plain(
                "both",
                vec![x()],
                vec![
                    lit_at("edge", vec![x(), y()], false, AsOf::current_at(5)),
                    lit_at("edge", vec![x(), z()], false, AsOf::current_at(15)),
                ],
            )],
            histories,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        // A rule that derives zero rows may be absent from `db` entirely
        // (the fixpoint loop only touches `db.entry(head)` for a nonempty
        // `rows`, matching the rest of this module's convention, e.g.
        // `normal_aggregation_over_no_rows`).
        assert!(
            db.get("both").is_none_or(BTreeSet::is_empty),
            "no key is present at both t=5 and t=15: {:?}",
            db.get("both")
        );
    }

    #[test]
    fn negation_without_its_own_as_of_is_not_refused() {
        // A negated literal that does NOT carry its own coordinate reads
        // the query-level default like any other literal — only an
        // EXPLICIT per-literal as-of on a negated literal is refused.
        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert("hist", vec![ev_assert(k(1), vec![], 5, 0)]);
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("base", [k(1), k(2)].into_iter().collect());
        let program = Program {
            rules: vec![Rule::plain(
                "absent",
                vec![x()],
                vec![lit("base", vec![x()], false), lit("hist", vec![x()], true)],
            )],
            facts,
            histories,
            ..Program::default()
        };
        let db = naive_eval_at(&program, AsOf::current()).unwrap();
        assert_eq!(db["absent"], [k(2)].into_iter().collect());
    }

    // ── The typed refusal: mirrors NegationOverTimeTravelError ──────────

    #[test]
    fn negation_over_time_travel_literal_is_refused() {
        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert("hist", vec![ev_assert(k(1), vec![], 5, 0)]);
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("base", [k(1)].into_iter().collect());
        let program = Program {
            rules: vec![Rule::plain(
                "out",
                vec![x()],
                vec![
                    lit("base", vec![x()], false),
                    lit_at("hist", vec![x()], true, AsOf { valid: 10, sys: 10 }),
                ],
            )],
            facts,
            histories,
            ..Program::default()
        };
        assert_eq!(
            naive_eval(&program),
            Err(Rejection::NegationOverTimeTravel("out"))
        );
        assert_eq!(
            check_time_travel_negation(&program),
            Err(Rejection::NegationOverTimeTravel("out"))
        );
    }

    // ── Generative campaigns: the grid differential and the diff
    // composition law, seeded per the splitmix64 discipline of
    // `query/trials.rs`. ─────────────────────────────────────────────────

    struct Rng {
        state: u64,
    }
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng { state: seed }
        }
        fn next_u64(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            debug_assert!(n > 0);
            self.next_u64() % n
        }
        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            debug_assert!(hi > lo);
            lo + self.below((hi - lo) as u64) as i64
        }
        fn one_of<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[self.below(xs.len() as u64) as usize]
        }
    }

    /// A random event history for one key: a handful of events at small,
    /// often-colliding valid/sys coordinates (so same-instant collisions,
    /// retract/erase interplay, and payload repeats are all common), plus
    /// noise from an unrelated key the resolution/derivation must ignore.
    fn gen_history(rng: &mut Rng, key: &Tuple) -> Vec<Event> {
        let n = rng.range(1, 10);
        let polarities = [
            ClaimPolarity::Assert,
            ClaimPolarity::Retract,
            ClaimPolarity::Erase,
        ];
        let mut history = Vec::new();
        for _ in 0..n {
            let valid = rng.range(0, 6);
            let sys = rng.range(0, 6);
            match rng.one_of(&polarities) {
                ClaimPolarity::Assert => {
                    history.push(ev_assert(key.clone(), pay(rng.range(0, 3)), valid, sys));
                }
                ClaimPolarity::Retract => history.push(ev_retract(key.clone(), valid, sys)),
                ClaimPolarity::Erase => history.push(ev_erase(key.clone(), valid, sys)),
            }
        }
        for _ in 0..rng.range(0, 4) {
            let valid = rng.range(0, 6);
            let sys = rng.range(0, 6);
            history.push(ev_assert(k(999), pay(rng.range(0, 3)), valid, sys));
        }
        history
    }

    /// Every distinct stored coordinate on `axis`, ± one tick, plus the
    /// extremes — the pointwise grid the ratified design claims is
    /// COMPLETE for a step function that only changes at stored
    /// coordinates.
    fn grid(history: &[Event], axis: Axis) -> Vec<i64> {
        let mut pts: Vec<i64> = history
            .iter()
            .flat_map(|e| {
                let c = match axis {
                    Axis::Valid => e.valid,
                    Axis::Sys => e.sys,
                };
                [c - 1, c, c + 1]
            })
            .collect();
        pts.push(i64::MIN);
        // Not `i64::MAX` itself as a QUERY point: `OPEN_END` and
        // `AsOf::current()`'s "see everything" bound share that one value
        // by construction, so the half-open interval `[start, OPEN_END)`
        // technically excludes the single instant `i64::MAX`. This is no
        // longer waved off as "unreachable" — the terminal tick is a
        // RESERVED coordinate (hostile-review ruling, issue #62 comment
        // 4882951801): `Event::assert`/`retract`/`erase` refuse
        // `valid == i64::MAX` at construction (see
        // `terminal_tick_is_reserved_and_refused_at_construction` and
        // `assert_at_terminal_tick_never_produces_a_zero_width_interval`
        // below), so no STORED coordinate can ever collide with the
        // sentinel; probing the grid one tick short of it is still the
        // right complete-grid extreme for a QUERY coordinate, which is
        // unrestricted (`AsOf::current()` legitimately queries at
        // `i64::MAX`).
        pts.push(i64::MAX - 1);
        pts.sort_unstable();
        pts.dedup();
        pts
    }

    #[test]
    fn grid_differential_derived_intervals_equal_maximal_runs() {
        let mut cases = 0usize;
        for seed in 0..500u64 {
            let mut rng = Rng::new(0xB17E_5EED_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let key = k(1);
            let history = gen_history(&mut rng, &key);
            let valid_grid = grid(&history, Axis::Valid);
            let sys_grid = grid(&history, Axis::Sys);
            for &sys_pt in &sys_grid {
                let ivs = derive_intervals(&history, &key, Axis::Valid, sys_pt);
                for &valid_pt in &valid_grid {
                    let direct = resolve(
                        &history,
                        &key,
                        AsOf {
                            valid: valid_pt,
                            sys: sys_pt,
                        },
                    );
                    let via_intervals = ivs
                        .iter()
                        .find(|iv| iv.start <= valid_pt && valid_pt < iv.end)
                        .map(|iv| iv.tuple.clone());
                    assert_eq!(
                        direct, via_intervals,
                        "seed {seed}: valid axis, valid={valid_pt} sys={sys_pt} history={history:?}"
                    );
                    cases += 1;
                }
            }
            for &fixed_valid in &[history.first().map(|e| e.valid).unwrap_or(0), 3] {
                let ivs = derive_intervals(&history, &key, Axis::Sys, fixed_valid);
                for &sys_pt in &sys_grid {
                    let direct = resolve(
                        &history,
                        &key,
                        AsOf {
                            valid: fixed_valid,
                            sys: sys_pt,
                        },
                    );
                    let via_intervals = ivs
                        .iter()
                        .find(|iv| iv.start <= sys_pt && sys_pt < iv.end)
                        .map(|iv| iv.tuple.clone());
                    assert_eq!(
                        direct, via_intervals,
                        "seed {seed}: sys axis, fixed_valid={fixed_valid} sys={sys_pt} history={history:?}"
                    );
                    cases += 1;
                }
            }
        }
        assert!(cases > 5000, "expected a rich grid campaign, ran {cases}");
    }

    #[test]
    fn diff_composition_law_holds_across_axes() {
        let mut cases = 0usize;
        for seed in 0..300u64 {
            let mut rng = Rng::new(0xD1FF_C0DE_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let key = k(1);
            let history = gen_history(&mut rng, &key);

            let sys_now = AsOf::current().sys;
            let a = AsOf {
                valid: 0,
                sys: sys_now,
            };
            let b = AsOf {
                valid: 3,
                sys: sys_now,
            };
            let c = AsOf {
                valid: 6,
                sys: sys_now,
            };
            let ab = diff(&history, a, b);
            let bc = diff(&history, b, c);
            let ac = diff(&history, a, c);
            assert_eq!(compose(&ab, &bc), ac, "seed {seed}: valid-axis composition");
            cases += 1;

            let fixed_valid = 3;
            let a = AsOf {
                valid: fixed_valid,
                sys: 0,
            };
            let b = AsOf {
                valid: fixed_valid,
                sys: 3,
            };
            let c = AsOf {
                valid: fixed_valid,
                sys: 6,
            };
            let ab = diff(&history, a, b);
            let bc = diff(&history, b, c);
            let ac = diff(&history, a, c);
            assert_eq!(compose(&ab, &bc), ac, "seed {seed}: sys-axis composition");
            cases += 1;
        }
        assert!(
            cases >= 500,
            "expected hundreds of composition cases, ran {cases}"
        );
    }
}

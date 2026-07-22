/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): aggregation kind is data only — names + meet-vs-normal —
 * with fold bodies and factories living in exec/fold. `choice_rand` is
 * refused at declaration admission (unseeded nondeterminism).
 */

//! Aggregation declaration vocabulary: names and meet-vs-normal kind as data.
//!
//! Fold implementations live in the engine. This module admits a name to an
//! [`Aggregation`] descriptor (or refuses it) and nothing more.

use std::fmt::{Debug, Formatter};

use miette::Diagnostic;
use serde::de::{Error as DeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Meet (semilattice, safe inside recursion) vs ordinary post-fixpoint fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggrKind {
    Meet,
    Normal,
}

/// A named aggregation: the user-facing name bound to its evaluation kind.
///
/// Identity is the name alone. Kind is what stratify rules on; fold bodies
/// are supplied by the engine against this declaration.
#[derive(Clone, Copy)]
pub struct Aggregation {
    pub name: &'static str,
    pub kind: AggrKind,
}

impl Aggregation {
    /// Whether this aggregation may run inside recursion.
    pub fn is_meet(&self) -> bool {
        matches!(self.kind, AggrKind::Meet)
    }
}

impl PartialEq for Aggregation {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Aggregation {}

impl Debug for Aggregation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Aggr<{}>", self.name)
    }
}

impl Serialize for Aggregation {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name)
    }
}

impl<'de> Deserialize<'de> for Aggregation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct AggrVisitor;
        impl<'de> Visitor<'de> for AggrVisitor {
            type Value = Aggregation;
            fn expecting(&self, f: &mut Formatter) -> std::fmt::Result {
                f.write_str("aggregation name")
            }
            fn visit_str<E: DeError>(self, v: &str) -> std::result::Result<Aggregation, E> {
                match parse_aggr(v) {
                    Ok(Some(a)) => Ok(a),
                    Ok(None) => Err(E::custom(format!("unknown aggregation: {v}"))),
                    Err(e) => Err(E::custom(e.to_string())),
                }
            }
        }
        deserializer.deserialize_str(AggrVisitor)
    }
}

/// Declaration-admission refusal for an aggregation name.
#[derive(Debug, Error, Diagnostic, Clone, Eq, PartialEq)]
pub enum AggrRefuse {
    /// `choice_rand` folds unseeded randomness — nondeterminism in the
    /// answer path with no determinism-as-data field. Refused until a
    /// seeded discipline lands.
    #[error("aggregation 'choice_rand' is refused: unseeded nondeterminism")]
    #[diagnostic(code(aggr::unseeded_choice_rand))]
    UnseededChoiceRand,
}

const fn meet(name: &'static str) -> Aggregation {
    Aggregation {
        name,
        kind: AggrKind::Meet,
    }
}

const fn normal(name: &'static str) -> Aggregation {
    Aggregation {
        name,
        kind: AggrKind::Normal,
    }
}

/// Admit an aggregation name to a declaration descriptor.
///
/// - `choice_rand` → [`Err`] [`AggrRefuse::UnseededChoiceRand`]
/// - known name → [`Ok`]`(`[`Some`]`)` with meet/normal kind
/// - unknown → [`Ok`]`(`[`None`]`)`
pub fn parse_aggr(name: &str) -> Result<Option<Aggregation>, AggrRefuse> {
    Ok(Some(match name {
        "and" => meet("and"),
        "or" => meet("or"),
        "unique" => normal("unique"),
        "group_count" => normal("group_count"),
        "union" => meet("union"),
        "intersection" => meet("intersection"),
        "count" => normal("count"),
        "count_unique" => normal("count_unique"),
        "variance" => normal("variance"),
        "std_dev" => normal("std_dev"),
        "sum" => normal("sum"),
        "product" => normal("product"),
        "min" => meet("min"),
        "max" => meet("max"),
        "mean" => normal("mean"),
        "choice" => meet("choice"),
        "collect" => normal("collect"),
        "shortest" => meet("shortest"),
        "min_cost" => meet("min_cost"),
        "bit_and" => meet("bit_and"),
        "bit_or" => meet("bit_or"),
        "bit_xor" => normal("bit_xor"),
        "latest_by" => normal("latest_by"),
        "smallest_by" => normal("smallest_by"),
        "choice_rand" => return Err(AggrRefuse::UnseededChoiceRand),
        // Sketch families (fold bodies in exec/fold/sketch); only hll_union is meet.
        "hll" => normal("hll"),
        "hll_sketch" => normal("hll_sketch"),
        "hll_union" => meet("hll_union"),
        "count_min" => normal("count_min"),
        "tdigest" => normal("tdigest"),
        "quantile" => normal("quantile"),
        _other => return Ok(None),
    }))
}

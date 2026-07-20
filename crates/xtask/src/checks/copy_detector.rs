/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Check 3: near-identical function/method/closure bodies across files, by
//! normalized token-stream similarity.
//!
//! Story #78 closed the original calibration fixture: the bitemporal
//! skip-scan walk that was triplicated across `store/fjall.rs`
//! (`SkipIterator::next`), `store/temp.rs` (the `iter::from_fn` closure
//! inside `range_skip_scan_tuple`), and `store/sim.rs` (`SimSkipIter::next`)
//! now exists once as `store/skip_walk.rs`. That deferral is resolved — not
//! an open cross-story dependency. Remaining `[[copy_detector]]` waivers in
//! `resonance-allow.toml` cite other independent twins (oracle/engine
//! differentials, etc.); every waiver member must still match a live unit
//! or the check fails as a stale allowlist entry (never silent green).
//!
//! Normalization: every identifier collapses to one placeholder token so
//! renamed locals/receivers don't defeat the comparison; every other token
//! (keyword, punctuation, literal, delimiter) keeps its own text, so
//! structurally different code still scores low. Similarity is the Jaccard
//! index over each body's set of contiguous `SHINGLE_LEN`-token windows
//! (see below) — cheap enough to compute over the shingle-set-size-bucketed
//! candidate pairs this check restricts itself to (a full O(n^2) cross
//! product over every body in the crate would be needlessly slow; two
//! bodies whose shingle-set sizes differ by more than the threshold ratio
//! can never reach that threshold's Jaccard score, so bucketing by size is
//! lossless for this check's purpose, not an approximation).

use quote::ToTokens;
use syn::visit::{self, Visit};

use crate::allowlist::Allowlist;
use crate::fsutil::{SourceFile, span_line};
use crate::synutil::mod_is_test_scope;

pub const MIN_TOKENS: usize = 100;
/// Contiguous-shingle width for the Jaccard similarity below. A plain LCS
/// ratio over the whole normalized token stream was tried first and
/// rejected: LCS allows an arbitrarily scattered common subsequence, and
/// Rust's own idiom density (`match`, `Ok(`, `?`, brace/paren nesting)
/// repeats often enough that unrelated functions of similar shape and
/// length shared 60-85% of their tokens as *some* common subsequence —
/// tens of thousands of spurious pairs on this tree. Requiring an exact
/// contiguous run of `SHINGLE_LEN` normalized tokens to match, then taking
/// the Jaccard index over the two *sets* of such shingles, is the standard
/// clone-detection metric for exactly this reason: coincidental idiom
/// repetition essentially never reproduces the same 8-token run in order,
/// while genuine copy-paste (renamed identifiers aside, since those
/// normalize to the same placeholder) does.
const SHINGLE_LEN: usize = 20;
/// `MIN_TOKENS = 100` restricts the comparison to bodies large enough that
/// "near-identical" is meaningful risk (a 15-line getter coincidentally
/// resembling another 15-line getter is not the hazard this check exists
/// for; a 50-60 line hand-copied algorithm is). At this size, historically
/// `fjall.rs`'s `SkipIterator::next` and `sim.rs`'s `SimSkipIter::next`
/// scored 0.81 against each other (story #78's calibration fixture, now
/// deleted into `skip_walk.rs`); unrelated functions in the tree score well
/// under 0.2 (see the story report for the full distribution this was
/// tuned against).
pub const THRESHOLD: f64 = 0.5;

pub struct Unit {
    pub file: String,
    pub label: String,
    pub line: usize,
    pub tokens: Vec<String>,
}

struct Collector<'a> {
    file: &'a str,
    units: Vec<Unit>,
    /// Enclosing fn/method name, for labeling closures found inside it.
    enclosing: Vec<String>,
}

fn normalize(ts: proc_macro2::TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    for tt in ts {
        match tt {
            proc_macro2::TokenTree::Ident(_) => out.push("ID".to_string()),
            proc_macro2::TokenTree::Literal(_) => out.push("LIT".to_string()),
            proc_macro2::TokenTree::Punct(p) => out.push(p.as_char().to_string()),
            proc_macro2::TokenTree::Group(g) => {
                out.push(format!("{:?}open", g.delimiter()));
                out.extend(normalize(g.stream()));
                out.push(format!("{:?}close", g.delimiter()));
            }
        }
    }
    out
}

impl<'a> Collector<'a> {
    fn push_unit(&mut self, label: String, line: usize, block: &syn::Block) {
        let tokens = normalize(block.to_token_stream());
        if tokens.len() >= MIN_TOKENS {
            self.units.push(Unit {
                file: self.file.to_string(),
                label,
                line,
                tokens,
            });
        }
    }
}

impl<'a, 'ast> Visit<'ast> for Collector<'a> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let name = node.sig.ident.to_string();
        self.push_unit(name.clone(), span_line(&node.sig.ident.span()), &node.block);
        self.enclosing.push(name);
        visit::visit_item_fn(self, node);
        self.enclosing.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let name = node.sig.ident.to_string();
        self.push_unit(name.clone(), span_line(&node.sig.ident.span()), &node.block);
        self.enclosing.push(name);
        visit::visit_impl_item_fn(self, node);
        self.enclosing.pop();
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        if let syn::Expr::Block(b) = node.body.as_ref() {
            let parent = self
                .enclosing
                .last()
                .cloned()
                .unwrap_or_else(|| "<top>".into());
            let label = format!(
                "{parent}::closure@{}",
                span_line(&b.block.brace_token.span.join())
            );
            self.push_unit(label, span_line(&b.block.brace_token.span.join()), &b.block);
        }
        visit::visit_expr_closure(self, node);
    }
}

pub fn collect_units(files: &[SourceFile]) -> Vec<Unit> {
    let mut units = Vec::new();
    for f in files {
        let mut c = Collector {
            file: &f.rel_path,
            units: Vec::new(),
            enclosing: Vec::new(),
        };
        c.visit_file(&f.ast);
        units.extend(c.units);
    }
    units
}

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn shingles(tokens: &[String]) -> HashSet<u64> {
    let mut set = HashSet::new();
    if tokens.len() < SHINGLE_LEN {
        // Too short to shingle at all; hash the whole thing as one shingle
        // so tiny-but-still-MIN_TOKENS bodies aren't silently excluded.
        let mut h = DefaultHasher::new();
        tokens.hash(&mut h);
        set.insert(h.finish());
        return set;
    }
    for w in tokens.windows(SHINGLE_LEN) {
        let mut h = DefaultHasher::new();
        w.hash(&mut h);
        set.insert(h.finish());
    }
    set
}

fn jaccard(a: &HashSet<u64>, b: &HashSet<u64>) -> f64 {
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

pub struct Pair {
    pub a: usize,
    pub b: usize,
    pub similarity: f64,
}

/// All unit-index pairs scoring >= threshold, length-bucketed so the
/// comparison stays near-linear instead of a full O(n^2) cross product.
pub fn find_similar_pairs(units: &[Unit], threshold: f64) -> Vec<Pair> {
    let shingle_sets: Vec<HashSet<u64>> = units.iter().map(|u| shingles(&u.tokens)).collect();
    let mut idx: Vec<usize> = (0..units.len()).collect();
    idx.sort_by_key(|&i| shingle_sets[i].len());

    // A margin below the threshold on the length-ratio early-break: the
    // Jaccard-index upper bound from set-size ratio is exact
    // (|intersection| <= min(|A|,|B|)), but sorting by raw set size before
    // this loop still leaves a small safety margin worth keeping simple
    // rather than proving airtight.
    let mut pairs = Vec::new();
    for (pos, &i) in idx.iter().enumerate() {
        let len_i = shingle_sets[i].len().max(1);
        for &j in &idx[pos + 1..] {
            let len_j = shingle_sets[j].len().max(1);
            // Sorted by shingle-set size: |intersection| <= min(|A|,|B|),
            // so Jaccard <= len_i/len_j once len_j >= len_i. Once that bound
            // drops below threshold, every later (larger) j is only lower.
            if (len_i as f64) / (len_j as f64) < threshold {
                break;
            }
            if units[i].file == units[j].file && units[i].label == units[j].label {
                continue;
            }
            // A closure never gets compared against its own directly
            // enclosing fn: the enclosing body contains the closure's
            // tokens verbatim, so that pair is trivially near-100%
            // similar by containment, not by copy-paste.
            if units[i].file == units[j].file
                && (units[i]
                    .label
                    .starts_with(&format!("{}::closure@", units[j].label))
                    || units[j]
                        .label
                        .starts_with(&format!("{}::closure@", units[i].label)))
            {
                continue;
            }
            let sim = jaccard(&shingle_sets[i], &shingle_sets[j]);
            if sim >= threshold {
                pairs.push(Pair {
                    a: i,
                    b: j,
                    similarity: sim,
                });
            }
        }
    }
    pairs
}

pub struct Violation {
    pub file_a: String,
    pub label_a: String,
    pub line_a: usize,
    pub file_b: String,
    pub label_b: String,
    pub line_b: usize,
    pub similarity: f64,
}

fn member_key(u: &Unit) -> String {
    format!("{}::{}", u.file, u.label)
}

fn allowlisted(allow: &Allowlist, key_a: &str, key_b: &str) -> bool {
    allow.copy_detector.iter().any(|e| {
        let members: Vec<&str> = e.members.iter().map(|s| s.as_str()).collect();
        members.iter().any(|m| key_a.ends_with(m)) && members.iter().any(|m| key_b.ends_with(m))
    })
}

pub fn check(files: &[SourceFile], allow: &Allowlist) -> (Vec<Violation>, Vec<Pair>, Vec<Unit>, Vec<String>) {
    let units = collect_units(files);
    let pairs = find_similar_pairs(&units, THRESHOLD);
    let mut violations = Vec::new();
    for p in &pairs {
        let key_a = member_key(&units[p.a]);
        let key_b = member_key(&units[p.b]);
        if allowlisted(allow, &key_a, &key_b) {
            continue;
        }
        violations.push(Violation {
            file_a: units[p.a].file.clone(),
            label_a: units[p.a].label.clone(),
            line_a: units[p.a].line,
            file_b: units[p.b].file.clone(),
            label_b: units[p.b].label.clone(),
            line_b: units[p.b].line,
            similarity: p.similarity,
        });
    }

    // Stale-waiver check: every allowlist member must still name a unit in
    // the tree. Story #78's SkipIterator/SimSkipIter members are gone —
    // leaving them in resonance-allow.toml must red, not pass as a no-op.
    let mut stale = Vec::new();
    for e in &allow.copy_detector {
        for member in &e.members {
            let still_present = units.iter().any(|u| member_key(u).ends_with(member.as_str()));
            if !still_present {
                stale.push(format!(
                    "copy_detector allowlist member `{member}` no longer matches any \
                     comparison unit — remove it (citation: {})",
                    e.citation
                ));
            }
        }
    }

    (violations, pairs, units, stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{Allowlist, CopyGroupEntry};

    #[test]
    fn deleted_story_78_fixture_members_are_stale_not_silent() {
        let allow = Allowlist {
            copy_detector: vec![CopyGroupEntry {
                members: vec![
                    "crates/kyzo-core/src/store/fjall.rs::next".into(),
                    "crates/kyzo-crashfs/src/sim.rs::next".into(),
                ],
                citation: "Story #78's known-real fixture — deleted into skip_walk.rs".into(),
            }],
            ..Allowlist::default()
        };
        let (_violations, _pairs, _units, stale) = check(&[], &allow);
        assert_eq!(
            stale.len(),
            2,
            "renamed/deleted #78 skip-scan copies must red as stale waivers"
        );
        assert!(stale.iter().all(|s| s.contains("no longer matches")));
    }
}


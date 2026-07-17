/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Regex`: textual identity under the **KyzoRegexV1** execution contract.
//!
//! Three separate laws, deliberately:
//!
//! - **Identity**: a regex value is exactly `(flags, pattern text)`.
//!   Canonical payload (format v1): `[flags byte][pattern UTF-8, escaped
//!   and terminated by the string grammar]`. Canonical bytes never
//!   contain compiled automata, normalized syntax trees, dependency
//!   versions, or dialect tags. `/foo/i` and `(?i)foo` are DIFFERENT
//!   values; the type claims byte identity of (flags, source), never
//!   pattern equivalence.
//! - **Validity**: [`RegexSource`] is the storage identity — flags +
//!   pattern text — and it is honest about what it proves. Its writer
//!   door, [`RegexSource::validated`], parses under KyzoRegexV1 *with its
//!   flags* (flags change the grammar: `ignore_whitespace` alone makes
//!   `#` start a comment), so nonsense cannot be *written*. Its decode
//!   door reconstructs stored identity WITHOUT re-parsing: stored bytes
//!   were checked when written, and a decode-side re-check against an
//!   evolving parser would turn parser drift into format drift. A
//!   `RegexSource` is therefore NOT an execution witness — the type that
//!   proves "this executes" is [`CompiledRegexV1`], minted only by
//!   [`RegexSource::compile`], and match operators refuse through it.
//! - **Execution**: match behavior is governed by [`KYZO_REGEX_DIALECT`]
//!   — the contract, which the pure-Rust `regex` crate *implements*.
//!   Changing the contract is a plane-version event (readers refuse at
//!   store open, per the reserved-activation rule in `tag.rs`), never a
//!   re-encoding. If multiple dialects ever coexist as first-class
//!   values, that is a new kind or format version, not a field slipped
//!   into today's payload.

/// The v1 flag bitset (closed; reserved bits refuse at decode). The
/// constants are typed `RegexFlags`, composed with [`RegexFlags::union`]
/// and probed with [`RegexFlags::contains`] — no loose `u8` bit passing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct RegexFlags(u8);

const _: () = assert!(std::mem::size_of::<RegexFlags>() == std::mem::size_of::<u8>());
const _: () = assert!(std::mem::align_of::<RegexFlags>() == std::mem::align_of::<u8>());

impl RegexFlags {
    pub const NONE: RegexFlags = RegexFlags(0);
    pub const CASE_INSENSITIVE: RegexFlags = RegexFlags(0x01);
    pub const MULTI_LINE: RegexFlags = RegexFlags(0x02);
    pub const DOT_MATCHES_NEWLINE: RegexFlags = RegexFlags(0x04);
    pub const SWAP_GREED: RegexFlags = RegexFlags(0x08);
    pub const IGNORE_WHITESPACE: RegexFlags = RegexFlags(0x10);
    pub const UNICODE_DISABLED: RegexFlags = RegexFlags(0x20);

    const MASK: u8 = 0x3F;

    /// Total: unknown bits are `None`, never a panic.
    pub fn from_bits(bits: u8) -> Option<RegexFlags> {
        if bits & !Self::MASK == 0 {
            Some(RegexFlags(bits))
        } else {
            None
        }
    }

    pub const fn union(self, other: RegexFlags) -> RegexFlags {
        RegexFlags(self.0 | other.0)
    }

    pub const fn contains(self, other: RegexFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn bits(self) -> u8 {
        self.0
    }
}

/// The regex **storage identity**: `(flags, pattern text)`. Two mints,
/// each saying exactly what it proves: [`RegexSource::validated`] (the
/// writer door — parser-checked under KyzoRegexV1) and the plane-internal
/// decode door (stored identity, checked when written, NOT re-proven).
/// This type never claims executability; that is [`CompiledRegexV1`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RegexSource {
    flags: RegexFlags,
    pattern: String,
}

/// Why a pattern failed under KyzoRegexV1 with its flags.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RegexParseRefusal {
    /// `regex_syntax` rejected the pattern under the active flags.
    Syntax { pattern: String },
    /// The regex engine refused to compile an already syntax-valid source.
    Compile { pattern: String },
}

impl std::fmt::Display for RegexParseRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegexParseRefusal::Syntax { pattern } => {
                write!(f, "invalid regex {pattern:?}")
            }
            RegexParseRefusal::Compile { pattern } => {
                write!(f, "regex compile failed for {pattern:?}")
            }
        }
    }
}

impl std::error::Error for RegexParseRefusal {}

/// The typed refusal for patterns that do not parse under KyzoRegexV1
/// with their flags. Private payload — inspect via [`InvalidRegex::reason`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InvalidRegex(RegexParseRefusal);

impl InvalidRegex {
    pub fn reason(&self) -> &RegexParseRefusal {
        &self.0
    }
}

impl std::fmt::Display for InvalidRegex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for InvalidRegex {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl RegexSource {
    /// The writer door: validate `(flags, pattern)` under KyzoRegexV1 —
    /// the flags participate, because they change the accepted grammar.
    pub fn validated(flags: RegexFlags, pattern: String) -> Result<RegexSource, InvalidRegex> {
        let mut parser = regex_syntax::ParserBuilder::new();
        parser
            .case_insensitive(flags.contains(RegexFlags::CASE_INSENSITIVE))
            .multi_line(flags.contains(RegexFlags::MULTI_LINE))
            .dot_matches_new_line(flags.contains(RegexFlags::DOT_MATCHES_NEWLINE))
            .swap_greed(flags.contains(RegexFlags::SWAP_GREED))
            .ignore_whitespace(flags.contains(RegexFlags::IGNORE_WHITESPACE))
            .unicode(!flags.contains(RegexFlags::UNICODE_DISABLED));
        match parser.build().parse(&pattern) {
            Ok(_) => Ok(RegexSource { flags, pattern }),
            Err(_) => Err(InvalidRegex(RegexParseRefusal::Syntax { pattern })),
        }
    }

    /// Plane-internal reconstruction from stored canonical bytes: stored
    /// identity, checked when written, deliberately NOT re-proven here —
    /// decode must stay total over stored history (see the module docs).
    pub(in super::super) fn from_stored(flags: RegexFlags, pattern: String) -> RegexSource {
        RegexSource { flags, pattern }
    }

    /// The execution mint: compile under KyzoRegexV1 with the full flag
    /// semantics. The ONLY path to a matchable regex — operators that
    /// receive hostile or stale sources refuse through this door's error.
    pub fn compile(&self) -> Result<CompiledRegexV1, InvalidRegex> {
        regex::RegexBuilder::new(&self.pattern)
            .case_insensitive(self.flags.contains(RegexFlags::CASE_INSENSITIVE))
            .multi_line(self.flags.contains(RegexFlags::MULTI_LINE))
            .dot_matches_new_line(self.flags.contains(RegexFlags::DOT_MATCHES_NEWLINE))
            .swap_greed(self.flags.contains(RegexFlags::SWAP_GREED))
            .ignore_whitespace(self.flags.contains(RegexFlags::IGNORE_WHITESPACE))
            .unicode(!self.flags.contains(RegexFlags::UNICODE_DISABLED))
            .build()
            .map(CompiledRegexV1)
            .map_err(|_| {
                InvalidRegex(RegexParseRefusal::Compile {
                    pattern: self.pattern.clone(),
                })
            })
    }

    pub fn flags(&self) -> RegexFlags {
        self.flags
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// The **execution witness**: a regex proven matchable under KyzoRegexV1,
/// mintable only by [`RegexSource::compile`]. Never stored, never part of
/// identity — compiled internals are engine-version territory and stay
/// out of canonical bytes by law.
pub struct CompiledRegexV1(regex::Regex);

impl CompiledRegexV1 {
    pub fn is_match(&self, haystack: &str) -> bool {
        self.0.is_match(haystack)
    }

    /// First-match replacement (`$name`/`$1` substitution per the
    /// dialect's underlying engine).
    pub fn replace<'h>(&self, haystack: &'h str, rep: &str) -> std::borrow::Cow<'h, str> {
        self.0.replace(haystack, rep)
    }

    /// All-matches replacement.
    pub fn replace_all<'h>(&self, haystack: &'h str, rep: &str) -> std::borrow::Cow<'h, str> {
        self.0.replace_all(haystack, rep)
    }

    /// The first match's text, if any.
    pub fn find<'h>(&self, haystack: &'h str) -> Option<&'h str> {
        self.0.find(haystack).map(|m| m.as_str())
    }

    /// Every non-overlapping match's text, in order.
    pub fn find_iter<'h>(&self, haystack: &'h str) -> impl Iterator<Item = &'h str> {
        self.0.find_iter(haystack).map(|m| m.as_str())
    }

    pub fn as_regex(&self) -> &regex::Regex {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_flag_bits_refuse() {
        assert!(RegexFlags::from_bits(0x3F).is_some());
        assert!(RegexFlags::from_bits(0x40).is_none());
        assert!(RegexFlags::from_bits(0x80).is_none());
        assert_eq!(RegexFlags::NONE.bits(), 0);
        let combo = RegexFlags::CASE_INSENSITIVE.union(RegexFlags::MULTI_LINE);
        assert!(combo.contains(RegexFlags::CASE_INSENSITIVE));
        assert!(!combo.contains(RegexFlags::UNICODE_DISABLED));
    }

    #[test]
    fn writer_door_refuses_nonsense_at_construction() {
        assert!(RegexSource::validated(RegexFlags::NONE, "a+".into()).is_ok());
        assert!(RegexSource::validated(RegexFlags::NONE, "(?i)foo".into()).is_ok());
        assert!(RegexSource::validated(RegexFlags::NONE, "(".into()).is_err());
        assert!(RegexSource::validated(RegexFlags::NONE, "a{2,1}".into()).is_err());
    }

    #[test]
    fn execution_witness_carries_flag_semantics_and_refuses_bad_sources() {
        let ci = RegexSource::validated(RegexFlags::CASE_INSENSITIVE, "foo".into()).unwrap();
        assert!(ci.compile().unwrap().is_match("FOO"));
        let cs = RegexSource::validated(RegexFlags::NONE, "foo".into()).unwrap();
        assert!(!cs.compile().unwrap().is_match("FOO"));
        // A stored-identity source that does not parse (hostile bytes, or
        // a pattern from an older accepted grammar) refuses at the
        // execution door instead of matching garbage.
        let hostile = RegexSource::from_stored(RegexFlags::NONE, "(".into());
        assert!(hostile.compile().is_err());
    }

    #[test]
    fn flags_participate_in_the_grammar() {
        // Under ignore_whitespace, `#` starts a comment, so the unclosed
        // `(` is commented out and the pattern parses; without the flag
        // it refuses. The pair is the value; the pair is what validates.
        let pattern = "a#(";
        assert!(RegexSource::validated(RegexFlags::IGNORE_WHITESPACE, pattern.into()).is_ok());
        assert!(RegexSource::validated(RegexFlags::NONE, pattern.into()).is_err());
    }

    #[test]
    fn flag_and_inline_pattern_are_distinct_identities() {
        use super::super::super::canonical::{Datum, encode};
        let flagged = RegexSource::validated(RegexFlags::CASE_INSENSITIVE, "foo".into()).unwrap();
        let inlined = RegexSource::validated(RegexFlags::NONE, "(?i)foo".into()).unwrap();
        assert_ne!(
            encode(Datum::Regex(&flagged)),
            encode(Datum::Regex(&inlined))
        );
        // Storage order: flags byte first, then pattern bytes.
        let plain = RegexSource::validated(RegexFlags::NONE, "zzz".into()).unwrap();
        assert!(encode(Datum::Regex(&plain)) < encode(Datum::Regex(&flagged)));
    }
}

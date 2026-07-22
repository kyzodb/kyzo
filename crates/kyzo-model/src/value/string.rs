/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `GermanStr`: the 16-byte inline-or-arena string (the `Str`/`Bytes` tag's realization of the cell mechanism).
//!
//! Not a new representation: a `GermanStr` IS a [`Value`] whose tag is
//! `Str` or `Bytes`, and the classic German-string behaviors fall out of
//! the cell's own laws — short strings fully inline in 16 bytes, long
//! strings as prefix + handle with prefix-first comparison, dereferencing
//! only on a prefix tie through an observer. The newtype adds exactly one
//! fact the cell alone cannot state: *this word is a string kind*, so the
//! typed accessors cannot be called on a number.
//!
//! The same authority discipline applies unchanged: no trait impl with
//! hidden context; out-of-line content is reachable only through the
//! observer/container stamp.

use std::borrow::Cow;

use super::admission::Denial;
use super::arena::Arena;
use super::canonical::{Datum, decode_terminated, encode};
use super::cell::Value;
use super::code::StampedCode;
use super::tag::Tag;

/// A [`Value`] proven to be of the `Str` or `Bytes` kind. Constructible
/// only through the minting constructors or the checked
/// [`GermanStr::from_value`]. The kind proof authorizes INLINE access
/// only: any future out-of-line accessor must take the observer/container
/// authority explicitly — the proof here is about the tag, never about
/// context.
#[derive(Clone, Copy, Debug)]
pub struct GermanStr(Value);

/// A minted string word with its context stamp structurally attached —
/// the string realization of [`super::cell::Minted`].
#[must_use = "an out-of-line string without its stamp is unspendable; carry the stamp"]
pub struct MintedStr {
    value: GermanStr,
    stamp: Option<StampedCode>,
}

impl MintedStr {
    fn new(m: super::cell::Minted) -> MintedStr {
        MintedStr {
            value: GermanStr(m.value()),
            stamp: m.stamp(),
        }
    }

    pub fn value(&self) -> GermanStr {
        self.value
    }

    pub fn stamp(&self) -> Option<StampedCode> {
        self.stamp
    }
}

impl GermanStr {
    /// Mint a string value (canonical-encodes, then follows the cell's
    /// residency law). The stamp rides inside [`MintedStr`] for the
    /// container to carry. Propagates [`Denial::ExtentOverflow`] from the
    /// wide mint path.
    pub fn from_str(s: &str, arena: &mut Arena) -> Result<MintedStr, Denial> {
        Ok(MintedStr::new(Value::mint(&encode(Datum::Str(s)), arena)?))
    }

    /// Mint a bytes value. Propagates [`Denial::ExtentOverflow`] from the
    /// wide mint path.
    pub fn decode(b: &[u8], arena: &mut Arena) -> Result<MintedStr, Denial> {
        Ok(MintedStr::new(Value::mint(
            &encode(Datum::Bytes(b)),
            arena,
        )?))
    }

    /// Claim an existing word as a string kind; `None` if it is not one.
    pub fn from_value(v: Value) -> Option<GermanStr> {
        matches!(v.tag(), Tag::Str | Tag::Bytes).then_some(GermanStr(v))
    }

    pub fn value(self) -> Value {
        self.0
    }

    pub fn is_bytes(self) -> bool {
        self.0.tag() == Tag::Bytes
    }

    /// The string content, if inline: borrowed when the payload carries
    /// no escapes, owned otherwise. `None` for `Bytes`-kind words and for
    /// out-of-line strings (whose content lives behind the observer).
    pub fn inline_str(&self) -> Option<Cow<'_, str>> {
        if self.0.tag() != Tag::Str {
            return None;
        }
        let payload = self.0.inline_payload()?;
        let content = &payload[..payload.len().checked_sub(2)?];
        if !content.contains(&0x00) {
            // No escapes: the content bytes are the string bytes.
            return match std::str::from_utf8(content) {
                Ok(s) => Some(Cow::Borrowed(s)),
                Err(_utf8) => None,
            };
        }
        let Ok((raw, _rest)) = decode_terminated(payload) else {
            return None;
        };
        match String::from_utf8(raw) {
            Ok(s) => Some(Cow::Owned(s)),
            Err(_utf8) => None,
        }
    }

    /// The bytes content, if inline: the `Bytes`-kind mirror of
    /// [`GermanStr::inline_str`], so callers never drop to raw [`Value`]
    /// payload poking. `None` for `Str`-kind words and out-of-line words.
    pub fn inline_bytes(&self) -> Option<Cow<'_, [u8]>> {
        if self.0.tag() != Tag::Bytes {
            return None;
        }
        let payload = self.0.inline_payload()?;
        let content = &payload[..payload.len().checked_sub(2)?];
        if !content.contains(&0x00) {
            return Some(Cow::Borrowed(content));
        }
        match decode_terminated(payload) {
            Ok((raw, _rest)) => Some(Cow::Owned(raw)),
            Err(_dec) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result, miette};

    use super::*;

    #[test]
    fn short_strings_are_fully_inline_words() -> Result<()> {
        let mut arena = Arena::new();
        let m = GermanStr::from_str("hello", &mut arena).into_diagnostic()?;
        assert!(m.stamp().is_none());
        let s = m.value();
        assert!(s.value().is_inline());
        assert_eq!(s.inline_str().ok_or_else(|| miette!("inline"))?, "hello");
        assert!(matches!(s.inline_str().ok_or_else(|| miette!("inline"))?, Cow::Borrowed(_)));
        Ok(())
    }

    #[test]
    fn nul_bearing_strings_unescape_through_the_grammar() -> Result<()> {
        let mut arena = Arena::new();
        let s = GermanStr::from_str("a\u{0}b", &mut arena)
            .into_diagnostic()?
            .value();
        let got = s.inline_str().ok_or_else(|| miette!("inline"))?;
        assert_eq!(got, "a\u{0}b");
        assert!(matches!(got, Cow::Owned(_)));
        Ok(())
    }

    #[test]
    fn long_strings_go_out_of_line_with_the_stamp_beside_them() -> Result<()> {
        let mut arena = Arena::new();
        let m = GermanStr::from_str("a string well past the inline max", &mut arena).into_diagnostic()?;
        let s = m.value();
        assert!(!s.value().is_inline());
        assert!(
            s.inline_str().is_none(),
            "out-of-line content needs an observer"
        );
        let sc = m.stamp().ok_or_else(|| miette!("stamp accompanies the word"))?;
        let f = arena.frame();
        let canonical = f.resolve(sc).into_diagnostic()?;
        assert_eq!(canonical[0], Tag::Str.byte());
        Ok(())
    }

    #[test]
    fn bytes_kind_is_typed_and_str_accessor_refuses_it() -> Result<()> {
        let mut arena = Arena::new();
        let b = GermanStr::decode(&[0xFF, 0x00], &mut arena)
            .into_diagnostic()?
            .value();
        assert!(b.is_bytes());
        assert!(b.inline_str().is_none());
        assert_eq!(
            b.inline_bytes().ok_or_else(|| miette!("inline bytes"))?,
            Cow::<[u8]>::Owned(vec![0xFF, 0x00])
        );
        // And a non-string word cannot be claimed.
        let n = Value::mint(&encode(Datum::Null), &mut arena)
            .into_diagnostic()?
            .value();
        assert!(GermanStr::from_value(n).is_none());
        Ok(())
    }
}

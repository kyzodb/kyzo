/*
 * Copyright 2025 the Kyzo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The staged-construction idiom: zero-sized typestate markers for tracking
//! which required fields of a multi-field construct have been supplied.
//!
//! A staged builder is generic over one marker per required field. Every
//! required field starts [`Unset`]; its setter consumes the builder and
//! returns one with that field's marker flipped to [`Set`], leaving the
//! others untouched (so required fields may be supplied in any order). The
//! `build()` method — a trait implemented ONLY for the fully-[`Set`]
//! instantiation — therefore exists only once every required field is
//! present. A construct missing a required field is a COMPILE error, never
//! an `Option`/sentinel checked at run time. The markers are erased by
//! monomorphization, so the guarantee costs nothing when the program runs.
//!
//! The product a builder yields is SEALED: it carries a private field only
//! the builder's own module can name, so the staged builder is its sole
//! constructor crate-wide — no struct literal elsewhere can assemble an
//! unproven, half-filled instance.
//!
//! The compile-fail property is itself witnessed in every build:
//! [`crate::data::value::proofs::assert_not_impl`] proves that an incomplete
//! instantiation does not implement the build trait, placed beside each
//! builder it guards.

/// A required builder field that has NOT yet been supplied.
#[derive(Debug)]
pub(crate) struct Unset;

/// A required builder field that HAS been supplied.
#[derive(Debug)]
pub(crate) struct Set;

/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared `cargo metadata` JSON shape for the pure-Rust and build-script-
//! sandbox gates. One deserialize type — never a per-check twin that can
//! drift field-by-field.

use serde::Deserialize;

/// Shape of `cargo metadata --format-version=1`, trimmed to the fields the
/// two gates need. `--no-deps` callers simply never see dependency packages;
/// full-graph callers get them. Extra JSON fields cargo may emit are ignored.
#[derive(Debug, Deserialize)]
pub struct CargoMetadata {
    pub packages: Vec<MetaPackage>,
    pub workspace_members: Vec<String>,
    pub workspace_root: String,
    /// Present on a full-graph `cargo metadata` response; empty string when
    /// a `--no-deps` parse omits it (serde default).
    #[serde(default)]
    pub target_directory: String,
}

#[derive(Debug, Deserialize)]
pub struct MetaPackage {
    pub id: String,
    pub name: String,
    pub manifest_path: String,
    /// Empty when a `--no-deps` payload omits version (serde default); the
    /// build-script sandbox always sees a real version on a full-graph parse.
    #[serde(default)]
    pub version: String,
    /// `null` unless the manifest sets `publish`; `Some([])` is exactly
    /// `publish = false`, `Some([..])` is a registry allowlist.
    #[serde(default)]
    pub publish: Option<Vec<String>>,
    #[serde(default)]
    pub targets: Vec<MetaTarget>,
}

#[derive(Debug, Deserialize)]
pub struct MetaTarget {
    pub kind: Vec<String>,
}

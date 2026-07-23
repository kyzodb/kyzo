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
use serde::de::Error as _;
use serde_json::{Map, Value};

/// Shape of `cargo metadata --format-version=1`, trimmed to the fields the
/// two gates need. `--no-deps` callers simply never see dependency packages;
/// full-graph callers get them. Extra JSON fields cargo may emit are ignored.
#[derive(Debug)]
pub struct CargoMetadata {
    pub packages: Vec<MetaPackage>,
    pub workspace_members: Vec<String>,
    pub workspace_root: String,
    /// Present on a full-graph `cargo metadata` response; empty string when
    /// a `--no-deps` parse omits the key (explicit absence at this door).
    pub target_directory: String,
}

#[derive(Debug)]
pub struct MetaPackage {
    pub id: String,
    pub name: String,
    pub manifest_path: String,
    /// Empty when a `--no-deps` payload omits version; the build-script
    /// sandbox always sees a real version on a full-graph parse.
    pub version: String,
    /// `null` unless the manifest sets `publish`; `Some([])` is exactly
    /// `publish = false`, `Some([..])` is a registry allowlist. Missing key
    /// is `None` — same meaning as cargo omitting the field.
    pub publish: Option<Vec<String>>,
    /// Empty when the payload omits targets.
    pub targets: Vec<MetaTarget>,
}

#[derive(Debug, Deserialize)]
pub struct MetaTarget {
    pub kind: Vec<String>,
}

fn take_string(map: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    match map.remove(key) {
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(format!("{key} must be a string, got {other}")),
        None => Err(format!("missing field `{key}`")),
    }
}

fn take_string_or_empty(map: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(format!("{key} must be a string, got {other}")),
    }
}

impl<'de> Deserialize<'de> for CargoMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut map = Map::<String, Value>::deserialize(deserializer)?;
        let packages = match map.remove("packages") {
            Some(v) => Vec::<MetaPackage>::deserialize(v).map_err(D::Error::custom)?,
            None => return Err(D::Error::missing_field("packages")),
        };
        let workspace_members = match map.remove("workspace_members") {
            Some(v) => Vec::<String>::deserialize(v).map_err(D::Error::custom)?,
            None => return Err(D::Error::missing_field("workspace_members")),
        };
        let workspace_root =
            take_string(&mut map, "workspace_root").map_err(D::Error::custom)?;
        let target_directory =
            take_string_or_empty(&mut map, "target_directory").map_err(D::Error::custom)?;
        Ok(Self {
            packages,
            workspace_members,
            workspace_root,
            target_directory,
        })
    }
}

impl<'de> Deserialize<'de> for MetaPackage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut map = Map::<String, Value>::deserialize(deserializer)?;
        let id = take_string(&mut map, "id").map_err(D::Error::custom)?;
        let name = take_string(&mut map, "name").map_err(D::Error::custom)?;
        let manifest_path = take_string(&mut map, "manifest_path").map_err(D::Error::custom)?;
        let version = take_string_or_empty(&mut map, "version").map_err(D::Error::custom)?;
        let publish = match map.remove("publish") {
            None | Some(Value::Null) => None,
            Some(v) => Some(Vec::<String>::deserialize(v).map_err(D::Error::custom)?),
        };
        let targets = match map.remove("targets") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => Vec::<MetaTarget>::deserialize(v).map_err(D::Error::custom)?,
        };
        Ok(Self {
            id,
            name,
            manifest_path,
            version,
            publish,
            targets,
        })
    }
}

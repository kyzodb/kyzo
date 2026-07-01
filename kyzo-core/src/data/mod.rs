/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

pub(crate) mod memcmp;
// The `expect` lint below fires the moment it stops being true: several items
// in this module have their callers in the query-engine layers, which grow
// around this kernel. When the engine lands and uses them, the expectation is
// unfulfilled and rustc warns, forcing the removal of the attribute.
pub(crate) mod tuple;
#[expect(dead_code)]
pub(crate) mod value;

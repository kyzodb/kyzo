/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Storage-engine selection for the CLI/server composition root.
//!
//! The CozoDB original dispatched over five backends (`mem`, `sqlite`,
//! `rocksdb`, `sled`, `tikv`) behind a `DbInstance` enum. kyzo-core has
//! exactly one production `Storage` implementation — `FjallStorage`
//! (`data/memcmp.rs`'s engine choice; see `deny.toml`'s `[bans]`, which makes
//! reopening it a visible edit, not a silent dependency swap) — so there is
//! no enum to dispatch over. What upstream's `mem` engine bought a caller was
//! "no persistence, no path to manage"; that is reproduced here as an
//! ephemeral `fjall` store in a process-owned temp directory rather than as a
//! second storage backend, because a second backend is not what the feature
//! ever meant.
//!
//! Engine::compose genesis-mints its own live admission seats (StoreId /
//! WriteAuthority / RootChain). This host does not keep a parallel unused
//! WriteAuthority / GenesisSealedView — that was dead theater. When a
//! config-once genesis injection door lands on Engine, it wires here.

use std::path::Path;

use kyzo::{Catalog, Engine, FjallStorage, StorageOptions, new_fjall_storage_with};
use miette::{IntoDiagnostic, Result, miette};

/// Resource knobs exposed on the CLI, passed straight through to
/// [`StorageOptions`]. Kept as a plain struct (rather than threading two
/// `Option` parameters) so `main.rs`'s `clap::Args` flattens into it and
/// engine-opening call sites stay one argument.
#[derive(Clone, Copy, Debug, Default, clap::Args)]
pub struct StorageArgs {
    /// Block/blob cache size in bytes. Unset uses fjall's own default.
    #[clap(long)]
    pub cache_size_bytes: Option<u64>,
    /// Background worker threads (flush/compaction). Unset uses fjall's own
    /// default.
    #[clap(long)]
    pub worker_threads: Option<usize>,
}

impl From<StorageArgs> for StorageOptions {
    fn from(args: StorageArgs) -> Self {
        StorageOptions {
            cache_size_bytes: args.cache_size_bytes,
            worker_threads: args.worker_threads,
            // Not CLI-exposed (see `StorageArgs`): `None` keeps the tuned
            // policy's own choice, exactly as before these fields existed.
            max_memtable_size_bytes: None,
            table_target_size_bytes: None,
        }
    }
}

/// A running database plus everything needed to keep it alive and to reach
/// its storage directly for whole-store dump/restore/verify — operations
/// that take `&FjallStorage`, not `&Engine<_>` (`Engine`'s `store` field is
/// crate-private by design: nothing outside kyzo-core reaches into a live
/// session's transactions). `FjallStorage` is a cheap `Clone` (a handle to
/// one shared store), so kyzo-bin keeps its own clone from before handing
/// one to `Engine::compose` instead of asking kyzo-core to expose an accessor.
pub struct DbHandle {
    pub db: Engine<FjallStorage>,
    pub storage: FjallStorage,
    /// Holds the ephemeral directory open for the process lifetime in `mem`
    /// mode; `None` for a `fjall` engine opened at a caller-given path. Never
    /// read, only held — dropping it deletes the directory.
    _ephemeral_dir: Option<tempfile::TempDir>,
}

/// Open (or create) the database this process will serve, per the CLI's
/// `--engine`/`--path` choice.
///
/// `engine` is `"mem"` or `"fjall"`; any other value is a refusal, not a
/// silent fallback — the CozoDB original's `DbInstance::new` had the same
/// posture toward an unrecognized engine name.
pub fn open(engine: &str, path: impl AsRef<Path>, opts: StorageArgs) -> Result<DbHandle> {
    let storage_opts: StorageOptions = opts.into();
    let (storage, ephemeral_dir) = match engine {
        "mem" => {
            let dir = tempfile::tempdir().into_diagnostic()?;
            let storage = new_fjall_storage_with(dir.path(), storage_opts)?;
            (storage, Some(dir))
        }
        "fjall" => {
            let storage = new_fjall_storage_with(path, storage_opts)?;
            (storage, None)
        }
        other => {
            return Err(miette!(
                "unknown engine '{other}': KyzoDB has exactly one storage backend (fjall); \
                 use `--engine fjall` for a persistent store at `--path`, or `--engine mem` \
                 for an ephemeral store that is discarded when the process exits"
            ));
        }
    };

    let db = Engine::compose(storage.clone(), Catalog::new())?;
    Ok(DbHandle {
        db,
        storage,
        _ephemeral_dir: ephemeral_dir,
    })
}

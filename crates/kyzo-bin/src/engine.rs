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
///
/// No `Default` derive/`impl`: both are banned shapes. Unset knobs are the
/// explicit [`StorageArgs::unset`] construction — clap still builds this
/// via `Args` with both fields `None` when flags are omitted.
#[derive(Clone, Copy, Debug, clap::Args)]
pub struct StorageArgs {
    /// Block/blob cache size in bytes. Unset uses fjall's own default.
    #[clap(long)]
    pub cache_size_bytes: Option<u64>,
    /// Background worker threads (flush/compaction). Unset uses fjall's own
    /// default.
    #[clap(long)]
    pub worker_threads: Option<usize>,
}

impl StorageArgs {
    /// Both knobs unset — fjall's own tuned policy applies.
    pub const fn unset() -> Self {
        Self {
            cache_size_bytes: None,
            worker_threads: None,
        }
    }
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
            max_journaling_size_bytes: None,
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
///
/// Persistent (`fjall`) opens go through the restore-completeness gate: a
/// store still carrying the in-progress mark refuses IncompleteRestore
/// rather than presenting a partial restore as a smaller complete store
/// (seat 26 / #375 T2).
pub fn open(engine: &str, path: impl AsRef<Path>, opts: StorageArgs) -> Result<DbHandle> {
    let storage_opts: StorageOptions = opts.into();
    let (storage, ephemeral_dir) = match engine {
        "mem" => {
            let dir = tempfile::tempdir().into_diagnostic()?;
            // Ephemeral path: still admit (empty store is complete) so mem and
            // fjall share one completeness law.
            let storage = new_fjall_storage_with(dir.path(), storage_opts)?;
            kyzo::admit_complete_store!(storage)?;
            (storage, Some(dir))
        }
        "fjall" => {
            let storage = kyzo::open_complete_store_with!(path, storage_opts)?;
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

#[cfg(test)]
mod restore_completeness {
    use super::{StorageArgs, open};
    use kyzo::{Storage, WriteTx, dump_storage, new_fjall_storage, restore_storage};
    use miette::{Result, miette};

    /// NASTY (#375 T2): interrupt a restore mid-pair put, then reopen via the
    /// production host door [`super::open`] — not `open_complete_store` alone —
    /// and assert typed IncompleteRestore (bare open never saw the mark).
    #[test]
    fn interrupted_restore_production_open_refuses_incomplete() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let src = new_fjall_storage(dir.path().join("src"))?;
        {
            let mut tx = src.write_tx()?;
            // Enough pairs that a mid-file truncate lands after the dump header
            // and through at least one restore chunk of applied pairs.
            for i in 0..256u64 {
                let mut key = 1u64.to_be_bytes().to_vec();
                key.extend_from_slice(&i.to_be_bytes());
                tx.put(&key, &[0xAB])?;
            }
            tx.commit()?;
        }
        let dump = dir.path().join("d.kyzo");
        dump_storage(&src, &dump)?;

        // Truncate mid-file so restore marks, applies a prefix, then fails on
        // a torn length-prefixed pair — the durable in-progress shape.
        let full_len = std::fs::metadata(&dump)
            .map_err(|e| miette!("dump metadata: {e}"))?
            .len();
        assert!(
            full_len > 64,
            "control: dump must be large enough to truncate"
        );
        let keep = full_len / 2;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&dump)
            .map_err(|e| miette!("open dump for truncate: {e}"))?;
        file.set_len(keep)
            .map_err(|e| miette!("truncate dump: {e}"))?;

        let tgt_path = dir.path().join("tgt");
        let tgt = new_fjall_storage(&tgt_path)?;
        let restore_err = match restore_storage(&tgt, &dump) {
            Err(e) => e,
            Ok(()) => {
                return Err(miette!(
                    "control: restore must fail from the truncated dump"
                ));
            }
        };
        assert!(
            !restore_err.to_string().is_empty(),
            "control: restore must fail from the truncated dump"
        );
        drop(tgt);

        // PRODUCTION entry point — not open_complete_store / admit alone.
        let open_err = match open("fjall", &tgt_path, StorageArgs::unset()) {
            Err(e) => e,
            Ok(_) => {
                return Err(miette!(
                    "kyzo_bin::engine::open must refuse a partial restore"
                ));
            }
        };
        assert!(
            kyzo::is_incomplete_restore!(open_err),
            "production open must typed-refuse IncompleteRestore, got: {open_err}"
        );
        Ok(())
    }
}

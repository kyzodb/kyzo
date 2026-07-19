/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Storage-engine selection and config-once genesis injection for the CLI/server.
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
//! ## Genesis / arm / keystore (07 seat wiring)
//!
//! Genesis-sealed parameters enter through `store/open.rs` construction and
//! are injected **config-once** here — the composition root. This host never
//! imports engine internals past the sealed public door and never keeps
//! shadow state the engine cannot account for. `StableCommitCap` arm
//! selection is injected (not defined here); the closed sum lands in
//! `store/commit_cap.rs`.

use std::path::Path;

use kyzo::{
    Db, EntropyArm, FjallStorage, GenesisParams, GenesisSealedView, SizeClass, StagingTtl,
    StableCommitCapArm, StorageOptions, WriteAuthority, genesis, new_fjall_storage_with,
};
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

/// Config-once genesis / `StableCommitCap` arm / keystore injection.
///
/// Read once at composition-root open; never re-read. Fields are declared
/// scalars / sealed enums from the store zone — not host shadow state.
#[derive(Clone, Debug)]
pub struct GenesisConfig {
    /// Opaque identity seed (not a filesystem path). Default: digest of the
    /// deployment label `"kyzo-bin.default"`.
    pub identity_seed: [u8; 32],
    /// Ordinal StagingTTL sealed at genesis.
    pub staging_ttl: StagingTtl,
    /// Human-operable size class.
    pub size_class: SizeClass,
    /// Approved entropy arm for incarnation mint.
    pub entropy_arm: EntropyArm,
    /// `StableCommitCap` arm selection — injected into genesis, not defined here.
    pub stable_commit_cap: StableCommitCapArm,
}

impl Default for GenesisConfig {
    fn default() -> Self {
        Self {
            // Fixed deployment seed — not a path. Host may override via
            // `open_with_genesis`; identity is never derived from `--path`.
            identity_seed: *b"kyzo-bin.default.identity.v1!!!!",
            staging_ttl: StagingTtl::new(1_024),
            size_class: SizeClass::Standard,
            entropy_arm: EntropyArm::OsRandom,
            // Native fsync proof: SnapshotFork=no until the live-fork SIV arm
            // campaign greens (signatures unfrozen — seat map carried obligation).
            stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
                snapshot_fork: false,
            },
        }
    }
}

/// A running database plus everything needed to keep it alive and to reach
/// its storage directly for whole-store dump/restore/verify — operations
/// that take `&FjallStorage`, not `&Db<_>` (`Db`'s `storage` field is
/// crate-private by design: nothing outside kyzo-core reaches into a live
/// session's transactions). `FjallStorage` is a cheap `Clone` (a handle to
/// one shared store), so kyzo-bin keeps its own clone from before handing
/// one to `Db::new` instead of asking kyzo-core to expose an accessor.
///
/// Genesis facts and the affine WriteAuthority (keystore) are held here as
/// config-once injection results — engine-accountable, not host shadow state.
pub struct DbHandle {
    pub db: Db<FjallStorage>,
    pub storage: FjallStorage,
    /// Genesis-sealed identity / open capability / CryptoDomain (WriteAuthority moved out).
    pub genesis: GenesisSealedView,
    /// Affine WriteAuthority in the host keystore (HA is token move).
    pub write_authority: WriteAuthority,
    /// Holds the ephemeral directory open for the process lifetime in `mem`
    /// mode; `None` for a `fjall` engine opened at a caller-given path. Never
    /// read, only held — dropping it deletes the directory.
    _ephemeral_dir: Option<tempfile::TempDir>,
}

/// Open (or create) the database this process will serve, per the CLI's
/// `--engine`/`--path` choice, with default genesis / `StableCommitCap` arm
/// injection.
///
/// `engine` is `"mem"` or `"fjall"`; any other value is a refusal, not a
/// silent fallback — the CozoDB original's `DbInstance::new` had the same
/// posture toward an unrecognized engine name.
pub fn open(engine: &str, path: impl AsRef<Path>, opts: StorageArgs) -> Result<DbHandle> {
    open_with_genesis(engine, path, opts, GenesisConfig::default())
}

/// Open with explicit config-once genesis / arm / keystore injection.
///
/// Path is adapter location only — Store identity is the genesis digest,
/// never the path (decisions.md §4/§5).
pub fn open_with_genesis(
    engine: &str,
    path: impl AsRef<Path>,
    opts: StorageArgs,
    genesis_config: GenesisConfig,
) -> Result<DbHandle> {
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

    // Config-once: genesis / StableCommitCap arm / keystore via store/open.
    let sealed = genesis(GenesisParams {
        identity_seed: genesis_config.identity_seed,
        recovery_matrix: None,
        staging_ttl: genesis_config.staging_ttl,
        size_class: genesis_config.size_class,
        entropy_arm: genesis_config.entropy_arm,
        stable_commit_cap: genesis_config.stable_commit_cap,
    });
    let (genesis_view, write_authority) = sealed.take_write_authority();

    let db = Db::new(storage.clone())?;
    Ok(DbHandle {
        db,
        storage,
        genesis: genesis_view,
        write_authority,
        _ephemeral_dir: ephemeral_dir,
    })
}

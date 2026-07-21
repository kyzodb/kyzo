# Re-homed from kyzo-core (crate wall)

These sources were cut out of `kyzo-core` `#[cfg(test)]` modules because they
imported `kyzo_oracle` (forbidden by the storage-era crate wall). Cap1
`gauntlet` and `verify_differential` already own the live differential meter.

Wired through `kyzo::oracle_harness` + `kyzo_oracle` and declared in
`kyzo-trials/src/lib.rs` via `#[path]` (holding-area paths kept so
`agreements.toml` reachability keys stay stable). Delete this directory only
after those `#[path]` decls move to first-class `src/` modules.

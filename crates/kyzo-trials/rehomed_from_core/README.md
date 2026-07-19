# Re-homed from kyzo-core (crate wall)

These sources were cut out of `kyzo-core` `#[cfg(test)]` modules because they
imported `kyzo_oracle` (forbidden by the storage-era crate wall). Cap1
`gauntlet` and `verify_differential` already own the live differential meter.

Re-wire through `kyzo::oracle_harness` + `kyzo_oracle` and declare in
`kyzo-trials/src/lib.rs` before deleting this holding area.

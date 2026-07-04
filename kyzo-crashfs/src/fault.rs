//! The fault plan: every decision is a pure function of
//! `(campaign seed, path, op kind, byte range, attempt count)`.
//!
//! This mirrors the identity-keyed discipline in `kyzo-core`'s
//! `storage/sim.rs` (read for the doctrine; that module is not depended on
//! here — this crate stands alone) lifted to the FUSE op level: an
//! operation's identity hashes *what it is* (op kind, relative path, byte
//! range), never when it runs or which thread carries it, so a seed
//! reproduces the exact fault sequence at any concurrency, and a retried
//! operation (same identity, next attempt) draws a fresh decision instead
//! of replaying the same one forever.
//!
//! Two mechanisms, matching the LazyFS vocabulary pinned in story #31:
//!
//! - **Trigger points** (exact): `(path glob, op kind, op-count)` — a test
//!   author names precisely which occurrence of which op on which path
//!   flips a fault, so a scenario is authored directly instead of hunted
//!   for by rate. This is what the standalone proof test drives.
//! - **Ambient rates** (probabilistic, ppm, `sim.rs`-style): for broad
//!   seed-swept campaigns where no exact trigger fires, an identity-keyed
//!   coin flip still may. Zero rates (the default) mean pure passthrough
//!   semantics with no ambient noise — only exact triggers act.
//!
//! A trigger's outcome for `TornSeq`/`TornOp` is decided once, at the write
//! that reaches the triggering count, and carried on that write's pending
//! record until it materializes at the next `fsync` (or is wiped whole by a
//! `ClearCache`, at a write or an `fsync`, before it ever gets there).

use std::collections::HashMap;

/// The kind of operation a trigger or identity is keyed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// A `write()` call landing bytes at an offset.
    Write,
    /// An `fsync()` call materializing a file's pending writes.
    Fsync,
}

/// The LazyFS-model fault vocabulary pinned by the story #31 design ruling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// This write, of a sequence of writes pending since the last fsync, is
    /// silently dropped when the sequence materializes — some persist,
    /// this one does not.
    TornSeq,
    /// This write is split at a seed-derived byte offset when it
    /// materializes: the prefix persists, the suffix is silently dropped.
    TornOp,
    /// Every byte buffered (written but not yet fsynced) for this file is
    /// dropped right now — the power-cut model: whatever the last real
    /// `fsync` landed survives, nothing since does.
    ClearCache,
}

/// One exact rule: the `at_count`-th occurrence of `op` on a path matching
/// `path_glob` fires `fault`. Op counts are per-path, 1-indexed, and
/// restart with the campaign (a fresh [`FaultPlan`] means fresh counters).
#[derive(Debug, Clone)]
pub struct Trigger {
    /// `*` matches zero or more characters; no other wildcard syntax.
    pub path_glob: String,
    pub op: OpKind,
    pub at_count: u64,
    pub fault: Fault,
}

impl Trigger {
    pub fn new(path_glob: impl Into<String>, op: OpKind, at_count: u64, fault: Fault) -> Self {
        Trigger {
            path_glob: path_glob.into(),
            op,
            at_count,
            fault,
        }
    }
}

/// Ambient, rate-based fault injection in parts-per-million — the
/// `sim.rs`-style knob for broad seed-swept campaigns. `0` (the default)
/// means no ambient faults: only exact [`Trigger`]s act.
#[derive(Debug, Clone, Copy, Default)]
pub struct AmbientRates {
    pub torn_seq_ppm: u32,
    pub torn_op_ppm: u32,
}

/// The full plan: a campaign seed plus the trigger list plus ambient rates.
/// Everything downstream of these three fields is a pure function of them.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    pub seed: u64,
    pub triggers: Vec<Trigger>,
    pub ambient: AmbientRates,
}

impl FaultPlan {
    pub fn new(seed: u64) -> Self {
        FaultPlan {
            seed,
            triggers: Vec::new(),
            ambient: AmbientRates::default(),
        }
    }

    pub fn with_trigger(mut self, trigger: Trigger) -> Self {
        self.triggers.push(trigger);
        self
    }

    pub fn with_ambient(mut self, ambient: AmbientRates) -> Self {
        self.ambient = ambient;
        self
    }
}

/// Per-(path, op-kind) occurrence counters. Not part of the pure-function
/// core: this is the caller-side state that turns "the Nth occurrence" into
/// a concrete number to feed the pure decision functions below. Restart it
/// (a fresh `Counters::default()`) on every simulated crash reopen, exactly
/// as `sim.rs`'s `attempts` map restarts on `sim_crash`/`sim_powercut`.
#[derive(Debug, Default)]
pub struct Counters {
    counts: HashMap<(String, OpKind), u64>,
}

impl Counters {
    /// Bump and return the new (1-indexed) count for `(path, op)`.
    pub fn bump(&mut self, path: &str, op: OpKind) -> u64 {
        let entry = self.counts.entry((path.to_string(), op)).or_insert(0);
        *entry += 1;
        *entry
    }
}

/// `*`-only glob match (no `?`, no character classes — the vocabulary the
/// story calls for and no more). Pure, total, no allocation beyond the
/// caller's strings.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    // Standard two-pointer glob match with backtracking on the last `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);
    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '*') {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if pi < pat.len() && pat[pi] == txt[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// FNV-1a 64 over the op-kind tag and the operation's semantic content,
/// length-delimited so distinct part lists never collide by concatenation.
/// Mirrors `sim.rs::op_identity` exactly (same construction, independently
/// implemented so this crate carries no dependency on `kyzo-core`).
fn op_identity(tag: u64, parts: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    fn eat(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h = (*h ^ u64::from(b)).wrapping_mul(PRIME);
        }
    }
    let mut h = OFFSET;
    eat(&mut h, &tag.to_be_bytes());
    for part in parts {
        eat(&mut h, &(part.len() as u64).to_be_bytes());
        eat(&mut h, part);
    }
    h
}

const TAG_WRITE: u64 = 0xFA01;

/// The identity of a write: a pure function of the op kind, its path, and
/// its byte range. Never of when it runs, what ran before it, or which
/// thread/attempt carries it — attempt is a separate axis, folded in only
/// at the finalizer step, exactly as `sim.rs` keeps identity and attempt
/// orthogonal.
pub fn write_identity(rel_path: &str, offset: u64, len: u64) -> u64 {
    op_identity(
        TAG_WRITE,
        &[
            rel_path.as_bytes(),
            &offset.to_be_bytes(),
            &len.to_be_bytes(),
        ],
    )
}

/// Splitmix-style finalizer over `(seed, identity, attempt, salt)` —
/// deterministic, stateless, replayable, and identical at any thread count.
/// Mirrors `sim.rs::fault_hit`'s construction.
fn finalize(seed: u64, identity: u64, attempt: u64, salt: u64) -> u64 {
    let mut z = seed
        ^ identity.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ attempt.wrapping_mul(0xA24B_AED4_963E_E407)
        ^ salt.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z
}

const SALT_AMBIENT_TORN_SEQ: u64 = 0xFA5E_0001;
const SALT_AMBIENT_TORN_OP: u64 = 0xFA5E_0002;
const SALT_SPLIT_POINT: u64 = 0xFA5E_0003;

fn ppm_hit(seed: u64, identity: u64, attempt: u64, salt: u64, ppm: u32) -> bool {
    if ppm == 0 {
        return false;
    }
    finalize(seed, identity, attempt, salt) % 1_000_000 < u64::from(ppm)
}

/// Resolve the exact trigger (if any) matching `(path, op, count)`. Pure:
/// first match in declaration order wins, deterministically — no seed
/// input needed, since an exact trigger's existence is itself the
/// authored decision.
pub fn resolve_trigger(plan: &FaultPlan, path: &str, op: OpKind, count: u64) -> Option<Fault> {
    plan.triggers
        .iter()
        .find(|t| t.op == op && t.at_count == count && glob_match(&t.path_glob, path))
        .map(|t| t.fault)
}

/// The outcome decided for one write at creation time: a pure function of
/// `(plan, path, offset, len, attempt)`, combining (in order) any exact
/// trigger at this attempt count, then the ambient ppm rates, defaulting to
/// a clean write. This is the write's fate the instant it is buffered;
/// materialization at the next `fsync` only carries it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Clean,
    /// Dropped entirely when materialized.
    Dropped,
    /// Only the first `split_at` bytes materialize; `1 <= split_at < len`.
    Split {
        split_at: u64,
    },
}

pub fn decide_write_outcome(
    plan: &FaultPlan,
    rel_path: &str,
    offset: u64,
    len: u64,
    attempt: u64,
) -> WriteOutcome {
    let identity = write_identity(rel_path, offset, len);
    let exact = resolve_trigger(plan, rel_path, OpKind::Write, attempt);
    let fault = exact.or_else(|| {
        if len == 0 {
            return None;
        }
        if ppm_hit(
            plan.seed,
            identity,
            attempt,
            SALT_AMBIENT_TORN_SEQ,
            plan.ambient.torn_seq_ppm,
        ) {
            Some(Fault::TornSeq)
        } else if ppm_hit(
            plan.seed,
            identity,
            attempt,
            SALT_AMBIENT_TORN_OP,
            plan.ambient.torn_op_ppm,
        ) {
            Some(Fault::TornOp)
        } else {
            None
        }
    });
    match fault {
        None | Some(Fault::ClearCache) => WriteOutcome::Clean,
        Some(Fault::TornSeq) => WriteOutcome::Dropped,
        Some(Fault::TornOp) => {
            if len < 2 {
                // Nothing to split; a one-byte (or empty) write torn-op
                // degrades to a clean write rather than a phantom split.
                WriteOutcome::Clean
            } else {
                let split_at =
                    1 + (finalize(plan.seed, identity, attempt, SALT_SPLIT_POINT) % (len - 1));
                WriteOutcome::Split { split_at }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_and_exact() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("data.bin", "data.bin"));
        assert!(!glob_match("data.bin", "other.bin"));
        assert!(glob_match("segments/*.sst", "segments/000123.sst"));
        assert!(!glob_match("segments/*.sst", "segments/000123.wal"));
        assert!(glob_match("*.sst", "a/b/c.sst"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn trigger_resolution_is_exact_and_first_match_wins() {
        let plan = FaultPlan::new(1)
            .with_trigger(Trigger::new(
                "data.bin",
                OpKind::Write,
                2,
                Fault::ClearCache,
            ))
            .with_trigger(Trigger::new("*", OpKind::Fsync, 1, Fault::ClearCache));
        assert_eq!(
            resolve_trigger(&plan, "data.bin", OpKind::Write, 2),
            Some(Fault::ClearCache)
        );
        assert_eq!(resolve_trigger(&plan, "data.bin", OpKind::Write, 1), None);
        assert_eq!(resolve_trigger(&plan, "other.bin", OpKind::Write, 2), None);
        assert_eq!(
            resolve_trigger(&plan, "whatever", OpKind::Fsync, 1),
            Some(Fault::ClearCache)
        );
    }

    #[test]
    fn write_identity_is_pure_and_sensitive_to_every_component() {
        let a = write_identity("x", 0, 4);
        let b = write_identity("x", 0, 5);
        let c = write_identity("x", 1, 4);
        let d = write_identity("y", 0, 4);
        let ids = [a, b, c, d];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "identity collided for inputs {i} and {j}");
            }
        }
        assert_eq!(a, write_identity("x", 0, 4), "identity must be pure");
    }

    #[test]
    fn same_seed_replays_the_identical_decision() {
        let plan = FaultPlan::new(42).with_ambient(AmbientRates {
            torn_seq_ppm: 200_000,
            torn_op_ppm: 200_000,
        });
        for attempt in 1..2000u64 {
            let first = decide_write_outcome(&plan, "wal/000.log", attempt, 37, attempt);
            let second = decide_write_outcome(&plan, "wal/000.log", attempt, 37, attempt);
            assert_eq!(first, second, "seed {} must replay exactly", plan.seed);
        }
    }

    #[test]
    fn different_seeds_explore_different_schedules() {
        let path = "wal/000.log";
        let mut saw_dropped = false;
        let mut saw_split = false;
        let mut saw_clean = false;
        for seed in 0..500u64 {
            let plan = FaultPlan::new(seed).with_ambient(AmbientRates {
                torn_seq_ppm: 250_000,
                torn_op_ppm: 250_000,
            });
            match decide_write_outcome(&plan, path, 0, 64, 1) {
                WriteOutcome::Clean => saw_clean = true,
                WriteOutcome::Dropped => saw_dropped = true,
                WriteOutcome::Split { split_at } => {
                    assert!((1..64).contains(&split_at));
                    saw_split = true;
                }
            }
        }
        assert!(
            saw_clean && saw_dropped && saw_split,
            "500 seeds at 25%/25% ppm should exercise all three outcomes"
        );
    }

    #[test]
    fn zero_ambient_rate_is_pure_passthrough() {
        let plan = FaultPlan::new(999);
        for attempt in 0..100u64 {
            assert_eq!(
                decide_write_outcome(&plan, "any", attempt, 128, attempt),
                WriteOutcome::Clean
            );
        }
    }

    #[test]
    fn exact_trigger_overrides_ambient_and_is_reported_as_split_or_dropped() {
        let plan = FaultPlan::new(7)
            .with_trigger(Trigger::new("*", OpKind::Write, 3, Fault::TornOp))
            .with_trigger(Trigger::new("*", OpKind::Write, 5, Fault::TornSeq));
        assert!(matches!(
            decide_write_outcome(&plan, "f", 0, 16, 3),
            WriteOutcome::Split { .. }
        ));
        assert_eq!(
            decide_write_outcome(&plan, "f", 0, 16, 5),
            WriteOutcome::Dropped
        );
        assert_eq!(
            decide_write_outcome(&plan, "f", 0, 16, 4),
            WriteOutcome::Clean
        );
    }

    #[test]
    fn torn_op_split_point_is_always_a_true_interior_cut() {
        for seed in 0..200u64 {
            let plan = FaultPlan::new(seed).with_trigger(Trigger::new(
                "*",
                OpKind::Write,
                1,
                Fault::TornOp,
            ));
            if let WriteOutcome::Split { split_at } = decide_write_outcome(&plan, "f", 0, 10, 1) {
                assert!((1..10).contains(&split_at));
            } else {
                panic!("expected a split outcome");
            }
        }
    }

    #[test]
    fn tiny_writes_cannot_torn_op_a_phantom_split() {
        let plan =
            FaultPlan::new(1).with_trigger(Trigger::new("*", OpKind::Write, 1, Fault::TornOp));
        assert_eq!(
            decide_write_outcome(&plan, "f", 0, 0, 1),
            WriteOutcome::Clean
        );
        assert_eq!(
            decide_write_outcome(&plan, "f", 0, 1, 1),
            WriteOutcome::Clean
        );
    }

    #[test]
    fn counters_bump_per_path_and_op_independently() {
        let mut c = Counters::default();
        assert_eq!(c.bump("a", OpKind::Write), 1);
        assert_eq!(c.bump("a", OpKind::Write), 2);
        assert_eq!(c.bump("a", OpKind::Fsync), 1);
        assert_eq!(c.bump("b", OpKind::Write), 1);
        assert_eq!(c.bump("a", OpKind::Write), 3);
    }
}

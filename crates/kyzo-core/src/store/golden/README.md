# Golden vectors — CanonicalTranscript (§59 / §81)

Authority for sealed-artifact byte identity. Goldens are **independently
derived** from the CanonicalTranscript wire format specification
(MAGIC `KTX1` / FormatVersion length-prefix / field id + tag + payload /
big-endian u64 / digest32 / length-prefixed bytes). They are **not**
captured by calling production `encode_*` and pasting the output.

Production `encode_*` must match the independent derivation for the same
normative parts. Phase-0 forbids encoder-self-capture goldens.

## FormatVersion decision

| Version | Decision |
| --- | --- |
| 6 | Current sealed-transcript stamp (`FormatVersion::CURRENT`). All vectors in this directory are FormatVersion 6. Changing any vector bytes requires a FormatVersion decision recorded here — never a silent test-fix commit. |

## Header law

Every `.vec` file begins with a header:

```
# FormatVersion: <n>
# Kind: <SealedArtifactKind>
# Decision: <why this vector exists or changed>
```

## Files

| File | Kind | Independent of |
| --- | --- | --- |
| `checkpoint_seal.vec` | CheckpointSeal | wire schema for `encode_checkpoint_seal` parts |
| `admission_certificate.vec` | AdmissionCertificate | wire schema for `encode_admission_certificate` parts |
| `fork_grant.vec` | ForkGrant | wire schema for `encode_fork_grant_payload` parts |
| `recovery_grant.vec` | RecoveryGrant | wire schema for `encode_recovery_grant_payload` parts |
| `merge_proof_header.vec` | MergeProofHeader | wire schema for `encode_merge_proof_header` parts |
| `audit_key_leaf.vec` | AuditKeyLeaf | wire schema for `encode_audit_key_leaf` parts |
| `wal_header.vec` | WalHeader | wire schema for `encode_wal_record` (commit) parts |
| `state_root_head.vec` | StateRootHead | wire schema for `encode_state_root_head` parts |
| `leave_is_free_pack.vec` | LeaveIsFreePack | wire schema for `encode_leave_is_free_pack` parts |
| `chained_state_root.vec` | ChainedStateRoot | wire schema for `encode_chained_state_root` parts |
| `ancestor_read_grant.vec` | AncestorReadGrant | wire schema for `encode_ancestor_read_grant_payload` parts |

`KeyCommit` golden lives as `KEY_COMMIT_GOLDEN_VEC` beside the production
constructor in `transcript.rs` (same independent-derivation law).

Normative fixture constants: `store = [0x11]*32`, `dig = [0x22]*32`,
`FormatVersion::CURRENT = 6` (ASCII `"6"`).

Payload lines after the header are lowercase hex of the independently
derived sealed transcript for that kind's normative fixture.

# Golden vectors — CanonicalTranscript (§59 / §81)

Authority for sealed-artifact byte identity. Goldens are **pinned production
encoder bytes** for the normative fixtures (`encode_*` / production
`CanonicalTranscript` is the authority) — not independent hand-derivation
theater.

Each `.vec` holds the sealed bytes of that kind's normative production fixture
(`store = [0x11]*32`, `dig = [0x22]*32`, `FormatVersion::CURRENT = 6`).

## Header law

Every `.vec` file begins with a header. Changing vector bytes requires a
`FormatVersion` decision recorded here — never a silent test-fix commit.

```
# FormatVersion: <n>
# Kind: <SealedArtifactKind>
# Decision: <why this vector exists or changed>
```

## Files

| File | Kind | Production encoder |
| --- | --- | --- |
| `checkpoint_seal.vec` | CheckpointSeal | `encode_checkpoint_seal` |
| `admission_certificate.vec` | AdmissionCertificate | `encode_admission_certificate` |
| `fork_grant.vec` | ForkGrant | `encode_fork_grant_payload` |
| `recovery_grant.vec` | RecoveryGrant | `encode_recovery_grant_payload` |
| `merge_proof_header.vec` | MergeProofHeader | `encode_merge_proof_header` |
| `audit_key_leaf.vec` | AuditKeyLeaf | `encode_audit_key_leaf` |
| `wal_header.vec` | WalHeader | `encode_wal_record` |
| `state_root_head.vec` | StateRootHead | `encode_state_root_head` |
| `leave_is_free_pack.vec` | LeaveIsFreePack | `encode_leave_is_free_pack` |
| `chained_state_root.vec` | ChainedStateRoot | `encode_chained_state_root` |
| `ancestor_read_grant.vec` | AncestorReadGrant | `encode_ancestor_read_grant_payload` |

`KeyCommit` golden lives as `KEY_COMMIT_GOLDEN_VEC` beside `encode_key_commitment`
in `transcript.rs` (same pin law).

Payload lines after the header are lowercase hex of the sealed
`CanonicalTranscript` for that kind's normative fixture.

# Golden vectors — CanonicalTranscript (§59 / §81)

Authority for sealed-artifact byte identity. The mutation campaign asserts
**implementation-against-vectors**, never vectors-against-implementation.

## Header law

Every `.vec` file begins with a header. Changing vector bytes requires a
`FormatVersion` decision recorded here — never a silent test-fix commit.

```
# FormatVersion: <n>
# Kind: <SealedArtifactKind>
# Decision: <why this vector exists or changed>
```

## Files

| File | Kind |
| --- | --- |
| `checkpoint_seal.vec` | CheckpointSeal |
| `admission_certificate.vec` | AdmissionCertificate |
| `fork_grant.vec` | ForkGrant |
| `recovery_grant.vec` | RecoveryGrant |
| `merge_proof_header.vec` | MergeProofHeader |
| `audit_key_leaf.vec` | AuditKeyLeaf |
| `wal_header.vec` | WalHeader |

Payload lines after the header are lowercase hex of the sealed
`CanonicalTranscript` for the normative fixture of that kind
(`encode_golden_fixture` in `store/transcript.rs`).

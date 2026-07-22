/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Trials-only certificate mutation injector (decisions.md seats 8 + 59).
//!
//! Production `::verify` / [`Engine`] expose no forge or sabotage hook —
//! seat 8 makes certificate construction at that door Unconstructible.
//! Seat 59's law is the mutation campaign: seal a golden artifact, perturb
//! it, and assert the checker surfaces mismatch — not a production API.
//!
//! This module is re-exported only through [`crate::oracle_harness`]. Cap2
//! (`kyzo-trials::provenance`) still covers independent checker rejection;
//! the store-side reduced-match sabotage in `verify_differential` stays.
//!
//! Soft-green refuse: the injector never returns `"mismatch"` without a
//! real [`verify_proof`] rejection of a mutated tree that differs from the
//! sealed golden. An identity "fault" or a checker that accepts corruption
//! panics here — the trials corpus cannot green on a silent wrong accept.

use std::collections::BTreeSet;
use std::num::NonZeroU64;

use miette::{Result, miette};

use crate::data::json::NamedRows;
use crate::exec::provenance::semiring::{
    BadCertificate, Derivation, DerivationGraph, DerivationId, ProofNode, verify_proof,
};
use crate::parse::{Script, parse_script};
use crate::session::verify::{MismatchProgram, VerifyOutcome};
use kyzo_model::value::{DataValue, Tuple, ValidityTs};

/// Structural fault the trials injector applies to a sealed golden proof.
///
/// Each variant produces a certificate the production [`verify_proof`]
/// checker rejects — the same checker `SysOp::Verify` uses before rendering
/// NamedRows status `"mismatch"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateFault {
    /// Claimed step cost disagrees with weight + verified children.
    CorruptClaimedCost,
    /// Derivation id cites no edge in the sealed graph.
    OutOfRangeDerivation,
    /// Premise child names a different node than the cited edge.
    CorruptPremiseNode,
}

impl CertificateFault {
    /// The [`BadCertificate`] variant this fault must provoke — typed so a
    /// wrong reject reason (or an accept) cannot soft-green.
    fn expected_reject(self) -> BadCertificate {
        match self {
            CertificateFault::CorruptClaimedCost => BadCertificate::CostMismatch,
            CertificateFault::OutOfRangeDerivation => BadCertificate::DerivationOutOfRange,
            CertificateFault::CorruptPremiseNode => BadCertificate::PremiseMismatch,
        }
    }
}

/// Seal a golden two-step path certificate, inject `fault`, run the
/// production [`verify_proof`] checker, and return the NamedRows
/// `::verify` emits for a certificate mismatch.
///
/// Mechanical failure modes (must go red, not soft-green):
/// - golden fails to verify → panic before mutation;
/// - mutation is a no-op (tree unchanged) → panic;
/// - checker accepts the corrupted tree → `expect_err` panic;
/// - checker rejects with the wrong [`BadCertificate`] → assert panic;
/// - `into_named_rows` does not emit status `"mismatch"` → caller asserts.
pub fn mismatch_named_rows_under_fault(fault: CertificateFault) -> Result<NamedRows> {
    let (graph, honest, answer) = seal_golden_path_certificate()?;
    verify_proof(&honest, &graph).map_err(|e| miette!("sealed golden certificate must verify: {e}"))?;

    let sabotaged = inject_fault(&honest, fault)?;
    assert_ne!(
        sabotaged, honest,
        "injector must diverge the sealed golden under {fault:?} — identity \
         mutation cannot manufacture a mismatch status"
    );

    let bad = match verify_proof(&sabotaged, &graph) {
        Err(e) => e,
        Ok(cost) => {
            return Err(miette!(
                "verify_proof silently accepted a corrupted certificate under \
                 {fault:?} (claimed cost {cost}) — mismatch path never fired"
            ));
        }
    };
    assert_eq!(
        bad,
        fault.expected_reject(),
        "fault {fault:?} must reject as {:?}, got {bad:?}",
        fault.expected_reject()
    );

    let program = fixture_mismatch_program()?;
    let rows: BTreeSet<Tuple> = BTreeSet::from([answer]);
    Ok(VerifyOutcome::Mismatch {
        program,
        evaluated: rows.clone(),
        provenance: rows,
        certificate: Some(format!(
            "verify_proof rejected injected certificate ({fault:?}): {bad}"
        )),
    }
    .into_named_rows())
}

/// Control: the sealed golden alone must verify. A corpus that only ever
/// saw mismatch rows without this control can soft-green on a broken seal.
pub fn golden_certificate_verifies() -> Result<()> {
    let (graph, honest, _) = seal_golden_path_certificate()?;
    verify_proof(&honest, &graph).map_err(|e| miette!("sealed golden must verify under verify_proof: {e}"))?;
    Ok(())
}

/// Miniature sealed graph: `path[1,3] ← path[1,2] ← edge[1,2]` plus
/// `edge[2,3]`, unit weights, tropical costs `{1, 2}`.
fn seal_golden_path_certificate() -> Result<(
    DerivationGraph<&'static str>,
    ProofNode<&'static str>,
    Tuple,
)> {
    let mut graph = DerivationGraph::empty();
    graph.add_fact("edge:1-2");
    graph.add_fact("edge:2-3");
    let d_base = graph
        .add_derivation(Derivation {
            head: "path:1-2",
            label: DerivationId::from_rule_index(0),
            weight: NonZeroU64::new(1).ok_or_else(|| miette!("1 is nonzero"))?,
            premises: vec!["edge:1-2"],
        })
        .map_err(|e| miette!("base derivation admits: {e}"))?;
    let d_rec = graph
        .add_derivation(Derivation {
            head: "path:1-3",
            label: DerivationId::from_rule_index(1),
            weight: NonZeroU64::new(1).ok_or_else(|| miette!("1 is nonzero"))?,
            premises: vec!["path:1-2", "edge:2-3"],
        })
        .map_err(|e| miette!("recursive derivation admits: {e}"))?;

    let honest = ProofNode::Step {
        node: "path:1-3",
        derivation: d_rec,
        label: DerivationId::from_rule_index(1),
        cost: 2,
        premises: vec![
            ProofNode::Step {
                node: "path:1-2",
                derivation: d_base,
                label: DerivationId::from_rule_index(0),
                cost: 1,
                premises: vec![ProofNode::Fact { node: "edge:1-2" }],
            },
            ProofNode::Fact { node: "edge:2-3" },
        ],
    };
    let answer = Tuple::from_vec(vec![DataValue::from(1i64), DataValue::from(3i64)]);
    Ok((graph, honest, answer))
}

fn inject_fault(
    proof: &ProofNode<&'static str>,
    fault: CertificateFault,
) -> Result<ProofNode<&'static str>> {
    match (fault, proof.clone()) {
        (
            CertificateFault::CorruptClaimedCost,
            ProofNode::Step {
                node,
                derivation,
                label,
                premises,
                ..
            },
        ) => Ok(ProofNode::Step {
            node,
            derivation,
            label,
            cost: 99,
            premises,
        }),
        (
            CertificateFault::OutOfRangeDerivation,
            ProofNode::Step {
                node,
                label,
                cost,
                premises,
                ..
            },
        ) => Ok(ProofNode::Step {
            node,
            derivation: DerivationId::unchecked(usize::MAX),
            label,
            cost,
            premises,
        }),
        (
            CertificateFault::CorruptPremiseNode,
            ProofNode::Step {
                node,
                derivation,
                label,
                cost,
                mut premises,
            },
        ) => {
            let Some(child) = premises.last_mut() else {
                return Err(miette!("golden recursive step must carry premises to corrupt"));
            };
            *child = ProofNode::Fact { node: "edge:9-9" };
            Ok(ProofNode::Step {
                node,
                derivation,
                label,
                cost,
                premises,
            })
        }
        (_, ProofNode::Fact { .. }) => Err(miette!(
            "golden certificate root must be a Step, not a Fact"
        )),
    }
}

fn fixture_mismatch_program() -> Result<MismatchProgram> {
    let script = parse_script(
        "path[x, y] := *edge[x, y]\n?[x, y] := path[x, y]",
        &Default::default(),
        ValidityTs::from_raw(0),
    )
    .map_err(|e| miette!("fixture program parses: {e}"))?;
    Ok(match script {
        Script::Query(prog) => MismatchProgram(prog),
        Script::Sys(_) | Script::Imperative(_) => {
            return Err(miette!("fixture must be a query script"))
        }
    })
}

#[cfg(test)]
mod tests {
    use miette::Result;

    use super::*;

    #[test]
    fn golden_control_verifies_and_every_fault_is_typed_mismatch() -> Result<()> {
        golden_certificate_verifies()?;
        for fault in [
            CertificateFault::CorruptClaimedCost,
            CertificateFault::OutOfRangeDerivation,
            CertificateFault::CorruptPremiseNode,
        ] {
            let rows = mismatch_named_rows_under_fault(fault)?;
            assert_eq!(rows.headers(), &["status", "summary", "detail"]);
            assert_eq!(
                rows.rows()[0][0],
                DataValue::from("mismatch"),
                "fault {fault:?}"
            );
            let detail: &str = match &rows.rows()[0][2] {
                DataValue::Str(s) => s.as_ref(),
                other @ DataValue::Null
                | other @ DataValue::Bool(_)
                | other @ DataValue::Num(_)
                | other @ DataValue::Bytes(_)
                | other @ DataValue::Uuid(_)
                | other @ DataValue::Regex(_)
                | other @ DataValue::Json(_)
                | other @ DataValue::Vector(_)
                | other @ DataValue::List(_)
                | other @ DataValue::Set(_)
                | other @ DataValue::Validity(_)
                | other @ DataValue::Interval(_)
                | other @ DataValue::Geometry(_) => return Err(miette!("detail must be Str, got {other:?}")),
            };
            assert!(
                detail.contains("verify_proof")
                    && detail.contains("injected certificate")
                    && detail.contains(&format!("{fault:?}")),
                "fault {fault:?}: detail must name the typed reject, got {detail}"
            );
            // Summary must reflect a certificate-bearing mismatch bundle,
            // not a reduced-match row count costume.
            let summary: &str = match &rows.rows()[0][1] {
                DataValue::Str(s) => s.as_ref(),
                other @ DataValue::Null
                | other @ DataValue::Bool(_)
                | other @ DataValue::Num(_)
                | other @ DataValue::Bytes(_)
                | other @ DataValue::Uuid(_)
                | other @ DataValue::Regex(_)
                | other @ DataValue::Json(_)
                | other @ DataValue::Vector(_)
                | other @ DataValue::List(_)
                | other @ DataValue::Set(_)
                | other @ DataValue::Validity(_)
                | other @ DataValue::Interval(_)
                | other @ DataValue::Geometry(_) => return Err(miette!("summary must be Str, got {other:?}")),
            };
            assert!(
                summary.contains("evaluated") && summary.contains("provenance"),
                "mismatch summary must name both answer sets, got {summary}"
            );
        }
        Ok(())
    }
}

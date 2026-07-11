use std::num::NonZeroU64;
use std::path::PathBuf;

use chrono::Utc;
use gh_envoy::conflict::{
    DiffOverlap, OverlapConfidence, OverlapRelationship, OverlapSeverity, ScopeWarning,
    ScopeWarningReason,
};
use gh_envoy::doctor::{CheckStatus, DoctorNodeReport, GateRollup, coordination_checks};
use gh_envoy::model::{Claim, SCHEMA_VERSION, WaitForRef};
use gh_envoy::observation::{ClaimObservation, DiffSummary};
use uuid::Uuid;

#[test]
fn risk_overlap_blocks_merge_without_blocking_publish() {
    let mut observed = observation(1);
    observed.overlaps.push(DiffOverlap {
        with_issue: issue(2),
        with_claim_id: Uuid::new_v4(),
        relationship: OverlapRelationship::Sibling,
        shared_paths: vec!["Cargo.lock".to_owned()],
        confidence: OverlapConfidence::Full,
        severity: OverlapSeverity::Blocking,
        labels: vec!["lockfile".to_owned()],
    });

    let (checks, recommendations) = coordination_checks(&observed);
    let node = DoctorNodeReport::new(
        observed.claim.issue,
        observed.claim.claim_id,
        checks,
        recommendations,
    );

    assert_eq!(node.gates.publish, GateRollup::Ok);
    assert_eq!(node.gates.merge, GateRollup::Blocked);
    assert!(
        node.checks
            .iter()
            .any(|check| { check.id == "merge.overlap" && check.status == CheckStatus::Fail })
    );
}

#[test]
fn scope_warns_and_consolidation_diff_is_neutral() {
    let mut observed = observation(3);
    observed.claim.wait_for.push(WaitForRef {
        issue: issue(4),
        claim_id: None,
    });
    observed.scope_warnings.push(ScopeWarning {
        path: "outside/file.rs".to_owned(),
        reason: ScopeWarningReason::OutsideAllowedScope,
    });

    let (checks, _) = coordination_checks(&observed);

    assert!(
        checks
            .iter()
            .any(|check| { check.id == "merge.scope" && check.status == CheckStatus::Warn })
    );
    assert!(checks.iter().any(|check| {
        check.id == "merge.consolidation_diff" && check.status == CheckStatus::Pass
    }));
}

fn observation(number: u64) -> ClaimObservation {
    ClaimObservation {
        claim: Claim {
            schema_version: SCHEMA_VERSION.to_owned(),
            claim_id: Uuid::new_v4(),
            repo: "local/fixture".to_owned(),
            issue: issue(number),
            title: None,
            branch: format!("branch-{number}"),
            worktree: PathBuf::from(format!("/tmp/worktree-{number}")),
            base_remote: "origin".to_owned(),
            base_ref: "main".to_owned(),
            base_sha: "base".to_owned(),
            base_issue: None,
            base_claim_id: None,
            wait_for: Vec::new(),
            declared_scope: None,
            note: None,
            created_at: Utc::now(),
        },
        diff: Some(DiffSummary {
            changed_paths: vec!["src/lib.rs".to_owned()],
            ..DiffSummary::default()
        }),
        overlaps: Vec::new(),
        scope_warnings: Vec::new(),
    }
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive issue")
}

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::PathBuf;

use chrono::Utc;
use gh_envoy::config::Config;
use gh_envoy::conflict::{
    OverlapConfidence, OverlapRelationship, OverlapSeverity, ScopeWarningReason, analyze_claims,
};
use gh_envoy::model::{Claim, DeclaredScope, SCHEMA_VERSION, WaitForRef};
use gh_envoy::observation::{ClaimObservation, DiffSummary};
use uuid::Uuid;

#[test]
fn every_relationship_confidence_and_risk_class_uses_the_normative_severity() {
    for relationship in [
        OverlapRelationship::Ancestor,
        OverlapRelationship::Descendant,
        OverlapRelationship::Sibling,
        OverlapRelationship::Consolidation,
        OverlapRelationship::Unrelated,
    ] {
        for confidence in [OverlapConfidence::Full, OverlapConfidence::Untracked] {
            for risk in [false, true] {
                let path = if risk {
                    "risk/shared.rs"
                } else {
                    "src/shared.rs"
                };
                let mut claims = related_pair(relationship, confidence, path);
                let risk_paths = if risk {
                    BTreeMap::from([("risk/**".to_owned(), "critical".to_owned())])
                } else {
                    BTreeMap::new()
                };

                analyze_claims(&mut claims, &risk_paths).expect("analyze claims");

                let overlap = claims[0]
                    .overlaps
                    .iter()
                    .find(|overlap| overlap.with_claim_id == claims[1].claim.claim_id)
                    .expect("overlap with second claim");
                assert_eq!(overlap.relationship, relationship);
                assert_eq!(overlap.confidence, confidence);
                assert_eq!(overlap.shared_paths, [path]);
                assert_eq!(overlap.labels, if risk { vec!["critical"] } else { vec![] });
                assert_eq!(
                    overlap.severity,
                    expected_severity(relationship, risk),
                    "relationship={relationship:?} confidence={confidence:?} risk={risk}"
                );
            }
        }
    }
}

#[test]
fn mixed_evidence_is_grouped_without_flattening_confidence_or_risk_labels() {
    let mut left = observed(
        claim(1),
        tracked(&["ordinary.rs", "risk/a.rs", "risk/b.rs"]),
    );
    left.diff.as_mut().unwrap().untracked_paths = vec!["new.rs".to_owned()];
    let right = observed(
        claim(2),
        tracked(&["ordinary.rs", "risk/a.rs", "risk/b.rs", "new.rs"]),
    );
    let risk_paths = BTreeMap::from([
        ("risk/**".to_owned(), "risk".to_owned()),
        ("**/b.rs".to_owned(), "special".to_owned()),
    ]);

    let mut claims = vec![left, right];
    analyze_claims(&mut claims, &risk_paths).expect("analyze claims");

    let rows = &claims[0].overlaps;
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().any(|row| {
        row.confidence == OverlapConfidence::Full
            && row.severity == OverlapSeverity::Warning
            && row.shared_paths == ["ordinary.rs"]
            && row.labels.is_empty()
    }));
    assert!(rows.iter().any(|row| {
        row.confidence == OverlapConfidence::Untracked
            && row.shared_paths == ["new.rs"]
            && row.labels.is_empty()
    }));
    assert!(
        rows.iter()
            .any(|row| { row.shared_paths == ["risk/a.rs"] && row.labels == ["risk"] })
    );
    assert!(
        rows.iter()
            .any(|row| { row.shared_paths == ["risk/b.rs"] && row.labels == ["risk", "special"] })
    );
}

#[test]
fn transitive_ancestry_is_directional_and_cycle_safe() {
    let root = claim(1);
    let mut middle = claim(2);
    middle.base_issue = Some(root.issue);
    middle.base_claim_id = Some(root.claim_id);
    let mut leaf = claim(3);
    leaf.base_issue = Some(middle.issue);
    leaf.base_claim_id = Some(middle.claim_id);
    let mut claims = vec![
        observed(leaf, tracked(&["shared.rs"])),
        observed(middle, tracked(&["shared.rs"])),
        observed(root, tracked(&["shared.rs"])),
    ];

    analyze_claims(&mut claims, &BTreeMap::new()).expect("analyze ancestry");

    let leaf_to_root = claims[0]
        .overlaps
        .iter()
        .find(|overlap| overlap.with_claim_id == claims[2].claim.claim_id)
        .expect("leaf to root overlap");
    let root_to_leaf = claims[2]
        .overlaps
        .iter()
        .find(|overlap| overlap.with_claim_id == claims[0].claim.claim_id)
        .expect("root to leaf overlap");
    assert_eq!(leaf_to_root.relationship, OverlapRelationship::Ancestor);
    assert_eq!(root_to_leaf.relationship, OverlapRelationship::Descendant);

    let first_id = claims[0].claim.claim_id;
    let second_id = claims[1].claim.claim_id;
    claims[0].claim.base_claim_id = Some(second_id);
    claims[1].claim.base_claim_id = Some(first_id);
    analyze_claims(&mut claims, &BTreeMap::new()).expect("cycles do not hang analysis");
}

#[test]
fn built_in_risk_paths_label_common_repository_safety_files() {
    let mut claims = related_pair(
        OverlapRelationship::Unrelated,
        OverlapConfidence::Full,
        "Cargo.lock",
    );

    analyze_claims(&mut claims, &Config::default().risk_paths).expect("analyze defaults");

    assert_eq!(claims[0].overlaps[0].severity, OverlapSeverity::Blocking);
    assert_eq!(claims[0].overlaps[0].labels, ["lockfile"]);
}

#[test]
fn invalid_persisted_scope_globs_are_structured_errors() {
    let mut invalid = claim(1);
    invalid.declared_scope = Some(DeclaredScope {
        allowed_paths: vec!["[".to_owned()],
        disallowed_paths: Vec::new(),
    });
    let mut claims = vec![observed(invalid, tracked(&["src/lib.rs"]))];

    let error = analyze_claims(&mut claims, &BTreeMap::new()).expect_err("invalid glob fails");

    assert!(error.to_string().contains("invalid path glob"));
}

#[test]
fn empty_declared_scope_performs_no_scope_check() {
    let mut unscoped = claim(1);
    unscoped.declared_scope = Some(DeclaredScope::default());
    let mut claims = vec![observed(unscoped, tracked(&["any/path.rs"]))];

    analyze_claims(&mut claims, &BTreeMap::new()).expect("analyze empty scope");

    assert!(claims[0].scope_warnings.is_empty());
}

#[test]
fn reclaimed_generations_and_null_wait_refs_remain_unrelated() {
    let old_generation = Uuid::new_v4();
    let mut current = claim(8);
    current.claim_id = Uuid::new_v4();
    let mut child = claim(9);
    child.base_issue = Some(issue(8));
    child.base_claim_id = Some(old_generation);
    child.wait_for = vec![WaitForRef {
        issue: issue(8),
        claim_id: None,
    }];
    let mut claims = vec![
        observed(child, tracked(&["shared.rs"])),
        observed(current, tracked(&["shared.rs"])),
    ];

    analyze_claims(&mut claims, &BTreeMap::new()).expect("analyze claims");

    assert_eq!(
        claims[0].overlaps[0].relationship,
        OverlapRelationship::Unrelated
    );
    assert_eq!(
        serde_json::to_value(&claims[0].overlaps[0]).unwrap()["relationship"],
        "unrelated"
    );
}

#[test]
fn scope_checks_cover_tracked_and_untracked_paths_with_independent_reasons() {
    let mut scoped = claim(1);
    scoped.declared_scope = Some(DeclaredScope {
        allowed_paths: vec!["src/**".to_owned()],
        disallowed_paths: vec!["**/generated/**".to_owned(), "outside/**".to_owned()],
    });
    let mut diff = tracked(&["src/lib.rs", "src/generated/code.rs", "outside/file.rs"]);
    diff.untracked_paths = vec!["outside/new.rs".to_owned()];
    let mut claims = vec![observed(scoped, diff)];

    analyze_claims(&mut claims, &BTreeMap::new()).expect("analyze scope");

    assert_eq!(claims[0].scope_warnings.len(), 5);
    assert!(claims[0].scope_warnings.iter().any(|warning| {
        warning.path == "src/generated/code.rs"
            && warning.reason == ScopeWarningReason::InsideDisallowedScope
    }));
    for path in ["outside/file.rs", "outside/new.rs"] {
        assert!(claims[0].scope_warnings.iter().any(|warning| {
            warning.path == path && warning.reason == ScopeWarningReason::OutsideAllowedScope
        }));
        assert!(claims[0].scope_warnings.iter().any(|warning| {
            warning.path == path && warning.reason == ScopeWarningReason::InsideDisallowedScope
        }));
    }
}

#[test]
fn dot_relative_scope_matches_repository_relative_git_paths() {
    let mut scoped = claim(1);
    scoped.declared_scope = Some(DeclaredScope {
        allowed_paths: vec!["./README.md".to_owned()],
        disallowed_paths: Vec::new(),
    });
    let mut claims = vec![observed(scoped, tracked(&["README.md"]))];

    analyze_claims(&mut claims, &BTreeMap::new()).expect("analyze dot-relative scope");

    assert!(claims[0].scope_warnings.is_empty());
}

#[test]
fn shared_declared_scope_is_not_an_overlap_until_both_diffs_touch_a_path() {
    let mut first = claim(1);
    let mut second = claim(2);
    for claim in [&mut first, &mut second] {
        claim.declared_scope = Some(DeclaredScope {
            allowed_paths: vec!["README.md".to_owned()],
            disallowed_paths: Vec::new(),
        });
    }
    let mut claims = vec![
        observed(first, DiffSummary::default()),
        observed(second, DiffSummary::default()),
    ];

    analyze_claims(&mut claims, &BTreeMap::new()).expect("analyze empty diffs");
    assert!(claims.iter().all(|claim| claim.overlaps.is_empty()));

    for claim in &mut claims {
        claim.diff = Some(tracked(&["README.md"]));
    }
    analyze_claims(&mut claims, &BTreeMap::new()).expect("analyze shared change");
    assert_eq!(claims[0].overlaps[0].shared_paths, ["README.md"]);
}

fn related_pair(
    relationship: OverlapRelationship,
    confidence: OverlapConfidence,
    path: &str,
) -> Vec<ClaimObservation> {
    let mut subject = claim(1);
    let mut other = claim(2);
    match relationship {
        OverlapRelationship::Ancestor => {
            subject.base_issue = Some(other.issue);
            subject.base_claim_id = Some(other.claim_id);
        }
        OverlapRelationship::Descendant => {
            other.base_issue = Some(subject.issue);
            other.base_claim_id = Some(subject.claim_id);
        }
        OverlapRelationship::Sibling => {
            let parent = Uuid::new_v4();
            subject.base_issue = Some(issue(99));
            subject.base_claim_id = Some(parent);
            other.base_issue = Some(issue(99));
            other.base_claim_id = Some(parent);
        }
        OverlapRelationship::Consolidation => {
            subject.wait_for = vec![WaitForRef {
                issue: other.issue,
                claim_id: Some(other.claim_id),
            }]
        }
        OverlapRelationship::Unrelated => {}
    }
    let (left, right) = match confidence {
        OverlapConfidence::Full => (tracked(&[path]), tracked(&[path])),
        OverlapConfidence::Untracked => (untracked(&[path]), tracked(&[path])),
    };
    vec![observed(subject, left), observed(other, right)]
}

fn expected_severity(relationship: OverlapRelationship, risk: bool) -> OverlapSeverity {
    match (relationship, risk) {
        (OverlapRelationship::Sibling | OverlapRelationship::Unrelated, false) => {
            OverlapSeverity::Warning
        }
        (OverlapRelationship::Sibling | OverlapRelationship::Unrelated, true) => {
            OverlapSeverity::Blocking
        }
        (_, false) => OverlapSeverity::Info,
        (_, true) => OverlapSeverity::Warning,
    }
}

fn observed(claim: Claim, diff: DiffSummary) -> ClaimObservation {
    ClaimObservation {
        claim,
        diff: Some(diff),
        overlaps: Vec::new(),
        scope_warnings: Vec::new(),
    }
}

fn tracked(paths: &[&str]) -> DiffSummary {
    DiffSummary {
        changed_paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        modified_paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        ..DiffSummary::default()
    }
}

fn untracked(paths: &[&str]) -> DiffSummary {
    DiffSummary {
        untracked_paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        ..DiffSummary::default()
    }
}

fn claim(number: u64) -> Claim {
    Claim {
        schema_version: SCHEMA_VERSION.to_owned(),
        claim_id: Uuid::new_v4(),
        repo: "local/fixture".to_owned(),
        issue: issue(number),
        title: None,
        branch: format!("issue-{number}"),
        worktree: PathBuf::from(format!("/tmp/issue-{number}")),
        base_remote: "origin".to_owned(),
        base_ref: "main".to_owned(),
        base_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        base_issue: None,
        base_claim_id: None,
        wait_for: Vec::new(),
        declared_scope: None,
        note: None,
        created_at: Utc::now(),
    }
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive issue")
}

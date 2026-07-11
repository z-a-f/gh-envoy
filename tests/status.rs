use std::num::NonZeroU64;
use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use gh_envoy::conflict::{
    DiffOverlap, OverlapConfidence, OverlapRelationship, OverlapSeverity, ScopeWarning,
    ScopeWarningReason,
};
use gh_envoy::model::{Claim, DeclaredScope, SCHEMA_VERSION};
use gh_envoy::observation::{DiffSummary, LocalProblem, LocalProblemCode};
use gh_envoy::status::{
    ClaimStatus, GithubState, StatusReport, render_status_human, render_status_human_colored,
    status_document,
};
use uuid::Uuid;

mod support;

use support::assert_text_eq;

#[test]
fn human_and_json_status_goldens_are_stable() {
    let report = fixture_report();

    let human = render_status_human(&report);
    let json = serde_json::to_string(&status_document(&report)).expect("serialize status");

    assert_text_eq(&human, include_str!("golden/status-human.txt"));
    assert_text_eq(&json, include_str!("golden/status-json.json").trim_end());
}

#[test]
fn golden_comparison_accepts_platform_line_endings() {
    assert_text_eq("first\nsecond\n", "first\r\nsecond\r\n");
}

#[test]
fn warning_rollup_ignores_info_only_overlap() {
    let mut report = fixture_report();
    report.problems.clear();
    report.claims[0].scope_warnings.clear();
    for overlap in &mut report.claims[0].overlaps {
        overlap.severity = OverlapSeverity::Info;
    }
    report.claims[1].overlaps.clear();

    assert!(!report.has_warnings());
    assert_eq!(status_document(&report).status, "success");
}

#[test]
fn twenty_claims_render_as_one_deterministic_row_each() {
    let template = fixture_report().claims.remove(0);
    let claims = (1..=20)
        .map(|number| {
            let mut status = template.clone();
            status.claim.issue = issue(number);
            status.claim.title = Some(format!(
                "Claim {number} with a title that is intentionally long"
            ));
            status.claim.branch = format!("envoy/issue-{number}-with-a-long-branch-name");
            status
        })
        .collect();
    let report = StatusReport {
        claims,
        problems: Vec::new(),
    };

    let human = render_status_human(&report);

    assert!(human.starts_with("Active claims: 20\n"));
    assert!(human.contains("intentionally long"));
    for number in 1..=20 {
        assert!(human.contains(&format!("#{} ", number)));
    }
}

#[test]
fn colored_status_uses_ansi_only_when_requested() {
    let report = fixture_report();

    let plain = render_status_human(&report);
    let colored = render_status_human_colored(&report);

    assert!(!plain.contains("\u{1b}["));
    assert!(colored.contains("\u{1b}[33m!\u{1b}[0m #12"));
    assert!(colored.contains("\u{1b}[2mBranch\u{1b}[0m"));
    assert!(colored.contains("\u{1b}[31m✗\u{1b}[0m missing_branch"));
}

#[test]
fn declared_scope_is_visible_before_the_diff_has_changes() {
    let mut report = fixture_report();
    report.claims[0].claim.declared_scope = Some(DeclaredScope {
        allowed_paths: vec!["README.md".to_owned()],
        disallowed_paths: vec![".github/**".to_owned()],
    });
    report.claims[0].diff = DiffSummary::default();
    report.claims[0].overlaps.clear();

    let human = render_status_human(&report);

    assert!(human.contains("Overlaps      none (diff-based)"));
    assert!(human.contains("Scope         allow: README.md; deny: .github/**"));

    report.claims[0].claim.declared_scope = Some(DeclaredScope::default());
    let human = render_status_human(&report);
    assert!(human.contains("Scope         none"));
}

#[test]
fn empty_human_status_is_concise() {
    assert_eq!(
        render_status_human(&StatusReport::default()),
        "No active claims.\n"
    );
}

fn fixture_report() -> StatusReport {
    let first_id = uuid("321ba92e-f076-4bc7-bd5b-6cc16cf76277");
    let second_id = uuid("7a4d91bf-8ef4-4f8d-a3f6-6611fe214a8f");
    let third_id = uuid("c121ab44-e6fb-4c62-a504-8fdd2c0a028d");
    let first = ClaimStatus {
        claim: claim(
            12,
            first_id,
            Some("Build fixture"),
            "envoy/issue-12",
            "…/fixture-issue-12",
        ),
        pr: None,
        github_state: GithubState::Unverified,
        diff: DiffSummary {
            changed_paths: vec!["src/lib.rs".to_owned(), "tests/status.rs".to_owned()],
            added_paths: vec!["tests/status.rs".to_owned()],
            modified_paths: vec!["src/lib.rs".to_owned()],
            deleted_paths: Vec::new(),
            untracked_paths: vec!["notes.txt".to_owned()],
        },
        overlaps: vec![
            overlap(
                13,
                second_id,
                OverlapRelationship::Sibling,
                OverlapSeverity::Warning,
            ),
            overlap(
                14,
                third_id,
                OverlapRelationship::Unrelated,
                OverlapSeverity::Warning,
            ),
        ],
        scope_warnings: vec![ScopeWarning {
            path: "notes.txt".to_owned(),
            reason: ScopeWarningReason::OutsideAllowedScope,
        }],
        stack_warnings: Vec::new(),
    };
    let second = ClaimStatus {
        claim: claim(13, second_id, None, "envoy/issue-13", "…/fixture-issue-13"),
        pr: None,
        github_state: GithubState::Unverified,
        diff: DiffSummary::default(),
        overlaps: Vec::new(),
        scope_warnings: Vec::new(),
        stack_warnings: Vec::new(),
    };
    StatusReport {
        claims: vec![first, second],
        problems: vec![LocalProblem {
            code: LocalProblemCode::MissingBranch,
            issue: Some(issue(13)),
            claim_id: Some(second_id),
            operation_id: None,
            path: None,
            message: "local branch \"envoy/issue-13\" does not resolve to a commit".to_owned(),
        }],
    }
}

fn claim(number: u64, claim_id: Uuid, title: Option<&str>, branch: &str, worktree: &str) -> Claim {
    Claim {
        schema_version: SCHEMA_VERSION.to_owned(),
        claim_id,
        repo: "local/fixture".to_owned(),
        issue: issue(number),
        title: title.map(str::to_owned),
        branch: branch.to_owned(),
        worktree: PathBuf::from(worktree),
        base_remote: "origin".to_owned(),
        base_ref: "main".to_owned(),
        base_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        base_issue: None,
        base_claim_id: None,
        wait_for: Vec::new(),
        declared_scope: None,
        note: None,
        created_at: Utc.with_ymd_and_hms(2026, 7, 10, 18, 0, 0).unwrap(),
    }
}

fn overlap(
    number: u64,
    claim_id: Uuid,
    relationship: OverlapRelationship,
    severity: OverlapSeverity,
) -> DiffOverlap {
    DiffOverlap {
        with_issue: issue(number),
        with_claim_id: claim_id,
        relationship,
        shared_paths: vec!["src/lib.rs".to_owned()],
        confidence: OverlapConfidence::Full,
        severity,
        labels: Vec::new(),
    }
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive issue")
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("valid UUID")
}

use serde_json::{Value, json};
use tempfile::TempDir;

use gh_envoy::model::{Claim, OperationPhase, OperationRecord, ReleaseMarker, ReleaseReport};

#[test]
fn claim_round_trips_the_normative_schema() {
    let directory = TempDir::new().expect("temporary worktree");
    let value = valid_claim(directory.path());

    let claim = Claim::from_value(value.clone()).expect("valid claim");
    let encoded = claim.to_value().expect("serialize claim");

    assert_eq!(encoded, value);
}

#[test]
fn claim_rejects_relative_worktrees_unknown_state_and_wrong_versions() {
    let directory = TempDir::new().expect("temporary worktree");
    let mut relative = valid_claim(directory.path());
    relative["worktree"] = json!("relative/path");

    let mut derived = valid_claim(directory.path());
    derived["status"] = json!("active");

    let mut wrong_version = valid_claim(directory.path());
    wrong_version["schema_version"] = json!("9.9");

    for invalid in [relative, derived, wrong_version] {
        assert!(Claim::from_value(invalid).is_err());
    }
}

#[test]
fn claim_requires_both_exact_parent_relationship_fields() {
    let directory = TempDir::new().expect("temporary worktree");
    for missing in ["base_issue", "base_claim_id"] {
        let mut value = valid_claim(directory.path());
        value["base_issue"] = json!(12);
        value["base_claim_id"] = json!("321ba92e-f076-4bc7-bd5b-6cc16cf76277");
        value.as_object_mut().expect("claim object").remove(missing);

        assert!(Claim::from_value(value).is_err());
    }
}

#[test]
fn release_and_every_operation_phase_are_representable() {
    let release = json!({
        "schema_version": "0.1",
        "repo": "z-a-f/fixture",
        "issue": 7,
        "claim_id": "321ba92e-f076-4bc7-bd5b-6cc16cf76277",
        "reason": "manual",
        "released_at": "2026-07-10T18:00:00Z"
    });
    let release = ReleaseMarker::from_value(release).expect("valid release marker");
    release.to_value().expect("serialize release marker");

    let directory = TempDir::new().expect("temporary worktree");
    for phase in [
        "reserved",
        "branch_created",
        "worktree_created",
        "claim_committed",
        "cleanup_pending",
    ] {
        let operation = json!({
            "schema_version": "0.1",
            "operation_id": "2c9d1ce8-1a34-4a14-b55f-8260b02dccd0",
            "kind": "claim",
            "claim_id": "321ba92e-f076-4bc7-bd5b-6cc16cf76277",
            "issue": 7,
            "branch": "envoy/issue-7-321ba92e",
            "worktree": directory.path(),
            "phase": phase,
            "started_at": "2026-07-10T18:00:00Z"
        });
        let record = OperationRecord::from_value(operation).expect("valid operation");
        assert_eq!(record.phase.as_str(), phase);
        record.to_value().expect("serialize operation");
    }

    assert_eq!(OperationPhase::CleanupPending.as_str(), "cleanup_pending");
}

#[test]
fn operation_journal_rejects_run_phases_for_claim_operations() {
    let directory = TempDir::new().expect("temporary worktree");
    let invalid = json!({
        "schema_version": "0.1",
        "operation_id": "2c9d1ce8-1a34-4a14-b55f-8260b02dccd0",
        "kind": "claim",
        "claim_id": "321ba92e-f076-4bc7-bd5b-6cc16cf76277",
        "issue": 7,
        "branch": "envoy/issue-7-321ba92e",
        "worktree": directory.path(),
        "phase": "run_artifacts_created",
        "started_at": "2026-07-10T18:00:00Z"
    });

    assert!(OperationRecord::from_value(invalid).is_err());
}

#[test]
fn marker_only_release_report_exposes_explicit_cleanup_state() {
    let claim_id = uuid::Uuid::new_v4();
    let report = ReleaseReport::marker_only(
        std::num::NonZeroU64::new(7).expect("positive issue"),
        claim_id,
        true,
    );
    let value = serde_json::to_value(report).expect("serialize release report");

    assert_eq!(value["claim_id"], claim_id.to_string());
    assert_eq!(value["already_released"], true);
    assert_eq!(value["worktree_deleted"], false);
    assert_eq!(value["branch_deleted"], false);
    assert_eq!(value["cleanup_pending"], false);
}

#[test]
fn persisted_models_reject_empty_required_strings() {
    let directory = TempDir::new().expect("temporary worktree");
    for field in ["repo", "branch", "base_remote", "base_ref", "base_sha"] {
        let mut value = valid_claim(directory.path());
        value[field] = json!("  ");
        assert!(Claim::from_value(value).is_err(), "accepted empty {field}");
    }

    let release = json!({
        "schema_version": "0.1",
        "repo": "",
        "issue": 7,
        "claim_id": "321ba92e-f076-4bc7-bd5b-6cc16cf76277",
        "reason": "manual",
        "released_at": "2026-07-10T18:00:00Z"
    });
    assert!(ReleaseMarker::from_value(release).is_err());

    let operation = json!({
        "schema_version": "0.1",
        "operation_id": "2c9d1ce8-1a34-4a14-b55f-8260b02dccd0",
        "kind": "cleanup",
        "claim_id": "321ba92e-f076-4bc7-bd5b-6cc16cf76277",
        "issue": 7,
        "branch": "",
        "worktree": directory.path(),
        "phase": "cleanup_pending",
        "started_at": "2026-07-10T18:00:00Z"
    });
    assert!(OperationRecord::from_value(operation).is_err());
}

#[cfg(unix)]
#[test]
fn persisted_models_reject_non_utf8_worktree_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    let directory = TempDir::new().expect("temporary worktree");
    let mut claim = Claim::from_value(valid_claim(directory.path())).expect("valid claim");
    claim.worktree = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));

    assert!(claim.to_value().is_err());
}

fn valid_claim(worktree: &std::path::Path) -> Value {
    json!({
        "schema_version": "0.1",
        "claim_id": "321ba92e-f076-4bc7-bd5b-6cc16cf76277",
        "repo": "z-a-f/fixture",
        "issue": 7,
        "title": "Build fixture",
        "branch": "envoy/issue-7-321ba92e",
        "worktree": worktree,
        "base_remote": "origin",
        "base_ref": "main",
        "base_sha": "0123456789abcdef",
        "base_issue": null,
        "base_claim_id": null,
        "wait_for": [],
        "declared_scope": {
            "allowed_paths": [],
            "disallowed_paths": []
        },
        "note": null,
        "created_at": "2026-07-10T18:00:00Z"
    })
}

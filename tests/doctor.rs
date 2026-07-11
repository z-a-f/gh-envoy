use std::num::NonZeroU64;

use chrono::{TimeZone, Utc};
use gh_envoy::doctor::{
    CheckGate, CheckStatus, DoctorCheck, DoctorNodeReport, DoctorReport, DoctorSubject, GateRollup,
    doctor_document, redact_doctor_paths, render_doctor_human, rollup_gate,
};
use gh_envoy::exit::EnvoyExitCode;
use tempfile::TempDir;

mod support;

use support::assert_text_eq;

#[test]
fn gate_rollup_uses_worst_required_result() {
    assert_eq!(rollup_gate(&[]), GateRollup::Ok);
    assert_eq!(rollup_gate(&[check(CheckStatus::Pass)]), GateRollup::Ok);
    assert_eq!(rollup_gate(&[check(CheckStatus::Skip)]), GateRollup::Ok);
    assert_eq!(
        rollup_gate(&[required_check(CheckStatus::Skip)]),
        GateRollup::Error
    );
    assert_eq!(
        rollup_gate(&[check(CheckStatus::Warn)]),
        GateRollup::Warning
    );
    assert_eq!(
        rollup_gate(&[check(CheckStatus::Fail)]),
        GateRollup::Blocked
    );
    assert_eq!(
        rollup_gate(&[check(CheckStatus::Fail), check(CheckStatus::Error)]),
        GateRollup::Error
    );
}

#[test]
fn overall_status_is_worst_gate_and_maps_to_stable_exit_codes() {
    let checks = vec![
        DoctorCheck::new(
            "publish.ready",
            CheckGate::Publish,
            "Publish readiness",
            CheckStatus::Pass,
            "ready",
        ),
        DoctorCheck::new(
            "merge.overlap",
            CheckGate::Merge,
            "Merge overlap",
            CheckStatus::Fail,
            "overlap blocks merge",
        ),
    ];
    let report = DoctorReport::new(subject(), checks, Vec::new(), timestamp());

    assert_eq!(report.gates.publish, GateRollup::Ok);
    assert_eq!(report.gates.merge, GateRollup::Blocked);
    assert_eq!(report.status, GateRollup::Blocked);
    assert_eq!(report.exit_code(), EnvoyExitCode::Blocked);

    assert_eq!(GateRollup::Ok.exit_code(), EnvoyExitCode::Success);
    assert_eq!(GateRollup::Warning.exit_code(), EnvoyExitCode::Warning);
    assert_eq!(
        GateRollup::Error.exit_code(),
        EnvoyExitCode::OperationalError
    );
}

#[test]
fn human_and_json_doctor_goldens_are_stable() {
    let checks = vec![
        DoctorCheck::new(
            "integrity.claim_schema",
            CheckGate::Integrity,
            "Claim schema",
            CheckStatus::Pass,
            "claim schema is valid",
        ),
        DoctorCheck::new(
            "publish.remote",
            CheckGate::Publish,
            "Remote verification",
            CheckStatus::Skip,
            "remote checks were not requested",
        ),
        DoctorCheck::new(
            "merge.overlap",
            CheckGate::Merge,
            "Merge overlap",
            CheckStatus::Fail,
            "claim overlaps an unrelated active claim",
        )
        .required()
        .with_evidence(serde_json::json!({"issues": [12, 13]})),
        DoctorCheck::new(
            "integrity.operation_journal",
            CheckGate::Integrity,
            "Operation journal",
            CheckStatus::Error,
            "operation journal could not be read",
        )
        .required(),
    ];
    let report = DoctorReport::new(
        subject(),
        checks,
        vec!["repair the operation journal before continuing".to_owned()],
        timestamp(),
    );

    let human = render_doctor_human(&report);
    let json = serde_json::to_string(&doctor_document(&report)).expect("serialize doctor");

    assert_text_eq(&human, include_str!("golden/doctor-human.txt"));
    assert_text_eq(&json, include_str!("golden/doctor-json.json").trim_end());
}

#[test]
fn json_path_redaction_preserves_human_report_source() {
    let fixture = TempDir::new().expect("temporary path fixture");
    let worktree = fixture.path().join("feature");
    let journal = fixture.path().join("main/.git/envoy/operations/op.json");
    let worktree_text = worktree.to_string_lossy();
    let report = DoctorReport::new(
        subject(),
        vec![
            DoctorCheck::new(
                "integrity.operation_journal",
                CheckGate::Integrity,
                "Operation journal",
                CheckStatus::Fail,
                format!("cleanup {worktree_text} before continuing"),
            )
            .with_evidence(serde_json::json!({
                "worktree": worktree,
                "recovery": {
                    "commands": [{
                        "program": "git",
                        "args": ["worktree", "remove", "--", worktree]
                    }],
                    "remove_journal": journal
                }
            })),
        ],
        vec![format!("Run: git worktree remove -- {worktree_text}")],
        timestamp(),
    );

    let redacted = redact_doctor_paths(&report);
    let evidence = redacted.checks[0].evidence.as_ref().expect("evidence");

    assert_eq!(
        redacted.checks[0].message,
        "cleanup …/feature before continuing"
    );
    assert_eq!(evidence["worktree"], "…/feature");
    assert_eq!(evidence["recovery"]["commands"][0]["args"][3], "…/feature");
    assert_eq!(evidence["recovery"]["remove_journal"], "…/op.json");
    assert_eq!(
        redacted.recommendations,
        ["Run: git worktree remove -- …/feature"]
    );
    assert!(report.recommendations[0].contains(worktree_text.as_ref()));
}

#[test]
fn human_renderer_distinguishes_warning_symbol_and_stack_subject() {
    let report = DoctorReport::new(
        DoctorSubject {
            repo: "local/fixture".to_owned(),
            issue: None,
            stack: true,
        },
        vec![check(CheckStatus::Warn)],
        Vec::new(),
        timestamp(),
    );

    let human = render_doctor_human(&report);

    assert!(human.starts_with("Doctor report for local/fixture stack"));
    assert!(human.contains("! [integrity] Example: example result"));
    assert!(human.ends_with("Recommendations:\n- None\n"));
}

#[test]
fn ordered_stack_nodes_roll_up_into_the_aggregate_report() {
    let root_id = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
    let child_id = uuid::Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
    let root = DoctorNodeReport::new(
        issue(11),
        root_id,
        vec![DoctorCheck::new(
            "publish.parent_generation",
            CheckGate::Publish,
            "Parent generation",
            CheckStatus::Pass,
            "root has no parent",
        )],
        Vec::new(),
    );
    let child = DoctorNodeReport::new(
        issue(12),
        child_id,
        vec![DoctorCheck::new(
            "merge.overlap",
            CheckGate::Merge,
            "Diff overlap",
            CheckStatus::Fail,
            "risk-path overlap blocks merge",
        )],
        vec!["resolve overlap before merge".to_owned()],
    );
    let report = DoctorReport::new(
        DoctorSubject {
            repo: "local/fixture".to_owned(),
            issue: Some(issue(12)),
            stack: true,
        },
        Vec::new(),
        Vec::new(),
        timestamp(),
    )
    .with_nodes(vec![root, child]);

    assert_eq!(report.status, GateRollup::Blocked);
    assert_eq!(report.gates.publish, GateRollup::Ok);
    assert_eq!(report.gates.merge, GateRollup::Blocked);
    assert_eq!(report.nodes[0].claim_id, root_id);
    assert_eq!(report.nodes[1].claim_id, child_id);
    let value = serde_json::to_value(doctor_document(&report)).expect("serialize stack doctor");
    assert_eq!(value["doctor"]["nodes"][0]["issue"], 11);
    assert_eq!(value["doctor"]["nodes"][1]["issue"], 12);
}

fn check(status: CheckStatus) -> DoctorCheck {
    DoctorCheck::new(
        "integrity.example",
        CheckGate::Integrity,
        "Example",
        status,
        "example result",
    )
}

fn required_check(status: CheckStatus) -> DoctorCheck {
    check(status).required()
}

fn subject() -> DoctorSubject {
    DoctorSubject {
        repo: "local/fixture".to_owned(),
        issue: Some(NonZeroU64::new(12).expect("positive issue")),
        stack: false,
    }
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive issue")
}

fn timestamp() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 10, 18, 0, 0).unwrap()
}

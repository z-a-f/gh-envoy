use std::num::NonZeroU64;

use chrono::{TimeZone, Utc};
use gh_envoy::doctor::{
    CheckGate, CheckStatus, DoctorCheck, DoctorReport, DoctorSubject, GateRollup, doctor_document,
    render_doctor_human, rollup_gate,
};
use gh_envoy::exit::EnvoyExitCode;

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

fn timestamp() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 10, 18, 0, 0).unwrap()
}

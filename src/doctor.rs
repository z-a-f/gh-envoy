use std::num::NonZeroU64;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::exit::EnvoyExitCode;
use crate::model::SCHEMA_VERSION;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckGate {
    Integrity,
    Publish,
    Merge,
}

impl CheckGate {
    fn as_str(self) -> &'static str {
        match self {
            Self::Integrity => "integrity",
            Self::Publish => "publish",
            Self::Merge => "merge",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
    Error,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS ",
            Self::Warn => "WARN ",
            Self::Fail => "FAIL ",
            Self::Skip => "SKIP ",
            Self::Error => "ERROR",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateRollup {
    Ok,
    Warning,
    Blocked,
    Error,
}

impl GateRollup {
    fn severity(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Warning => 1,
            Self::Blocked => 2,
            Self::Error => 3,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    pub fn exit_code(self) -> EnvoyExitCode {
        match self {
            Self::Ok => EnvoyExitCode::Success,
            Self::Warning => EnvoyExitCode::Warning,
            Self::Blocked => EnvoyExitCode::Blocked,
            Self::Error => EnvoyExitCode::OperationalError,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DoctorSubject {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<NonZeroU64>,
    pub stack: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub gate: CheckGate,
    pub title: String,
    pub status: CheckStatus,
    #[serde(skip_serializing_if = "is_false")]
    pub required: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}

impl DoctorCheck {
    pub fn new(
        id: impl Into<String>,
        gate: CheckGate,
        title: impl Into<String>,
        status: CheckStatus,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            gate,
            title: title.into(),
            status,
            required: false,
            message: message.into(),
            evidence: None,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn with_evidence(mut self, evidence: Value) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct GateRollups {
    pub integrity: GateRollup,
    pub publish: GateRollup,
    pub merge: GateRollup,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DoctorReport {
    pub subject: DoctorSubject,
    pub status: GateRollup,
    pub gates: GateRollups,
    pub checks: Vec<DoctorCheck>,
    pub recommendations: Vec<String>,
    pub generated_at: DateTime<Utc>,
}

impl DoctorReport {
    pub fn new(
        subject: DoctorSubject,
        checks: Vec<DoctorCheck>,
        recommendations: Vec<String>,
        generated_at: DateTime<Utc>,
    ) -> Self {
        let gates = GateRollups {
            integrity: rollup_gate_for(&checks, CheckGate::Integrity),
            publish: rollup_gate_for(&checks, CheckGate::Publish),
            merge: rollup_gate_for(&checks, CheckGate::Merge),
        };
        let status = [gates.integrity, gates.publish, gates.merge]
            .into_iter()
            .max_by_key(|rollup| rollup.severity())
            .unwrap_or(GateRollup::Ok);
        Self {
            subject,
            status,
            gates,
            checks,
            recommendations,
            generated_at,
        }
    }

    pub fn exit_code(&self) -> EnvoyExitCode {
        self.status.exit_code()
    }
}

pub fn rollup_gate(checks: &[DoctorCheck]) -> GateRollup {
    checks
        .iter()
        .map(check_rollup)
        .max_by_key(|rollup| rollup.severity())
        .unwrap_or(GateRollup::Ok)
}

fn rollup_gate_for(checks: &[DoctorCheck], gate: CheckGate) -> GateRollup {
    let checks = checks
        .iter()
        .filter(|check| check.gate == gate)
        .cloned()
        .collect::<Vec<_>>();
    rollup_gate(&checks)
}

fn check_rollup(check: &DoctorCheck) -> GateRollup {
    match check.status {
        CheckStatus::Pass | CheckStatus::Skip if !check.required => GateRollup::Ok,
        CheckStatus::Warn => GateRollup::Warning,
        CheckStatus::Fail => GateRollup::Blocked,
        CheckStatus::Error | CheckStatus::Skip => GateRollup::Error,
        CheckStatus::Pass => GateRollup::Ok,
    }
}

#[derive(Debug, Serialize)]
pub struct DoctorDocument<'a> {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: GateRollup,
    pub doctor: &'a DoctorReport,
}

pub fn doctor_document(report: &DoctorReport) -> DoctorDocument<'_> {
    DoctorDocument {
        schema_version: SCHEMA_VERSION,
        command: "doctor",
        status: report.status,
        doctor: report,
    }
}

pub fn render_doctor_human(report: &DoctorReport) -> String {
    let mut output = format!("Doctor report for {}", report.subject.repo);
    if let Some(issue) = report.subject.issue {
        output.push_str(&format!(" issue #{issue}"));
    } else if report.subject.stack {
        output.push_str(" stack");
    }
    output.push_str(&format!(
        "\n\nOverall: {}\nGates: integrity={} publish={} merge={}\n\nChecks:\n",
        report.status.as_str(),
        report.gates.integrity.as_str(),
        report.gates.publish.as_str(),
        report.gates.merge.as_str(),
    ));
    for check in &report.checks {
        output.push_str(&format!(
            "{} [{}] {}: {}\n",
            check.status.label(),
            check.gate.as_str(),
            check.title,
            check.message,
        ));
    }
    output.push_str("\nRecommendations:\n");
    if report.recommendations.is_empty() {
        output.push_str("- None\n");
    } else {
        for recommendation in &report.recommendations {
            output.push_str(&format!("- {recommendation}\n"));
        }
    }
    output
}

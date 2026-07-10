use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::command::{CommandRunner, text_from_utf8_output};
use crate::config::{Config, ConfigError};
use crate::exit::EnvoyExitCode;
use crate::git::{GitCli, RepositoryContext, RepositoryError};
use crate::model::SCHEMA_VERSION;
use crate::observation::{LocalProblem, LocalProblemCode, ObservationError, observe_repository};
use crate::store::Store;

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
    fn symbol(self) -> &'static str {
        match self {
            Self::Pass => "✓",
            Self::Warn => "!",
            Self::Fail => "✗",
            Self::Skip => "-",
            Self::Error => "×",
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
    #[serde(skip_serializing_if = "is_false")]
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
            check.status.symbol(),
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

pub fn redact_doctor_paths(report: &DoctorReport) -> DoctorReport {
    let mut redacted = report.clone();
    let mut replacements = Vec::new();
    for check in &report.checks {
        if let Some(evidence) = &check.evidence {
            collect_absolute_paths(evidence, &mut replacements);
        }
    }
    replacements.sort_by(|left, right| right.0.len().cmp(&left.0.len()));
    replacements.dedup_by(|left, right| left.0 == right.0);
    for check in &mut redacted.checks {
        check.message = redact_text(&check.message, &replacements);
        if let Some(evidence) = &mut check.evidence {
            redact_value(evidence, &replacements);
        }
    }
    for recommendation in &mut redacted.recommendations {
        *recommendation = redact_text(recommendation, &replacements);
    }
    redacted
}

fn collect_absolute_paths(value: &Value, replacements: &mut Vec<(String, String)>) {
    match value {
        Value::String(value) if Path::new(value).is_absolute() => {
            replacements.push((value.clone(), shortened_path(Path::new(value))))
        }
        Value::Array(values) => {
            for value in values {
                collect_absolute_paths(value, replacements);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_absolute_paths(value, replacements);
            }
        }
        _ => {}
    }
}

fn redact_value(value: &mut Value, replacements: &[(String, String)]) {
    match value {
        Value::String(value) => *value = redact_text(value, replacements),
        Value::Array(values) => {
            for value in values {
                redact_value(value, replacements);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_value(value, replacements);
            }
        }
        _ => {}
    }
}

fn redact_text(value: &str, replacements: &[(String, String)]) -> String {
    replacements
        .iter()
        .fold(value.to_owned(), |text, (absolute, shortened)| {
            text.replace(absolute, shortened)
        })
}

fn shortened_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "…".to_owned(), |name| format!("…/{name}"))
}

pub fn doctor_repository<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
    issue: Option<NonZeroU64>,
) -> Result<DoctorReport, DoctorError> {
    let common_dir = RepositoryContext::discover_common_dir_with_runner(runner, cwd)?;
    let config = Config::load(&common_dir)?;
    let repository = RepositoryContext::discover_with_runner(runner, cwd, &config.base_remote)?;
    let subject = DoctorSubject {
        repo: repository.repository.clone(),
        issue,
        stack: false,
    };
    let observation = match observe_repository(runner, cwd) {
        Ok(observation) => observation,
        Err(error) => {
            let id = if matches!(error, ObservationError::Store(_)) {
                "integrity.claim_store"
            } else {
                "integrity.observation"
            };
            let check = DoctorCheck::new(
                id,
                CheckGate::Integrity,
                if id == "integrity.claim_store" {
                    "Claim store"
                } else {
                    "Local observation"
                },
                CheckStatus::Error,
                error.to_string(),
            )
            .required();
            return Ok(DoctorReport::new(
                subject,
                vec![check],
                Vec::new(),
                Utc::now(),
            ));
        }
    };

    let mut checks = vec![
        DoctorCheck::new(
            "integrity.claim_store",
            CheckGate::Integrity,
            "Claim store",
            CheckStatus::Pass,
            "persisted claim and operation records are readable",
        )
        .required(),
    ];
    let selected = observation
        .claims
        .iter()
        .filter(|observed| issue.is_none_or(|issue| observed.claim.issue == issue))
        .collect::<Vec<_>>();
    if let Some(issue) = issue {
        checks.push(
            DoctorCheck::new(
                "integrity.claim_exists",
                CheckGate::Integrity,
                "Active claim",
                if selected.is_empty() {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Pass
                },
                if selected.is_empty() {
                    format!("issue #{issue} has no active local claim")
                } else {
                    format!("issue #{issue} has an active local claim")
                },
            )
            .required(),
        );
    }

    let ownership = ownership_groups(&observation.claims);
    for observed in selected {
        let claim = &observed.claim;
        let evidence = || json!({"issue": claim.issue, "claim_id": claim.claim_id});
        checks.push(
            DoctorCheck::new(
                "integrity.claim_schema",
                CheckGate::Integrity,
                "Claim schema",
                CheckStatus::Pass,
                "claim schema and persisted location are valid",
            )
            .required()
            .with_evidence(evidence()),
        );
        checks.push(problem_check(
            claim.issue,
            claim.claim_id,
            &observation.problems,
            "integrity.worktree",
            "Worktree",
            &[
                LocalProblemCode::MissingWorktree,
                LocalProblemCode::WorktreeMismatch,
            ],
            "claimed worktree exists, is registered, and is attached to the claimed branch",
        ));
        checks.push(problem_check(
            claim.issue,
            claim.claim_id,
            &observation.problems,
            "integrity.branch",
            "Branch",
            &[LocalProblemCode::MissingBranch],
            "claimed branch resolves to a local commit",
        ));
        checks.push(problem_check(
            claim.issue,
            claim.claim_id,
            &observation.problems,
            "integrity.base",
            "Captured base",
            &[LocalProblemCode::MissingBase],
            "captured base resolves to the exact commit",
        ));
        checks.push(if let Some(diff) = &observed.diff {
            DoctorCheck::new(
                "integrity.diff",
                CheckGate::Integrity,
                "Diff derivation",
                CheckStatus::Pass,
                format!(
                    "derived {} changed and {} untracked paths",
                    diff.changed_paths.len(),
                    diff.untracked_paths.len()
                ),
            )
            .with_evidence(evidence())
        } else {
            DoctorCheck::new(
                "integrity.diff",
                CheckGate::Integrity,
                "Diff derivation",
                CheckStatus::Skip,
                "diff cannot be derived until branch and base checks pass",
            )
            .with_evidence(evidence())
        });

        let key = canonical_key(&claim.worktree);
        let owners = ownership
            .get(&key)
            .expect("every selected claim has an owner group");
        checks.push(
            DoctorCheck::new(
                "integrity.ownership",
                CheckGate::Integrity,
                "Declared worktree ownership",
                if owners.len() > 1 {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Pass
                },
                if owners.len() > 1 {
                    "multiple active claims declare the same canonical worktree".to_owned()
                } else {
                    "canonical worktree has one active owner".to_owned()
                },
            )
            .required()
            .with_evidence(json!({
                "worktree": claim.worktree,
                "issues": owners.iter().map(|owner| owner.0).collect::<Vec<_>>(),
                "claim_ids": owners.iter().map(|owner| owner.1).collect::<Vec<_>>(),
            })),
        );
    }

    let selected_operations = observation
        .operations
        .iter()
        .filter(|operation| issue.is_none_or(|issue| operation.issue == issue))
        .collect::<Vec<_>>();
    let mut recommendations = Vec::new();
    if selected_operations.is_empty() {
        checks.push(
            DoctorCheck::new(
                "integrity.operation_journal",
                CheckGate::Integrity,
                "Operation journal",
                CheckStatus::Pass,
                "no interrupted operations are recorded",
            )
            .required(),
        );
    } else {
        let git = GitCli::new(runner);
        let store = Store::new(repository.store_root());
        for operation in selected_operations {
            let (recovery, operation_recommendations) = recovery_plan(
                &git,
                &repository.main_worktree,
                &store,
                operation,
                &observation.claims,
            );
            recommendations.extend(operation_recommendations);
            checks.push(
                DoctorCheck::new(
                    "integrity.operation_journal",
                    CheckGate::Integrity,
                    "Operation journal",
                    CheckStatus::Fail,
                    format!(
                        "operation {} for issue #{} was interrupted in phase {}",
                        operation.operation_id,
                        operation.issue,
                        operation.phase.as_str()
                    ),
                )
                .required()
                .with_evidence(json!({"operation": operation, "recovery": recovery})),
            );
        }
    }

    Ok(DoctorReport::new(
        subject,
        checks,
        recommendations,
        Utc::now(),
    ))
}

fn problem_check(
    issue: NonZeroU64,
    claim_id: Uuid,
    problems: &[LocalProblem],
    id: &str,
    title: &str,
    codes: &[LocalProblemCode],
    success: &str,
) -> DoctorCheck {
    let problem = problems
        .iter()
        .find(|problem| problem.claim_id == Some(claim_id) && codes.contains(&problem.code));
    DoctorCheck::new(
        id,
        CheckGate::Integrity,
        title,
        if problem.is_some() {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        },
        problem.map_or_else(|| success.to_owned(), |problem| problem.message.clone()),
    )
    .required()
    .with_evidence(json!({"issue": issue, "claim_id": claim_id}))
}

fn canonical_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn ownership_groups(
    claims: &[crate::observation::ClaimObservation],
) -> BTreeMap<PathBuf, Vec<(NonZeroU64, Uuid)>> {
    let mut groups = BTreeMap::<PathBuf, Vec<(NonZeroU64, Uuid)>>::new();
    for observed in claims {
        groups
            .entry(canonical_key(&observed.claim.worktree))
            .or_default()
            .push((observed.claim.issue, observed.claim.claim_id));
    }
    groups
}

#[derive(Serialize)]
struct RecoveryCommand {
    program: &'static str,
    args: Vec<String>,
}

#[derive(Serialize)]
struct RecoveryPlan {
    commands: Vec<RecoveryCommand>,
    remove_journal: PathBuf,
}

fn recovery_plan<R: CommandRunner>(
    git: &GitCli<'_, R>,
    main_worktree: &Path,
    store: &Store,
    operation: &crate::model::OperationRecord,
    claims: &[crate::observation::ClaimObservation],
) -> (RecoveryPlan, Vec<String>) {
    let journal = store.operation_path(operation.operation_id);
    if claims
        .iter()
        .any(|observed| observed.claim.claim_id == operation.claim_id)
    {
        return (
            RecoveryPlan {
                commands: Vec::new(),
                remove_journal: journal.clone(),
            },
            vec![format!(
                "Remove operation journal {} after confirming the committed claim is intact",
                journal.display()
            )],
        );
    }

    let mut commands = Vec::new();
    let mut recommendations = Vec::new();
    if registered_worktree(git, main_worktree, &operation.worktree) {
        let args = vec![
            "worktree".to_owned(),
            "remove".to_owned(),
            "--".to_owned(),
            operation.worktree.to_string_lossy().into_owned(),
        ];
        recommendations.push(format!(
            "Run: git worktree remove -- {}",
            operation.worktree.display()
        ));
        commands.push(RecoveryCommand {
            program: "git",
            args,
        });
    }

    let generated = operation.branch
        == format!(
            "envoy/issue-{}-{}",
            operation.issue,
            &operation.claim_id.simple().to_string()[..8]
        );
    if generated && branch_exists(git, main_worktree, &operation.branch) {
        recommendations.push(format!("Run: git branch -d -- {}", operation.branch));
        commands.push(RecoveryCommand {
            program: "git",
            args: vec![
                "branch".to_owned(),
                "-d".to_owned(),
                "--".to_owned(),
                operation.branch.clone(),
            ],
        });
    } else if !generated && branch_exists(git, main_worktree, &operation.branch) {
        recommendations.push(format!(
            "Review adopted branch {} manually; Envoy will not recommend deleting it",
            operation.branch
        ));
    }
    recommendations.push(format!(
        "Remove operation journal {} only after cleanup succeeds",
        journal.display()
    ));
    (
        RecoveryPlan {
            commands,
            remove_journal: journal,
        },
        recommendations,
    )
}

fn registered_worktree<R: CommandRunner>(
    git: &GitCli<'_, R>,
    main_worktree: &Path,
    expected: &Path,
) -> bool {
    let Ok(output) = git.run(main_worktree, ["worktree", "list", "--porcelain", "-z"]) else {
        return false;
    };
    let Ok(text) = text_from_utf8_output(&output.stdout, "git worktree list --porcelain -z") else {
        return false;
    };
    let expected = canonical_key(expected);
    text.split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
        .any(|path| canonical_key(Path::new(path)) == expected)
}

fn branch_exists<R: CommandRunner>(git: &GitCli<'_, R>, cwd: &Path, branch: &str) -> bool {
    let reference = format!("refs/heads/{branch}^{{commit}}");
    git.attempt(cwd, ["rev-parse", "--verify", "--quiet", &reference])
        .is_ok_and(|output| output.exit_code == Some(0))
}

#[derive(Debug, Error)]
pub enum DoctorError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

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
use crate::conflict::{OverlapRelationship, OverlapSeverity, ScopeWarningReason};
use crate::exit::EnvoyExitCode;
use crate::git::{GitCli, RepositoryContext, RepositoryError};
use crate::github::{
    GithubIssueError, GithubIssueObservation, GithubIssueState, GithubPullRequestObservation,
    GithubPullRequestState, observe_issue, observe_pull_request,
};
use crate::model::{Claim, SCHEMA_VERSION};
use crate::observation::{
    LocalProblem, LocalProblemCode, ObservationError, observe_claims, observe_repository,
};
use crate::stack::{StackError, StackNode, StackProblem, resolve_stack, wait_for_cycles};
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
pub struct DoctorNodeReport {
    pub issue: NonZeroU64,
    pub claim_id: Uuid,
    pub status: GateRollup,
    pub gates: GateRollups,
    pub checks: Vec<DoctorCheck>,
    pub recommendations: Vec<String>,
}

impl DoctorNodeReport {
    pub fn new(
        issue: NonZeroU64,
        claim_id: Uuid,
        checks: Vec<DoctorCheck>,
        recommendations: Vec<String>,
    ) -> Self {
        let gates = rollups(&checks);
        Self {
            issue,
            claim_id,
            status: worst_gate(gates),
            gates,
            checks,
            recommendations,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DoctorReport {
    pub subject: DoctorSubject,
    pub status: GateRollup,
    pub gates: GateRollups,
    pub checks: Vec<DoctorCheck>,
    pub recommendations: Vec<String>,
    pub generated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<DoctorNodeReport>,
}

impl DoctorReport {
    pub fn new(
        subject: DoctorSubject,
        checks: Vec<DoctorCheck>,
        recommendations: Vec<String>,
        generated_at: DateTime<Utc>,
    ) -> Self {
        let gates = rollups(&checks);
        let status = worst_gate(gates);
        Self {
            subject,
            status,
            gates,
            checks,
            recommendations,
            generated_at,
            nodes: Vec::new(),
        }
    }

    pub fn with_nodes(mut self, nodes: Vec<DoctorNodeReport>) -> Self {
        let mut checks = self.checks.clone();
        for node in &nodes {
            checks.extend(node.checks.iter().cloned());
            self.recommendations
                .extend(node.recommendations.iter().cloned());
        }
        self.recommendations.sort();
        self.recommendations.dedup();
        self.gates = rollups(&checks);
        self.status = worst_gate(self.gates);
        self.nodes = nodes;
        self
    }

    pub fn exit_code(&self) -> EnvoyExitCode {
        self.status.exit_code()
    }
}

fn rollups(checks: &[DoctorCheck]) -> GateRollups {
    GateRollups {
        integrity: rollup_gate_for(checks, CheckGate::Integrity),
        publish: rollup_gate_for(checks, CheckGate::Publish),
        merge: rollup_gate_for(checks, CheckGate::Merge),
    }
}

fn worst_gate(gates: GateRollups) -> GateRollup {
    [gates.integrity, gates.publish, gates.merge]
        .into_iter()
        .max_by_key(|rollup| rollup.severity())
        .unwrap_or(GateRollup::Ok)
}

pub fn coordination_checks(
    observed: &crate::observation::ClaimObservation,
) -> (Vec<DoctorCheck>, Vec<String>) {
    let mut checks = Vec::new();
    let mut recommendations = Vec::new();
    for overlap in &observed.overlaps {
        let status = match overlap.severity {
            OverlapSeverity::Info => CheckStatus::Pass,
            OverlapSeverity::Warning => CheckStatus::Warn,
            OverlapSeverity::Blocking => CheckStatus::Fail,
        };
        checks.push(
            DoctorCheck::new(
                "merge.overlap",
                CheckGate::Merge,
                "Diff overlap",
                status,
                format!(
                    "{} overlap with issue #{} generation {} across {} path(s)",
                    relationship_name(overlap.relationship),
                    overlap.with_issue,
                    &overlap.with_claim_id.to_string()[..8],
                    overlap.shared_paths.len()
                ),
            )
            .with_evidence(json!({
                "issue": observed.claim.issue,
                "claim_id": observed.claim.claim_id,
                "overlap": overlap,
            })),
        );
        if !matches!(overlap.severity, OverlapSeverity::Info) {
            recommendations.push(format!(
                "Resolve overlap between issue #{} and issue #{} before merge",
                observed.claim.issue, overlap.with_issue
            ));
        }
    }
    for warning in &observed.scope_warnings {
        checks.push(
            DoctorCheck::new(
                "merge.scope",
                CheckGate::Merge,
                "Declared scope",
                CheckStatus::Warn,
                format!(
                    "{} {}",
                    warning.path,
                    match warning.reason {
                        ScopeWarningReason::OutsideAllowedScope => "is outside allowed scope",
                        ScopeWarningReason::InsideDisallowedScope => "is inside disallowed scope",
                    }
                ),
            )
            .with_evidence(json!({
                "issue": observed.claim.issue,
                "claim_id": observed.claim.claim_id,
                "warning": warning,
            })),
        );
    }
    if !observed.claim.wait_for.is_empty() {
        let (changed, untracked) = observed.diff.as_ref().map_or((0, 0), |diff| {
            (diff.changed_paths.len(), diff.untracked_paths.len())
        });
        checks.push(
            DoctorCheck::new(
                "merge.consolidation_diff",
                CheckGate::Merge,
                "Consolidation diff",
                CheckStatus::Pass,
                "consolidation diff may appear oversized until multi-parent diff bases are supported",
            )
            .with_evidence(json!({
                "issue": observed.claim.issue,
                "claim_id": observed.claim.claim_id,
                "changed_paths": changed,
                "untracked_paths": untracked,
                "wait_for": observed.claim.wait_for,
            })),
        );
    }
    (checks, recommendations)
}

fn relationship_name(relationship: OverlapRelationship) -> &'static str {
    match relationship {
        OverlapRelationship::Sibling => "sibling",
        OverlapRelationship::Unrelated => "unrelated",
        OverlapRelationship::Ancestor => "ancestor",
        OverlapRelationship::Descendant => "descendant",
        OverlapRelationship::Consolidation => "consolidation",
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
    if !report.nodes.is_empty() {
        output.push_str("\nStack nodes (root -> target):\n");
        for node in &report.nodes {
            output.push_str(&format!(
                "\n#{} {}: {} (integrity={} publish={} merge={})\n",
                node.issue,
                &node.claim_id.to_string()[..8],
                node.status.as_str(),
                node.gates.integrity.as_str(),
                node.gates.publish.as_str(),
                node.gates.merge.as_str(),
            ));
            for check in &node.checks {
                output.push_str(&format!(
                    "  {} [{}] {}: {}\n",
                    check.status.symbol(),
                    check.gate.as_str(),
                    check.title,
                    check.message,
                ));
            }
        }
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
    for node in &report.nodes {
        for check in &node.checks {
            if let Some(evidence) = &check.evidence {
                collect_absolute_paths(evidence, &mut replacements);
            }
        }
    }
    replacements.sort_by_key(|item| std::cmp::Reverse(item.0.len()));
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
    for node in &mut redacted.nodes {
        for check in &mut node.checks {
            check.message = redact_text(&check.message, &replacements);
            if let Some(evidence) = &mut check.evidence {
                redact_value(evidence, &replacements);
            }
        }
        for recommendation in &mut node.recommendations {
            *recommendation = redact_text(recommendation, &replacements);
        }
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
    let mut recommendations = Vec::new();
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

    let store = Store::new(repository.store_root());
    let roots = issue.map_or_else(
        || {
            observation
                .claims
                .iter()
                .map(|observed| observed.claim.issue)
                .collect::<Vec<_>>()
        },
        |issue| vec![issue],
    );
    let active = observation
        .claims
        .iter()
        .map(|observed| observed.claim.clone())
        .collect::<Vec<_>>();
    let (stack_checks, stack_recommendations) =
        stack_checks(runner, &repository.main_worktree, &store, &active, &roots)?;
    checks.extend(stack_checks);
    recommendations.extend(stack_recommendations);

    let ownership = ownership_groups(&observation.claims);
    for observed in selected {
        let claim = &observed.claim;
        let evidence = || json!({"issue": claim.issue, "claim_id": claim.claim_id});
        let has_problem = |code| {
            observation
                .problems
                .iter()
                .any(|problem| problem.claim_id == Some(claim.claim_id) && problem.code == code)
        };
        if has_problem(LocalProblemCode::MissingBranch)
            && has_problem(LocalProblemCode::MissingWorktree)
        {
            recommendations.push(format!(
                "If issue #{} is stale, run: gh envoy release {} --reason abandoned",
                claim.issue, claim.issue
            ));
        }
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
        let (coordination, coordination_recommendations) = coordination_checks(observed);
        checks.extend(coordination);
        recommendations.extend(coordination_recommendations);
        if repository.is_github_remote() {
            let (github_checks, github_recommendations) = github_checks_for_claim(
                runner,
                &repository.main_worktree,
                &repository.repository,
                claim,
            )?;
            checks.extend(github_checks);
            recommendations.extend(github_recommendations);
        }
    }

    let selected_operations = observation
        .operations
        .iter()
        .filter(|operation| issue.is_none_or(|issue| operation.issue == issue))
        .collect::<Vec<_>>();
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

pub fn doctor_stack<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
    target_issue: NonZeroU64,
) -> Result<DoctorReport, DoctorError> {
    let common_dir = RepositoryContext::discover_common_dir_with_runner(runner, cwd)?;
    let config = Config::load(&common_dir)?;
    let repository = RepositoryContext::discover_with_runner(runner, cwd, &config.base_remote)?;
    let store = Store::new(repository.store_root());
    let active = store.active_claims()?;
    let resolution = resolve_stack(&store, &active, target_issue)?;
    let mut checks = Vec::new();
    let mut recommendations = Vec::new();
    let (mut graph_checks, graph_recommendations) = stack_checks(
        runner,
        &repository.main_worktree,
        &store,
        &active,
        &[target_issue],
    )?;
    checks.append(&mut graph_checks);
    recommendations.extend(graph_recommendations);

    let active_ids = active
        .iter()
        .map(|claim| claim.claim_id)
        .collect::<std::collections::BTreeSet<_>>();
    let mut claims = active.clone();
    for node in &resolution.nodes {
        if !active_ids.contains(&node.claim.claim_id) {
            claims.push(node.claim.clone());
        }
    }
    let observation = observe_claims(runner, cwd, claims)?;
    let active_observed = observation
        .claims
        .iter()
        .filter(|observed| active_ids.contains(&observed.claim.claim_id))
        .cloned()
        .collect::<Vec<_>>();
    let ownership = ownership_groups(&active_observed);
    let mut nodes = Vec::new();
    for node in &resolution.nodes {
        let Some(observed) = observation
            .claims
            .iter()
            .find(|observed| observed.claim.claim_id == node.claim.claim_id)
        else {
            continue;
        };
        let mut node_checks = basic_claim_checks(
            observed,
            &observation.problems,
            if node.active { Some(&ownership) } else { None },
        );
        let mut node_recommendations = Vec::new();
        if node.active {
            let mut active_only = observed.clone();
            active_only
                .overlaps
                .retain(|overlap| active_ids.contains(&overlap.with_claim_id));
            let (coordination, coordination_recommendations) = coordination_checks(&active_only);
            node_checks.extend(coordination);
            node_recommendations.extend(coordination_recommendations);
            if repository.is_github_remote() {
                let (github_checks, github_recommendations) = github_checks_for_claim(
                    runner,
                    &repository.main_worktree,
                    &repository.repository,
                    &node.claim,
                )?;
                node_checks.extend(github_checks);
                node_recommendations.extend(github_recommendations);
            }
        }
        if let Some(release) = &node.release {
            node_checks.push(
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Released generation",
                    CheckStatus::Fail,
                    "this exact stack generation has been released",
                )
                .required()
                .with_evidence(json!({"release": release})),
            );
        }
        nodes.push(DoctorNodeReport::new(
            node.claim.issue,
            node.claim.claim_id,
            node_checks,
            node_recommendations,
        ));
    }
    Ok(DoctorReport::new(
        DoctorSubject {
            repo: repository.repository,
            issue: Some(target_issue),
            stack: true,
        },
        checks,
        recommendations,
        Utc::now(),
    )
    .with_nodes(nodes))
}

fn github_checks_for_claim<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
    repository: &str,
    claim: &Claim,
) -> Result<(Vec<DoctorCheck>, Vec<String>), GithubIssueError> {
    let issue = observe_issue(runner, cwd, repository, claim.issue)?;
    let pull_request = observe_pull_request(runner, cwd, repository, &claim.branch)?;
    let mut checks = Vec::new();
    let mut recommendations = Vec::new();

    match issue {
        GithubIssueObservation::Available(issue) => {
            let closed = issue.state == GithubIssueState::Closed;
            checks.push(
                DoctorCheck::new(
                    "publish.issue_state",
                    CheckGate::Publish,
                    "GitHub issue",
                    if closed {
                        CheckStatus::Warn
                    } else {
                        CheckStatus::Pass
                    },
                    if closed {
                        format!("GitHub issue #{} is closed", claim.issue)
                    } else {
                        format!("GitHub issue #{} is open", claim.issue)
                    },
                )
                .with_evidence(json!({
                    "issue": claim.issue,
                    "title": issue.title,
                    "state": if closed { "closed" } else { "open" },
                })),
            );
            if closed {
                recommendations.push(format!(
                    "Release idempotently: gh envoy release {} --reason closed",
                    claim.issue
                ));
            }
        }
        GithubIssueObservation::NotFound => checks.push(
            DoctorCheck::new(
                "publish.issue_state",
                CheckGate::Publish,
                "GitHub issue",
                CheckStatus::Fail,
                format!(
                    "GitHub issue #{} does not exist or is not reachable in this repository",
                    claim.issue
                ),
            )
            .required(),
        ),
        GithubIssueObservation::Unavailable => checks.push(DoctorCheck::new(
            "publish.issue_state",
            CheckGate::Publish,
            "GitHub issue",
            CheckStatus::Skip,
            "GitHub issue state is unavailable; local checks remain valid",
        )),
    }

    match pull_request {
        GithubPullRequestObservation::Available(Some(pr)) => {
            let base_matches = pr.base == claim.base_ref;
            checks.push(
                DoctorCheck::new(
                    "publish.pr_base",
                    CheckGate::Publish,
                    "Pull request base",
                    if base_matches {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                    if base_matches {
                        format!(
                            "pull request #{} targets expected base {:?}",
                            pr.number, claim.base_ref
                        )
                    } else {
                        format!(
                            "pull request #{} targets {:?}, expected {:?}",
                            pr.number, pr.base, claim.base_ref
                        )
                    },
                )
                .required()
                .with_evidence(json!({
                    "number": pr.number,
                    "url": pr.url,
                    "head": pr.head,
                    "base": pr.base,
                    "expected_base": claim.base_ref,
                    "draft": pr.draft,
                    "state": match pr.state {
                        GithubPullRequestState::Open => "open",
                        GithubPullRequestState::Closed => "closed",
                        GithubPullRequestState::Merged => "merged",
                    },
                })),
            );
            if pr.state == GithubPullRequestState::Merged {
                recommendations.push(format!(
                    "Release idempotently: gh envoy release {} --reason merged",
                    claim.issue
                ));
            }
        }
        GithubPullRequestObservation::Available(None) => checks.push(DoctorCheck::new(
            "publish.pr_base",
            CheckGate::Publish,
            "Pull request base",
            CheckStatus::Skip,
            "no pull request exists for the exact claimed branch",
        )),
        GithubPullRequestObservation::Unavailable => checks.push(DoctorCheck::new(
            "publish.pr_base",
            CheckGate::Publish,
            "Pull request base",
            CheckStatus::Skip,
            "pull request facts are unavailable; local checks remain valid",
        )),
    }

    recommendations.sort();
    recommendations.dedup();
    Ok((checks, recommendations))
}

fn basic_claim_checks(
    observed: &crate::observation::ClaimObservation,
    problems: &[LocalProblem],
    ownership: Option<&BTreeMap<PathBuf, Vec<(NonZeroU64, Uuid)>>>,
) -> Vec<DoctorCheck> {
    let claim = &observed.claim;
    let evidence = || json!({"issue": claim.issue, "claim_id": claim.claim_id});
    let mut checks = vec![
        DoctorCheck::new(
            "integrity.claim_schema",
            CheckGate::Integrity,
            "Claim schema",
            CheckStatus::Pass,
            "claim schema and persisted location are valid",
        )
        .required()
        .with_evidence(evidence()),
        problem_check(
            claim.issue,
            claim.claim_id,
            problems,
            "integrity.worktree",
            "Worktree",
            &[
                LocalProblemCode::MissingWorktree,
                LocalProblemCode::WorktreeMismatch,
            ],
            "claimed worktree exists, is registered, and is attached to the claimed branch",
        ),
        problem_check(
            claim.issue,
            claim.claim_id,
            problems,
            "integrity.branch",
            "Branch",
            &[LocalProblemCode::MissingBranch],
            "claimed branch resolves to a local commit",
        ),
        problem_check(
            claim.issue,
            claim.claim_id,
            problems,
            "integrity.base",
            "Captured base",
            &[LocalProblemCode::MissingBase],
            "captured base resolves to the exact commit",
        ),
    ];
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
    if let Some(ownership) = ownership {
        let owners = ownership
            .get(&canonical_key(&claim.worktree))
            .expect("active claim has an owner group");
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
                    "multiple active claims declare the same canonical worktree"
                } else {
                    "canonical worktree has one active owner"
                },
            )
            .required()
            .with_evidence(json!({"worktree": claim.worktree, "owners": owners})),
        );
    }
    checks
}

fn stack_checks<R: CommandRunner>(
    runner: &R,
    main_worktree: &Path,
    store: &Store,
    active: &[Claim],
    roots: &[NonZeroU64],
) -> Result<(Vec<DoctorCheck>, Vec<String>), DoctorError> {
    let mut checks = Vec::new();
    let mut recommendations = Vec::new();
    for cycle in wait_for_cycles(active, roots) {
        checks.push(
            DoctorCheck::new(
                "publish.wait_for_cycle",
                CheckGate::Publish,
                "Consolidation dependency cycle",
                CheckStatus::Error,
                "wait_for dependencies contain a cycle",
            )
            .required()
            .with_evidence(json!({"cycle": cycle})),
        );
    }
    let git = GitCli::new(runner);
    let mut assessed = std::collections::BTreeSet::new();
    for root in roots {
        let resolution = resolve_stack(store, active, *root)?;
        if let Some(problem) = &resolution.problem {
            let check = stack_problem_check(problem);
            if !checks.contains(&check) {
                checks.push(check);
            }
            if let StackProblem::MissingParent {
                child_claim_id,
                parent_issue,
                parent_claim_id,
                ..
            } = problem
            {
                recommendations.push(format!(
                    "Restack child generation {} manually; exact parent #{} generation {} is unavailable",
                    &child_claim_id.to_string()[..8],
                    parent_issue,
                    &parent_claim_id.to_string()[..8]
                ));
            }
        }
        for pair in resolution.nodes.windows(2) {
            let parent = &pair[0];
            let child = &pair[1];
            if assessed.insert(child.claim.claim_id) {
                let (mut drift_checks, recommendation) =
                    parent_drift_checks(&git, main_worktree, parent, &child.claim)?;
                checks.append(&mut drift_checks);
                recommendations.extend(recommendation);
            }
        }
    }
    Ok((checks, recommendations))
}

fn stack_problem_check(problem: &StackProblem) -> DoctorCheck {
    match problem {
        StackProblem::BaseCycle { cycle } => DoctorCheck::new(
            "publish.base_cycle",
            CheckGate::Publish,
            "Stack dependency cycle",
            CheckStatus::Error,
            "base_claim_id dependencies contain a cycle",
        )
        .required()
        .with_evidence(json!({"cycle": cycle})),
        StackProblem::MissingParent {
            child_claim_id,
            parent_issue,
            parent_claim_id,
            replacement_claim_id,
        } => DoctorCheck::new(
            "publish.parent_generation",
            CheckGate::Publish,
            "Parent generation",
            CheckStatus::Fail,
            "the exact recorded parent generation is missing",
        )
        .required()
        .with_evidence(json!({
            "child_claim_id": child_claim_id,
            "parent_issue": parent_issue,
            "parent_claim_id": parent_claim_id,
            "replacement_claim_id": replacement_claim_id,
        })),
        StackProblem::MissingTarget { issue } => DoctorCheck::new(
            "integrity.claim_exists",
            CheckGate::Integrity,
            "Active claim",
            CheckStatus::Fail,
            format!("issue #{issue} has no active local claim"),
        )
        .required(),
        StackProblem::DuplicateTarget { issue } => DoctorCheck::new(
            "integrity.claim_exists",
            CheckGate::Integrity,
            "Active claim",
            CheckStatus::Error,
            format!("issue #{issue} has multiple active local claims"),
        )
        .required(),
    }
}

fn parent_drift_checks<R: CommandRunner>(
    git: &GitCli<'_, R>,
    main_worktree: &Path,
    parent: &StackNode,
    child: &Claim,
) -> Result<(Vec<DoctorCheck>, Vec<String>), DoctorError> {
    let evidence = || {
        json!({
            "issue": child.issue,
            "claim_id": child.claim_id,
            "parent_issue": parent.claim.issue,
            "parent_claim_id": parent.claim.claim_id,
            "captured_sha": child.base_sha,
        })
    };
    if let Some(release) = &parent.release {
        return Ok((
            vec![
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Parent generation",
                    CheckStatus::Fail,
                    "the exact recorded parent generation has been released",
                )
                .required()
                .with_evidence(json!({"relationship": evidence(), "release": release})),
            ],
            vec![manual_restack_recommendation(child, &parent.claim, false)],
        ));
    }
    if !parent.active {
        return Ok((
            vec![
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Parent generation",
                    CheckStatus::Error,
                    "the exact parent generation is neither active nor released",
                )
                .required()
                .with_evidence(evidence()),
            ],
            Vec::new(),
        ));
    }
    let reference = format!("refs/heads/{}^{{commit}}", parent.claim.branch);
    let output = git.attempt(
        main_worktree,
        ["rev-parse", "--verify", "--quiet", &reference],
    )?;
    if output.exit_code != Some(0) {
        return Ok((
            vec![
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Parent generation",
                    CheckStatus::Error,
                    "the exact parent branch does not resolve",
                )
                .required()
                .with_evidence(evidence()),
            ],
            Vec::new(),
        ));
    }
    let tip = text_from_utf8_output(&output.stdout, "git rev-parse parent branch")
        .map_err(DoctorError::InvalidGitOutput)?
        .to_owned();
    if tip == child.base_sha {
        return Ok((
            vec![
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Parent generation",
                    CheckStatus::Pass,
                    "captured parent generation is current",
                )
                .required()
                .with_evidence(json!({"relationship": evidence(), "parent_tip": tip})),
            ],
            Vec::new(),
        ));
    }
    let ancestry = git.attempt(
        main_worktree,
        [
            "merge-base",
            "--is-ancestor",
            child.base_sha.as_str(),
            tip.as_str(),
        ],
    )?;
    if ancestry.exit_code == Some(0) {
        Ok((
            vec![
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Parent generation",
                    CheckStatus::Pass,
                    "captured parent SHA remains in current parent history",
                )
                .required()
                .with_evidence(json!({"relationship": evidence(), "parent_tip": tip})),
                DoctorCheck::new(
                    "merge.parent_advanced",
                    CheckGate::Merge,
                    "Parent branch advanced",
                    CheckStatus::Warn,
                    "parent branch advanced after the child captured its base",
                )
                .with_evidence(json!({"relationship": evidence(), "parent_tip": tip})),
            ],
            Vec::new(),
        ))
    } else if ancestry.exit_code == Some(1) {
        Ok((
            vec![
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Parent generation",
                    CheckStatus::Fail,
                    "captured parent SHA is no longer in current parent history",
                )
                .required()
                .with_evidence(json!({"relationship": evidence(), "parent_tip": tip})),
            ],
            vec![manual_restack_recommendation(child, &parent.claim, true)],
        ))
    } else {
        Ok((
            vec![
                DoctorCheck::new(
                    "publish.parent_generation",
                    CheckGate::Publish,
                    "Parent generation",
                    CheckStatus::Error,
                    "could not compare captured and current parent history",
                )
                .required()
                .with_evidence(json!({
                    "relationship": evidence(),
                    "parent_tip": tip,
                    "exit_code": ancestry.exit_code,
                })),
            ],
            Vec::new(),
        ))
    }
}

fn manual_restack_recommendation(child: &Claim, parent: &Claim, active_parent: bool) -> String {
    let effective_base = if active_parent {
        parent.branch.clone()
    } else {
        format!("{}/{}", parent.base_remote, parent.base_ref)
    };
    format!(
        "Restack manually: git -C {} rebase --onto {} {}",
        child.worktree.display(),
        effective_base,
        child.base_sha
    )
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
    #[error(transparent)]
    Stack(#[from] StackError),
    #[error(transparent)]
    Runner(#[from] crate::command::RunnerError),
    #[error("invalid Git output: {0}")]
    InvalidGitOutput(String),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Observation(#[from] ObservationError),
    #[error(transparent)]
    Github(#[from] GithubIssueError),
}

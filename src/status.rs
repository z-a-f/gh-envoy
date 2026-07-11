use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::command::CommandRunner;
use crate::config::{Config, ConfigError};
use crate::conflict::{DiffOverlap, OverlapRelationship, OverlapSeverity, ScopeWarning};
use crate::git::{RepositoryContext, RepositoryError};
use crate::github::{
    GithubIssueError, GithubIssueObservation, GithubPullRequestObservation, GithubPullRequestState,
    observe_issue, observe_pull_request,
};
use crate::model::{Claim, SCHEMA_VERSION};
use crate::observation::{
    DiffSummary, LocalProblem, LocalProblemCode, ObservationError, observe_repository,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubState {
    Available,
    Unavailable,
    Unverified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrSummary {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub base: String,
    pub state: PrState,
    pub draft: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StackWarning {
    pub code: String,
    pub issue: NonZeroU64,
    pub related_issue: Option<NonZeroU64>,
    pub related_claim_id: Option<Uuid>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClaimStatus {
    pub claim: Claim,
    pub pr: Option<PrSummary>,
    pub github_state: GithubState,
    pub diff: DiffSummary,
    pub overlaps: Vec<DiffOverlap>,
    pub scope_warnings: Vec<ScopeWarning>,
    pub stack_warnings: Vec<StackWarning>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct StatusReport {
    pub claims: Vec<ClaimStatus>,
    pub problems: Vec<LocalProblem>,
}

impl StatusReport {
    pub fn has_warnings(&self) -> bool {
        !self.problems.is_empty()
            || self.claims.iter().any(|status| {
                !status.scope_warnings.is_empty()
                    || !status.stack_warnings.is_empty()
                    || status.overlaps.iter().any(|overlap| {
                        matches!(
                            overlap.severity,
                            OverlapSeverity::Warning | OverlapSeverity::Blocking
                        )
                    })
            })
    }
}

#[derive(Debug, Serialize)]
pub struct StatusDocument<'a> {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub claims: &'a [ClaimStatus],
    pub problems: &'a [LocalProblem],
}

pub fn status_document(report: &StatusReport) -> StatusDocument<'_> {
    StatusDocument {
        schema_version: SCHEMA_VERSION,
        command: "status",
        status: if report.has_warnings() {
            "warning"
        } else {
            "success"
        },
        claims: &report.claims,
        problems: &report.problems,
    }
}

pub fn get_status<R: CommandRunner>(runner: &R, cwd: &Path) -> Result<StatusReport, StatusError> {
    let common_dir = RepositoryContext::discover_common_dir_with_runner(runner, cwd)?;
    let config = Config::load(&common_dir)?;
    let repository = RepositoryContext::discover_with_runner(runner, cwd, &config.base_remote)?;
    let observation = observe_repository(runner, cwd)?;
    let replacements = observation
        .claims
        .iter()
        .map(|observed| {
            (
                observed.claim.worktree.to_string_lossy().into_owned(),
                shortened_worktree(&observed.claim.worktree),
            )
        })
        .collect::<Vec<_>>();
    let claims = observation
        .claims
        .into_iter()
        .map(|observed| -> Result<ClaimStatus, StatusError> {
            let mut claim = observed.claim;
            let mut pr = None;
            let mut github_state = GithubState::Unverified;
            let mut stack_warnings = Vec::new();
            if repository.is_github_remote() {
                let issue = observe_issue(
                    runner,
                    &repository.main_worktree,
                    &repository.repository,
                    claim.issue,
                )?;
                let pull_request = observe_pull_request(
                    runner,
                    &repository.main_worktree,
                    &repository.repository,
                    &claim.branch,
                )?;
                github_state = if matches!(&issue, GithubIssueObservation::Unavailable)
                    || matches!(
                        &pull_request,
                        GithubPullRequestObservation::Unavailable
                    )
                {
                    GithubState::Unavailable
                } else {
                    GithubState::Available
                };
                match issue {
                    GithubIssueObservation::Available(issue) => {
                        if claim.title.is_none() {
                            claim.title = Some(issue.title);
                        }
                    }
                    GithubIssueObservation::NotFound => stack_warnings.push(StackWarning {
                        code: "github.issue_not_found".to_owned(),
                        issue: claim.issue,
                        related_issue: None,
                        related_claim_id: None,
                        message: format!(
                            "GitHub issue #{} does not exist or is not reachable in this repository",
                            claim.issue
                        ),
                    }),
                    GithubIssueObservation::Unavailable => {}
                }
                if let GithubPullRequestObservation::Available(Some(observed_pr)) = pull_request {
                    if observed_pr.base != claim.base_ref {
                        stack_warnings.push(StackWarning {
                            code: "github.pr_base_mismatch".to_owned(),
                            issue: claim.issue,
                            related_issue: claim.base_issue,
                            related_claim_id: claim.base_claim_id,
                            message: format!(
                                "pull request #{} targets {:?}, expected {:?}",
                                observed_pr.number, observed_pr.base, claim.base_ref
                            ),
                        });
                    }
                    pr = Some(PrSummary {
                        number: observed_pr.number,
                        url: observed_pr.url,
                        head: observed_pr.head,
                        base: observed_pr.base,
                        state: match observed_pr.state {
                            GithubPullRequestState::Open => PrState::Open,
                            GithubPullRequestState::Closed => PrState::Closed,
                            GithubPullRequestState::Merged => PrState::Merged,
                        },
                        draft: observed_pr.draft,
                    });
                }
            }
            if config.redact_paths_in_json {
                claim.worktree = PathBuf::from(shortened_worktree(&claim.worktree));
            }
            Ok(ClaimStatus {
                claim,
                pr,
                github_state,
                diff: observed.diff.unwrap_or_default(),
                overlaps: observed.overlaps,
                scope_warnings: observed.scope_warnings,
                stack_warnings,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let problems = observation
        .problems
        .into_iter()
        .map(|mut problem| {
            if config.redact_paths_in_json {
                problem.message = redact_message(problem.message, &replacements);
            }
            problem
        })
        .collect();
    Ok(StatusReport { claims, problems })
}

pub fn render_status_human(report: &StatusReport) -> String {
    render_status(report, false)
}

pub fn render_status_human_colored(report: &StatusReport) -> String {
    render_status(report, true)
}

fn render_status(report: &StatusReport, color: bool) -> String {
    if report.claims.is_empty() {
        let mut output = String::from("No active claims.\n");
        append_problems(&mut output, report, color);
        return output;
    }

    let mut output = format!("Active claims: {}\n", report.claims.len());
    for status in &report.claims {
        let title = status.claim.title.as_deref().unwrap_or("(untitled)");
        let worktree = shortened_worktree(&status.claim.worktree);
        let base = format!(
            "{}/{}@{}",
            status.claim.base_remote,
            status.claim.base_ref,
            short_text(&status.claim.base_sha, 8)
        );
        let changes = if report.problems.iter().any(|problem| {
            problem.claim_id == Some(status.claim.claim_id)
                && matches!(
                    problem.code,
                    LocalProblemCode::MissingBase | LocalProblemCode::MissingBranch
                )
        }) {
            "unavailable".to_owned()
        } else {
            format!(
                "{} changed, {} untracked",
                status.diff.changed_paths.len(),
                status.diff.untracked_paths.len()
            )
        };
        let overlaps = relationship_summary(&status.overlaps);
        let pr = status
            .pr
            .as_ref()
            .map_or_else(|| "-".to_owned(), |pr| format!("#{}", pr.number));
        let local = local_summary(status, &report.problems);
        let warning = claim_has_warnings(status, &report.problems);
        let marker = paint(
            color,
            if warning { "33" } else { "32" },
            if warning { "!" } else { "●" },
        );
        output.push_str(&format!(
            "\n{marker} #{} {}  {}\n",
            status.claim.issue,
            &status.claim.claim_id.to_string()[..8],
            title,
        ));
        append_field(&mut output, color, "Branch", &status.claim.branch);
        append_field(&mut output, color, "Worktree", &worktree);
        append_field(&mut output, color, "Base", &base);
        append_field(&mut output, color, "Changes", &changes);
        append_field(
            &mut output,
            color,
            "Overlaps",
            if overlaps == "-" {
                "none (diff-based)"
            } else {
                &overlaps
            },
        );
        append_field(
            &mut output,
            color,
            "Scope",
            &declared_scope_summary(status.claim.declared_scope.as_ref()),
        );
        append_field(&mut output, color, "Pull request", &pr);
        append_field(
            &mut output,
            color,
            "GitHub",
            github_state_name(status.github_state),
        );
        append_field(&mut output, color, "Local", &local);
    }
    append_problems(&mut output, report, color);
    output
}

fn declared_scope_summary(scope: Option<&crate::model::DeclaredScope>) -> String {
    let Some(scope) = scope else {
        return "none".to_owned();
    };
    let mut parts = Vec::new();
    if !scope.allowed_paths.is_empty() {
        parts.push(format!("allow: {}", scope.allowed_paths.join(", ")));
    }
    if !scope.disallowed_paths.is_empty() {
        parts.push(format!("deny: {}", scope.disallowed_paths.join(", ")));
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join("; ")
    }
}

fn claim_has_warnings(status: &ClaimStatus, problems: &[LocalProblem]) -> bool {
    !status.scope_warnings.is_empty()
        || !status.stack_warnings.is_empty()
        || problems
            .iter()
            .any(|problem| problem.claim_id == Some(status.claim.claim_id))
        || status.overlaps.iter().any(|overlap| {
            matches!(
                overlap.severity,
                OverlapSeverity::Warning | OverlapSeverity::Blocking
            )
        })
}

fn append_field(output: &mut String, color: bool, label: &str, value: &str) {
    output.push_str(&format!(
        "  {}{}{}\n",
        paint(color, "2", label),
        " ".repeat(14usize.saturating_sub(label.chars().count())),
        value
    ));
}

fn relationship_summary(overlaps: &[DiffOverlap]) -> String {
    let mut relationships = BTreeMap::<OverlapRelationship, BTreeSet<Uuid>>::new();
    for overlap in overlaps {
        relationships
            .entry(overlap.relationship)
            .or_default()
            .insert(overlap.with_claim_id);
    }
    if relationships.is_empty() {
        return "-".to_owned();
    }
    relationships
        .into_iter()
        .map(|(relationship, claims)| {
            format!("{}:{}", relationship_name(relationship), claims.len())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn local_summary(status: &ClaimStatus, problems: &[LocalProblem]) -> String {
    let mut hints = BTreeSet::new();
    if !status.scope_warnings.is_empty() {
        hints.insert(format!("scope:{}", status.scope_warnings.len()));
    }
    for problem in problems
        .iter()
        .filter(|problem| problem.claim_id == Some(status.claim.claim_id))
    {
        hints.insert(problem_code(problem.code).to_owned());
    }
    if hints.is_empty() {
        "ok".to_owned()
    } else {
        hints.into_iter().collect::<Vec<_>>().join(",")
    }
}

fn append_problems(output: &mut String, report: &StatusReport, color: bool) {
    if report.problems.is_empty() {
        return;
    }
    output.push_str("\nProblems:\n");
    for problem in &report.problems {
        let issue = problem
            .issue
            .map_or_else(String::new, |issue| format!(" #{issue}"));
        let message = redact_message(
            problem.message.clone(),
            &report
                .claims
                .iter()
                .map(|status| {
                    (
                        status.claim.worktree.to_string_lossy().into_owned(),
                        shortened_worktree(&status.claim.worktree),
                    )
                })
                .collect::<Vec<_>>(),
        );
        output.push_str(&format!(
            "{} {}{}: {}\n",
            paint(color, "31", "✗"),
            problem_code(problem.code),
            issue,
            message
        ));
    }
}

fn paint(color: bool, code: &str, value: &str) -> String {
    if color {
        format!("\u{1b}[{code}m{value}\u{1b}[0m")
    } else {
        value.to_owned()
    }
}

fn redact_message(mut message: String, replacements: &[(String, String)]) -> String {
    for (absolute, shortened) in replacements {
        message = message.replace(
            &format!("{:?}", Path::new(absolute)),
            &format!("{shortened:?}"),
        );
        message = message.replace(absolute, shortened);
    }
    message
}

fn shortened_worktree(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.starts_with("…/") {
        return text.into_owned();
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "…".to_owned(), |name| format!("…/{name}"))
}

fn short_text(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

fn relationship_name(value: OverlapRelationship) -> &'static str {
    match value {
        OverlapRelationship::Sibling => "sibling",
        OverlapRelationship::Unrelated => "unrelated",
        OverlapRelationship::Ancestor => "ancestor",
        OverlapRelationship::Descendant => "descendant",
        OverlapRelationship::Consolidation => "consolidation",
    }
}

fn github_state_name(value: GithubState) -> &'static str {
    match value {
        GithubState::Available => "available",
        GithubState::Unavailable => "unavailable",
        GithubState::Unverified => "unverified",
    }
}

fn problem_code(value: LocalProblemCode) -> &'static str {
    match value {
        LocalProblemCode::MissingBase => "missing_base",
        LocalProblemCode::MissingBranch => "missing_branch",
        LocalProblemCode::MissingWorktree => "missing_worktree",
        LocalProblemCode::WorktreeMismatch => "worktree_mismatch",
        LocalProblemCode::AbandonedOperation => "abandoned_operation",
    }
}

#[derive(Debug, Error)]
pub enum StatusError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Observation(#[from] ObservationError),
    #[error(transparent)]
    Github(#[from] GithubIssueError),
}

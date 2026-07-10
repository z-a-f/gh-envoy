use std::collections::BTreeSet;
use std::ffi::OsString;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::command::{CommandOutput, CommandRunner, RunnerError, text_from_utf8_output};
use crate::config::{Config, ConfigError};
use crate::conflict::{ConflictError, DiffOverlap, ScopeWarning, analyze_claims};
use crate::git::{CliCommandError, GitCli, RepositoryContext, RepositoryError};
use crate::model::{Claim, OperationRecord};
use crate::store::{Store, StoreError};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DiffSummary {
    pub changed_paths: Vec<String>,
    pub added_paths: Vec<String>,
    pub modified_paths: Vec<String>,
    pub deleted_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClaimObservation {
    pub claim: Claim,
    pub diff: Option<DiffSummary>,
    pub overlaps: Vec<DiffOverlap>,
    pub scope_warnings: Vec<ScopeWarning>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProblemCode {
    MissingBase,
    MissingBranch,
    MissingWorktree,
    WorktreeMismatch,
    AbandonedOperation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LocalProblem {
    pub code: LocalProblemCode,
    pub issue: Option<NonZeroU64>,
    pub claim_id: Option<Uuid>,
    pub operation_id: Option<Uuid>,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct LocalObservation {
    pub claims: Vec<ClaimObservation>,
    pub operations: Vec<OperationRecord>,
    pub problems: Vec<LocalProblem>,
}

pub fn observe_repository<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
) -> Result<LocalObservation, ObservationError> {
    let common_dir = RepositoryContext::discover_common_dir_with_runner(runner, cwd)?;
    let config = Config::load(&common_dir)?;
    let repository = RepositoryContext::discover_with_runner(runner, cwd, &config.base_remote)?;
    let git = GitCli::new(runner);
    let worktrees = list_worktrees(&git, &repository.main_worktree)?;
    let store = Store::new(repository.store_root());
    let claims = store.active_claims()?;
    let operations = store.list_operations()?;
    let mut problems = operations
        .iter()
        .map(|operation| LocalProblem {
            code: LocalProblemCode::AbandonedOperation,
            issue: Some(operation.issue),
            claim_id: Some(operation.claim_id),
            operation_id: Some(operation.operation_id),
            message: format!(
                "operation {} for issue {} was interrupted in phase {}",
                operation.operation_id,
                operation.issue,
                operation.phase.as_str()
            ),
        })
        .collect::<Vec<_>>();
    let mut observed_claims = Vec::with_capacity(claims.len());

    for claim in claims {
        let base_exists = resolve_commit(&git, &repository.main_worktree, &claim.base_sha)?
            .is_some_and(|resolved| resolved == claim.base_sha);
        if !base_exists {
            problems.push(claim_problem(
                &claim,
                LocalProblemCode::MissingBase,
                format!(
                    "captured base {} does not resolve to a commit",
                    claim.base_sha
                ),
            ));
        }
        let branch_ref = format!("refs/heads/{}", claim.branch);
        let branch_exists = resolve_commit(&git, &repository.main_worktree, &branch_ref)?.is_some();
        if !branch_exists {
            problems.push(claim_problem(
                &claim,
                LocalProblemCode::MissingBranch,
                format!(
                    "local branch {:?} does not resolve to a commit",
                    claim.branch
                ),
            ));
        }

        let usable_worktree = reconcile_worktree(&claim, &worktrees, &mut problems);
        let diff = if base_exists && branch_exists {
            Some(derive_diff(
                &git,
                &repository.main_worktree,
                &claim,
                usable_worktree,
            )?)
        } else {
            None
        };
        observed_claims.push(ClaimObservation {
            claim,
            diff,
            overlaps: Vec::new(),
            scope_warnings: Vec::new(),
        });
    }

    analyze_claims(&mut observed_claims, &config.risk_paths)?;

    Ok(LocalObservation {
        claims: observed_claims,
        operations,
        problems,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
}

fn list_worktrees<R: CommandRunner>(
    git: &GitCli<'_, R>,
    cwd: &Path,
) -> Result<Vec<WorktreeEntry>, ObservationError> {
    let output = git.run(cwd, ["worktree", "list", "--porcelain", "-z"])?;
    let text = nul_text(&output.stdout, "git worktree list --porcelain -z")?;
    let mut entries = Vec::new();
    let mut path = None;
    let mut branch = None;
    for field in text.split('\0') {
        if field.is_empty() {
            if let Some(path) = path.take() {
                entries.push(WorktreeEntry { path, branch });
                branch = None;
            }
        } else if let Some(value) = field.strip_prefix("worktree ") {
            if path.is_some() {
                return Err(ObservationError::InvalidGitOutput(
                    "git worktree list reported an unterminated entry".to_owned(),
                ));
            }
            path = Some(PathBuf::from(value));
        } else if let Some(value) = field.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_owned());
        }
    }
    if path.is_some() {
        return Err(ObservationError::InvalidGitOutput(
            "git worktree list reported an unterminated entry".to_owned(),
        ));
    }
    if entries.is_empty() {
        return Err(ObservationError::InvalidGitOutput(
            "git worktree list did not report a worktree".to_owned(),
        ));
    }
    Ok(entries)
}

fn reconcile_worktree<'a>(
    claim: &Claim,
    entries: &'a [WorktreeEntry],
    problems: &mut Vec<LocalProblem>,
) -> Option<&'a Path> {
    let canonical_claim = match claim.worktree.canonicalize() {
        Ok(path) => path,
        Err(_) => {
            problems.push(claim_problem(
                claim,
                LocalProblemCode::MissingWorktree,
                format!("claimed worktree {:?} does not exist", claim.worktree),
            ));
            return None;
        }
    };
    let matching = entries.iter().find(|entry| {
        entry
            .path
            .canonicalize()
            .is_ok_and(|path| path == canonical_claim)
    });
    let Some(entry) = matching else {
        problems.push(claim_problem(
            claim,
            LocalProblemCode::WorktreeMismatch,
            format!(
                "claimed worktree {:?} is not registered with Git",
                claim.worktree
            ),
        ));
        return None;
    };
    if entry.branch.as_deref() != Some(claim.branch.as_str()) {
        problems.push(claim_problem(
            claim,
            LocalProblemCode::WorktreeMismatch,
            format!(
                "claimed worktree {:?} is not attached to branch {:?}",
                claim.worktree, claim.branch
            ),
        ));
        return None;
    }
    Some(entry.path.as_path())
}

fn resolve_commit<R: CommandRunner>(
    git: &GitCli<'_, R>,
    cwd: &Path,
    object: &str,
) -> Result<Option<String>, ObservationError> {
    let object = format!("{object}^{{commit}}");
    let output = git.attempt(cwd, ["rev-parse", "--verify", "--quiet", object.as_str()])?;
    if output.exit_code != Some(0) {
        return Ok(None);
    }
    let resolved = text_from_utf8_output(&output.stdout, "git rev-parse --verify")
        .map_err(ObservationError::InvalidGitOutput)?;
    if resolved.is_empty() {
        return Err(ObservationError::InvalidGitOutput(
            "git rev-parse --verify returned an empty object ID".to_owned(),
        ));
    }
    Ok(Some(resolved.to_owned()))
}

fn derive_diff<R: CommandRunner>(
    git: &GitCli<'_, R>,
    main_worktree: &Path,
    claim: &Claim,
    worktree: Option<&Path>,
) -> Result<DiffSummary, ObservationError> {
    let (cwd, endpoint) = if let Some(worktree) = worktree {
        (worktree, None)
    } else {
        (main_worktree, Some(format!("refs/heads/{}", claim.branch)))
    };
    let mut args = vec![
        OsString::from("diff"),
        OsString::from("--no-renames"),
        OsString::from("--name-status"),
        OsString::from("-z"),
        OsString::from(&claim.base_sha),
    ];
    if let Some(endpoint) = endpoint {
        args.push(OsString::from(endpoint));
    }
    args.push(OsString::from("--"));
    let tracked = git.run(cwd, args)?;
    let mut summary = parse_name_status(&tracked)?;
    if worktree.is_some() {
        let output = git.run(
            cwd,
            ["ls-files", "--others", "--exclude-standard", "-z", "--"],
        )?;
        summary.untracked_paths = parse_paths(&output, "git ls-files --others")?;
    }
    Ok(summary)
}

fn parse_name_status(output: &CommandOutput) -> Result<DiffSummary, ObservationError> {
    let text = nul_text(&output.stdout, "git diff --name-status -z")?;
    let fields = text
        .split('\0')
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() % 2 != 0 {
        return Err(ObservationError::InvalidGitOutput(
            "git diff --name-status -z returned an incomplete record".to_owned(),
        ));
    }
    let mut added = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    for record in fields.chunks_exact(2) {
        let status = record[0];
        let path = record[1].to_owned();
        match status.as_bytes().first() {
            Some(b'A') => {
                added.insert(path);
            }
            Some(b'D') => {
                deleted.insert(path);
            }
            Some(_) => {
                modified.insert(path);
            }
            None => {
                return Err(ObservationError::InvalidGitOutput(
                    "git diff returned an empty status".to_owned(),
                ));
            }
        }
    }
    let changed_paths = added
        .iter()
        .chain(modified.iter())
        .chain(deleted.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(DiffSummary {
        changed_paths,
        added_paths: added.into_iter().collect(),
        modified_paths: modified.into_iter().collect(),
        deleted_paths: deleted.into_iter().collect(),
        untracked_paths: Vec::new(),
    })
}

fn parse_paths(output: &CommandOutput, context: &str) -> Result<Vec<String>, ObservationError> {
    let text = nul_text(&output.stdout, context)?;
    let mut paths = text
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn nul_text<'a>(bytes: &'a [u8], context: &str) -> Result<&'a str, ObservationError> {
    std::str::from_utf8(bytes).map_err(|error| {
        ObservationError::InvalidGitOutput(format!("{context} returned non-UTF-8 output: {error}"))
    })
}

fn claim_problem(claim: &Claim, code: LocalProblemCode, message: String) -> LocalProblem {
    LocalProblem {
        code,
        issue: Some(claim.issue),
        claim_id: Some(claim.claim_id),
        operation_id: None,
        message,
    }
}

#[derive(Debug, Error)]
pub enum ObservationError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Conflict(#[from] ConflictError),
    #[error(transparent)]
    Git(#[from] CliCommandError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("invalid Git output: {0}")]
    InvalidGitOutput(String),
}

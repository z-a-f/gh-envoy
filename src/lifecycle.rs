use std::ffi::OsString;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use chrono::Utc;
use thiserror::Error;
use uuid::Uuid;

use crate::command::{CommandRunner, RunnerError, text_from_utf8_output};
use crate::config::{Config, ConfigError};
use crate::git::{CliCommandError, GitCli, RepositoryContext, RepositoryError, canonical_existing};
use crate::model::{
    Claim, DeclaredScope, OperationKind, OperationPhase, OperationRecord, ReleaseMarker,
    ReleaseReason, ReleaseReport, SCHEMA_VERSION,
};
use crate::store::{LockedStore, Store, StoreError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimOutcome {
    pub claim: Claim,
    pub warnings: Vec<String>,
}

pub fn claim_issue<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
    issue: NonZeroU64,
) -> Result<ClaimOutcome, LifecycleError> {
    let common_dir = RepositoryContext::discover_common_dir_with_runner(runner, cwd)?;
    let config = Config::load(&common_dir)?;
    let repository = RepositoryContext::discover_with_runner(runner, cwd, &config.base_remote)?;
    let git = GitCli::new(runner);
    let base = resolve_base(&git, &repository, &config)?;
    let claim_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let suffix = short_id(claim_id);
    let branch = format!("envoy/issue-{}-{suffix}", issue.get());
    let repository_name = repository
        .repository
        .rsplit_once('/')
        .map_or(repository.repository.as_str(), |(_, name)| name);
    let worktree_root = match &config.worktree_root {
        Some(root) => root.clone(),
        None => repository
            .main_worktree
            .parent()
            .ok_or_else(|| LifecycleError::InvalidState("main worktree has no parent".to_owned()))?
            .to_path_buf(),
    };
    let worktree = worktree_root.join(format!("{repository_name}-issue-{}-{suffix}", issue.get()));
    if !worktree.is_absolute() {
        return Err(LifecycleError::InvalidState(
            "generated worktree path is not absolute".to_owned(),
        ));
    }

    let store = Store::new(repository.store_root());
    let locked = store.lock()?;
    reserve(&locked, issue, &branch, &worktree)?;

    let mut operation = OperationRecord {
        schema_version: SCHEMA_VERSION.to_owned(),
        operation_id,
        kind: OperationKind::Claim,
        claim_id,
        issue,
        branch: branch.clone(),
        worktree: worktree.clone(),
        phase: OperationPhase::Reserved,
        started_at: Utc::now(),
    };
    locked.write_operation(&operation)?;

    if let Err(error) = git.run(
        &repository.main_worktree,
        ["branch", branch.as_str(), base.sha.as_str()],
    ) {
        locked.remove_operation(operation_id)?;
        return Err(error.into());
    }
    operation.phase = OperationPhase::BranchCreated;
    if let Err(error) = locked.write_operation(&operation) {
        return rollback_failure(
            &git,
            &repository.main_worktree,
            &locked,
            &mut operation,
            false,
            error.into(),
        );
    }

    if let Some(parent) = worktree.parent()
        && let Err(source) = fs::create_dir_all(parent)
    {
        return rollback_failure(
            &git,
            &repository.main_worktree,
            &locked,
            &mut operation,
            false,
            LifecycleError::Io {
                action: "create worktree root",
                path: parent.to_path_buf(),
                source,
            },
        );
    }
    if let Err(error) = git.run(
        &repository.main_worktree,
        [
            OsString::from("worktree"),
            OsString::from("add"),
            worktree.as_os_str().to_owned(),
            OsString::from(&branch),
        ],
    ) {
        return rollback_failure(
            &git,
            &repository.main_worktree,
            &locked,
            &mut operation,
            false,
            error.into(),
        );
    }
    operation.phase = OperationPhase::WorktreeCreated;
    if let Err(error) = locked.write_operation(&operation) {
        return rollback_failure(
            &git,
            &repository.main_worktree,
            &locked,
            &mut operation,
            true,
            error.into(),
        );
    }

    let canonical_worktree = match canonical_worktree(&git, &repository.main_worktree, &branch) {
        Ok(path) => path,
        Err(error) => {
            return rollback_failure(
                &git,
                &repository.main_worktree,
                &locked,
                &mut operation,
                true,
                error,
            );
        }
    };
    operation.worktree = canonical_worktree.clone();
    let claim = Claim {
        schema_version: SCHEMA_VERSION.to_owned(),
        claim_id,
        repo: repository.repository,
        issue,
        title: None,
        branch,
        worktree: canonical_worktree,
        base_remote: config.base_remote,
        base_ref: base.reference,
        base_sha: base.sha,
        base_issue: None,
        base_claim_id: None,
        wait_for: Vec::new(),
        declared_scope: Some(DeclaredScope::default()),
        note: None,
        created_at: Utc::now(),
    };
    if let Err(error) = locked.create_claim(&claim) {
        return rollback_failure(
            &git,
            &repository.main_worktree,
            &locked,
            &mut operation,
            true,
            error.into(),
        );
    }

    operation.phase = OperationPhase::ClaimCommitted;
    locked.write_operation(&operation)?;
    locked.remove_operation(operation_id)?;
    Ok(ClaimOutcome {
        claim,
        warnings: base.warnings,
    })
}

pub fn release_claim<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
    issue: NonZeroU64,
    reason: ReleaseReason,
) -> Result<ReleaseReport, LifecycleError> {
    let common_dir = RepositoryContext::discover_common_dir_with_runner(runner, cwd)?;
    let config = Config::load(&common_dir)?;
    let repository = RepositoryContext::discover_with_runner(runner, cwd, &config.base_remote)?;
    let store = Store::new(repository.store_root());
    let locked = store.lock()?;
    let mut active = locked
        .active_claims()?
        .into_iter()
        .filter(|claim| claim.issue == issue)
        .collect::<Vec<_>>();
    if active.len() > 1 {
        return Err(LifecycleError::InvalidState(format!(
            "issue {} has multiple active claim generations",
            issue.get()
        )));
    }
    if let Some(claim) = active.pop() {
        let marker = ReleaseMarker {
            schema_version: SCHEMA_VERSION.to_owned(),
            repo: claim.repo,
            issue,
            claim_id: claim.claim_id,
            reason,
            released_at: Utc::now(),
        };
        locked.create_release(&marker)?;
        return Ok(ReleaseReport::marker_only(issue, claim.claim_id, false));
    }

    let claims = locked.list_claims(issue)?;
    let latest = claims.last().ok_or(LifecycleError::NoClaim(issue))?;
    let releases = locked.list_releases(issue)?;
    if releases
        .iter()
        .any(|release| release.claim_id == latest.claim_id)
    {
        Ok(ReleaseReport::marker_only(issue, latest.claim_id, true))
    } else {
        Err(LifecycleError::InvalidState(format!(
            "issue {} has no active claim but its latest generation is not released",
            issue.get()
        )))
    }
}

fn reserve(
    store: &LockedStore<'_>,
    issue: NonZeroU64,
    branch: &str,
    worktree: &Path,
) -> Result<(), LifecycleError> {
    let active = store.active_claims()?;
    if let Some(claim) = active.iter().find(|claim| claim.issue == issue) {
        return Err(LifecycleError::AlreadyClaimed {
            issue,
            claim_id: claim.claim_id,
        });
    }
    if let Some(claim) = active.iter().find(|claim| claim.branch == branch) {
        return Err(LifecycleError::Reserved(format!(
            "branch {branch:?} is already owned by issue {}",
            claim.issue
        )));
    }
    if let Some(claim) = active.iter().find(|claim| claim.worktree == worktree) {
        return Err(LifecycleError::Reserved(format!(
            "worktree {worktree:?} is already owned by issue {}",
            claim.issue
        )));
    }
    Ok(())
}

fn rollback_failure<R: CommandRunner, T>(
    git: &GitCli<'_, R>,
    repository: &Path,
    store: &LockedStore<'_>,
    operation: &mut OperationRecord,
    worktree_created: bool,
    original: LifecycleError,
) -> Result<T, LifecycleError> {
    let mut failures = Vec::new();
    if worktree_created
        && let Err(error) = git.run(
            repository,
            [
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                operation.worktree.as_os_str().to_owned(),
            ],
        )
    {
        failures.push(error.to_string());
    }
    if let Err(error) = git.run(repository, ["branch", "-D", operation.branch.as_str()]) {
        failures.push(error.to_string());
    }
    if failures.is_empty() {
        store.remove_operation(operation.operation_id)?;
        Err(original)
    } else {
        operation.phase = OperationPhase::CleanupPending;
        store.write_operation(operation)?;
        Err(LifecycleError::Rollback {
            original: original.to_string(),
            cleanup: failures.join("; "),
        })
    }
}

struct ResolvedBase {
    reference: String,
    sha: String,
    warnings: Vec<String>,
}

fn resolve_base<R: CommandRunner>(
    git: &GitCli<'_, R>,
    repository: &RepositoryContext,
    config: &Config,
) -> Result<ResolvedBase, LifecycleError> {
    let base_ref = match &config.default_base_ref {
        Some(reference) => reference.clone(),
        None => remote_head(git, &repository.current_worktree, &config.base_remote)
            .unwrap_or_else(|| "main".to_owned()),
    };
    let fetch = git.attempt(
        &repository.current_worktree,
        ["fetch", config.base_remote.as_str(), base_ref.as_str()],
    )?;
    let remote_ref = format!(
        "refs/remotes/{}/{}^{{commit}}",
        config.base_remote, base_ref
    );
    let remote_sha = resolve_optional(git, &repository.current_worktree, &remote_ref)?;
    let mut warnings = Vec::new();
    if fetch.exit_code != Some(0) && remote_sha.is_some() {
        warnings.push(format!(
            "could not refresh {}/{}; using the existing remote-tracking branch",
            config.base_remote, base_ref
        ));
    }
    let sha = if let Some(sha) = remote_sha {
        sha
    } else {
        let local_ref = format!("refs/heads/{base_ref}^{{commit}}");
        let sha =
            resolve_optional(git, &repository.current_worktree, &local_ref)?.ok_or_else(|| {
                LifecycleError::BaseUnavailable {
                    remote: config.base_remote.clone(),
                    reference: base_ref.clone(),
                }
            })?;
        warnings.push(format!(
            "base {}/{} is unverified; using local branch {base_ref} at {sha}",
            config.base_remote, base_ref
        ));
        sha
    };
    Ok(ResolvedBase {
        reference: base_ref,
        sha,
        warnings,
    })
}

fn remote_head<R: CommandRunner>(git: &GitCli<'_, R>, cwd: &Path, remote: &str) -> Option<String> {
    let symbolic = format!("refs/remotes/{remote}/HEAD");
    let output = git
        .attempt(cwd, ["symbolic-ref", "--quiet", "--short", &symbolic])
        .ok()?;
    if output.exit_code != Some(0) {
        return None;
    }
    let value = text_from_utf8_output(&output.stdout, "git symbolic-ref").ok()?;
    value.strip_prefix(&format!("{remote}/")).map(str::to_owned)
}

fn resolve_optional<R: CommandRunner>(
    git: &GitCli<'_, R>,
    cwd: &Path,
    reference: &str,
) -> Result<Option<String>, LifecycleError> {
    let output = git.attempt(cwd, ["rev-parse", "--verify", reference])?;
    if output.exit_code != Some(0) {
        return Ok(None);
    }
    let value = text_from_utf8_output(&output.stdout, "git rev-parse --verify")
        .map_err(LifecycleError::InvalidState)?;
    if value.is_empty() {
        return Err(LifecycleError::InvalidState(
            "git resolved an empty base SHA".to_owned(),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn canonical_worktree<R: CommandRunner>(
    git: &GitCli<'_, R>,
    cwd: &Path,
    branch: &str,
) -> Result<PathBuf, LifecycleError> {
    let output = git.run(cwd, ["worktree", "list", "--porcelain"])?;
    let text = text_from_utf8_output(&output.stdout, "git worktree list --porcelain")
        .map_err(LifecycleError::InvalidState)?;
    let expected_branch = format!("refs/heads/{branch}");
    let mut path = None;
    for block in text.split("\n\n") {
        let mut candidate = None;
        let mut candidate_branch = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                candidate = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("branch ") {
                candidate_branch = Some(value);
            }
        }
        if candidate_branch == Some(expected_branch.as_str()) {
            path = candidate;
            break;
        }
    }
    let path = path.ok_or_else(|| {
        LifecycleError::InvalidState(format!(
            "git worktree list did not report branch {branch:?}"
        ))
    })?;
    canonical_existing(path).map_err(LifecycleError::Repository)
}

fn short_id(id: Uuid) -> String {
    id.simple().to_string()[..8].to_owned()
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Git(#[from] CliCommandError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("issue {issue} is already claimed by generation {claim_id}")]
    AlreadyClaimed { issue: NonZeroU64, claim_id: Uuid },
    #[error("claim reservation refused: {0}")]
    Reserved(String),
    #[error("issue {0} has no claim to release")]
    NoClaim(NonZeroU64),
    #[error("could not resolve base {remote}/{reference} from a remote-tracking or local branch")]
    BaseUnavailable { remote: String, reference: String },
    #[error("invalid lifecycle state: {0}")]
    InvalidState(String),
    #[error("claim failed: {original}; rollback is incomplete: {cleanup}")]
    Rollback { original: String, cleanup: String },
    #[error("failed to {action} at {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl LifecycleError {
    pub fn is_refusal(&self) -> bool {
        matches!(
            self,
            Self::AlreadyClaimed { .. } | Self::Reserved(_) | Self::NoClaim(_)
        )
    }
}

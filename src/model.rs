use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub const SCHEMA_VERSION: &str = "0.1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub schema_version: String,
    pub claim_id: Uuid,
    pub repo: String,
    pub issue: NonZeroU64,
    pub title: Option<String>,
    pub branch: String,
    pub worktree: PathBuf,
    pub base_remote: String,
    pub base_ref: String,
    pub base_sha: String,
    pub base_issue: Option<NonZeroU64>,
    pub base_claim_id: Option<Uuid>,
    #[serde(default)]
    pub wait_for: Vec<WaitForRef>,
    pub declared_scope: Option<DeclaredScope>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Claim {
    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        decode_and_validate(value)
    }

    pub fn to_value(&self) -> Result<Value, ModelError> {
        self.validate()?;
        serde_json::to_value(self).map_err(ModelError::Json)
    }
}

impl Validate for Claim {
    fn validate(&self) -> Result<(), ModelError> {
        validate_version(&self.schema_version)?;
        validate_absolute_path(&self.worktree, "claim worktree")?;
        validate_non_empty("repo", &self.repo)?;
        validate_non_empty("branch", &self.branch)?;
        validate_non_empty("base_remote", &self.base_remote)?;
        validate_non_empty("base_ref", &self.base_ref)?;
        validate_non_empty("base_sha", &self.base_sha)?;
        if self.base_issue.is_some() != self.base_claim_id.is_some() {
            return Err(ModelError::Invalid(
                "base_issue and base_claim_id must either both be set or both be null".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitForRef {
    pub issue: NonZeroU64,
    pub claim_id: Option<Uuid>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredScope {
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub disallowed_paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    Merged,
    Closed,
    Abandoned,
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMarker {
    pub schema_version: String,
    pub repo: String,
    pub issue: NonZeroU64,
    pub claim_id: Uuid,
    pub reason: ReleaseReason,
    pub released_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReport {
    pub issue: NonZeroU64,
    pub claim_id: Uuid,
    pub already_released: bool,
    pub worktree_deleted: bool,
    pub branch_deleted: bool,
    pub cleanup_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Interactive,
    Background,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Succeeded,
    Failed,
    StopRequested,
    Stopped,
}

impl RunState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Stopped)
    }

    pub const fn is_active(self) -> bool {
        !self.is_terminal()
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::StopRequested => "stop_requested",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub schema_version: String,
    pub run_id: Uuid,
    pub repo: String,
    pub claim_id: Uuid,
    pub issue: NonZeroU64,
    pub agent: String,
    pub mode: RunMode,
    pub state: RunState,
    pub worktree: PathBuf,
    pub worker_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub prompt_path: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunTransition {
    Start {
        worker_pid: Option<u32>,
        child_pid: u32,
        started_at: DateTime<Utc>,
    },
    SpawnFailed {
        worker_pid: Option<u32>,
        finished_at: DateTime<Utc>,
        error: String,
    },
    Exit {
        exit_code: i32,
        finished_at: DateTime<Utc>,
    },
    RequestStop,
    Stopped {
        finished_at: DateTime<Utc>,
        exit_code: Option<i32>,
    },
}

impl RunRecord {
    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        decode_and_validate(value)
    }

    pub fn to_value(&self) -> Result<Value, ModelError> {
        self.validate()?;
        serde_json::to_value(self).map_err(ModelError::Json)
    }

    pub fn apply_transition(&mut self, transition: RunTransition) -> Result<(), ModelError> {
        let from = self.state;
        match (from, transition) {
            (
                RunState::Queued,
                RunTransition::Start {
                    worker_pid,
                    child_pid,
                    started_at,
                },
            ) => {
                self.state = RunState::Running;
                self.worker_pid = worker_pid;
                self.child_pid = Some(child_pid);
                self.started_at = Some(started_at);
            }
            (
                RunState::Queued,
                RunTransition::SpawnFailed {
                    worker_pid,
                    finished_at,
                    error,
                },
            ) => {
                self.state = RunState::Failed;
                self.worker_pid = worker_pid;
                self.finished_at = Some(finished_at);
                self.error = Some(error);
            }
            (
                RunState::Running,
                RunTransition::Exit {
                    exit_code,
                    finished_at,
                },
            ) => {
                self.state = if exit_code == 0 {
                    RunState::Succeeded
                } else {
                    RunState::Failed
                };
                self.exit_code = Some(exit_code);
                self.finished_at = Some(finished_at);
            }
            (RunState::Queued | RunState::Running, RunTransition::RequestStop) => {
                self.state = RunState::StopRequested;
            }
            (
                RunState::StopRequested,
                RunTransition::Stopped {
                    finished_at,
                    exit_code,
                },
            ) => {
                self.state = RunState::Stopped;
                self.finished_at = Some(finished_at);
                self.exit_code = exit_code;
            }
            _ => {
                return Err(ModelError::Invalid(format!(
                    "run cannot transition from {} using the requested event",
                    from.as_str()
                )));
            }
        }
        self.validate()
    }
}

impl Validate for RunRecord {
    fn validate(&self) -> Result<(), ModelError> {
        validate_version(&self.schema_version)?;
        validate_non_empty("repo", &self.repo)?;
        validate_non_empty("agent", &self.agent)?;
        validate_absolute_path(&self.worktree, "run worktree")?;
        validate_absolute_path(&self.prompt_path, "run prompt_path")?;
        validate_absolute_path(&self.stdout_path, "run stdout_path")?;
        validate_absolute_path(&self.stderr_path, "run stderr_path")?;
        if self.mode == RunMode::Interactive && self.worker_pid.is_some() {
            return Err(ModelError::Invalid(
                "interactive runs cannot have a worker_pid".to_owned(),
            ));
        }
        if self
            .started_at
            .is_some_and(|started| started < self.created_at)
            || self
                .finished_at
                .is_some_and(|finished| finished < self.created_at)
            || self
                .started_at
                .zip(self.finished_at)
                .is_some_and(|(started, finished)| finished < started)
        {
            return Err(ModelError::Invalid(
                "run timestamps are not in lifecycle order".to_owned(),
            ));
        }
        let started_child_pair = self.started_at.is_some() == self.child_pid.is_some();
        if !started_child_pair {
            return Err(ModelError::Invalid(
                "started_at and child_pid must either both be set or both be null".to_owned(),
            ));
        }
        let valid = match self.state {
            RunState::Queued => {
                self.started_at.is_none()
                    && self.finished_at.is_none()
                    && self.exit_code.is_none()
                    && self.error.is_none()
            }
            RunState::Running => {
                self.started_at.is_some()
                    && self.finished_at.is_none()
                    && self.exit_code.is_none()
                    && self.error.is_none()
            }
            RunState::Succeeded => {
                self.started_at.is_some()
                    && self.finished_at.is_some()
                    && self.exit_code == Some(0)
                    && self.error.is_none()
            }
            RunState::Failed => {
                self.finished_at.is_some()
                    && ((self.started_at.is_none()
                        && self.exit_code.is_none()
                        && self
                            .error
                            .as_deref()
                            .is_some_and(|error| !error.trim().is_empty()))
                        || (self.started_at.is_some()
                            && self.exit_code.is_some_and(|code| code != 0)
                            && self.error.is_none()))
            }
            RunState::StopRequested => {
                self.finished_at.is_none() && self.exit_code.is_none() && self.error.is_none()
            }
            RunState::Stopped => {
                self.finished_at.is_some()
                    && self.error.is_none()
                    && (self.started_at.is_some() || self.exit_code.is_none())
            }
        };
        if valid {
            Ok(())
        } else {
            Err(ModelError::Invalid(format!(
                "run fields are inconsistent with state {}",
                self.state.as_str()
            )))
        }
    }
}

impl ReleaseReport {
    pub fn marker_only(issue: NonZeroU64, claim_id: Uuid, already_released: bool) -> Self {
        Self {
            issue,
            claim_id,
            already_released,
            worktree_deleted: false,
            branch_deleted: false,
            cleanup_pending: false,
        }
    }
}

impl ReleaseMarker {
    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        decode_and_validate(value)
    }

    pub fn to_value(&self) -> Result<Value, ModelError> {
        self.validate()?;
        serde_json::to_value(self).map_err(ModelError::Json)
    }
}

impl Validate for ReleaseMarker {
    fn validate(&self) -> Result<(), ModelError> {
        validate_version(&self.schema_version)?;
        validate_non_empty("repo", &self.repo)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Claim,
    Cleanup,
    Run,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Reserved,
    BranchCreated,
    WorktreeCreated,
    ClaimCommitted,
    CleanupPending,
    RunArtifactsCreated,
    RunCommitted,
}

impl OperationPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::BranchCreated => "branch_created",
            Self::WorktreeCreated => "worktree_created",
            Self::ClaimCommitted => "claim_committed",
            Self::CleanupPending => "cleanup_pending",
            Self::RunArtifactsCreated => "run_artifacts_created",
            Self::RunCommitted => "run_committed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub schema_version: String,
    pub operation_id: Uuid,
    pub kind: OperationKind,
    pub claim_id: Uuid,
    pub issue: NonZeroU64,
    pub branch: String,
    pub worktree: PathBuf,
    pub phase: OperationPhase,
    pub started_at: DateTime<Utc>,
}

impl OperationRecord {
    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        decode_and_validate(value)
    }

    pub fn to_value(&self) -> Result<Value, ModelError> {
        self.validate()?;
        serde_json::to_value(self).map_err(ModelError::Json)
    }
}

impl Validate for OperationRecord {
    fn validate(&self) -> Result<(), ModelError> {
        validate_version(&self.schema_version)?;
        validate_absolute_path(&self.worktree, "operation worktree")?;
        validate_non_empty("branch", &self.branch)?;
        let run_phase = matches!(
            self.phase,
            OperationPhase::RunArtifactsCreated | OperationPhase::RunCommitted
        );
        if run_phase != (self.kind == OperationKind::Run)
            && !(self.kind == OperationKind::Run && self.phase == OperationPhase::Reserved)
        {
            return Err(ModelError::Invalid(
                "operation kind and phase describe different workflows".to_owned(),
            ));
        }
        Ok(())
    }
}

pub trait Validate {
    fn validate(&self) -> Result<(), ModelError>;
}

fn decode_and_validate<T>(value: Value) -> Result<T, ModelError>
where
    T: DeserializeOwned + Validate,
{
    let decoded: T = serde_json::from_value(value).map_err(ModelError::Json)?;
    decoded.validate()?;
    Ok(decoded)
}

fn validate_version(version: &str) -> Result<(), ModelError> {
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ModelError::Invalid(format!(
            "unsupported schema_version {version:?}; expected {SCHEMA_VERSION:?}"
        )))
    }
}

fn validate_absolute_path(path: &Path, label: &str) -> Result<(), ModelError> {
    if !path.is_absolute() {
        return Err(ModelError::Invalid(format!(
            "{label} must be an absolute path"
        )));
    }
    if path.to_str().is_none() {
        return Err(ModelError::Invalid(format!("{label} must be valid UTF-8")));
    }
    Ok(())
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), ModelError> {
    if value.trim().is_empty() {
        Err(ModelError::Invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("invalid persisted model: {0}")]
    Invalid(String),
    #[error("invalid persisted JSON: {0}")]
    Json(#[source] serde_json::Error),
}

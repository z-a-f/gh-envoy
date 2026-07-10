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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Reserved,
    BranchCreated,
    WorktreeCreated,
    ClaimCommitted,
    CleanupPending,
}

impl OperationPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::BranchCreated => "branch_created",
            Self::WorktreeCreated => "worktree_created",
            Self::ClaimCommitted => "claim_committed",
            Self::CleanupPending => "cleanup_pending",
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
        validate_non_empty("branch", &self.branch)
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

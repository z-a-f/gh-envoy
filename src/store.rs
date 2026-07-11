use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs4::FileExt;
use serde::Serialize;
use serde_json::Value;
use tempfile::NamedTempFile;
use thiserror::Error;
use uuid::Uuid;

use crate::model::{
    Claim, ModelError, OperationKind, OperationPhase, OperationRecord, ReleaseMarker, RunMode,
    RunRecord, RunTransition, SCHEMA_VERSION, Validate,
};

#[derive(Clone, Copy, Debug)]
pub struct NewRun<'a> {
    pub run_id: Uuid,
    pub agent: &'a str,
    pub mode: RunMode,
    pub prompt: &'a [u8],
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunStoreProblem {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunInventory {
    pub runs: Vec<RunRecord>,
    pub problems: Vec<RunStoreProblem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock(&self) -> Result<LockedStore<'_>, StoreError> {
        create_directory(&self.root)?;
        let lock_path = self.root.join("lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| StoreError::Io {
                action: "open store lock",
                path: lock_path.clone(),
                source,
            })?;
        FileExt::lock(&lock_file).map_err(|source| StoreError::Io {
            action: "acquire store lock",
            path: lock_path,
            source,
        })?;
        let locked = LockedStore {
            store: self,
            _lock_file: lock_file,
        };
        locked.create_layout()?;
        Ok(locked)
    }

    pub fn list_claims(&self, issue: NonZeroU64) -> Result<Vec<Claim>, StoreError> {
        let directory = self.claim_issue_directory(issue);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    action: "list claim generations",
                    path: directory,
                    source,
                });
            }
        };

        let mut claims = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                action: "read claim directory entry",
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let claim = read_claim(&path)?;
            validate_claim_location(&path, issue, &claim)?;
            claims.push(claim);
        }
        claims.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.claim_id.cmp(&right.claim_id))
        });
        Ok(claims)
    }

    pub fn list_releases(&self, issue: NonZeroU64) -> Result<Vec<ReleaseMarker>, StoreError> {
        let directory = self.release_issue_directory(issue);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    action: "list release generations",
                    path: directory,
                    source,
                });
            }
        };

        let mut releases = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                action: "read release directory entry",
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let release = read_release(&path)?;
            validate_release_location(&path, issue, &release)?;
            releases.push(release);
        }
        releases.sort_by(|left, right| {
            left.released_at
                .cmp(&right.released_at)
                .then_with(|| left.claim_id.cmp(&right.claim_id))
        });
        Ok(releases)
    }

    pub fn active_claims(&self) -> Result<Vec<Claim>, StoreError> {
        let claims_root = self.root.join("claims");
        let entries = match fs::read_dir(&claims_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    action: "list claim issues",
                    path: claims_root,
                    source,
                });
            }
        };
        let mut active = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                action: "read claim issue entry",
                path: claims_root.clone(),
                source,
            })?;
            if !entry.path().is_dir() {
                continue;
            }
            let Some(issue) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<NonZeroU64>().ok())
            else {
                continue;
            };
            let releases = self.list_releases(issue)?;
            let released = releases
                .iter()
                .map(|release| release.claim_id)
                .collect::<std::collections::BTreeSet<_>>();
            active.extend(
                self.list_claims(issue)?
                    .into_iter()
                    .filter(|claim| !released.contains(&claim.claim_id)),
            );
        }
        active.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.claim_id.cmp(&right.claim_id))
        });
        Ok(active)
    }

    pub fn all_claims(&self) -> Result<Vec<Claim>, StoreError> {
        let claims_root = self.root.join("claims");
        let entries = match fs::read_dir(&claims_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    action: "list claim issues",
                    path: claims_root,
                    source,
                });
            }
        };
        let mut claims = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                action: "read claim issue entry",
                path: claims_root.clone(),
                source,
            })?;
            if !entry.path().is_dir() {
                continue;
            }
            let Some(issue) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<NonZeroU64>().ok())
            else {
                continue;
            };
            claims.extend(self.list_claims(issue)?);
        }
        claims.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.claim_id.cmp(&right.claim_id))
        });
        Ok(claims)
    }

    pub fn read_operation(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let path = self.operation_path(operation_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(StoreError::Io {
                    action: "read operation record",
                    path,
                    source,
                });
            }
        };
        let value = serde_json::from_slice(&bytes).map_err(|source| StoreError::Json {
            path: path.clone(),
            source,
        })?;
        let operation = OperationRecord::from_value(value)?;
        if operation.operation_id != operation_id {
            return Err(StoreError::LocationMismatch {
                path,
                message: "operation_id does not match its filename".to_owned(),
            });
        }
        Ok(Some(operation))
    }

    pub fn list_operations(&self) -> Result<Vec<OperationRecord>, StoreError> {
        let directory = self.root.join("operations");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    action: "list operation records",
                    path: directory,
                    source,
                });
            }
        };
        let mut operations = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                action: "read operation directory entry",
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let operation = read_operation(&path)?;
            let filename = path.file_stem().and_then(|value| value.to_str());
            if filename != Some(operation.operation_id.to_string().as_str()) {
                return Err(StoreError::LocationMismatch {
                    path,
                    message: "operation_id does not match its filename".to_owned(),
                });
            }
            operations.push(operation);
        }
        operations.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.operation_id.cmp(&right.operation_id))
        });
        Ok(operations)
    }

    pub fn read_run(&self, run_id: Uuid) -> Result<Option<RunRecord>, StoreError> {
        let directory = self.run_directory(run_id);
        if !directory.exists() {
            return Ok(None);
        }
        let path = directory.join("run.json");
        let run = read_run(&path).map_err(|error| StoreError::InvalidRunStore {
            path: path.clone(),
            message: error.to_string(),
        })?;
        validate_run_location(&path, run_id, &run)?;
        Ok(Some(run))
    }

    pub fn inspect_runs(&self) -> Result<RunInventory, StoreError> {
        let directory = self.root.join("runs");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RunInventory::default());
            }
            Err(source) => {
                return Err(StoreError::Io {
                    action: "list run records",
                    path: directory,
                    source,
                });
            }
        };
        let mut inventory = RunInventory::default();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                action: "read run directory entry",
                path: directory.clone(),
                source,
            })?;
            if !entry.path().is_dir() {
                continue;
            }
            let Some(run_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            let path = entry.path().join("run.json");
            match read_run(&path).and_then(|run| {
                validate_run_location(&path, run_id, &run)?;
                Ok(run)
            }) {
                Ok(run) => inventory.runs.push(run),
                Err(error) => inventory.problems.push(RunStoreProblem {
                    path,
                    message: error.to_string(),
                }),
            }
        }
        inventory.runs.sort_by(|left, right| {
            right
                .state
                .is_active()
                .cmp(&left.state.is_active())
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        inventory
            .problems
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(inventory)
    }

    pub fn list_runs(&self) -> Result<Vec<RunRecord>, StoreError> {
        let inventory = self.inspect_runs()?;
        if let Some(problem) = inventory.problems.into_iter().next() {
            return Err(StoreError::InvalidRunStore {
                path: problem.path,
                message: problem.message,
            });
        }
        Ok(inventory.runs)
    }

    pub fn operation_path(&self, operation_id: Uuid) -> PathBuf {
        self.root
            .join("operations")
            .join(format!("{operation_id}.json"))
    }

    pub fn run_directory(&self, run_id: Uuid) -> PathBuf {
        self.root.join("runs").join(run_id.to_string())
    }

    fn claim_issue_directory(&self, issue: NonZeroU64) -> PathBuf {
        self.root.join("claims").join(issue.to_string())
    }

    fn release_issue_directory(&self, issue: NonZeroU64) -> PathBuf {
        self.root.join("releases").join(issue.to_string())
    }

    fn claim_path(&self, issue: NonZeroU64, claim_id: Uuid) -> PathBuf {
        self.claim_issue_directory(issue)
            .join(format!("{claim_id}.json"))
    }
}

pub struct LockedStore<'a> {
    store: &'a Store,
    _lock_file: File,
}

impl LockedStore<'_> {
    fn create_layout(&self) -> Result<(), StoreError> {
        for directory in [
            self.store.root.join("operations"),
            self.store.root.join("claims"),
            self.store.root.join("releases"),
            self.store.root.join("runs"),
        ] {
            create_directory(&directory)?;
        }
        Ok(())
    }

    pub fn create_claim(&self, claim: &Claim) -> Result<PathBuf, StoreError> {
        claim.validate()?;
        let directory = self.store.claim_issue_directory(claim.issue);
        create_directory(&directory)?;
        let path = directory.join(format!("{}.json", claim.claim_id));
        PreparedAtomicWrite::json(&path, claim)?.create_new()?;
        Ok(path)
    }

    pub fn create_release(&self, release: &ReleaseMarker) -> Result<PathBuf, StoreError> {
        release.validate()?;
        let directory = self.store.release_issue_directory(release.issue);
        create_directory(&directory)?;
        let path = directory.join(format!("{}.json", release.claim_id));
        PreparedAtomicWrite::json(&path, release)?.create_new()?;
        Ok(path)
    }

    pub fn write_operation(&self, operation: &OperationRecord) -> Result<PathBuf, StoreError> {
        operation.validate()?;
        let path = self.store.operation_path(operation.operation_id);
        PreparedAtomicWrite::json(&path, operation)?.replace()?;
        Ok(path)
    }

    pub fn remove_operation(&self, operation_id: Uuid) -> Result<(), StoreError> {
        let path = self.store.operation_path(operation_id);
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(
                path.parent()
                    .expect("operation path always has a parent directory"),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                action: "remove operation record",
                path,
                source,
            }),
        }
    }

    pub fn create_run(&self, claim: &Claim, new_run: NewRun<'_>) -> Result<RunRecord, StoreError> {
        claim.validate()?;
        let claim_path = self.store.claim_path(claim.issue, claim.claim_id);
        let persisted = match read_claim(&claim_path) {
            Ok(claim) => claim,
            Err(StoreError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::MissingClaimGeneration {
                    issue: claim.issue,
                    claim_id: claim.claim_id,
                });
            }
            Err(error) => return Err(error),
        };
        if persisted != *claim {
            return Err(StoreError::ClaimGenerationMismatch {
                issue: claim.issue,
                claim_id: claim.claim_id,
            });
        }

        let run_directory = self.store.run_directory(new_run.run_id);
        match fs::symlink_metadata(&run_directory) {
            Ok(_) => {
                return Err(StoreError::RunAlreadyExists {
                    run_id: new_run.run_id,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StoreError::Io {
                    action: "inspect run destination",
                    path: run_directory,
                    source,
                });
            }
        }
        let mut operation = OperationRecord {
            schema_version: SCHEMA_VERSION.to_owned(),
            operation_id: new_run.run_id,
            kind: OperationKind::Run,
            claim_id: claim.claim_id,
            issue: claim.issue,
            branch: claim.branch.clone(),
            worktree: claim.worktree.clone(),
            phase: OperationPhase::Reserved,
            started_at: new_run.created_at,
        };
        operation.validate()?;
        PreparedAtomicWrite::json(
            &self.store.operation_path(operation.operation_id),
            &operation,
        )?
        .create_new()?;

        let result = self.create_run_artifacts(claim, new_run, &run_directory, &mut operation);
        if result.is_err() {
            let removed_run = match fs::remove_dir_all(&run_directory) {
                Ok(()) => sync_directory(
                    run_directory
                        .parent()
                        .expect("run directory always has a parent"),
                )
                .is_ok(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            };
            if removed_run {
                let _ = self.remove_operation(operation.operation_id);
            }
        }
        result
    }

    fn create_run_artifacts(
        &self,
        claim: &Claim,
        new_run: NewRun<'_>,
        directory: &Path,
        operation: &mut OperationRecord,
    ) -> Result<RunRecord, StoreError> {
        create_private_directory(directory)?;
        let prompt_path = directory.join("prompt.md");
        let stdout_path = directory.join("stdout.log");
        let stderr_path = directory.join("stderr.log");
        write_private_file(&prompt_path, new_run.prompt)?;
        write_private_file(&stdout_path, b"")?;
        write_private_file(&stderr_path, b"")?;
        sync_directory(directory)?;

        operation.phase = OperationPhase::RunArtifactsCreated;
        self.write_operation(operation)?;
        let run = RunRecord {
            schema_version: SCHEMA_VERSION.to_owned(),
            run_id: new_run.run_id,
            repo: claim.repo.clone(),
            claim_id: claim.claim_id,
            issue: claim.issue,
            agent: new_run.agent.to_owned(),
            mode: new_run.mode,
            state: crate::model::RunState::Queued,
            worktree: claim.worktree.clone(),
            worker_pid: None,
            child_pid: None,
            created_at: new_run.created_at,
            started_at: None,
            finished_at: None,
            exit_code: None,
            prompt_path,
            stdout_path,
            stderr_path,
            error: None,
        };
        run.validate()?;
        PreparedAtomicWrite::json(&directory.join("run.json"), &run)?.create_new()?;
        operation.phase = OperationPhase::RunCommitted;
        self.write_operation(operation)?;
        self.remove_operation(operation.operation_id)?;
        Ok(run)
    }

    pub fn transition_run(
        &self,
        run_id: Uuid,
        transition: RunTransition,
    ) -> Result<RunRecord, StoreError> {
        let mut run = self
            .store
            .read_run(run_id)?
            .ok_or(StoreError::RunNotFound { run_id })?;
        let from = run.state;
        run.apply_transition(transition)
            .map_err(|error| StoreError::InvalidRunTransition {
                run_id,
                from: from.as_str(),
                message: error.to_string(),
            })?;
        PreparedAtomicWrite::json(&self.store.run_directory(run_id).join("run.json"), &run)?
            .replace()?;
        Ok(run)
    }

    pub fn active_claims(&self) -> Result<Vec<Claim>, StoreError> {
        self.store.active_claims()
    }

    pub fn list_claims(&self, issue: NonZeroU64) -> Result<Vec<Claim>, StoreError> {
        self.store.list_claims(issue)
    }

    pub fn list_releases(&self, issue: NonZeroU64) -> Result<Vec<ReleaseMarker>, StoreError> {
        self.store.list_releases(issue)
    }
}

pub struct PreparedAtomicWrite {
    temporary: NamedTempFile,
    destination: PathBuf,
}

impl PreparedAtomicWrite {
    pub fn json<T: Serialize + ?Sized>(destination: &Path, value: &T) -> Result<Self, StoreError> {
        let parent = destination
            .parent()
            .ok_or_else(|| StoreError::InvalidDestination(destination.to_path_buf()))?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| StoreError::Io {
            action: "create temporary store file",
            path: parent.to_path_buf(),
            source,
        })?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), value).map_err(|source| {
            StoreError::Json {
                path: temporary.path().to_path_buf(),
                source,
            }
        })?;
        temporary
            .as_file_mut()
            .write_all(b"\n")
            .and_then(|()| temporary.as_file_mut().flush())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| StoreError::Io {
                action: "flush temporary store file",
                path: temporary.path().to_path_buf(),
                source,
            })?;
        Ok(Self {
            temporary,
            destination: destination.to_path_buf(),
        })
    }

    pub fn replace(self) -> Result<(), StoreError> {
        let Self {
            temporary,
            destination,
        } = self;
        let parent = destination
            .parent()
            .expect("prepared destination always has a parent")
            .to_path_buf();
        temporary
            .persist(&destination)
            .map_err(|error| StoreError::Io {
                action: "atomically replace store file",
                path: destination,
                source: error.error,
            })?;
        sync_directory(&parent)
    }

    pub fn create_new(self) -> Result<(), StoreError> {
        let Self {
            temporary,
            destination,
        } = self;
        let parent = destination
            .parent()
            .expect("prepared destination always has a parent")
            .to_path_buf();
        temporary
            .persist_noclobber(&destination)
            .map_err(|error| StoreError::Io {
                action: "atomically create immutable store file",
                path: destination,
                source: error.error,
            })?;
        sync_directory(&parent)
    }
}

fn create_directory(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|source| StoreError::Io {
        action: "create store directory",
        path: path.to_path_buf(),
        source,
    })
}

fn create_private_directory(path: &Path) -> Result<(), StoreError> {
    fs::create_dir(path).map_err(|source| StoreError::Io {
        action: "create private run directory",
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            StoreError::Io {
                action: "set private run directory permissions",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| StoreError::Io {
        action: "create private run artifact",
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(contents)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|source| StoreError::Io {
            action: "write private run artifact",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| StoreError::Io {
            action: "sync store directory",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn read_claim(path: &Path) -> Result<Claim, StoreError> {
    let bytes = fs::read(path).map_err(|source| StoreError::Io {
        action: "read claim generation",
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    Claim::from_value(value).map_err(StoreError::Model)
}

fn read_release(path: &Path) -> Result<ReleaseMarker, StoreError> {
    let bytes = fs::read(path).map_err(|source| StoreError::Io {
        action: "read release generation",
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    ReleaseMarker::from_value(value).map_err(StoreError::Model)
}

fn read_operation(path: &Path) -> Result<OperationRecord, StoreError> {
    let bytes = fs::read(path).map_err(|source| StoreError::Io {
        action: "read operation record",
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    OperationRecord::from_value(value).map_err(StoreError::Model)
}

fn read_run(path: &Path) -> Result<RunRecord, StoreError> {
    let bytes = fs::read(path).map_err(|source| StoreError::Io {
        action: "read run record",
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    RunRecord::from_value(value).map_err(StoreError::Model)
}

fn validate_run_location(path: &Path, run_id: Uuid, run: &RunRecord) -> Result<(), StoreError> {
    if run.run_id != run_id {
        return Err(StoreError::LocationMismatch {
            path: path.to_path_buf(),
            message: "run_id does not match its directory name".to_owned(),
        });
    }
    let directory = path
        .parent()
        .expect("run record path always has a parent directory");
    for (name, artifact) in [
        ("prompt.md", &run.prompt_path),
        ("stdout.log", &run.stdout_path),
        ("stderr.log", &run.stderr_path),
    ] {
        if artifact != &directory.join(name) {
            return Err(StoreError::LocationMismatch {
                path: path.to_path_buf(),
                message: format!("run artifact path for {name} does not match its run directory"),
            });
        }
        let metadata = fs::symlink_metadata(artifact).map_err(|source| StoreError::Io {
            action: "inspect run artifact",
            path: artifact.to_path_buf(),
            source,
        })?;
        if !metadata.file_type().is_file() {
            return Err(StoreError::LocationMismatch {
                path: artifact.to_path_buf(),
                message: format!("run artifact {name} is not a regular file"),
            });
        }
    }
    Ok(())
}

fn validate_claim_location(
    path: &Path,
    issue: NonZeroU64,
    claim: &Claim,
) -> Result<(), StoreError> {
    let filename = path.file_stem().and_then(|value| value.to_str());
    let expected_filename = claim.claim_id.to_string();
    if claim.issue != issue || filename != Some(expected_filename.as_str()) {
        return Err(StoreError::LocationMismatch {
            path: path.to_path_buf(),
            message: "claim issue or claim_id does not match its storage path".to_owned(),
        });
    }
    Ok(())
}

fn validate_release_location(
    path: &Path,
    issue: NonZeroU64,
    release: &ReleaseMarker,
) -> Result<(), StoreError> {
    let filename = path.file_stem().and_then(|value| value.to_str());
    let expected_filename = release.claim_id.to_string();
    if release.issue != issue || filename != Some(expected_filename.as_str()) {
        return Err(StoreError::LocationMismatch {
            path: path.to_path_buf(),
            message: "release issue or claim_id does not match its storage path".to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to {action} at {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("invalid atomic-write destination {0}")]
    InvalidDestination(PathBuf),
    #[error("invalid store record location {path}: {message}")]
    LocationMismatch { path: PathBuf, message: String },
    #[error("claim generation {claim_id} for issue {issue} does not exist")]
    MissingClaimGeneration { issue: NonZeroU64, claim_id: Uuid },
    #[error("claim generation {claim_id} for issue {issue} does not match persisted identity")]
    ClaimGenerationMismatch { issue: NonZeroU64, claim_id: Uuid },
    #[error("run {run_id} does not exist")]
    RunNotFound { run_id: Uuid },
    #[error("run {run_id} already exists")]
    RunAlreadyExists { run_id: Uuid },
    #[error("run {run_id} cannot transition from {from}: {message}")]
    InvalidRunTransition {
        run_id: Uuid,
        from: &'static str,
        message: String,
    },
    #[error("invalid run store state at {path}: {message}")]
    InvalidRunStore { path: PathBuf, message: String },
}

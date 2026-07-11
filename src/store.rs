use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use fs4::FileExt;
use serde::Serialize;
use serde_json::Value;
use tempfile::NamedTempFile;
use thiserror::Error;
use uuid::Uuid;

use crate::model::{Claim, ModelError, OperationRecord, ReleaseMarker, Validate};

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

    pub fn operation_path(&self, operation_id: Uuid) -> PathBuf {
        self.root
            .join("operations")
            .join(format!("{operation_id}.json"))
    }

    fn claim_issue_directory(&self, issue: NonZeroU64) -> PathBuf {
        self.root.join("claims").join(issue.to_string())
    }

    fn release_issue_directory(&self, issue: NonZeroU64) -> PathBuf {
        self.root.join("releases").join(issue.to_string())
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
}

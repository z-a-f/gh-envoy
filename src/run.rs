use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::Utc;
use thiserror::Error;
use uuid::Uuid;

use crate::execution::{ProcessSpec, StdioPolicy, WaitOutcome};
use crate::git::{RepositoryContext, RepositoryError};
use crate::model::{Claim, RunMode, RunRecord, RunTransition};
use crate::store::{NewRun, Store, StoreError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundRunRequest {
    pub agent: String,
    pub prompt: Option<String>,
    pub agent_args: Vec<OsString>,
}

pub fn run_foreground(
    cwd: &Path,
    request: ForegroundRunRequest,
    interrupted: Arc<AtomicBool>,
) -> Result<RunRecord, ForegroundRunError> {
    let repository = match RepositoryContext::discover(cwd, "origin") {
        Ok(repository) => repository,
        Err(error) if error.is_outside_worktree() => {
            return Err(ForegroundRunError::Refused(format!(
                "directory {} is not a registered Git worktree with an active claim",
                cwd.display()
            )));
        }
        Err(error) => return Err(error.into()),
    };
    let store = Store::new(repository.store_root());
    let claim = resolve_owned_claim(&repository, &store)?;

    let run_id = Uuid::new_v4();
    let created_at = Utc::now();
    let prompt = request.prompt.as_deref().unwrap_or("").as_bytes();
    let queued = store.lock()?.create_run(
        &claim,
        NewRun {
            run_id,
            agent: &request.agent,
            mode: RunMode::Interactive,
            prompt,
            created_at,
        },
    )?;

    let mut args = request.agent_args;
    if let Some(prompt) = request.prompt {
        args.push(prompt.into());
    }
    let spec = ProcessSpec {
        executable: request.agent.into(),
        args,
        cwd: claim.worktree.clone(),
        env_overrides: Vec::new(),
        stdio: StdioPolicy::Inherit,
    };
    let mut child = match spec.spawn_grouped() {
        Ok(child) => child,
        Err(source) => {
            let error = source.to_string();
            store.lock()?.transition_run(
                queued.run_id,
                RunTransition::SpawnFailed {
                    worker_pid: None,
                    finished_at: Utc::now(),
                    error,
                },
            )?;
            return Err(ForegroundRunError::Spawn(source));
        }
    };
    let child_pid = child.id();
    if let Err(error) = store.lock()?.transition_run(
        queued.run_id,
        RunTransition::Start {
            worker_pid: None,
            child_pid,
            started_at: Utc::now(),
        },
    ) {
        let _ = child.kill_and_wait();
        return Err(error.into());
    }

    let exit_code = match child.wait_interruptibly(&interrupted) {
        Ok(WaitOutcome::Exited(exit_code)) => exit_code,
        Ok(WaitOutcome::Interrupted) => 130,
        Err(source) => {
            let _ = child.kill_and_wait();
            let _ = store.lock().and_then(|locked| {
                locked.transition_run(
                    queued.run_id,
                    RunTransition::Exit {
                        exit_code: 1,
                        finished_at: Utc::now(),
                    },
                )
            });
            return Err(ForegroundRunError::Wait(source));
        }
    };
    store
        .lock()?
        .transition_run(
            queued.run_id,
            RunTransition::Exit {
                exit_code,
                finished_at: Utc::now(),
            },
        )
        .map_err(Into::into)
}

fn resolve_owned_claim(
    repository: &RepositoryContext,
    store: &Store,
) -> Result<Claim, ForegroundRunError> {
    let claim = store
        .active_claims()?
        .into_iter()
        .find(|claim| claim.worktree == repository.current_worktree)
        .ok_or_else(|| {
            ForegroundRunError::Refused(format!(
                "worktree {} has no active claim",
                repository.current_worktree.display()
            ))
        })?;
    if repository.current_branch.as_deref() != Some(claim.branch.as_str()) {
        return Err(ForegroundRunError::Refused(format!(
            "worktree {} is registered on branch {:?}, but active claim generation {} owns branch {:?}",
            repository.current_worktree.display(),
            repository.current_branch,
            claim.claim_id,
            claim.branch
        )));
    }
    Ok(claim)
}

#[derive(Debug, Error)]
pub enum ForegroundRunError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("failed to start agent process: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("failed while waiting for agent process: {0}")]
    Wait(#[source] std::io::Error),
    #[error("failed to install Ctrl-C handler: {0}")]
    InterruptHandler(#[from] ctrlc::Error),
}

impl ForegroundRunError {
    pub const fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused(_))
    }
}

pub fn install_interrupt_handler() -> Result<Arc<AtomicBool>, ForegroundRunError> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        handler_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })?;
    Ok(interrupted)
}

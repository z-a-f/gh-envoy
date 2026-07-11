use std::num::NonZeroU64;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

use crate::command::CommandRunner;
use crate::git::GithubCli;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubIssueState {
    Open,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubIssue {
    pub title: String,
    pub state: GithubIssueState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GithubIssueObservation {
    Available(GithubIssue),
    Unavailable,
}

pub fn observe_issue<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
    repository: &str,
    issue: NonZeroU64,
) -> Result<GithubIssueObservation, GithubIssueError> {
    let issue_number = issue.to_string();
    let output = match GithubCli::new(runner).attempt(
        cwd,
        [
            "issue",
            "view",
            &issue_number,
            "--repo",
            repository,
            "--json",
            "state,title",
        ],
    ) {
        Ok(output) => output,
        Err(_) => return Ok(GithubIssueObservation::Unavailable),
    };
    if output.exit_code != Some(0) {
        return Ok(GithubIssueObservation::Unavailable);
    }
    let observed: IssueView = serde_json::from_slice(&output.stdout)?;
    let state = match observed.state.as_str() {
        "OPEN" => GithubIssueState::Open,
        "CLOSED" => GithubIssueState::Closed,
        state => return Err(GithubIssueError::UnknownState(state.to_owned())),
    };
    Ok(GithubIssueObservation::Available(GithubIssue {
        title: observed.title,
        state,
    }))
}

#[derive(Debug, Deserialize)]
struct IssueView {
    state: String,
    title: String,
}

#[derive(Debug, Error)]
pub enum GithubIssueError {
    #[error("GitHub issue query returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GitHub issue query returned unknown state {0:?}")]
    UnknownState(String),
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::command::{CommandOutput, CommandSpec, RunnerError};

    struct FailingRunner;

    impl CommandRunner for FailingRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
            Err(RunnerError::Spawn {
                program: OsString::from(&spec.program),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing gh"),
            })
        }
    }

    #[test]
    fn missing_github_cli_is_an_unavailable_observation() {
        let observed = observe_issue(
            &FailingRunner,
            Path::new("."),
            "owner/repository",
            NonZeroU64::new(8).unwrap(),
        )
        .expect("a missing gh executable is an offline condition");

        assert_eq!(observed, GithubIssueObservation::Unavailable);
    }
}

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
    NotFound,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubPullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubPullRequest {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub base: String,
    pub state: GithubPullRequestState,
    pub draft: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GithubPullRequestObservation {
    Available(Option<GithubPullRequest>),
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
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if stderr.contains("could not resolve to an issue")
            || stderr.contains("could not resolve to an issue or pull request")
        {
            return Ok(GithubIssueObservation::NotFound);
        }
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

pub fn observe_pull_request<R: CommandRunner>(
    runner: &R,
    cwd: &Path,
    repository: &str,
    branch: &str,
) -> Result<GithubPullRequestObservation, GithubIssueError> {
    let output = match GithubCli::new(runner).attempt(
        cwd,
        [
            "pr",
            "list",
            "--repo",
            repository,
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,url,headRefName,baseRefName,state,isDraft,mergedAt",
        ],
    ) {
        Ok(output) => output,
        Err(_) => return Ok(GithubPullRequestObservation::Unavailable),
    };
    if output.exit_code != Some(0) {
        return Ok(GithubPullRequestObservation::Unavailable);
    }
    let observed: Vec<PullRequestView> = serde_json::from_slice(&output.stdout)?;
    let Some(observed) = observed
        .into_iter()
        .find(|candidate| candidate.head_ref_name == branch)
    else {
        return Ok(GithubPullRequestObservation::Available(None));
    };
    let state = if observed.merged_at.is_some() || observed.state == "MERGED" {
        GithubPullRequestState::Merged
    } else {
        match observed.state.as_str() {
            "OPEN" => GithubPullRequestState::Open,
            "CLOSED" => GithubPullRequestState::Closed,
            state => return Err(GithubIssueError::UnknownPullRequestState(state.to_owned())),
        }
    };
    Ok(GithubPullRequestObservation::Available(Some(
        GithubPullRequest {
            number: observed.number,
            url: observed.url,
            head: observed.head_ref_name,
            base: observed.base_ref_name,
            state,
            draft: observed.is_draft,
        },
    )))
}

#[derive(Debug, Deserialize)]
struct IssueView {
    state: String,
    title: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestView {
    number: u64,
    url: String,
    head_ref_name: String,
    base_ref_name: String,
    state: String,
    is_draft: bool,
    merged_at: Option<String>,
}

#[derive(Debug, Error)]
pub enum GithubIssueError {
    #[error("GitHub issue query returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GitHub issue query returned unknown state {0:?}")]
    UnknownState(String),
    #[error("GitHub pull request query returned unknown state {0:?}")]
    UnknownPullRequestState(String),
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

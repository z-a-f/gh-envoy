use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::command::{
    CommandOutput, CommandRunner, CommandSpec, RunnerError, SystemRunner, display_command,
    path_from_utf8_output, text_from_utf8_output,
};

pub struct GitCli<'a, R: CommandRunner> {
    runner: &'a R,
}

impl<'a, R: CommandRunner> GitCli<'a, R> {
    pub fn new(runner: &'a R) -> Self {
        Self { runner }
    }

    pub fn run<I, S>(&self, cwd: &Path, args: I) -> Result<CommandOutput, CliCommandError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        run_cli(self.runner, "git", cwd, args)
    }

    pub fn attempt<I, S>(&self, cwd: &Path, args: I) -> Result<CommandOutput, RunnerError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let spec = CommandSpec::new("git").with_args(args).in_directory(cwd);
        self.runner.run(&spec)
    }
}

pub struct GithubCli<'a, R: CommandRunner> {
    runner: &'a R,
}

impl<'a, R: CommandRunner> GithubCli<'a, R> {
    pub fn new(runner: &'a R) -> Self {
        Self { runner }
    }

    pub fn run<I, S>(&self, cwd: &Path, args: I) -> Result<CommandOutput, CliCommandError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        run_cli(self.runner, "gh", cwd, args)
    }
}

fn run_cli<I, S, R>(
    runner: &R,
    program: &'static str,
    cwd: &Path,
    args: I,
) -> Result<CommandOutput, CliCommandError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
    R: CommandRunner,
{
    let spec = CommandSpec::new(program).with_args(args).in_directory(cwd);
    let output = runner.run(&spec)?;
    if output.exit_code == Some(0) {
        Ok(output)
    } else {
        Err(CliCommandError::Failed {
            command: display_command(&spec.program, &spec.args),
            exit_code: output.exit_code,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

#[derive(Debug, Error)]
pub enum CliCommandError {
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("{command} failed with exit code {exit_code:?}: {stderr}")]
    Failed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryContext {
    pub repository: String,
    pub remote_name: String,
    pub remote_url: String,
    pub current_worktree: PathBuf,
    pub main_worktree: PathBuf,
    pub common_dir: PathBuf,
}

impl RepositoryContext {
    pub fn discover(cwd: &Path, remote_name: &str) -> Result<Self, RepositoryError> {
        Self::discover_with_runner(&SystemRunner, cwd, remote_name)
    }

    pub fn discover_with_runner<R: CommandRunner>(
        runner: &R,
        cwd: &Path,
        remote_name: &str,
    ) -> Result<Self, RepositoryError> {
        let git = GitCli::new(runner);
        let current_output = git.run(cwd, ["rev-parse", "--show-toplevel"])?;
        let current_worktree = canonical_existing(
            path_from_utf8_output(&current_output.stdout, "git rev-parse --show-toplevel")
                .map_err(RepositoryError::InvalidOutput)?,
        )?;

        let common_output = git.run(
            cwd,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common_dir = canonical_existing(
            path_from_utf8_output(
                &common_output.stdout,
                "git rev-parse --path-format=absolute --git-common-dir",
            )
            .map_err(RepositoryError::InvalidOutput)?,
        )?;

        let worktrees_output = git.run(cwd, ["worktree", "list", "--porcelain"])?;
        let worktrees_text =
            text_from_utf8_output(&worktrees_output.stdout, "git worktree list --porcelain")
                .map_err(RepositoryError::InvalidOutput)?;
        let main_worktree = worktrees_text
            .lines()
            .find_map(|line| line.strip_prefix("worktree "))
            .ok_or_else(|| {
                RepositoryError::InvalidOutput(
                    "git worktree list --porcelain did not report a worktree".to_owned(),
                )
            })?;
        let main_worktree = canonical_existing(PathBuf::from(main_worktree))?;

        let (remote_url, repository) = match git.run(cwd, ["remote", "get-url", remote_name]) {
            Ok(remote_output) => {
                let remote_url = text_from_utf8_output(
                    &remote_output.stdout,
                    &format!("git remote get-url {remote_name}"),
                )
                .map_err(RepositoryError::InvalidOutput)?
                .to_owned();
                let repository = repository_slug(&remote_url)?;
                (remote_url, repository)
            }
            Err(CliCommandError::Failed { .. }) => {
                let name = main_worktree
                    .file_name()
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        RepositoryError::InvalidOutput(
                            "main worktree does not have a usable repository name".to_owned(),
                        )
                    })?;
                (String::new(), format!("local/{name}"))
            }
            Err(error) => return Err(error.into()),
        };

        Ok(Self {
            repository,
            remote_name: remote_name.to_owned(),
            remote_url,
            current_worktree,
            main_worktree,
            common_dir,
        })
    }

    pub fn store_root(&self) -> PathBuf {
        self.common_dir.join("envoy")
    }

    pub fn discover_common_dir(cwd: &Path) -> Result<PathBuf, RepositoryError> {
        Self::discover_common_dir_with_runner(&SystemRunner, cwd)
    }

    pub fn discover_common_dir_with_runner<R: CommandRunner>(
        runner: &R,
        cwd: &Path,
    ) -> Result<PathBuf, RepositoryError> {
        let output = GitCli::new(runner).run(
            cwd,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        canonical_existing(
            path_from_utf8_output(
                &output.stdout,
                "git rev-parse --path-format=absolute --git-common-dir",
            )
            .map_err(RepositoryError::InvalidOutput)?,
        )
    }
}

pub(crate) fn canonical_existing(path: PathBuf) -> Result<PathBuf, RepositoryError> {
    path.canonicalize()
        .map(without_windows_verbatim_prefix)
        .map_err(|source| RepositoryError::Canonicalize { path, source })
}

fn repository_slug(remote_url: &str) -> Result<String, RepositoryError> {
    let normalized = remote_url.replace('\\', "/");
    let remote_url = normalized.as_str();
    let path = if let Some((_, path)) = remote_url.rsplit_once(':') {
        if remote_url.contains("://") {
            remote_url
                .split_once("://")
                .and_then(|(_, remainder)| remainder.split_once('/'))
                .map_or(remote_url, |(_, path)| path)
        } else {
            path
        }
    } else {
        remote_url
            .split_once("://")
            .and_then(|(_, remainder)| remainder.split_once('/'))
            .map_or(remote_url, |(_, path)| path)
    };
    let slug = path.trim_matches('/').strip_suffix(".git").unwrap_or(path);
    let mut segments = slug.rsplit('/');
    let repo = segments.next().unwrap_or_default();
    let owner = segments.next().unwrap_or_default();
    if repo.is_empty() || owner.is_empty() {
        return Err(RepositoryError::InvalidOutput(format!(
            "remote URL does not identify an owner/repository: {remote_url}"
        )));
    }
    Ok(format!("{owner}/{repo}"))
}

fn without_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path;
    };
    if let Some(value) = value.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{value}"))
    } else if let Some(value) = value.strip_prefix(r"\\?\") {
        PathBuf::from(value)
    } else {
        path
    }
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Command(#[from] CliCommandError),
    #[error("invalid Git output: {0}")]
    InvalidOutput(String),
    #[error("failed to canonicalize {path:?}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{repository_slug, without_windows_verbatim_prefix};

    #[test]
    fn repository_slug_supports_common_github_remote_forms() {
        for remote in [
            "git@github.com:z-a-f/gh-envoy.git",
            "https://github.com/z-a-f/gh-envoy.git",
            "ssh://git@github.com/z-a-f/gh-envoy.git",
        ] {
            assert_eq!(
                repository_slug(remote).expect("valid remote"),
                "z-a-f/gh-envoy"
            );
        }
    }

    #[test]
    fn repository_slug_supports_windows_local_remote_paths() {
        assert_eq!(
            repository_slug(r"C:\Users\runner\fixture\remote.git").expect("valid local remote"),
            "fixture/remote"
        );
    }

    #[test]
    fn git_paths_drop_windows_verbatim_prefixes() {
        assert_eq!(
            without_windows_verbatim_prefix(PathBuf::from(r"\\?\C:\work\fixture")),
            PathBuf::from(r"C:\work\fixture")
        );
        assert_eq!(
            without_windows_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\fixture")),
            PathBuf::from(r"\\server\share\fixture")
        );
    }
}

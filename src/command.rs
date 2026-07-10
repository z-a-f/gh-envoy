use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
        }
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn in_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cwd = Some(directory.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait CommandRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        if let Some(directory) = &spec.cwd {
            command.current_dir(directory);
        }

        let output = command.output().map_err(|source| RunnerError::Spawn {
            program: spec.program.clone(),
            source,
        })?;

        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("failed to start {program:?}: {source}")]
    Spawn {
        program: OsString,
        #[source]
        source: std::io::Error,
    },
}

pub(crate) fn display_command(program: &OsStr, args: &[OsString]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn path_from_utf8_output(output: &[u8], command: &str) -> Result<PathBuf, String> {
    let value = text_from_utf8_output(output, command)?;
    Ok(Path::new(value).to_path_buf())
}

pub(crate) fn text_from_utf8_output<'a>(
    output: &'a [u8],
    command: &str,
) -> Result<&'a str, String> {
    std::str::from_utf8(output)
        .map(str::trim)
        .map_err(|_| format!("{command} returned non-UTF-8 output"))
}

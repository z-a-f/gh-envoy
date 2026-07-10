use std::io::{self, Write};
use std::num::NonZeroU64;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::exit::EnvoyExitCode;

pub const SCHEMA_VERSION: &str = "0.1";

#[derive(Debug, Parser)]
#[command(
    name = "gh-envoy",
    version,
    about = "Coordinate parallel GitHub issue work across Git worktrees"
)]
pub struct Cli {
    #[arg(long, global = true, help = "Emit machine-readable JSON")]
    pub json: bool,

    #[command(subcommand)]
    pub command: EnvoyCommand,
}

#[derive(Debug, Subcommand)]
pub enum EnvoyCommand {
    /// Claim an issue for work in an isolated worktree.
    Claim(IssueArgs),
    /// Show active claims and coordination findings.
    Status,
    /// Check local integrity, publish readiness, and merge coordination.
    Doctor(DoctorArgs),
    /// Release an active claim.
    Release(IssueArgs),
}

impl EnvoyCommand {
    fn name(&self) -> &'static str {
        match self {
            Self::Claim(_) => "claim",
            Self::Status => "status",
            Self::Doctor(_) => "doctor",
            Self::Release(_) => "release",
        }
    }
}

#[derive(Debug, Args)]
pub struct IssueArgs {
    #[arg(value_parser = parse_issue)]
    pub issue: NonZeroU64,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(value_parser = parse_issue, conflicts_with = "stack")]
    pub issue: Option<NonZeroU64>,

    #[arg(long, value_name = "ISSUE", value_parser = parse_issue)]
    pub stack: Option<NonZeroU64>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    schema_version: &'static str,
    command: &'a str,
    status: &'static str,
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'static str,
    message: &'a str,
}

pub fn main_entry() -> EnvoyExitCode {
    match Cli::try_parse() {
        Ok(cli) => run_stub(cli),
        Err(error) => {
            let is_help = matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            let _ = error.print();
            if is_help {
                EnvoyExitCode::Success
            } else {
                EnvoyExitCode::OperationalError
            }
        }
    }
}

fn run_stub(cli: Cli) -> EnvoyExitCode {
    let command = cli.command.name();
    let message = format!("{command} is not implemented yet");

    if cli.json {
        let envelope = ErrorEnvelope {
            schema_version: SCHEMA_VERSION,
            command,
            status: "error",
            error: ErrorBody {
                code: "not_implemented",
                message: &message,
            },
        };
        if serde_json::to_writer(io::stdout().lock(), &envelope).is_ok() {
            let _ = writeln!(io::stdout().lock());
        }
    } else {
        let _ = writeln!(io::stderr().lock(), "error: {message}");
    }

    EnvoyExitCode::OperationalError
}

fn parse_issue(value: &str) -> Result<NonZeroU64, String> {
    value
        .parse::<NonZeroU64>()
        .map_err(|_| "issue must be a positive integer".to_owned())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, EnvoyCommand};

    #[test]
    fn doctor_allows_no_specific_subject() {
        let cli = Cli::try_parse_from(["gh-envoy", "doctor"]).expect("doctor parses");
        assert!(matches!(cli.command, EnvoyCommand::Doctor(_)));
    }
}

use std::io::{self, Write};
use std::num::NonZeroU64;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::command::SystemRunner;
use crate::exit::EnvoyExitCode;
use crate::lifecycle::{ClaimOptions, LifecycleError, claim_issue_with_options, release_claim};
use crate::model::{Claim, ReleaseReason, ReleaseReport};
use crate::status::{get_status, render_status_human, status_document};

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
    Claim(ClaimArgs),
    /// Show active claims and coordination findings.
    Status,
    /// Check local integrity, publish readiness, and merge coordination.
    Doctor(DoctorArgs),
    /// Release an active claim.
    Release(ReleaseArgs),
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
pub struct ClaimArgs {
    #[arg(value_parser = parse_issue)]
    pub issue: NonZeroU64,

    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,

    #[arg(long, value_name = "PATH")]
    pub worktree: Option<PathBuf>,

    #[arg(long, value_name = "ISSUE", value_parser = parse_issue)]
    pub onto: Option<NonZeroU64>,

    #[arg(long, value_name = "ISSUE", value_parser = parse_issue)]
    pub after: Vec<NonZeroU64>,

    #[arg(long, value_name = "GLOB")]
    pub scope: Vec<String>,

    #[arg(long, value_name = "GLOB")]
    pub disallow: Vec<String>,

    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    #[arg(value_parser = parse_issue)]
    pub issue: NonZeroU64,

    #[arg(long, value_enum, default_value_t = ReleaseReasonArg::Manual)]
    pub reason: ReleaseReasonArg,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ReleaseReasonArg {
    Merged,
    Closed,
    Abandoned,
    Manual,
}

impl From<ReleaseReasonArg> for ReleaseReason {
    fn from(value: ReleaseReasonArg) -> Self {
        match value {
            ReleaseReasonArg::Merged => Self::Merged,
            ReleaseReasonArg::Closed => Self::Closed,
            ReleaseReasonArg::Abandoned => Self::Abandoned,
            ReleaseReasonArg::Manual => Self::Manual,
        }
    }
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

#[derive(Debug, Serialize)]
struct ClaimEnvelope<'a> {
    schema_version: &'static str,
    command: &'static str,
    status: &'static str,
    claim: &'a Claim,
    warnings: &'a [String],
}

#[derive(Debug, Serialize)]
struct ReleaseEnvelope<'a> {
    schema_version: &'static str,
    command: &'static str,
    status: &'static str,
    release: &'a ReleaseReport,
    warnings: &'a [String],
}

pub fn main_entry() -> EnvoyExitCode {
    match Cli::try_parse() {
        Ok(cli) => run(cli),
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

fn run(cli: Cli) -> EnvoyExitCode {
    match cli.command {
        EnvoyCommand::Claim(arguments) => run_claim(arguments, cli.json),
        EnvoyCommand::Status => run_status(cli.json),
        EnvoyCommand::Release(arguments) => run_release(arguments, cli.json),
        command => run_stub(command.name(), cli.json),
    }
}

fn run_status(json: bool) -> EnvoyExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return render_error(
                "status",
                json,
                "current_directory",
                &error.to_string(),
                false,
            );
        }
    };
    match get_status(&SystemRunner, &cwd) {
        Ok(report) => {
            if json {
                write_json(&status_document(&report));
            } else {
                let _ = write!(io::stdout().lock(), "{}", render_status_human(&report));
            }
            if report.has_warnings() {
                EnvoyExitCode::Warning
            } else {
                EnvoyExitCode::Success
            }
        }
        Err(error) => render_error(
            "status",
            json,
            "operational_error",
            &error.to_string(),
            false,
        ),
    }
}

fn run_claim(arguments: ClaimArgs, json: bool) -> EnvoyExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return render_error(
                "claim",
                json,
                "current_directory",
                &error.to_string(),
                false,
            );
        }
    };
    let options = ClaimOptions {
        branch: arguments.branch,
        worktree: arguments.worktree,
        onto: arguments.onto,
        after: arguments.after,
        allowed_paths: arguments.scope,
        disallowed_paths: arguments.disallow,
        note: arguments.note,
    };
    match claim_issue_with_options(&SystemRunner, &cwd, arguments.issue, options) {
        Ok(outcome) => {
            let status = if outcome.warnings.is_empty() {
                "success"
            } else {
                "warning"
            };
            if json {
                write_json(&ClaimEnvelope {
                    schema_version: SCHEMA_VERSION,
                    command: "claim",
                    status,
                    claim: &outcome.claim,
                    warnings: &outcome.warnings,
                });
            } else {
                for warning in &outcome.warnings {
                    let _ = writeln!(io::stderr().lock(), "warning: {warning}");
                }
                let _ = writeln!(
                    io::stdout().lock(),
                    "Claimed issue #{} as {}\nBranch: {}\nWorktree: {}\nBase: {}/{} at {}",
                    outcome.claim.issue,
                    &outcome.claim.claim_id.to_string()[..8],
                    outcome.claim.branch,
                    outcome.claim.worktree.display(),
                    outcome.claim.base_remote,
                    outcome.claim.base_ref,
                    outcome.claim.base_sha,
                );
            }
            if outcome.warnings.is_empty() {
                EnvoyExitCode::Success
            } else {
                EnvoyExitCode::Warning
            }
        }
        Err(error) => render_lifecycle_error("claim", json, &error),
    }
}

fn run_release(arguments: ReleaseArgs, json: bool) -> EnvoyExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return render_error(
                "release",
                json,
                "current_directory",
                &error.to_string(),
                false,
            );
        }
    };
    match release_claim(
        &SystemRunner,
        &cwd,
        arguments.issue,
        arguments.reason.into(),
    ) {
        Ok(report) => {
            if json {
                write_json(&ReleaseEnvelope {
                    schema_version: SCHEMA_VERSION,
                    command: "release",
                    status: "success",
                    release: &report,
                    warnings: &[],
                });
            } else if report.already_released {
                let _ = writeln!(
                    io::stdout().lock(),
                    "Issue #{} generation {} is already released",
                    report.issue,
                    &report.claim_id.to_string()[..8]
                );
            } else {
                let _ = writeln!(
                    io::stdout().lock(),
                    "Released issue #{} generation {}",
                    report.issue,
                    &report.claim_id.to_string()[..8]
                );
            }
            EnvoyExitCode::Success
        }
        Err(error) => render_lifecycle_error("release", json, &error),
    }
}

fn render_lifecycle_error(
    command: &'static str,
    json: bool,
    error: &LifecycleError,
) -> EnvoyExitCode {
    let refused = error.is_refusal();
    let code = if refused {
        "refused"
    } else {
        "operational_error"
    };
    render_error(command, json, code, &error.to_string(), refused)
}

fn render_error(
    command: &'static str,
    json: bool,
    code: &'static str,
    message: &str,
    refused: bool,
) -> EnvoyExitCode {
    if json {
        let envelope = ErrorEnvelope {
            schema_version: SCHEMA_VERSION,
            command,
            status: if refused { "blocked" } else { "error" },
            error: ErrorBody { code, message },
        };
        write_json(&envelope);
    } else {
        let _ = writeln!(io::stderr().lock(), "error: {message}");
    }
    if refused {
        EnvoyExitCode::Blocked
    } else {
        EnvoyExitCode::OperationalError
    }
}

fn write_json(value: &impl Serialize) {
    if serde_json::to_writer(io::stdout().lock(), value).is_ok() {
        let _ = writeln!(io::stdout().lock());
    }
}

fn run_stub(command: &'static str, json: bool) -> EnvoyExitCode {
    let message = format!("{command} is not implemented yet");

    if json {
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::SecondsFormat;
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::command::CommandRunner;
use crate::config::{Config, ConfigError};
use crate::git::{RepositoryContext, RepositoryError};
use crate::model::{Claim, ReleaseMarker, ReleaseReason, SCHEMA_VERSION};
use crate::store::{Store, StoreError};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimState {
    Active,
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClaimListEntry {
    pub claim: Claim,
    pub state: ClaimState,
    pub release: Option<ReleaseMarker>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ClaimList {
    pub claims: Vec<ClaimListEntry>,
}

#[derive(Debug, Serialize)]
pub struct ClaimListDocument<'a> {
    pub schema_version: &'static str,
    pub command: &'static str,
    pub status: &'static str,
    pub claims: &'a [ClaimListEntry],
}

pub fn list_document(list: &ClaimList) -> ClaimListDocument<'_> {
    ClaimListDocument {
        schema_version: SCHEMA_VERSION,
        command: "list",
        status: "success",
        claims: &list.claims,
    }
}

pub fn get_claim_list<R: CommandRunner>(runner: &R, cwd: &Path) -> Result<ClaimList, ListError> {
    let common_dir = RepositoryContext::discover_common_dir_with_runner(runner, cwd)?;
    let config = Config::load(&common_dir)?;
    let store = Store::new(common_dir.join("envoy"));
    let claims = store.all_claims()?;
    let mut releases = BTreeMap::<(std::num::NonZeroU64, Uuid), ReleaseMarker>::new();
    let mut loaded_issues = BTreeSet::new();
    for issue in claims.iter().map(|claim| claim.issue) {
        if !loaded_issues.insert(issue) {
            continue;
        }
        for release in store.list_releases(issue)? {
            releases.insert((issue, release.claim_id), release);
        }
    }
    let claims = claims
        .into_iter()
        .map(|mut claim| {
            let release = releases.remove(&(claim.issue, claim.claim_id));
            if config.redact_paths_in_json {
                claim.worktree = PathBuf::from(shortened_worktree(&claim.worktree));
            }
            ClaimListEntry {
                state: if release.is_some() {
                    ClaimState::Released
                } else {
                    ClaimState::Active
                },
                claim,
                release,
            }
        })
        .collect();
    Ok(ClaimList { claims })
}

pub fn render_claim_list_human(list: &ClaimList) -> String {
    render_claim_list(list, false)
}

pub fn render_claim_list_human_colored(list: &ClaimList) -> String {
    render_claim_list(list, true)
}

fn render_claim_list(list: &ClaimList, color: bool) -> String {
    if list.claims.is_empty() {
        return "No claims have been recorded.\n".to_owned();
    }
    let active = list
        .claims
        .iter()
        .filter(|entry| entry.state == ClaimState::Active)
        .count();
    let released = list.claims.len() - active;
    let mut output = format!(
        "Claim history: {} generations ({} active, {} released)\n",
        list.claims.len(),
        active,
        released
    );
    for entry in &list.claims {
        let (marker, state) = match &entry.release {
            Some(release) => (
                paint(color, "2", "○"),
                format!("released ({})", release_reason(release.reason)),
            ),
            None => (paint(color, "32", "●"), "active".to_owned()),
        };
        output.push_str(&format!(
            "\n{marker} #{} {}  {}\n",
            entry.claim.issue,
            &entry.claim.claim_id.to_string()[..8],
            paint(
                color,
                if entry.release.is_some() { "2" } else { "32" },
                &state
            )
        ));
        append_field(&mut output, color, "Branch", &entry.claim.branch);
        append_field(
            &mut output,
            color,
            "Worktree",
            &shortened_worktree(&entry.claim.worktree),
        );
        append_field(
            &mut output,
            color,
            "Created",
            &entry
                .claim
                .created_at
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        );
    }
    output
}

fn append_field(output: &mut String, color: bool, label: &str, value: &str) {
    output.push_str(&format!(
        "  {}{}{}\n",
        paint(color, "2", label),
        " ".repeat(10usize.saturating_sub(label.chars().count())),
        value
    ));
}

fn shortened_worktree(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.starts_with("…/") {
        return text.into_owned();
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "…".to_owned(), |name| format!("…/{name}"))
}

fn release_reason(reason: ReleaseReason) -> &'static str {
    match reason {
        ReleaseReason::Merged => "merged",
        ReleaseReason::Closed => "closed",
        ReleaseReason::Abandoned => "abandoned",
        ReleaseReason::Manual => "manual",
    }
}

fn paint(color: bool, code: &str, value: &str) -> String {
    if color {
        format!("\u{1b}[{code}m{value}\u{1b}[0m")
    } else {
        value.to_owned()
    }
}

#[derive(Debug, Error)]
pub enum ListError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

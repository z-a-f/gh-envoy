use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use gh_envoy::command::{CommandOutput, CommandRunner, CommandSpec, RunnerError};
use gh_envoy::doctor::{CheckStatus, doctor_repository};
use gh_envoy::github::{
    GithubIssueObservation, GithubIssueState, GithubPullRequestObservation, GithubPullRequestState,
    observe_issue, observe_pull_request,
};
use gh_envoy::model::{Claim, SCHEMA_VERSION};
use gh_envoy::status::{GithubState, PrState, get_status};
use gh_envoy::store::Store;
use tempfile::TempDir;
use uuid::Uuid;

struct GithubRunner {
    calls: Mutex<Vec<CommandSpec>>,
}

impl GithubRunner {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl CommandRunner for GithubRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        if spec.program != "gh" {
            return gh_envoy::command::SystemRunner.run(spec);
        }
        self.calls.lock().unwrap().push(spec.clone());
        let args = spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        let stdout = if args.first().is_some_and(|arg| arg == "issue") {
            br#"{"state":"CLOSED","title":"Observed title"}"#.to_vec()
        } else {
            br#"[{"number":42,"url":"https://github.com/o/r/pull/42","headRefName":"topic","baseRefName":"main","state":"MERGED","isDraft":false,"mergedAt":"2026-07-10T18:00:00Z"}]"#.to_vec()
        };
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout,
            stderr: Vec::new(),
        })
    }
}

#[test]
fn issue_and_exact_branch_pr_facts_use_only_read_only_github_calls() {
    let runner = GithubRunner::new();

    let issue =
        observe_issue(&runner, Path::new("."), "o/r", NonZeroU64::new(10).unwrap()).unwrap();
    let pr = observe_pull_request(&runner, Path::new("."), "o/r", "topic").unwrap();

    let GithubIssueObservation::Available(issue) = issue else {
        panic!("issue should be available");
    };
    assert_eq!(issue.title, "Observed title");
    assert_eq!(issue.state, GithubIssueState::Closed);
    let GithubPullRequestObservation::Available(Some(pr)) = pr else {
        panic!("pull request should be available");
    };
    assert_eq!(pr.number, 42);
    assert_eq!(pr.head, "topic");
    assert_eq!(pr.base, "main");
    assert_eq!(pr.state, GithubPullRequestState::Merged);
    assert!(!pr.draft);

    let calls = runner.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.program == "gh"));
    assert!(calls.iter().all(|call| {
        !call.args.iter().any(|arg| {
            matches!(
                arg.to_string_lossy().as_ref(),
                "create" | "edit" | "close" | "merge" | "reopen" | "ready"
            )
        })
    }));
    assert!(
        calls[1]
            .args
            .windows(2)
            .any(|args| args == ["--head", "topic"])
    );
}

struct MissingIssueRunner;

impl CommandRunner for MissingIssueRunner {
    fn run(&self, _spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        Ok(CommandOutput {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"could not resolve to an issue with the number of 999".to_vec(),
        })
    }
}

#[test]
fn a_reachable_missing_issue_is_distinct_from_unavailable_github() {
    let observed = observe_issue(
        &MissingIssueRunner,
        Path::new("."),
        "o/r",
        NonZeroU64::new(999).unwrap(),
    )
    .unwrap();

    assert_eq!(observed, GithubIssueObservation::NotFound);
}

#[test]
fn status_derives_remote_facts_without_rewriting_claims_and_doctor_blocks_wrong_base() {
    let fixture = RepositoryFixture::new();
    let claim = fixture.persist_claim();
    let claim_path = fixture
        .store
        .root()
        .join(format!("claims/{}/{}.json", claim.issue, claim.claim_id));
    let before = fs::read(&claim_path).unwrap();
    let runner = GithubRunner::new();

    let status = get_status(&runner, &fixture.worktree).unwrap();

    assert_eq!(
        status.claims[0].claim.title.as_deref(),
        Some("Observed title")
    );
    assert_eq!(status.claims[0].github_state, GithubState::Available);
    assert_eq!(status.claims[0].pr.as_ref().unwrap().state, PrState::Merged);
    assert_eq!(fs::read(&claim_path).unwrap(), before);

    let doctor = doctor_repository(&runner, &fixture.worktree, Some(claim.issue)).unwrap();
    assert_eq!(doctor.gates.publish, gh_envoy::doctor::GateRollup::Blocked);
    assert!(
        doctor
            .checks
            .iter()
            .any(|check| { check.id == "publish.pr_base" && check.status == CheckStatus::Fail })
    );
    assert!(
        doctor
            .recommendations
            .iter()
            .any(|recommendation| recommendation.contains("--reason merged"))
    );
    assert!(
        doctor
            .recommendations
            .iter()
            .any(|recommendation| recommendation.contains("--reason closed"))
    );
}

struct OfflineGithubRunner;

impl CommandRunner for OfflineGithubRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        if spec.program != "gh" {
            return gh_envoy::command::SystemRunner.run(spec);
        }
        Ok(CommandOutput {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"network unavailable".to_vec(),
        })
    }
}

#[test]
fn doctor_keeps_local_checks_available_when_github_is_offline() {
    let fixture = RepositoryFixture::new();
    let claim = fixture.persist_claim();

    let doctor =
        doctor_repository(&OfflineGithubRunner, &fixture.worktree, Some(claim.issue)).unwrap();

    assert!(doctor.checks.iter().any(|check| {
        check.id == "integrity.claim_schema" && check.status == CheckStatus::Pass
    }));
    assert!(
        doctor.checks.iter().any(|check| {
            check.id == "publish.issue_state" && check.status == CheckStatus::Skip
        })
    );
    assert!(
        doctor
            .checks
            .iter()
            .any(|check| { check.id == "publish.pr_base" && check.status == CheckStatus::Skip })
    );
}

struct RepositoryFixture {
    _root: TempDir,
    worktree: PathBuf,
    store: Store,
    base_sha: String,
}

impl RepositoryFixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let repository = root.path().join("fixture");
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Envoy Tests"]);
        git(
            &repository,
            &["config", "user.email", "envoy@example.invalid"],
        );
        fs::write(repository.join("README.md"), "fixture\n").unwrap();
        git(&repository, &["add", "README.md"]);
        git(&repository, &["commit", "-qm", "initial"]);
        git(
            &repository,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        let base_sha = git_stdout(&repository, &["rev-parse", "HEAD"]);
        let worktree = root.path().join("topic");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "topic",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        let common = git_stdout(
            &repository,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        let store = Store::new(PathBuf::from(common).join("envoy"));
        Self {
            _root: root,
            worktree,
            store,
            base_sha,
        }
    }

    fn persist_claim(&self) -> Claim {
        let claim = Claim {
            schema_version: SCHEMA_VERSION.to_owned(),
            claim_id: Uuid::new_v4(),
            repo: "o/r".to_owned(),
            issue: NonZeroU64::new(10).unwrap(),
            title: None,
            branch: "topic".to_owned(),
            worktree: self.worktree.canonicalize().unwrap(),
            base_remote: "origin".to_owned(),
            base_ref: "expected-parent".to_owned(),
            base_sha: self.base_sha.clone(),
            base_issue: Some(NonZeroU64::new(9).unwrap()),
            base_claim_id: Some(Uuid::new_v4()),
            wait_for: Vec::new(),
            declared_scope: None,
            note: None,
            created_at: Utc::now(),
        };
        self.store.lock().unwrap().create_claim(&claim).unwrap();
        claim
    }
}

fn git(directory: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(directory: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

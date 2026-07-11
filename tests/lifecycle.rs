use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use gh_envoy::command::{CommandOutput, CommandRunner, CommandSpec, RunnerError, SystemRunner};
use gh_envoy::lifecycle::{ClaimOptions, claim_issue, claim_issue_with_options};
use serde_json::Value;
use tempfile::TempDir;

mod support;

use support::assert_same_existing_path;

fn envoy() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("gh-envoy").expect("gh-envoy binary should build")
}

#[test]
fn failed_fetch_uses_existing_remote_tracking_sha_with_a_warning() {
    let fixture = RepositoryFixture::with_remote();
    let expected_base = fixture.git_stdout(&["rev-parse", "refs/remotes/origin/main"]);
    let missing = fixture._root.path().join("missing-remote.git");
    fixture.git(&["remote", "set-url", "origin", path(&missing)]);

    let output = envoy()
        .current_dir(fixture.repository())
        .args(["claim", "8", "--json"])
        .output()
        .expect("run claim with stale remote-tracking ref");

    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).expect("claim JSON");
    assert_eq!(value["claim"]["base_sha"], expected_base);
    assert!(
        value["warnings"][0]
            .as_str()
            .expect("warning")
            .contains("existing remote-tracking branch")
    );
}

#[test]
fn configured_base_and_worktree_root_override_defaults() {
    let fixture = RepositoryFixture::with_remote();
    fixture.git(&["branch", "trunk"]);
    fixture.git(&["push", "-q", "origin", "trunk"]);
    let worktree_root = fixture._root.path().join("configured-worktrees");
    fixture.write_config(&format!(
        "default_base_ref: trunk\nworktree_root: {}\n",
        worktree_root.display()
    ));

    let value = run_envoy_json(fixture.repository(), &["claim", "9", "--json"]);

    assert_eq!(value["claim"]["base_ref"], "trunk");
    let worktree = PathBuf::from(value["claim"]["worktree"].as_str().expect("worktree"));
    assert_same_existing_path(worktree.parent().expect("worktree parent"), &worktree_root);
}

#[test]
fn missing_configured_base_fails_before_creating_git_state() {
    let fixture = RepositoryFixture::with_remote();
    fixture.write_config("default_base_ref: missing\n");

    let output = envoy()
        .current_dir(fixture.repository())
        .args(["claim", "10", "--json"])
        .output()
        .expect("run claim with missing base");

    assert_eq!(output.status.code(), Some(3));
    let value: Value = serde_json::from_slice(&output.stdout).expect("error JSON");
    assert!(
        value["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("could not resolve base")
    );
    assert!(!fixture.store_root().join("claims").exists());
    assert!(!fixture.store_root().join("operations").exists());
}

#[test]
fn human_output_reports_claim_warning_and_release_result() {
    let fixture = RepositoryFixture::new();
    let claim = envoy()
        .current_dir(fixture.repository())
        .args(["claim", "11"])
        .output()
        .expect("run human claim");
    assert_eq!(claim.status.code(), Some(1));
    let claim_stdout = String::from_utf8_lossy(&claim.stdout);
    assert!(claim_stdout.contains("Claimed issue #11"));
    assert!(claim_stdout.contains("Next: change directory to the claimed worktree above"));
    assert!(String::from_utf8_lossy(&claim.stderr).contains("warning: base"));

    let release = envoy()
        .current_dir(fixture.repository())
        .args(["release", "11"])
        .output()
        .expect("run human release");
    assert_success(&release);
    assert!(String::from_utf8_lossy(&release.stdout).contains("Released issue #11"));

    let repeated = envoy()
        .current_dir(fixture.repository())
        .args(["release", "11"])
        .output()
        .expect("repeat human release");
    assert_success(&repeated);
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("already released"));
}

#[test]
fn concurrent_claims_produce_one_winner_without_git_leaks() {
    let fixture = RepositoryFixture::with_remote();
    let first = spawn_envoy(fixture.repository(), &["claim", "55", "--json"]);
    let second = spawn_envoy(fixture.repository(), &["claim", "55", "--json"]);
    let outputs = [wait(first), wait(second)];

    let success_count = outputs
        .iter()
        .filter(|output| matches!(output.status.code(), Some(0 | 1)))
        .count();
    let refusal_count = outputs
        .iter()
        .filter(|output| output.status.code() == Some(2))
        .count();
    assert_eq!(success_count, 1, "outputs: {outputs:?}");
    assert_eq!(refusal_count, 1, "outputs: {outputs:?}");

    let branches = fixture.git_stdout(&[
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/heads/envoy",
    ]);
    assert_eq!(
        branches
            .lines()
            .filter(|line| line.starts_with("envoy/issue-55-"))
            .count(),
        1
    );
    let worktrees = fixture.git_stdout(&["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        2,
        "only the main checkout and winning claim worktree remain"
    );
}

#[test]
fn failure_after_branch_creation_rolls_back_branch_and_operation() {
    let fixture = RepositoryFixture::with_remote();
    let blocker = fixture._root.path().join("blocked-root");
    fs::write(&blocker, "not a directory").expect("write worktree root blocker");
    fixture.write_config(&format!("worktree_root: {}\n", blocker.display()));

    let output = envoy()
        .current_dir(fixture.repository())
        .args(["claim", "76", "--json"])
        .output()
        .expect("run claim");

    assert_eq!(output.status.code(), Some(3));
    assert_no_claim_git_or_operation(&fixture, 76);
}

#[test]
fn failure_after_worktree_creation_rolls_back_worktree_branch_and_operation() {
    let fixture = RepositoryFixture::with_remote();
    let store = fixture.store_root();
    fs::create_dir_all(store.join("claims")).expect("create claims root");
    fs::write(store.join("claims/77"), "blocks issue directory")
        .expect("write claim directory blocker");

    let output = envoy()
        .current_dir(fixture.repository())
        .args(["claim", "77", "--json"])
        .output()
        .expect("run claim");

    assert_eq!(output.status.code(), Some(3));
    assert_no_claim_git_or_operation(&fixture, 77);
}

#[test]
fn marker_only_release_is_idempotent_and_reclaim_creates_a_new_generation() {
    let fixture = RepositoryFixture::with_remote();
    let claim = run_envoy_json(fixture.repository(), &["claim", "88", "--json"]);
    let first_id = claim["claim"]["claim_id"].as_str().expect("claim ID");
    let branch = claim["claim"]["branch"].as_str().expect("branch");
    let worktree = PathBuf::from(claim["claim"]["worktree"].as_str().expect("worktree"));

    let released = run_envoy_json(
        fixture.repository(),
        &["release", "88", "--reason", "merged", "--json"],
    );
    assert_eq!(released["release"]["claim_id"], first_id);
    assert_eq!(released["release"]["already_released"], false);
    assert!(worktree.exists(), "marker-only release preserves worktree");
    assert_eq!(fixture.git_stdout(&["rev-parse", branch]).len(), 40);
    let marker: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .store_root()
                .join(format!("releases/88/{first_id}.json")),
        )
        .expect("read release marker"),
    )
    .expect("release marker JSON");
    assert_eq!(marker["reason"], "merged");

    let repeated = run_envoy_json(fixture.repository(), &["release", "88", "--json"]);
    assert_eq!(repeated["release"]["already_released"], true);
    assert_eq!(repeated["release"]["claim_id"], first_id);

    let reclaimed = run_envoy_json(fixture.repository(), &["claim", "88", "--json"]);
    assert_ne!(reclaimed["claim"]["claim_id"], first_id);
    assert_eq!(
        fs::read_dir(fixture.store_root().join("claims/88"))
            .expect("claim generations")
            .count(),
        2
    );
}

#[test]
fn release_without_any_claim_is_refused_without_creating_a_marker() {
    let fixture = RepositoryFixture::with_remote();
    let output = envoy()
        .current_dir(fixture.repository())
        .args(["release", "99", "--json"])
        .output()
        .expect("run release");

    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout).expect("error JSON");
    assert_eq!(value["status"], "blocked");
    assert!(!fixture.store_root().join("releases/99").exists());
}

#[test]
fn killed_claim_process_preserves_branch_and_worktree_phase_journals() {
    if std::env::var_os("ENVOY_LIFECYCLE_HELPER_MODE").is_some() {
        return;
    }
    for (checkpoint, expected_phase) in [
        ("branch", "branch_created"),
        ("worktree", "worktree_created"),
    ] {
        let fixture = RepositoryFixture::with_remote();
        let ready = fixture._root.path().join("ready");
        let mut child = spawn_lifecycle_helper(fixture.repository(), &ready, checkpoint);
        wait_for_file(&ready);
        child.kill().expect("kill claim helper");
        child.wait().expect("reap claim helper");

        let operations = fs::read_dir(fixture.store_root().join("operations"))
            .expect("operations directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("operation entries");
        assert_eq!(operations.len(), 1);
        let operation: Value = serde_json::from_slice(
            &fs::read(operations[0].path()).expect("read operation journal"),
        )
        .expect("operation JSON");
        assert_eq!(operation["phase"], expected_phase);
    }
}

#[test]
fn failed_rollback_keeps_cleanup_pending_operation_for_repair() {
    let fixture = RepositoryFixture::with_remote();
    let runner = FailingRollbackRunner {
        worktree_added: AtomicBool::new(false),
    };
    let issue = std::num::NonZeroU64::new(102).expect("positive issue");

    let error = claim_issue(&runner, fixture.repository(), issue)
        .expect_err("injected canonicalization failure");

    assert!(
        error.to_string().contains("rollback is incomplete"),
        "{error}"
    );
    let operations = fs::read_dir(fixture.store_root().join("operations"))
        .expect("operations directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("operation entries");
    assert_eq!(operations.len(), 1);
    let operation: Value =
        serde_json::from_slice(&fs::read(operations[0].path()).expect("read operation journal"))
            .expect("operation JSON");
    assert_eq!(operation["phase"], "cleanup_pending");
}

#[test]
fn rollback_removes_created_worktree_but_preserves_adopted_branch() {
    let fixture = RepositoryFixture::with_remote();
    fixture.git(&["branch", "adopted", "main"]);
    let original_tip = fixture.git_stdout(&["rev-parse", "adopted"]);
    let runner = FailCanonicalizationOnceRunner {
        worktree_added: AtomicBool::new(false),
        failure_injected: AtomicBool::new(false),
    };
    let issue = std::num::NonZeroU64::new(103).expect("positive issue");

    let error = claim_issue_with_options(
        &runner,
        fixture.repository(),
        issue,
        ClaimOptions {
            branch: Some("adopted".to_owned()),
            ..ClaimOptions::default()
        },
    )
    .expect_err("injected canonicalization failure");

    assert!(error.to_string().contains("worktree list"), "{error}");
    assert_eq!(fixture.git_stdout(&["rev-parse", "adopted"]), original_tip);
    let worktrees = fixture.git_stdout(&["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("branch refs/heads/adopted").count(), 0);
    let operations = fs::read_dir(fixture.store_root().join("operations"))
        .expect("operations directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("operation entries");
    assert!(operations.is_empty());
}

#[test]
fn lifecycle_subprocess_helper() {
    let Some(checkpoint) = std::env::var_os("ENVOY_LIFECYCLE_HELPER_MODE") else {
        return;
    };
    let repository = PathBuf::from(
        std::env::var_os("ENVOY_LIFECYCLE_HELPER_REPOSITORY").expect("helper repository"),
    );
    let ready =
        PathBuf::from(std::env::var_os("ENVOY_LIFECYCLE_HELPER_READY").expect("helper ready path"));
    let runner = CheckpointRunner {
        checkpoint: checkpoint.to_string_lossy().into_owned(),
        ready,
        worktree_added: AtomicBool::new(false),
    };
    let issue = std::num::NonZeroU64::new(101).expect("positive issue");
    claim_issue(&runner, &repository, issue).expect("claim eventually succeeds");
}

struct RepositoryFixture {
    _root: TempDir,
    repository: PathBuf,
}

impl RepositoryFixture {
    fn with_remote() -> Self {
        let fixture = Self::new();
        let remote = fixture._root.path().join("remote.git");
        git(
            fixture._root.path(),
            &["init", "-q", "--bare", path(&remote)],
        );
        fixture.git(&["remote", "add", "origin", path(&remote)]);
        fixture.git(&["push", "-q", "-u", "origin", "main"]);
        fixture.git(&["remote", "set-head", "origin", "main"]);
        fixture
    }

    fn new() -> Self {
        let root = TempDir::new().expect("temporary fixture root");
        let repository = root.path().join("fixture");
        fs::create_dir(&repository).expect("create repository directory");
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Envoy Tests"]);
        git(
            &repository,
            &["config", "user.email", "envoy@example.invalid"],
        );
        fs::write(repository.join("README.md"), "fixture\n").expect("write fixture file");
        git(&repository, &["add", "README.md"]);
        git(&repository, &["commit", "-qm", "initial"]);
        Self {
            _root: root,
            repository,
        }
    }

    fn repository(&self) -> &Path {
        &self.repository
    }

    fn git(&self, arguments: &[&str]) {
        git(&self.repository, arguments);
    }

    fn git_stdout(&self, arguments: &[&str]) -> String {
        git_stdout(&self.repository, arguments)
    }

    fn store_root(&self) -> PathBuf {
        PathBuf::from(self.git_stdout(&["rev-parse", "--path-format=absolute", "--git-common-dir"]))
            .join("envoy")
    }

    fn write_config(&self, contents: &str) {
        fs::create_dir_all(self.store_root()).expect("create store root");
        fs::write(self.store_root().join("config.yml"), contents).expect("write config");
    }
}

struct CheckpointRunner {
    checkpoint: String,
    ready: PathBuf,
    worktree_added: AtomicBool,
}

struct FailingRollbackRunner {
    worktree_added: AtomicBool,
}

struct FailCanonicalizationOnceRunner {
    worktree_added: AtomicBool,
    failure_injected: AtomicBool,
}

impl CommandRunner for FailCanonicalizationOnceRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        let args = spec
            .args
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();
        let worktree_add = args.first().is_some_and(|value| value == "worktree")
            && args.get(1).is_some_and(|value| value == "add");
        let canonical_list = self.worktree_added.load(Ordering::SeqCst)
            && args.first().is_some_and(|value| value == "worktree")
            && args.get(1).is_some_and(|value| value == "list")
            && !self.failure_injected.swap(true, Ordering::SeqCst);
        if canonical_list {
            return Ok(CommandOutput {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"injected canonicalization failure\n".to_vec(),
            });
        }
        let output = SystemRunner.run(spec)?;
        if worktree_add && output.exit_code == Some(0) {
            self.worktree_added.store(true, Ordering::SeqCst);
        }
        Ok(output)
    }
}

impl CommandRunner for FailingRollbackRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        let args = spec
            .args
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();
        let worktree_add = args.first().is_some_and(|value| value == "worktree")
            && args.get(1).is_some_and(|value| value == "add");
        let after_add = self.worktree_added.load(Ordering::SeqCst);
        let canonical_list = after_add
            && args.first().is_some_and(|value| value == "worktree")
            && args.get(1).is_some_and(|value| value == "list");
        let rollback_remove = after_add
            && args.first().is_some_and(|value| value == "worktree")
            && args.get(1).is_some_and(|value| value == "remove");
        if canonical_list || rollback_remove {
            return Ok(CommandOutput {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"injected lifecycle failure\n".to_vec(),
            });
        }
        let output = SystemRunner.run(spec)?;
        if worktree_add && output.exit_code == Some(0) {
            self.worktree_added.store(true, Ordering::SeqCst);
        }
        Ok(output)
    }
}

impl CommandRunner for CheckpointRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        let args = spec
            .args
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();
        let before_worktree_add = args.first().is_some_and(|value| value == "worktree")
            && args.get(1).is_some_and(|value| value == "add");
        let before_created_worktree_list = self.worktree_added.load(Ordering::SeqCst)
            && args.first().is_some_and(|value| value == "worktree")
            && args.get(1).is_some_and(|value| value == "list");
        if (self.checkpoint == "branch" && before_worktree_add)
            || (self.checkpoint == "worktree" && before_created_worktree_list)
        {
            fs::write(&self.ready, "ready").expect("signal checkpoint");
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        let output = SystemRunner.run(spec)?;
        if before_worktree_add && output.exit_code == Some(0) {
            self.worktree_added.store(true, Ordering::SeqCst);
        }
        Ok(output)
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = StdCommand::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run git");
    assert_success(&output);
}

fn git_stdout(directory: &Path, arguments: &[&str]) -> String {
    let output = StdCommand::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run git");
    assert_success(&output);
    String::from_utf8(output.stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_owned()
}

fn run_envoy_json(repository: &Path, arguments: &[&str]) -> Value {
    let output = envoy()
        .current_dir(repository)
        .args(arguments)
        .output()
        .expect("run Envoy");
    assert_success(&output);
    serde_json::from_slice(&output.stdout).expect("Envoy JSON")
}

fn spawn_envoy(repository: &Path, arguments: &[&str]) -> Child {
    StdCommand::new(env!("CARGO_BIN_EXE_gh-envoy"))
        .current_dir(repository)
        .args(arguments)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn Envoy")
}

fn wait(child: Child) -> Output {
    child.wait_with_output().expect("wait for Envoy")
}

fn assert_no_claim_git_or_operation(fixture: &RepositoryFixture, issue: u64) {
    let branches = fixture.git_stdout(&[
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/heads/envoy",
    ]);
    assert!(
        !branches
            .lines()
            .any(|line| line.starts_with(&format!("envoy/issue-{issue}-"))),
        "leaked branches: {branches}"
    );
    let worktrees = fixture.git_stdout(&["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "leaked worktree: {worktrees}"
    );
    assert!(
        fs::read_dir(fixture.store_root().join("operations"))
            .expect("operations directory")
            .next()
            .is_none(),
        "successful rollback removes operation journal"
    );
}

fn spawn_lifecycle_helper(repository: &Path, ready: &Path, checkpoint: &str) -> Child {
    StdCommand::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "lifecycle_subprocess_helper", "--nocapture"])
        .env("ENVOY_LIFECYCLE_HELPER_MODE", checkpoint)
        .env("ENVOY_LIFECYCLE_HELPER_REPOSITORY", repository)
        .env("ENVOY_LIFECYCLE_HELPER_READY", ready)
        .spawn()
        .expect("spawn lifecycle helper")
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn path(value: &Path) -> &str {
    value.to_str().expect("test path is UTF-8")
}

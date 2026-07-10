use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn envoy() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("gh-envoy").expect("gh-envoy binary should build")
}

#[test]
fn fresh_claim_and_marker_only_release_work_end_to_end() {
    let fixture = RepositoryFixture::with_remote();
    let expected_base = fixture.git_stdout(&["rev-parse", "refs/remotes/origin/main"]);
    let claim = fixture.envoy_json(&["claim", "123", "--json"], 0);

    assert_eq!(claim["status"], "success");
    assert_eq!(claim["claim"]["base_sha"], expected_base);
    assert_eq!(claim["claim"]["title"], Value::Null);
    let claim_id = claim["claim"]["claim_id"].as_str().expect("claim ID");
    let branch = claim["claim"]["branch"].as_str().expect("branch");
    let worktree = PathBuf::from(claim["claim"]["worktree"].as_str().expect("worktree"));
    assert!(branch.starts_with("envoy/issue-123-"));
    assert!(worktree.exists());
    let actual_parent = worktree
        .parent()
        .expect("worktree parent")
        .canonicalize()
        .expect("canonical worktree parent");
    let expected_parent = fixture
        .repository()
        .parent()
        .expect("repository parent")
        .canonicalize()
        .expect("canonical repository parent");
    assert_eq!(actual_parent, expected_parent);
    assert_eq!(fixture.git_stdout(&["rev-parse", branch]), expected_base);

    let release = fixture.envoy_json(&["release", "123", "--reason", "merged", "--json"], 0);
    assert_eq!(release["release"]["claim_id"], claim_id);
    assert_eq!(release["release"]["already_released"], false);
    assert!(worktree.exists());
    let repeated = fixture.envoy_json(&["release", "123", "--json"], 0);
    assert_eq!(repeated["release"]["already_released"], true);
}

#[test]
fn offline_local_base_fallback_is_explicit_and_successful() {
    let fixture = RepositoryFixture::new();
    let expected_base = fixture.git_stdout(&["rev-parse", "main"]);
    let claim = fixture.envoy_json(&["claim", "7", "--json"], 1);

    assert_eq!(claim["status"], "warning");
    assert_eq!(claim["claim"]["base_sha"], expected_base);
    assert!(
        claim["warnings"][0]
            .as_str()
            .expect("warning")
            .contains("local branch")
    );
}

struct RepositoryFixture {
    _root: TempDir,
    repository: PathBuf,
}

impl RepositoryFixture {
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

    fn repository(&self) -> &Path {
        &self.repository
    }

    fn git(&self, arguments: &[&str]) {
        git(&self.repository, arguments);
    }

    fn git_stdout(&self, arguments: &[&str]) -> String {
        git_stdout(&self.repository, arguments)
    }

    fn envoy_json(&self, arguments: &[&str], exit_code: i32) -> Value {
        let output = envoy()
            .current_dir(&self.repository)
            .args(arguments)
            .output()
            .expect("run Envoy");
        assert_eq!(
            output.status.code(),
            Some(exit_code),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("Envoy JSON")
    }
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn path(value: &Path) -> &str {
    value.to_str().expect("test path is UTF-8")
}

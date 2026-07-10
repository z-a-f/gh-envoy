use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

mod support;

use support::assert_same_existing_path;

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
    assert_same_existing_path(
        worktree.parent().expect("worktree parent"),
        fixture.repository().parent().expect("repository parent"),
    );
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

#[test]
fn claim_records_stack_generation_wait_refs_scope_and_note() {
    let fixture = RepositoryFixture::with_remote();
    let parent = fixture.envoy_json(&["claim", "122", "--json"], 0);
    let parent_id = parent["claim"]["claim_id"]
        .as_str()
        .expect("parent claim ID");
    let parent_branch = parent["claim"]["branch"].as_str().expect("parent branch");
    let parent_worktree = PathBuf::from(
        parent["claim"]["worktree"]
            .as_str()
            .expect("parent worktree"),
    );
    fs::write(parent_worktree.join("parent.txt"), "parent change\n").expect("write parent change");
    git(&parent_worktree, &["add", "parent.txt"]);
    git(&parent_worktree, &["commit", "-qm", "advance parent"]);
    let parent_tip = fixture.git_stdout(&["rev-parse", parent_branch]);

    let child = fixture.envoy_json(
        &[
            "claim",
            "123",
            "--onto",
            "122",
            "--after",
            "122",
            "--after",
            "124",
            "--scope",
            "src/**",
            "--scope",
            "tests/**",
            "--disallow",
            ".github/**",
            "--note",
            "coordinate manually",
            "--json",
        ],
        2,
    );
    assert_eq!(child["status"], "blocked");

    let child = fixture.envoy_json(
        &[
            "claim",
            "123",
            "--onto",
            "122",
            "--after",
            "124",
            "--scope",
            "src/**",
            "--scope",
            "tests/**",
            "--disallow",
            ".github/**",
            "--note",
            "coordinate manually",
            "--json",
        ],
        0,
    );

    assert_eq!(child["claim"]["base_issue"], 122);
    assert_eq!(child["claim"]["base_claim_id"], parent_id);
    assert_eq!(child["claim"]["base_ref"], parent_branch);
    assert_eq!(child["claim"]["base_sha"], parent_tip);
    let child_branch = child["claim"]["branch"].as_str().expect("child branch");
    assert_eq!(fixture.git_stdout(&["rev-parse", child_branch]), parent_tip);
    assert_eq!(child["claim"]["wait_for"][0]["issue"], 124);
    assert_eq!(child["claim"]["wait_for"][0]["claim_id"], Value::Null);
    assert_eq!(
        child["claim"]["declared_scope"]["allowed_paths"],
        serde_json::json!(["src/**", "tests/**"])
    );
    assert_eq!(
        child["claim"]["declared_scope"]["disallowed_paths"],
        serde_json::json!([".github/**"])
    );
    assert_eq!(child["claim"]["note"], "coordinate manually");
}

#[test]
fn reclaiming_parent_does_not_rewrite_existing_child_generation() {
    let fixture = RepositoryFixture::with_remote();
    let parent = fixture.envoy_json(&["claim", "122", "--json"], 0);
    let original_parent_id = parent["claim"]["claim_id"].as_str().expect("parent ID");
    let child = fixture.envoy_json(&["claim", "123", "--onto", "122", "--json"], 0);
    let child_id = child["claim"]["claim_id"].as_str().expect("child ID");

    fixture.envoy_json(&["release", "122", "--json"], 0);
    let replacement = fixture.envoy_json(&["claim", "122", "--json"], 0);
    assert_ne!(replacement["claim"]["claim_id"], original_parent_id);

    let persisted: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .store_root()
                .join("claims/123")
                .join(format!("{child_id}.json")),
        )
        .expect("read child claim"),
    )
    .expect("child claim JSON");
    assert_eq!(persisted["base_claim_id"], original_parent_id);
}

#[test]
fn after_captures_active_generation_and_never_infers_absent_generation() {
    let fixture = RepositoryFixture::with_remote();
    let dependency = fixture.envoy_json(&["claim", "114", "--json"], 0);
    let dependency_id = dependency["claim"]["claim_id"].as_str().expect("claim ID");

    let claim = fixture.envoy_json(
        &["claim", "113", "--after", "114", "--after", "115", "--json"],
        0,
    );

    assert_eq!(claim["claim"]["wait_for"][0]["issue"], 114);
    assert_eq!(claim["claim"]["wait_for"][0]["claim_id"], dependency_id);
    assert_eq!(claim["claim"]["wait_for"][1]["issue"], 115);
    assert_eq!(claim["claim"]["wait_for"][1]["claim_id"], Value::Null);
}

#[test]
fn branch_and_worktree_adoption_preserve_existing_git_state() {
    let fixture = RepositoryFixture::with_remote();
    fixture.git(&["branch", "existing-branch", "main"]);

    let branch_claim = fixture.envoy_json(
        &["claim", "201", "--branch", "existing-branch", "--json"],
        0,
    );
    let generated_worktree = PathBuf::from(
        branch_claim["claim"]["worktree"]
            .as_str()
            .expect("generated worktree"),
    );
    assert_eq!(branch_claim["claim"]["branch"], "existing-branch");
    assert!(generated_worktree.exists());
    let error = fixture.envoy_json(
        &["claim", "204", "--branch", "existing-branch", "--json"],
        2,
    );
    assert_eq!(error["status"], "blocked");

    fixture.git(&["branch", "worktree-branch", "main"]);
    let adopted_worktree = fixture._root.path().join("adopted-worktree");
    fixture.git(&[
        "worktree",
        "add",
        "-q",
        path(&adopted_worktree),
        "worktree-branch",
    ]);
    let worktree_claim = fixture.envoy_json(
        &[
            "claim",
            "202",
            "--worktree",
            path(&adopted_worktree),
            "--json",
        ],
        0,
    );

    assert_eq!(worktree_claim["claim"]["branch"], "worktree-branch");
    assert_same_existing_path(
        PathBuf::from(
            worktree_claim["claim"]["worktree"]
                .as_str()
                .expect("worktree"),
        ),
        &adopted_worktree,
    );

    fixture.git(&["branch", "registered-branch", "main"]);
    let registered_worktree = fixture._root.path().join("registered-worktree");
    fixture.git(&[
        "worktree",
        "add",
        "-q",
        path(&registered_worktree),
        "registered-branch",
    ]);
    let registered_claim = fixture.envoy_json(
        &["claim", "203", "--branch", "registered-branch", "--json"],
        0,
    );
    assert_same_existing_path(
        PathBuf::from(
            registered_claim["claim"]["worktree"]
                .as_str()
                .expect("registered worktree"),
        ),
        &registered_worktree,
    );

    fixture.git(&["branch", "paired-branch", "main"]);
    let paired_worktree = fixture._root.path().join("paired-worktree");
    fixture.git(&[
        "worktree",
        "add",
        "-q",
        path(&paired_worktree),
        "paired-branch",
    ]);
    let paired_claim = fixture.envoy_json(
        &[
            "claim",
            "205",
            "--branch",
            "paired-branch",
            "--worktree",
            "../paired-worktree",
            "--json",
        ],
        0,
    );
    assert_eq!(paired_claim["claim"]["branch"], "paired-branch");
}

#[test]
fn invalid_relationships_and_adoptions_are_blocked_without_moving_branches() {
    let fixture = RepositoryFixture::with_remote();
    for arguments in [
        vec!["claim", "7", "--onto", "7", "--json"],
        vec!["claim", "7", "--after", "7", "--json"],
        vec!["claim", "7", "--after", "8", "--after", "8", "--json"],
        vec!["claim", "7", "--onto", "8", "--after", "8", "--json"],
    ] {
        let error = fixture.envoy_json(&arguments, 2);
        assert_eq!(error["status"], "blocked");
    }
    let unregistered = fixture._root.path().join("unregistered-worktree");
    fs::create_dir(&unregistered).expect("create unregistered worktree path");
    let error = fixture.envoy_json(
        &["claim", "8", "--worktree", path(&unregistered), "--json"],
        2,
    );
    assert_eq!(error["status"], "blocked");
    for arguments in [
        vec!["claim", "8", "--onto", "999", "--json"],
        vec!["claim", "8", "--branch", "missing", "--json"],
        vec!["claim", "8", "--worktree", "missing-worktree", "--json"],
    ] {
        let error = fixture.envoy_json(&arguments, 2);
        assert_eq!(error["status"], "blocked");
    }

    fixture.git(&["switch", "--orphan", "unrelated"]);
    fs::write(fixture.repository().join("README.md"), "unrelated\n")
        .expect("replace unrelated README");
    fixture.git(&["add", "README.md"]);
    fixture.git(&["commit", "-qm", "unrelated root"]);
    let unrelated_tip = fixture.git_stdout(&["rev-parse", "unrelated"]);
    fixture.git(&["switch", "main"]);

    let error = fixture.envoy_json(&["claim", "9", "--branch", "unrelated", "--json"], 2);
    assert_eq!(error["status"], "blocked");
    assert_eq!(
        fixture.git_stdout(&["rev-parse", "unrelated"]),
        unrelated_tip
    );

    fixture.git(&["branch", "first", "main"]);
    fixture.git(&["branch", "second", "main"]);
    let mismatch = fixture._root.path().join("mismatch-worktree");
    fixture.git(&["worktree", "add", "-q", path(&mismatch), "first"]);
    let error = fixture.envoy_json(
        &[
            "claim",
            "10",
            "--branch",
            "second",
            "--worktree",
            path(&mismatch),
            "--json",
        ],
        2,
    );
    assert_eq!(error["status"], "blocked");

    let detached = fixture._root.path().join("detached-worktree");
    fixture.git(&["worktree", "add", "-q", "--detach", path(&detached), "main"]);
    let error = fixture.envoy_json(&["claim", "11", "--worktree", path(&detached), "--json"], 2);
    assert_eq!(error["status"], "blocked");
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

    fn store_root(&self) -> PathBuf {
        PathBuf::from(self.git_stdout(&["rev-parse", "--path-format=absolute", "--git-common-dir"]))
            .join("envoy")
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

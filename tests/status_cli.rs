use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use chrono::Utc;
use gh_envoy::model::{Claim, ReleaseMarker, ReleaseReason, SCHEMA_VERSION};
use gh_envoy::store::Store;
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

fn envoy() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("gh-envoy").expect("gh-envoy binary should build")
}

#[test]
fn status_is_identical_from_main_and_secondary_worktrees() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("feature", &base);
    let claim = fixture.persist_claim(11, "feature", &worktree, &base);

    for json in [false, true] {
        let main = fixture.status(fixture.repository(), json);
        let secondary = fixture.status(&worktree, json);
        assert_eq!(main.status.code(), Some(0));
        assert_eq!(main.stdout, secondary.stdout);
        assert_eq!(main.stderr, secondary.stderr);
        if json {
            let value: Value = serde_json::from_slice(&main.stdout).expect("status JSON");
            assert_eq!(
                value["claims"][0]["claim"]["claim_id"],
                claim.claim_id.to_string()
            );
            assert_eq!(value["claims"][0]["claim"]["worktree"], "…/feature");
            assert_eq!(value["claims"][0]["pr"], Value::Null);
            assert_eq!(value["claims"][0]["github_state"], "unverified");
        }
    }
}

#[test]
fn json_path_redaction_can_be_disabled_without_changing_storage() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("visible", &base);
    let claim = fixture.persist_claim(12, "visible", &worktree, &base);
    fixture.write_config("redact_paths_in_json: false\n");
    let before = fixture.claim_bytes(&claim);

    let output = fixture.status(fixture.repository(), true);

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(
        PathBuf::from(value["claims"][0]["claim"]["worktree"].as_str().unwrap()),
        worktree
    );
    assert_eq!(fixture.claim_bytes(&claim), before);
}

#[test]
fn released_claims_are_hidden_and_missing_state_warns_with_an_empty_diff() {
    let released_fixture = RepositoryFixture::new();
    let base = released_fixture.git_stdout(released_fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = released_fixture.add_worktree("released", &base);
    let released = released_fixture.persist_claim(13, "released", &worktree, &base);
    released_fixture.persist_release(&released);

    let hidden = released_fixture.status(released_fixture.repository(), true);
    assert_eq!(hidden.status.code(), Some(0));
    let hidden: Value = serde_json::from_slice(&hidden.stdout).expect("status JSON");
    assert_eq!(hidden["claims"], serde_json::json!([]));

    let broken_fixture = RepositoryFixture::new();
    let base = broken_fixture.git_stdout(broken_fixture.repository(), &["rev-parse", "HEAD"]);
    let missing = broken_fixture.claim(
        14,
        "missing",
        &broken_fixture.root.path().join("missing"),
        &base,
    );
    broken_fixture.persist(&missing);

    let warning = broken_fixture.status(broken_fixture.repository(), true);
    assert_eq!(warning.status.code(), Some(1));
    let warning: Value = serde_json::from_slice(&warning.stdout).expect("status JSON");
    assert_eq!(warning["status"], "warning");
    assert_eq!(
        warning["claims"][0]["diff"],
        serde_json::json!({
            "changed_paths": [],
            "added_paths": [],
            "modified_paths": [],
            "deleted_paths": [],
            "untracked_paths": []
        })
    );
    assert!(
        warning["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| {
                problem["code"] == "missing_branch" || problem["code"] == "missing_worktree"
            })
    );
    assert!(
        !warning
            .to_string()
            .contains(broken_fixture.root.path().to_str().unwrap())
    );
}

#[test]
fn status_outside_a_repository_is_an_operational_error() {
    let directory = TempDir::new().expect("temporary non-repository");

    let output = envoy()
        .current_dir(directory.path())
        .args(["status", "--json"])
        .output()
        .expect("run status");

    assert_eq!(output.status.code(), Some(3));
    let value: Value = serde_json::from_slice(&output.stdout).expect("error JSON");
    assert_eq!(value["command"], "status");
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], "operational_error");
}

struct RepositoryFixture {
    root: TempDir,
    repository: PathBuf,
}

impl RepositoryFixture {
    fn new() -> Self {
        let root = TempDir::new().expect("temporary fixture root");
        let repository = root.path().join("fixture");
        fs::create_dir(&repository).expect("create repository");
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Envoy Tests"]);
        git(
            &repository,
            &["config", "user.email", "envoy@example.invalid"],
        );
        fs::write(repository.join("README.md"), "fixture\n").expect("write fixture");
        git(&repository, &["add", "README.md"]);
        git(&repository, &["commit", "-qm", "initial"]);
        Self { root, repository }
    }

    fn repository(&self) -> &Path {
        &self.repository
    }

    fn add_worktree(&self, branch: &str, start: &str) -> PathBuf {
        let path = self.root.path().join(branch);
        git(
            &self.repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path_str(&path),
                start,
            ],
        );
        path.canonicalize().expect("canonical worktree")
    }

    fn status(&self, directory: &Path, json: bool) -> std::process::Output {
        let mut command = envoy();
        command.current_dir(directory).arg("status");
        if json {
            command.arg("--json");
        }
        command.output().expect("run status")
    }

    fn claim(&self, number: u64, branch: &str, worktree: &Path, base_sha: &str) -> Claim {
        Claim {
            schema_version: SCHEMA_VERSION.to_owned(),
            claim_id: Uuid::new_v4(),
            repo: "local/fixture".to_owned(),
            issue: issue(number),
            title: None,
            branch: branch.to_owned(),
            worktree: worktree.to_path_buf(),
            base_remote: "origin".to_owned(),
            base_ref: "main".to_owned(),
            base_sha: base_sha.to_owned(),
            base_issue: None,
            base_claim_id: None,
            wait_for: Vec::new(),
            declared_scope: None,
            note: None,
            created_at: Utc::now(),
        }
    }

    fn persist_claim(&self, number: u64, branch: &str, worktree: &Path, base_sha: &str) -> Claim {
        let claim = self.claim(number, branch, worktree, base_sha);
        self.persist(&claim);
        claim
    }

    fn persist(&self, claim: &Claim) {
        self.store()
            .lock()
            .expect("lock store")
            .create_claim(claim)
            .expect("persist claim");
    }

    fn persist_release(&self, claim: &Claim) {
        self.store()
            .lock()
            .expect("lock store")
            .create_release(&ReleaseMarker {
                schema_version: SCHEMA_VERSION.to_owned(),
                repo: claim.repo.clone(),
                issue: claim.issue,
                claim_id: claim.claim_id,
                reason: ReleaseReason::Manual,
                released_at: Utc::now(),
            })
            .expect("persist release");
    }

    fn write_config(&self, contents: &str) {
        fs::create_dir_all(self.store().root()).expect("create store root");
        fs::write(self.store().root().join("config.yml"), contents).expect("write config");
    }

    fn store(&self) -> Store {
        Store::new(
            PathBuf::from(self.git_stdout(
                &self.repository,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ))
            .join("envoy"),
        )
    }

    fn claim_bytes(&self, claim: &Claim) -> Vec<u8> {
        fs::read(
            self.store()
                .root()
                .join(format!("claims/{}/{}.json", claim.issue, claim.claim_id)),
        )
        .expect("read claim")
    }

    fn git_stdout(&self, directory: &Path, arguments: &[&str]) -> String {
        git_stdout(directory, arguments)
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(directory: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_owned()
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive issue")
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}

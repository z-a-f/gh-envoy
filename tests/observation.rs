use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use chrono::Utc;
use gh_envoy::command::SystemRunner;
use gh_envoy::model::{Claim, OperationKind, OperationPhase, OperationRecord, SCHEMA_VERSION};
use gh_envoy::observation::{LocalProblemCode, observe_repository};
use gh_envoy::store::Store;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn stacked_child_diff_excludes_parent_changes_from_every_worktree() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let parent_worktree = fixture.add_worktree("parent", &base);
    fs::write(parent_worktree.join("parent.txt"), "parent\n").expect("write parent change");
    fixture.git(&parent_worktree, &["add", "parent.txt"]);
    fixture.git(&parent_worktree, &["commit", "-qm", "parent change"]);
    let parent_tip = fixture.git_stdout(&parent_worktree, &["rev-parse", "HEAD"]);
    let child_worktree = fixture.add_worktree("child", &parent_tip);

    let parent = fixture.persist_claim(11, "parent", &parent_worktree, &base);
    let mut child = fixture.claim(12, "child", &child_worktree, &parent_tip);
    child.base_issue = Some(issue(11));
    child.base_claim_id = Some(parent.claim_id);
    fixture.persist(&child);

    let from_main = observe_repository(&SystemRunner, fixture.repository()).expect("observe main");
    assert!(
        from_main
            .claims
            .iter()
            .find(|observed| observed.claim.claim_id == child.claim_id)
            .expect("child observation")
            .diff
            .as_ref()
            .expect("child diff")
            .changed_paths
            .is_empty()
    );

    fs::write(parent_worktree.join("later.txt"), "later\n").expect("write later parent change");
    fixture.git(&parent_worktree, &["add", "later.txt"]);
    fixture.git(&parent_worktree, &["commit", "-qm", "advance parent"]);

    let from_main = observe_repository(&SystemRunner, fixture.repository()).expect("observe main");
    let from_child = observe_repository(&SystemRunner, &child_worktree).expect("observe child");
    assert_eq!(from_main, from_child);
    assert!(
        from_child
            .claims
            .iter()
            .find(|observed| observed.claim.claim_id == child.claim_id)
            .expect("child observation")
            .diff
            .as_ref()
            .expect("child diff")
            .changed_paths
            .is_empty()
    );
}

#[test]
fn observation_collects_every_tracked_state_and_excludes_ignored_files() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("feature", &base);
    fixture.persist_claim(21, "feature", &worktree, &base);

    fs::write(worktree.join("committed-added.txt"), "added\n").expect("write committed add");
    fs::write(worktree.join("committed-modified.txt"), "changed\n")
        .expect("write committed modification");
    fs::remove_file(worktree.join("committed-deleted.txt")).expect("delete committed file");
    fixture.git(&worktree, &["add", "-A"]);
    fixture.git(&worktree, &["commit", "-qm", "committed changes"]);

    fs::write(worktree.join("staged-added.txt"), "added\n").expect("write staged add");
    fs::write(worktree.join("staged-modified.txt"), "changed\n")
        .expect("write staged modification");
    fs::remove_file(worktree.join("staged-deleted.txt")).expect("delete staged file");
    fixture.git(&worktree, &["add", "-A"]);

    fs::write(worktree.join("unstaged-modified.txt"), "changed\n")
        .expect("write unstaged modification");
    fs::remove_file(worktree.join("unstaged-deleted.txt")).expect("delete unstaged file");
    fs::write(worktree.join("untracked.txt"), "untracked\n").expect("write untracked file");
    fs::write(worktree.join(" leading-space.txt"), "untracked\n")
        .expect("write untracked file with leading space");
    fs::write(worktree.join("ignored.log"), "ignored\n").expect("write ignored file");

    let observation = observe_repository(&SystemRunner, fixture.repository()).expect("observe");
    let diff = observation.claims[0].diff.as_ref().expect("complete diff");

    assert_eq!(
        diff.added_paths,
        ["committed-added.txt", "staged-added.txt"]
    );
    assert_eq!(
        diff.modified_paths,
        [
            "committed-modified.txt",
            "staged-modified.txt",
            "unstaged-modified.txt"
        ]
    );
    assert_eq!(
        diff.deleted_paths,
        [
            "committed-deleted.txt",
            "staged-deleted.txt",
            "unstaged-deleted.txt"
        ]
    );
    assert_eq!(
        diff.untracked_paths,
        [" leading-space.txt", "untracked.txt"]
    );
    assert_eq!(diff.changed_paths.len(), 8);
    assert!(!diff.changed_paths.iter().any(|path| path == "ignored.log"));
}

#[test]
fn missing_git_objects_and_worktrees_are_structured_read_only_problems() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("missing-base", &base);
    let missing_base = fixture.claim(31, "missing-base", &worktree, &base[..8]);
    fixture.persist(&missing_base);

    let missing_worktree_path = fixture.root.path().join("removed-worktree");
    let missing_worktree = fixture.claim(32, "missing-branch", &missing_worktree_path, &base);
    fixture.persist(&missing_worktree);
    fixture.git(fixture.repository(), &["branch", "wrong-branch", &base]);
    let mismatched = fixture.claim(33, "wrong-branch", &worktree, &base);
    fixture.persist(&mismatched);
    let before = fixture.claim_bytes(&missing_base);

    let observation = observe_repository(&SystemRunner, fixture.repository()).expect("observe");

    assert!(observation.problems.iter().any(|problem| {
        problem.code == LocalProblemCode::MissingBase
            && problem.claim_id == Some(missing_base.claim_id)
    }));
    assert!(observation.problems.iter().any(|problem| {
        problem.code == LocalProblemCode::MissingBranch
            && problem.claim_id == Some(missing_worktree.claim_id)
    }));
    assert!(observation.problems.iter().any(|problem| {
        problem.code == LocalProblemCode::MissingWorktree
            && problem.claim_id == Some(missing_worktree.claim_id)
    }));
    assert!(observation.problems.iter().any(|problem| {
        problem.code == LocalProblemCode::WorktreeMismatch
            && problem.claim_id == Some(mismatched.claim_id)
    }));
    assert_eq!(fixture.claim_bytes(&missing_base), before);
}

#[test]
fn abandoned_operations_are_visible_as_observed_problems() {
    let fixture = RepositoryFixture::new();
    let operation = OperationRecord {
        schema_version: SCHEMA_VERSION.to_owned(),
        operation_id: Uuid::new_v4(),
        kind: OperationKind::Claim,
        claim_id: Uuid::new_v4(),
        issue: issue(41),
        branch: "interrupted".to_owned(),
        worktree: fixture.repository().to_path_buf(),
        phase: OperationPhase::WorktreeCreated,
        started_at: Utc::now(),
    };
    fixture
        .store()
        .lock()
        .expect("lock store")
        .write_operation(&operation)
        .expect("write operation");

    let observation = observe_repository(&SystemRunner, fixture.repository()).expect("observe");

    assert_eq!(observation.operations, vec![operation.clone()]);
    assert!(observation.problems.iter().any(|problem| {
        problem.code == LocalProblemCode::AbandonedOperation
            && problem.operation_id == Some(operation.operation_id)
    }));
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
        fs::write(repository.join(".gitignore"), "*.log\n").expect("write ignore file");
        for name in [
            "committed-modified.txt",
            "committed-deleted.txt",
            "staged-modified.txt",
            "staged-deleted.txt",
            "unstaged-modified.txt",
            "unstaged-deleted.txt",
        ] {
            fs::write(repository.join(name), "initial\n").expect("write tracked fixture");
        }
        git(&repository, &["add", "."]);
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

    fn git(&self, directory: &Path, arguments: &[&str]) {
        git(directory, arguments);
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

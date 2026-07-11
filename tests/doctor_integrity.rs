use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use chrono::Utc;
use gh_envoy::command::SystemRunner;
use gh_envoy::doctor::{CheckStatus, GateRollup, doctor_repository};
use gh_envoy::model::{
    Claim, DeclaredScope, OperationKind, OperationPhase, OperationRecord, SCHEMA_VERSION,
    WaitForRef,
};
use gh_envoy::store::Store;
use tempfile::TempDir;
use uuid::Uuid;

mod support;

use support::assert_same_existing_path;

#[test]
fn valid_claim_passes_every_local_integrity_check() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("feature", &base);
    fixture.persist(&fixture.claim(11, "feature", &worktree, &base));

    let report = doctor_repository(&SystemRunner, fixture.repository(), Some(issue(11)))
        .expect("run doctor");

    assert_eq!(report.status, GateRollup::Ok);
    for id in [
        "integrity.claim_store",
        "integrity.claim_exists",
        "integrity.claim_schema",
        "integrity.worktree",
        "integrity.branch",
        "integrity.base",
        "integrity.diff",
        "integrity.ownership",
        "integrity.operation_journal",
    ] {
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.id == id && check.status == CheckStatus::Pass),
            "missing passing check {id}: {:?}",
            report.checks
        );
    }
}

#[test]
fn missing_claim_branch_worktree_and_base_block_without_mutation() {
    let fixture = RepositoryFixture::new();
    let claim = fixture.claim(
        21,
        "missing-branch",
        &fixture.root.path().join("missing-worktree"),
        "deadbeef",
    );
    fixture.persist(&claim);
    let before = fixture.claim_bytes(&claim);

    let report = doctor_repository(&SystemRunner, fixture.repository(), Some(issue(21)))
        .expect("run doctor");

    assert_eq!(report.status, GateRollup::Blocked);
    for id in ["integrity.worktree", "integrity.branch", "integrity.base"] {
        assert!(report.checks.iter().any(|check| {
            check.id == id && check.status == CheckStatus::Fail && check.required
        }));
    }
    assert!(report.checks.iter().any(|check| {
        check.id == "integrity.diff" && check.status == CheckStatus::Skip && !check.required
    }));
    assert!(report.recommendations.iter().any(|recommendation| {
        recommendation == "If issue #21 is stale, run: gh envoy release 21 --reason abandoned"
    }));
    assert_eq!(fixture.claim_bytes(&claim), before);
}

#[test]
fn issue_filter_still_detects_duplicate_worktree_ownership() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("shared", &base);
    fixture.persist(&fixture.claim(31, "shared", &worktree, &base));
    fixture.persist(&fixture.claim(32, "shared", &worktree, &base));

    let report = doctor_repository(&SystemRunner, fixture.repository(), Some(issue(31)))
        .expect("run doctor");

    let check = report
        .checks
        .iter()
        .find(|check| check.id == "integrity.ownership")
        .expect("ownership check");
    assert_eq!(check.status, CheckStatus::Fail);
    assert_eq!(
        check.evidence.as_ref().expect("evidence")["issues"],
        serde_json::json!([31, 32])
    );
}

#[test]
fn corrupt_claim_store_is_a_structured_error_check() {
    let fixture = RepositoryFixture::new();
    let path = fixture.store().root().join("claims/41/broken.json");
    fs::create_dir_all(path.parent().expect("claim parent")).expect("create claim directory");
    fs::write(&path, "{not-json\n").expect("write corrupt claim");

    let report = doctor_repository(&SystemRunner, fixture.repository(), None).expect("run doctor");

    assert_eq!(report.status, GateRollup::Error);
    assert!(report.checks.iter().any(|check| {
        check.id == "integrity.claim_store"
            && check.status == CheckStatus::Error
            && check.required
            && check.message.contains("invalid JSON")
    }));
}

#[test]
fn requested_issue_without_an_active_claim_is_blocked() {
    let fixture = RepositoryFixture::new();

    let report = doctor_repository(&SystemRunner, fixture.repository(), Some(issue(45)))
        .expect("run doctor");

    assert_eq!(report.status, GateRollup::Blocked);
    assert!(report.checks.iter().any(|check| {
        check.id == "integrity.claim_exists" && check.status == CheckStatus::Fail && check.required
    }));
}

#[test]
fn observation_failure_is_distinct_from_corrupt_store() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("bad-scope", &base);
    let mut claim = fixture.claim(46, "bad-scope", &worktree, &base);
    claim.declared_scope = Some(DeclaredScope {
        allowed_paths: vec!["[".to_owned()],
        disallowed_paths: Vec::new(),
    });
    fixture.persist(&claim);

    let report = doctor_repository(&SystemRunner, fixture.repository(), Some(issue(46)))
        .expect("run doctor");

    assert_eq!(report.status, GateRollup::Error);
    assert!(report.checks.iter().any(|check| {
        check.id == "integrity.observation" && check.status == CheckStatus::Error && check.required
    }));
}

#[test]
fn base_and_wait_for_cycles_are_publish_errors() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let first_worktree = fixture.add_worktree("cycle-first", &base);
    let second_worktree = fixture.add_worktree("cycle-second", &base);
    let mut first = fixture.claim(47, "cycle-first", &first_worktree, &base);
    let mut second = fixture.claim(48, "cycle-second", &second_worktree, &base);
    first.base_issue = Some(second.issue);
    first.base_claim_id = Some(second.claim_id);
    second.base_issue = Some(first.issue);
    second.base_claim_id = Some(first.claim_id);
    first.wait_for.push(WaitForRef {
        issue: second.issue,
        claim_id: Some(second.claim_id),
    });
    second.wait_for.push(WaitForRef {
        issue: first.issue,
        claim_id: None,
    });
    fixture.persist(&first);
    fixture.persist(&second);

    let report = doctor_repository(&SystemRunner, fixture.repository(), None).expect("run doctor");

    assert_eq!(report.status, GateRollup::Error);
    for id in ["publish.base_cycle", "publish.wait_for_cycle"] {
        assert!(report.checks.iter().any(|check| {
            check.id == id && check.status == CheckStatus::Error && check.required
        }));
    }
}

#[test]
fn abandoned_generated_worktree_has_safe_exact_recovery_commands() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let claim_id = Uuid::parse_str("12345678-1111-4111-8111-111111111111").unwrap();
    let branch = "envoy/issue-51-12345678";
    let worktree = fixture.add_worktree(branch, &base);
    let operation = OperationRecord {
        schema_version: SCHEMA_VERSION.to_owned(),
        operation_id: Uuid::new_v4(),
        kind: OperationKind::Claim,
        claim_id,
        issue: issue(51),
        branch: branch.to_owned(),
        worktree: worktree.clone(),
        phase: OperationPhase::WorktreeCreated,
        started_at: Utc::now(),
    };
    fixture
        .store()
        .lock()
        .expect("lock store")
        .write_operation(&operation)
        .expect("write operation");

    let report = doctor_repository(&SystemRunner, fixture.repository(), Some(issue(51)))
        .expect("run doctor");

    let check = report
        .checks
        .iter()
        .find(|check| check.id == "integrity.operation_journal")
        .expect("journal check");
    assert_eq!(check.status, CheckStatus::Fail);
    let recovery = &check.evidence.as_ref().expect("evidence")["recovery"];
    assert_eq!(
        recovery["commands"],
        serde_json::json!([
            {"program": "git", "args": ["worktree", "remove", "--", worktree]},
            {"program": "git", "args": ["branch", "-d", "--", branch]}
        ])
    );
    assert_same_existing_path(
        PathBuf::from(
            recovery["remove_journal"]
                .as_str()
                .expect("journal path string"),
        ),
        fixture.store().operation_path(operation.operation_id),
    );
    assert!(
        report
            .recommendations
            .iter()
            .any(|value| value.contains("git worktree remove --"))
    );
    assert!(
        report
            .recommendations
            .iter()
            .any(|value| value.contains("only after cleanup succeeds"))
    );
}

#[test]
fn branch_only_and_adopted_operations_get_conservative_recovery() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let generated_id = Uuid::parse_str("87654321-1111-4111-8111-111111111111").unwrap();
    let generated_branch = "envoy/issue-61-87654321";
    git(fixture.repository(), &["branch", generated_branch, &base]);
    let generated = fixture.operation(
        61,
        generated_id,
        generated_branch,
        &fixture.root.path().join("never-created"),
        OperationPhase::BranchCreated,
    );
    let adopted_branch = "user-owned-branch";
    git(fixture.repository(), &["branch", adopted_branch, &base]);
    let adopted = fixture.operation(
        62,
        Uuid::new_v4(),
        adopted_branch,
        &fixture.root.path().join("also-not-created"),
        OperationPhase::Reserved,
    );
    for operation in [&generated, &adopted] {
        fixture
            .store()
            .lock()
            .expect("lock store")
            .write_operation(operation)
            .expect("write operation");
    }

    let generated_report =
        doctor_repository(&SystemRunner, fixture.repository(), Some(generated.issue))
            .expect("run generated doctor");
    let generated_recovery = &generated_report
        .checks
        .iter()
        .find(|check| check.id == "integrity.operation_journal")
        .expect("generated journal check")
        .evidence
        .as_ref()
        .expect("generated evidence")["recovery"];
    assert_eq!(
        generated_recovery["commands"],
        serde_json::json!([{
            "program": "git",
            "args": ["branch", "-d", "--", generated_branch]
        }])
    );

    let adopted_report =
        doctor_repository(&SystemRunner, fixture.repository(), Some(adopted.issue))
            .expect("run adopted doctor");
    let adopted_recovery = &adopted_report
        .checks
        .iter()
        .find(|check| check.id == "integrity.operation_journal")
        .expect("adopted journal check")
        .evidence
        .as_ref()
        .expect("adopted evidence")["recovery"];
    assert_eq!(adopted_recovery["commands"], serde_json::json!([]));
    assert!(
        adopted_report
            .recommendations
            .iter()
            .any(|value| value.contains("will not recommend deleting"))
    );
}

#[test]
fn committed_claim_operation_retains_git_state_and_only_clears_journal() {
    let fixture = RepositoryFixture::new();
    let base = fixture.git_stdout(fixture.repository(), &["rev-parse", "HEAD"]);
    let worktree = fixture.add_worktree("committed", &base);
    let claim = fixture.claim(63, "committed", &worktree, &base);
    fixture.persist(&claim);
    let operation = fixture.operation(
        63,
        claim.claim_id,
        &claim.branch,
        &claim.worktree,
        OperationPhase::ClaimCommitted,
    );
    fixture
        .store()
        .lock()
        .expect("lock store")
        .write_operation(&operation)
        .expect("write operation");

    let report = doctor_repository(&SystemRunner, fixture.repository(), Some(issue(63)))
        .expect("run doctor");
    let recovery = &report
        .checks
        .iter()
        .find(|check| check.id == "integrity.operation_journal")
        .expect("journal check")
        .evidence
        .as_ref()
        .expect("evidence")["recovery"];

    assert_eq!(recovery["commands"], serde_json::json!([]));
    assert!(
        report
            .recommendations
            .iter()
            .any(|value| value.contains("committed claim is intact"))
    );
    assert!(worktree.exists());
    assert!(
        fixture
            .git_stdout(fixture.repository(), &["branch", "--list", "committed"])
            .contains("committed")
    );
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
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-qm", "initial"]);
        Self { root, repository }
    }

    fn repository(&self) -> &Path {
        &self.repository
    }

    fn add_worktree(&self, branch: &str, start: &str) -> PathBuf {
        let path = self.root.path().join(branch.replace('/', "-"));
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

    fn persist(&self, claim: &Claim) {
        self.store()
            .lock()
            .expect("lock store")
            .create_claim(claim)
            .expect("persist claim");
    }

    fn operation(
        &self,
        number: u64,
        claim_id: Uuid,
        branch: &str,
        worktree: &Path,
        phase: OperationPhase,
    ) -> OperationRecord {
        OperationRecord {
            schema_version: SCHEMA_VERSION.to_owned(),
            operation_id: Uuid::new_v4(),
            kind: OperationKind::Claim,
            claim_id,
            issue: issue(number),
            branch: branch.to_owned(),
            worktree: worktree.to_path_buf(),
            phase,
            started_at: Utc::now(),
        }
    }

    fn store(&self) -> Store {
        Store::new(
            PathBuf::from(self.git_stdout(
                self.repository(),
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

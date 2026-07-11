use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use chrono::{TimeZone, Utc};
use gh_envoy::list::{ClaimList, ClaimListEntry, ClaimState, render_claim_list_human_colored};
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
fn list_shows_active_and_released_claim_generations() {
    let fixture = RepositoryFixture::new();
    let released = fixture.claim(7, "released", "11111111-1111-4111-8111-111111111111", 18);
    let active = fixture.claim(7, "active", "22222222-2222-4222-8222-222222222222", 19);
    fixture.persist(&released);
    fixture.release(&released);
    fixture.persist(&active);

    let json = envoy()
        .current_dir(fixture.repository())
        .args(["list", "--json"])
        .output()
        .expect("run JSON list");
    assert_eq!(json.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&json.stdout).expect("list JSON");
    assert_eq!(json["command"], "list");
    assert_eq!(json["status"], "success");
    assert_eq!(json["claims"].as_array().unwrap().len(), 2);
    assert_eq!(json["claims"][0]["state"], "released");
    assert_eq!(json["claims"][0]["release"]["reason"], "abandoned");
    assert_eq!(json["claims"][1]["state"], "active");
    assert_eq!(json["claims"][1]["release"], Value::Null);
    assert_eq!(json["claims"][1]["claim"]["worktree"], "…/active");

    let human = envoy()
        .current_dir(fixture.repository())
        .arg("list")
        .output()
        .expect("run human list");
    assert_eq!(human.status.code(), Some(0));
    let human = String::from_utf8(human.stdout).expect("human list");
    assert!(human.starts_with("Claim history: 2 generations (1 active, 1 released)\n"));
    assert!(human.contains("○ #7 11111111  released (abandoned)"));
    assert!(human.contains("● #7 22222222  active"));
    assert!(human.contains("Branch    active"));
    assert!(human.contains("Branch    released"));
}

#[test]
fn list_is_read_only_when_no_claims_exist() {
    let fixture = RepositoryFixture::new();

    let output = envoy()
        .current_dir(fixture.repository())
        .arg("list")
        .output()
        .expect("run empty list");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"No claims have been recorded.\n");
    assert!(!fixture.repository().join(".git/envoy").exists());
}

#[test]
fn list_honors_unredacted_json_configuration() {
    let fixture = RepositoryFixture::new();
    let claim = fixture.claim(9, "visible", "33333333-3333-4333-8333-333333333333", 18);
    fixture.persist(&claim);
    fs::write(
        fixture.repository().join(".git/envoy/config.yml"),
        "redact_paths_in_json: false\n",
    )
    .expect("write config");

    let output = envoy()
        .current_dir(fixture.repository())
        .args(["list", "--json"])
        .output()
        .expect("run JSON list");

    assert_eq!(output.status.code(), Some(0));
    let output: Value = serde_json::from_slice(&output.stdout).expect("list JSON");
    assert_eq!(
        PathBuf::from(output["claims"][0]["claim"]["worktree"].as_str().unwrap()),
        claim.worktree
    );
}

#[test]
fn colored_list_styles_active_and_released_entries() {
    let fixture = RepositoryFixture::new();
    let active = fixture.claim(10, "active", "44444444-4444-4444-8444-444444444444", 18);
    let released = [
        (
            11,
            "closed",
            "55555555-5555-4555-8555-555555555555",
            ReleaseReason::Closed,
        ),
        (
            12,
            "merged",
            "66666666-6666-4666-8666-666666666666",
            ReleaseReason::Merged,
        ),
        (
            13,
            "manual",
            "77777777-7777-4777-8777-777777777777",
            ReleaseReason::Manual,
        ),
    ]
    .into_iter()
    .map(|(issue, branch, id, reason)| {
        let claim = fixture.claim(issue, branch, id, 19);
        let release = ReleaseMarker {
            schema_version: SCHEMA_VERSION.to_owned(),
            repo: claim.repo.clone(),
            issue: claim.issue,
            claim_id: claim.claim_id,
            reason,
            released_at: Utc.with_ymd_and_hms(2026, 7, 10, 20, 0, 0).unwrap(),
        };
        ClaimListEntry {
            claim,
            state: ClaimState::Released,
            release: Some(release),
        }
    });
    let mut claims = vec![ClaimListEntry {
        claim: active,
        state: ClaimState::Active,
        release: None,
    }];
    claims.extend(released);
    let list = ClaimList { claims };

    let rendered = render_claim_list_human_colored(&list);

    assert!(rendered.contains("\u{1b}[32m●\u{1b}[0m #10"));
    assert!(rendered.contains("\u{1b}[2m○\u{1b}[0m #11"));
    assert!(rendered.contains("released (closed)"));
    assert!(rendered.contains("released (merged)"));
    assert!(rendered.contains("released (manual)"));
    assert!(rendered.contains("\u{1b}[2mBranch\u{1b}[0m"));
}

#[test]
fn list_outside_a_repository_is_an_operational_error() {
    let directory = TempDir::new().expect("temporary non-repository");

    let output = envoy()
        .current_dir(directory.path())
        .args(["list", "--json"])
        .output()
        .expect("run list");

    assert_eq!(output.status.code(), Some(3));
    let output: Value = serde_json::from_slice(&output.stdout).expect("error JSON");
    assert_eq!(output["command"], "list");
    assert_eq!(output["status"], "error");
    assert_eq!(output["error"]["code"], "operational_error");
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
        Self { root, repository }
    }

    fn repository(&self) -> &Path {
        &self.repository
    }

    fn claim(&self, number: u64, branch: &str, id: &str, hour: u32) -> Claim {
        Claim {
            schema_version: SCHEMA_VERSION.to_owned(),
            claim_id: Uuid::parse_str(id).unwrap(),
            repo: "local/fixture".to_owned(),
            issue: issue(number),
            title: None,
            branch: branch.to_owned(),
            worktree: self.root.path().join(branch),
            base_remote: "origin".to_owned(),
            base_ref: "main".to_owned(),
            base_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            base_issue: None,
            base_claim_id: None,
            wait_for: Vec::new(),
            declared_scope: None,
            note: None,
            created_at: Utc.with_ymd_and_hms(2026, 7, 10, hour, 0, 0).unwrap(),
        }
    }

    fn persist(&self, claim: &Claim) {
        self.store()
            .lock()
            .expect("lock store")
            .create_claim(claim)
            .expect("persist claim");
    }

    fn release(&self, claim: &Claim) {
        self.store()
            .lock()
            .expect("lock store")
            .create_release(&ReleaseMarker {
                schema_version: SCHEMA_VERSION.to_owned(),
                repo: claim.repo.clone(),
                issue: claim.issue,
                claim_id: claim.claim_id,
                reason: ReleaseReason::Abandoned,
                released_at: Utc.with_ymd_and_hms(2026, 7, 10, 20, 0, 0).unwrap(),
            })
            .expect("release claim");
    }

    fn store(&self) -> Store {
        Store::new(self.repository.join(".git/envoy"))
    }
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .status()
        .expect("run git");
    assert!(status.success());
}

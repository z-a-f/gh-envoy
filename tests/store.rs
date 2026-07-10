use std::env;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use gh_envoy::model::{
    Claim, DeclaredScope, OperationKind, OperationPhase, OperationRecord, ReleaseMarker,
    ReleaseReason, SCHEMA_VERSION,
};
use gh_envoy::store::{PreparedAtomicWrite, Store};
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn read_only_listing_does_not_create_store_state() {
    let common_dir = TempDir::new().expect("temporary common directory");
    let root = common_dir.path().join("envoy");
    let store = Store::new(root.clone());

    let claims = store.list_claims(issue(7)).expect("list absent claims");

    assert!(claims.is_empty());
    assert!(!root.exists());
}

#[test]
fn locked_store_creates_layout_and_preserves_claim_generations() {
    let common_dir = TempDir::new().expect("temporary common directory");
    let root = common_dir.path().join("envoy");
    let store = Store::new(root.clone());
    let first = claim(common_dir.path(), Uuid::new_v4());
    let second = claim(common_dir.path(), Uuid::new_v4());

    {
        let locked = store.lock().expect("lock store");
        locked.create_claim(&first).expect("write first claim");
        locked.create_claim(&second).expect("write second claim");
        assert!(locked.create_claim(&first).is_err(), "claims are immutable");
    }

    for path in ["lock", "operations", "claims", "releases"] {
        assert!(root.join(path).exists(), "missing {path}");
    }
    assert!(!root.join("config.yml").exists());
    let claims = store.list_claims(issue(7)).expect("read claim history");
    assert_eq!(claims.len(), 2);
    assert!(claims.iter().any(|claim| claim.claim_id == first.claim_id));
    assert!(claims.iter().any(|claim| claim.claim_id == second.claim_id));
}

#[test]
fn operation_updates_replace_whole_json_and_release_markers_are_immutable() {
    let common_dir = TempDir::new().expect("temporary common directory");
    let store = Store::new(common_dir.path().join("envoy"));
    let operation_id = Uuid::new_v4();
    let mut operation = operation(common_dir.path(), operation_id);
    let release = release(Uuid::new_v4());

    {
        let locked = store.lock().expect("lock store");
        locked
            .write_operation(&operation)
            .expect("write reserved operation");
        operation.phase = OperationPhase::WorktreeCreated;
        locked
            .write_operation(&operation)
            .expect("replace operation");
        locked
            .create_release(&release)
            .expect("write release marker");
        assert!(locked.create_release(&release).is_err());
    }

    let persisted = store
        .read_operation(operation_id)
        .expect("read operation")
        .expect("operation exists");
    assert_eq!(persisted.phase, OperationPhase::WorktreeCreated);
    let bytes = fs::read(store.operation_path(operation_id)).expect("read operation JSON");
    serde_json::from_slice::<Value>(&bytes).expect("operation is complete JSON");
}

#[test]
fn advisory_lock_serializes_processes_and_releases_when_holder_dies() {
    if env::var_os("ENVOY_STORE_HELPER_MODE").is_some() {
        return;
    }
    let directory = TempDir::new().expect("temporary store");
    let root = directory.path().join("envoy");
    let holder_ready = directory.path().join("holder-ready");
    let waiter_acquired = directory.path().join("waiter-acquired");
    let mut holder = spawn_helper("hold-lock", &root, &holder_ready);
    wait_for_file(&holder_ready);
    let mut waiter = spawn_helper("acquire-lock", &root, &waiter_acquired);

    thread::sleep(Duration::from_millis(150));
    assert!(!waiter_acquired.exists(), "second process bypassed lock");

    holder.kill().expect("kill lock holder");
    holder.wait().expect("reap lock holder");
    wait_for_file(&waiter_acquired);
    assert!(waiter.wait().expect("reap waiter").success());
}

#[test]
fn interruption_before_atomic_rename_never_tears_destination_json() {
    if env::var_os("ENVOY_STORE_HELPER_MODE").is_some() {
        return;
    }
    let directory = TempDir::new().expect("temporary directory");
    let destination = directory.path().join("operation.json");
    let ready = directory.path().join("prepared");
    fs::write(&destination, b"{\"generation\":\"old\"}\n").expect("write old JSON");

    let mut child = spawn_helper("prepare-write", &destination, &ready);
    wait_for_file(&ready);
    child.kill().expect("kill prepared writer");
    child.wait().expect("reap prepared writer");

    let value: Value = serde_json::from_slice(&fs::read(&destination).expect("read destination"))
        .expect("destination remains valid JSON");
    assert_eq!(value, json!({"generation": "old"}));
}

#[test]
fn store_subprocess_helper() {
    let Some(mode) = env::var_os("ENVOY_STORE_HELPER_MODE") else {
        return;
    };
    let target = PathBuf::from(env::var_os("ENVOY_STORE_HELPER_TARGET").expect("helper target"));
    let ready = PathBuf::from(env::var_os("ENVOY_STORE_HELPER_READY").expect("helper ready"));

    match mode.to_str().expect("helper mode is UTF-8") {
        "hold-lock" => {
            let store = Store::new(target);
            let _locked = store.lock().expect("child locks store");
            fs::write(ready, b"ready").expect("signal lock acquired");
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        "acquire-lock" => {
            let store = Store::new(target);
            let _locked = store.lock().expect("waiter locks store");
            fs::write(ready, b"acquired").expect("signal lock acquired");
        }
        "prepare-write" => {
            let value = json!({"generation": "new", "payload": "x".repeat(1024 * 1024)});
            let _prepared =
                PreparedAtomicWrite::json(&target, &value).expect("prepare atomic JSON");
            fs::write(ready, b"prepared").expect("signal prepared write");
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        other => panic!("unknown helper mode {other}"),
    }
}

fn spawn_helper(mode: &str, target: &Path, ready: &Path) -> Child {
    Command::new(env::current_exe().expect("current test executable"))
        .args(["--exact", "store_subprocess_helper", "--nocapture"])
        .env("ENVOY_STORE_HELPER_MODE", mode)
        .env("ENVOY_STORE_HELPER_TARGET", target)
        .env("ENVOY_STORE_HELPER_READY", ready)
        .spawn()
        .expect("spawn store helper")
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive issue")
}

fn claim(worktree: &Path, claim_id: Uuid) -> Claim {
    Claim {
        schema_version: SCHEMA_VERSION.to_owned(),
        claim_id,
        repo: "z-a-f/fixture".to_owned(),
        issue: issue(7),
        title: Some("Fixture".to_owned()),
        branch: format!("envoy/issue-7-{}", &claim_id.to_string()[..8]),
        worktree: worktree.to_path_buf(),
        base_remote: "origin".to_owned(),
        base_ref: "main".to_owned(),
        base_sha: "0123456789abcdef".to_owned(),
        base_issue: None,
        base_claim_id: None,
        wait_for: Vec::new(),
        declared_scope: Some(DeclaredScope::default()),
        note: None,
        created_at: Utc.with_ymd_and_hms(2026, 7, 10, 18, 0, 0).unwrap(),
    }
}

fn operation(worktree: &Path, operation_id: Uuid) -> OperationRecord {
    OperationRecord {
        schema_version: SCHEMA_VERSION.to_owned(),
        operation_id,
        kind: OperationKind::Claim,
        claim_id: Uuid::new_v4(),
        issue: issue(7),
        branch: "envoy/issue-7-fixture".to_owned(),
        worktree: worktree.to_path_buf(),
        phase: OperationPhase::Reserved,
        started_at: Utc.with_ymd_and_hms(2026, 7, 10, 18, 0, 0).unwrap(),
    }
}

fn release(claim_id: Uuid) -> ReleaseMarker {
    ReleaseMarker {
        schema_version: SCHEMA_VERSION.to_owned(),
        repo: "z-a-f/fixture".to_owned(),
        issue: issue(7),
        claim_id,
        reason: ReleaseReason::Manual,
        released_at: Utc.with_ymd_and_hms(2026, 7, 10, 19, 0, 0).unwrap(),
    }
}

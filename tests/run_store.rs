use std::fs;
use std::num::NonZeroU64;
use std::process::Command;

use chrono::{TimeZone, Utc};
use gh_envoy::model::{Claim, RunMode, RunState, RunTransition, SCHEMA_VERSION};
use gh_envoy::store::{NewRun, Store, StoreError};
use tempfile::TempDir;
use uuid::Uuid;

mod support;

use support::assert_text_eq;

#[test]
fn run_creation_binds_an_exact_persisted_claim_and_keeps_artifacts_private() {
    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let prompt = b"private prompt token";

    let run = fixture
        .store
        .lock()
        .unwrap()
        .create_run(&claim, fixture.new_run(prompt, RunMode::Background))
        .expect("create run");

    assert_eq!(run.claim_id, claim.claim_id);
    assert_eq!(run.issue, claim.issue);
    assert_eq!(run.worktree, claim.worktree);
    assert_eq!(run.state, RunState::Queued);
    assert!(fixture.store.list_operations().unwrap().is_empty());
    assert_eq!(fs::read(&run.prompt_path).unwrap(), prompt);
    assert_eq!(fs::read(&run.stdout_path).unwrap(), b"");
    assert_eq!(fs::read(&run.stderr_path).unwrap(), b"");
    assert!(
        !fixture
            .store
            .run_directory(run.run_id)
            .join("stop-request")
            .exists()
    );
    let json = serde_json::to_string(&run).unwrap();
    assert!(!json.contains("private prompt token"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(run.prompt_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(fixture.store.run_directory(run.run_id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(fixture.store.run_directory(run.run_id).join("run.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn malformed_run_records_reject_unknown_versions_relative_paths_and_missing_identity() {
    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let run = fixture
        .store
        .lock()
        .unwrap()
        .create_run(&claim, fixture.new_run(b"prompt", RunMode::Interactive))
        .unwrap();
    let valid = run.to_value().unwrap();

    let mut wrong_version = valid.clone();
    wrong_version["schema_version"] = serde_json::json!("9.9");
    let mut relative_worktree = valid.clone();
    relative_worktree["worktree"] = serde_json::json!("relative/path");
    let mut missing_identity = valid;
    missing_identity.as_object_mut().unwrap().remove("claim_id");

    let mut interactive_worker = serde_json::to_value(&run).unwrap();
    interactive_worker["worker_pid"] = serde_json::json!(7);
    let mut reversed_time = serde_json::to_value(&run).unwrap();
    reversed_time["state"] = serde_json::json!("running");
    reversed_time["child_pid"] = serde_json::json!(8);
    reversed_time["started_at"] = serde_json::json!("2026-07-10T18:00:00Z");
    let mut child_without_start = serde_json::to_value(&run).unwrap();
    child_without_start["child_pid"] = serde_json::json!(9);

    for invalid in [
        wrong_version,
        relative_worktree,
        missing_identity,
        interactive_worker,
        reversed_time,
        child_without_start,
    ] {
        assert!(gh_envoy::model::RunRecord::from_value(invalid).is_err());
    }
}

#[test]
fn run_creation_rejects_an_unpersisted_or_mismatched_claim_generation() {
    let fixture = RunFixture::new();
    let claim = fixture.claim();

    assert!(matches!(
        fixture
            .store
            .lock()
            .unwrap()
            .create_run(&claim, fixture.new_run(b"prompt", RunMode::Interactive),),
        Err(StoreError::MissingClaimGeneration { .. })
    ));

    fixture.persist_claim(&claim);
    let mut mismatched = claim.clone();
    mismatched.branch = "different".to_owned();
    assert!(matches!(
        fixture.store.lock().unwrap().create_run(
            &mismatched,
            fixture.new_run(b"prompt", RunMode::Interactive),
        ),
        Err(StoreError::ClaimGenerationMismatch { .. })
    ));
}

#[test]
fn run_id_collisions_preserve_the_existing_run_and_failed_creation_rolls_back() {
    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let run_id = Uuid::new_v4();
    let existing = fixture
        .store
        .lock()
        .unwrap()
        .create_run(
            &claim,
            fixture.new_run_with_id(run_id, b"original prompt", RunMode::Background),
        )
        .unwrap();
    let original_record = fs::read(fixture.store.run_directory(run_id).join("run.json")).unwrap();

    assert!(matches!(
        fixture.store.lock().unwrap().create_run(
            &claim,
            fixture.new_run_with_id(run_id, b"replacement prompt", RunMode::Background),
        ),
        Err(StoreError::RunAlreadyExists { .. })
    ));
    assert_eq!(fixture.store.read_run(run_id).unwrap(), Some(existing));
    assert_eq!(
        fs::read(fixture.store.run_directory(run_id).join("run.json")).unwrap(),
        original_record
    );

    let invalid_id = Uuid::new_v4();
    assert!(
        fixture
            .store
            .lock()
            .unwrap()
            .create_run(
                &claim,
                NewRun {
                    run_id: invalid_id,
                    agent: "  ",
                    mode: RunMode::Background,
                    prompt: b"must be removed",
                    created_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 0, 0).unwrap(),
                },
            )
            .is_err()
    );
    assert!(!fixture.store.run_directory(invalid_id).exists());
    assert!(fixture.store.list_operations().unwrap().is_empty());
}

#[test]
fn run_transitions_are_atomic_and_terminal_states_are_immutable() {
    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let run = fixture
        .store
        .lock()
        .unwrap()
        .create_run(&claim, fixture.new_run(b"prompt", RunMode::Interactive))
        .unwrap();

    let started = fixture
        .store
        .lock()
        .unwrap()
        .transition_run(
            run.run_id,
            RunTransition::Start {
                worker_pid: None,
                child_pid: 42,
                started_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 1, 0).unwrap(),
            },
        )
        .unwrap();
    assert_eq!(started.state, RunState::Running);

    let finished = fixture
        .store
        .lock()
        .unwrap()
        .transition_run(
            run.run_id,
            RunTransition::Exit {
                exit_code: 0,
                finished_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 2, 0).unwrap(),
            },
        )
        .unwrap();
    assert_eq!(finished.state, RunState::Succeeded);
    assert_eq!(finished.exit_code, Some(0));

    assert!(matches!(
        fixture
            .store
            .lock()
            .unwrap()
            .transition_run(run.run_id, RunTransition::RequestStop,),
        Err(StoreError::InvalidRunTransition { .. })
    ));
    assert_eq!(fixture.store.read_run(run.run_id).unwrap(), Some(finished));
}

#[test]
fn run_validation_covers_spawn_failure_stop_and_inconsistent_terminal_fields() {
    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);

    let spawn = fixture
        .store
        .lock()
        .unwrap()
        .create_run(&claim, fixture.new_run(b"prompt", RunMode::Background))
        .unwrap();
    let failed = fixture
        .store
        .lock()
        .unwrap()
        .transition_run(
            spawn.run_id,
            RunTransition::SpawnFailed {
                worker_pid: Some(9),
                finished_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 1, 0).unwrap(),
                error: "executable not found".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(failed.state, RunState::Failed);
    assert_eq!(failed.started_at, None);

    let stopping = fixture
        .store
        .lock()
        .unwrap()
        .create_run(
            &claim,
            fixture.new_run_with_id(Uuid::new_v4(), b"prompt", RunMode::Background),
        )
        .unwrap();
    fixture
        .store
        .lock()
        .unwrap()
        .transition_run(stopping.run_id, RunTransition::RequestStop)
        .unwrap();
    let stopped = fixture
        .store
        .lock()
        .unwrap()
        .transition_run(
            stopping.run_id,
            RunTransition::Stopped {
                finished_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 3, 0).unwrap(),
                exit_code: None,
            },
        )
        .unwrap();
    assert_eq!(stopped.state, RunState::Stopped);

    let mut invalid = serde_json::to_value(stopped).unwrap();
    invalid["state"] = serde_json::json!("succeeded");
    invalid["exit_code"] = serde_json::Value::Null;
    assert!(gh_envoy::model::RunRecord::from_value(invalid).is_err());

    let mut stopped_without_child =
        serde_json::to_value(fixture.store.read_run(stopping.run_id).unwrap().unwrap()).unwrap();
    stopped_without_child["exit_code"] = serde_json::json!(130);
    assert!(gh_envoy::model::RunRecord::from_value(stopped_without_child).is_err());
}

#[test]
fn missing_artifacts_are_structured_run_store_errors() {
    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let run = fixture
        .store
        .lock()
        .unwrap()
        .create_run(&claim, fixture.new_run(b"prompt", RunMode::Background))
        .unwrap();
    fs::remove_file(&run.stdout_path).unwrap();

    assert!(matches!(
        fixture.store.list_runs(),
        Err(StoreError::InvalidRunStore { .. })
    ));
    let inventory = fixture.store.inspect_runs().unwrap();
    assert!(inventory.runs.is_empty());
    assert_eq!(inventory.problems.len(), 1);
    assert!(inventory.problems[0].message.contains("stdout.log"));
}

#[test]
fn run_lookup_rejects_misplaced_records_and_non_file_artifacts() {
    let empty = RunFixture::new();
    let missing_id = Uuid::new_v4();
    assert_eq!(empty.store.read_run(missing_id).unwrap(), None);
    assert!(matches!(
        empty
            .store
            .lock()
            .unwrap()
            .transition_run(missing_id, RunTransition::RequestStop),
        Err(StoreError::RunNotFound { .. })
    ));

    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let run = fixture
        .store
        .lock()
        .unwrap()
        .create_run(&claim, fixture.new_run(b"prompt", RunMode::Background))
        .unwrap();
    let record_path = fixture.store.run_directory(run.run_id).join("run.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    value["prompt_path"] = serde_json::json!(fixture.root.path().join("elsewhere/prompt.md"));
    fs::write(&record_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    assert!(matches!(
        fixture.store.list_runs(),
        Err(StoreError::InvalidRunStore { .. })
    ));

    let other = RunFixture::new();
    let claim = other.claim();
    other.persist_claim(&claim);
    let run = other
        .store
        .lock()
        .unwrap()
        .create_run(&claim, other.new_run(b"prompt", RunMode::Background))
        .unwrap();
    fs::remove_file(&run.stderr_path).unwrap();
    fs::create_dir(&run.stderr_path).unwrap();
    let problem = other.store.inspect_runs().unwrap().problems.pop().unwrap();
    assert!(problem.message.contains("not a regular file"));
}

#[test]
fn run_listing_is_read_only_deterministic_and_active_first() {
    let empty = TempDir::new().unwrap();
    let empty_store = Store::new(empty.path().join("missing/envoy"));
    assert!(empty_store.list_runs().unwrap().is_empty());
    assert!(!empty_store.root().exists());

    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let first = fixture
        .store
        .lock()
        .unwrap()
        .create_run(
            &claim,
            fixture.new_run_with_id(
                Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
                b"first private prompt",
                RunMode::Interactive,
            ),
        )
        .unwrap();
    fixture
        .store
        .lock()
        .unwrap()
        .transition_run(
            first.run_id,
            RunTransition::SpawnFailed {
                worker_pid: None,
                finished_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 1, 0).unwrap(),
                error: "spawn failed".to_owned(),
            },
        )
        .unwrap();
    let second = fixture
        .store
        .lock()
        .unwrap()
        .create_run(
            &claim,
            NewRun {
                run_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
                agent: "codex",
                mode: RunMode::Background,
                prompt: b"second private prompt",
                created_at: Utc.with_ymd_and_hms(2026, 7, 11, 19, 0, 0).unwrap(),
            },
        )
        .unwrap();

    let runs = fixture.store.list_runs().unwrap();
    assert_eq!(
        runs.iter().map(|run| run.run_id).collect::<Vec<_>>(),
        vec![second.run_id, first.run_id]
    );
    let json = serde_json::to_string_pretty(&runs).unwrap();
    assert!(!json.contains("first private prompt"));
    assert!(!json.contains("second private prompt"));
}

#[test]
fn run_record_json_golden_contains_paths_but_never_private_contents_or_arguments() {
    for (state, name) in [
        (RunState::Queued, "queued"),
        (RunState::Running, "running"),
        (RunState::Succeeded, "succeeded"),
        (RunState::Failed, "failed"),
        (RunState::StopRequested, "stop_requested"),
        (RunState::Stopped, "stopped"),
    ] {
        assert_eq!(state.as_str(), name);
    }
    let value = serde_json::json!([{
        "schema_version": "0.1",
        "run_id": "22222222-2222-4222-8222-222222222222",
        "repo": "z-a-f/fixture",
        "claim_id": "321ba92e-f076-4bc7-bd5b-6cc16cf76277",
        "issue": 24,
        "agent": "codex",
        "mode": "background",
        "state": "queued",
        "worktree": "/worktrees/issue-24",
        "worker_pid": null,
        "child_pid": null,
        "created_at": "2026-07-11T18:00:00Z",
        "started_at": null,
        "finished_at": null,
        "exit_code": null,
        "prompt_path": "/common/envoy/runs/22222222-2222-4222-8222-222222222222/prompt.md",
        "stdout_path": "/common/envoy/runs/22222222-2222-4222-8222-222222222222/stdout.log",
        "stderr_path": "/common/envoy/runs/22222222-2222-4222-8222-222222222222/stderr.log",
        "error": null
    }]);
    let rendered = serde_json::to_string_pretty(&value).unwrap() + "\n";

    assert_text_eq(&rendered, include_str!("golden/run-records-json.json"));
    for secret in ["prompt", "arguments", "log_contents", "private token"] {
        assert!(!value[0].as_object().unwrap().contains_key(secret));
    }
}

#[test]
fn concurrent_terminal_transitions_produce_exactly_one_winner() {
    const MODE: &str = "ENVOY_RUN_TRANSITION_HELPER";
    if std::env::var_os(MODE).is_some() {
        return;
    }
    let fixture = RunFixture::new();
    let claim = fixture.claim();
    fixture.persist_claim(&claim);
    let run = fixture
        .store
        .lock()
        .unwrap()
        .create_run(&claim, fixture.new_run(b"prompt", RunMode::Interactive))
        .unwrap();
    fixture
        .store
        .lock()
        .unwrap()
        .transition_run(
            run.run_id,
            RunTransition::Start {
                worker_pid: None,
                child_pid: 42,
                started_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 1, 0).unwrap(),
            },
        )
        .unwrap();
    let gate = fixture.root.path().join("go");
    let mut children = [1, 2].map(|code| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "run_transition_subprocess_helper", "--nocapture"])
            .env(MODE, "1")
            .env("ENVOY_RUN_STORE", fixture.store.root())
            .env("ENVOY_RUN_ID", run.run_id.to_string())
            .env("ENVOY_RUN_EXIT", code.to_string())
            .env("ENVOY_RUN_GATE", &gate)
            .spawn()
            .unwrap()
    });
    fs::write(&gate, b"go").unwrap();
    let statuses = children
        .each_mut()
        .map(|child| child.wait().unwrap().code().unwrap());
    assert_eq!(statuses.iter().filter(|code| **code == 0).count(), 1);
    assert_eq!(statuses.iter().filter(|code| **code == 2).count(), 1);
    let stored = fixture.store.read_run(run.run_id).unwrap().unwrap();
    assert_eq!(stored.state, RunState::Failed);
    assert!(matches!(stored.exit_code, Some(1 | 2)));
}

#[test]
fn run_transition_subprocess_helper() {
    if std::env::var_os("ENVOY_RUN_TRANSITION_HELPER").is_none() {
        return;
    }
    let gate = std::path::PathBuf::from(std::env::var_os("ENVOY_RUN_GATE").unwrap());
    while !gate.exists() {
        std::thread::yield_now();
    }
    let store = Store::new(std::path::PathBuf::from(
        std::env::var_os("ENVOY_RUN_STORE").unwrap(),
    ));
    let run_id = Uuid::parse_str(&std::env::var("ENVOY_RUN_ID").unwrap()).unwrap();
    let exit_code = std::env::var("ENVOY_RUN_EXIT").unwrap().parse().unwrap();
    let result = store.lock().unwrap().transition_run(
        run_id,
        RunTransition::Exit {
            exit_code,
            finished_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 2, 0).unwrap(),
        },
    );
    std::process::exit(if result.is_ok() { 0 } else { 2 });
}

struct RunFixture {
    root: TempDir,
    store: Store,
}

impl RunFixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let store = Store::new(root.path().join("envoy"));
        Self { root, store }
    }

    fn claim(&self) -> Claim {
        Claim {
            schema_version: SCHEMA_VERSION.to_owned(),
            claim_id: Uuid::parse_str("321ba92e-f076-4bc7-bd5b-6cc16cf76277").unwrap(),
            repo: "z-a-f/fixture".to_owned(),
            issue: NonZeroU64::new(24).unwrap(),
            title: Some("Run records".to_owned()),
            branch: "issue/24".to_owned(),
            worktree: self.root.path().join("worktree"),
            base_remote: "origin".to_owned(),
            base_ref: "main".to_owned(),
            base_sha: "0123456789abcdef".to_owned(),
            base_issue: None,
            base_claim_id: None,
            wait_for: Vec::new(),
            declared_scope: None,
            note: None,
            created_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 0, 0).unwrap(),
        }
    }

    fn persist_claim(&self, claim: &Claim) {
        self.store.lock().unwrap().create_claim(claim).unwrap();
    }

    fn new_run<'a>(&self, prompt: &'a [u8], mode: RunMode) -> NewRun<'a> {
        self.new_run_with_id(Uuid::new_v4(), prompt, mode)
    }

    fn new_run_with_id<'a>(&self, run_id: Uuid, prompt: &'a [u8], mode: RunMode) -> NewRun<'a> {
        NewRun {
            run_id,
            agent: "codex",
            mode,
            prompt,
            created_at: Utc.with_ymd_and_hms(2026, 7, 11, 18, 0, 0).unwrap(),
        }
    }
}

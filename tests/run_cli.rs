use std::fs;
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use chrono::Utc;
use gh_envoy::git::RepositoryContext;
use gh_envoy::model::{
    Claim, DeclaredScope, ReleaseMarker, ReleaseReason, RunState, SCHEMA_VERSION,
};
use gh_envoy::store::Store;
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

fn envoy() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("gh-envoy").expect("gh-envoy binary should build")
}

#[test]
fn foreground_run_preserves_arguments_prompt_and_child_exit() {
    let fixture = RunFixture::new();
    fixture.persist_claim("main");
    let capture = fixture.root.path().join("arguments.json");
    let agent = std::env::current_exe().unwrap();

    let output = envoy()
        .current_dir(fixture.root.path())
        .env("ENVOY_FAKE_CAPTURE", &capture)
        .env("ENVOY_FAKE_EXIT", "0")
        .arg("run")
        .arg(&agent)
        .arg("prompt ☃")
        .arg("--")
        .args(["--exact", "fake_agent_helper", "--nocapture", "--"])
        .args(["semi;colon", "two words", "", "unicode-❤"])
        .output()
        .expect("run foreground agent");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured: Vec<String> =
        serde_json::from_slice(&fs::read(&capture).expect("captured arguments")).unwrap();
    assert_eq!(
        captured,
        ["semi;colon", "two words", "", "unicode-❤", "prompt ☃"]
    );
    let runs = fixture.store().list_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, RunState::Succeeded);
    assert_eq!(runs[0].exit_code, Some(0));
    assert_eq!(
        fs::read_to_string(&runs[0].prompt_path).unwrap(),
        "prompt ☃"
    );

    let empty_capture = fixture.root.path().join("empty-prompt.json");
    let empty = envoy()
        .current_dir(fixture.root.path())
        .env("ENVOY_FAKE_CAPTURE", &empty_capture)
        .arg("run")
        .arg(&agent)
        .arg("")
        .arg("--")
        .args(["--exact", "fake_agent_helper", "--nocapture", "--"])
        .output()
        .expect("run with empty prompt");
    assert_eq!(empty.status.code(), Some(0));
    let captured: Vec<String> = serde_json::from_slice(&fs::read(empty_capture).unwrap()).unwrap();
    assert_eq!(captured, [""]);

    let failure = envoy()
        .current_dir(fixture.root.path())
        .env("ENVOY_FAKE_EXIT", "7")
        .arg("run")
        .arg(agent)
        .arg("--")
        .args(["--exact", "fake_agent_helper", "--nocapture"])
        .output()
        .expect("run failing foreground agent");
    assert_eq!(failure.status.code(), Some(3));
    let runs = fixture.store().list_runs().unwrap();
    let failed = runs
        .iter()
        .find(|run| run.exit_code == Some(7))
        .expect("failed run");
    assert_eq!(failed.state, RunState::Failed);
}

#[test]
fn ownership_and_json_refusals_create_no_run_or_process() {
    let outside = TempDir::new().unwrap();
    let agent = std::env::current_exe().unwrap();
    let refused = envoy()
        .current_dir(outside.path())
        .args(["run"])
        .arg(&agent)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));

    let unclaimed = RunFixture::new();
    let marker = unclaimed.root.path().join("spawned");
    let refused = envoy()
        .current_dir(unclaimed.root.path())
        .env("ENVOY_FAKE_MARKER", &marker)
        .args(["run"])
        .arg(&agent)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert!(!marker.exists());
    assert!(!unclaimed.store().root().exists());

    let released = RunFixture::new();
    let claim = released.persist_claim("main");
    released.persist_release(&claim);
    let refused = envoy()
        .current_dir(released.root.path())
        .args(["run"])
        .arg(&agent)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert!(released.store().list_runs().unwrap().is_empty());

    let mismatched = RunFixture::new();
    mismatched.persist_claim("different-branch");
    let refused = envoy()
        .current_dir(mismatched.root.path())
        .args(["run"])
        .arg(&agent)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert!(mismatched.store().list_runs().unwrap().is_empty());

    let claimed = RunFixture::new();
    claimed.persist_claim("main");
    let refused = envoy()
        .current_dir(claimed.root.path())
        .args(["--json", "run"])
        .arg(&agent)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    let json: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(json["status"], "blocked");
    assert!(claimed.store().list_runs().unwrap().is_empty());
}

#[test]
fn spawn_failure_is_terminal_and_reserved_agent_names_are_refused() {
    let fixture = RunFixture::new();
    fixture.persist_claim("main");
    let failed = envoy()
        .current_dir(fixture.root.path())
        .args(["run", "envoy-agent-that-does-not-exist"])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(3));
    let runs = fixture.store().list_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, RunState::Failed);
    assert_eq!(runs[0].exit_code, None);
    assert!(
        runs[0]
            .error
            .as_deref()
            .is_some_and(|error| !error.trim().is_empty())
    );

    for reserved in ["list", "status", "wait", "stop"] {
        let output = envoy()
            .current_dir(fixture.root.path())
            .args(["run", reserved])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "reserved {reserved}");
    }
    assert_eq!(fixture.store().list_runs().unwrap().len(), 1);
}

#[test]
fn inherited_stdio_passes_through_the_envoy_process() {
    let fixture = RunFixture::new();
    fixture.persist_claim("main");
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_gh-envoy"))
        .current_dir(fixture.root.path())
        .env("ENVOY_FAKE_ECHO", "1")
        .args(["run"])
        .arg(std::env::current_exe().unwrap())
        .arg("--")
        .args(["--exact", "fake_agent_helper", "--nocapture"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"terminal input\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("stdout:terminal input")
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("stderr:terminal input")
    );
}

#[cfg(unix)]
#[test]
fn ctrl_c_records_failed_exit_130_and_kills_the_group() {
    let fixture = RunFixture::new();
    fixture.persist_claim("main");
    let ready = fixture.root.path().join("ready");
    let heartbeat = fixture.root.path().join("heartbeat");
    let child = ProcessCommand::new(env!("CARGO_BIN_EXE_gh-envoy"))
        .current_dir(fixture.root.path())
        .env("ENVOY_FAKE_GROUP_READY", &ready)
        .env("ENVOY_FAKE_HEARTBEAT", &heartbeat)
        .args(["run"])
        .arg(std::env::current_exe().unwrap())
        .arg("--")
        .args(["--exact", "fake_agent_helper", "--nocapture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_path(&ready);
    let status = ProcessCommand::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let run = fixture.store().list_runs().unwrap().pop().unwrap();
    assert_eq!(run.state, RunState::Failed);
    assert_eq!(run.exit_code, Some(130));
    let before = fs::read(&heartbeat).unwrap();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(fs::read(&heartbeat).unwrap(), before);
}

#[test]
#[allow(clippy::zombie_processes)] // The group-kill fixture intentionally leaves reaping to Envoy.
fn fake_agent_helper() {
    if std::env::var_os("ENVOY_FAKE_CAPTURE").is_none()
        && std::env::var_os("ENVOY_FAKE_EXIT").is_none()
        && std::env::var_os("ENVOY_FAKE_MARKER").is_none()
        && std::env::var_os("ENVOY_FAKE_ECHO").is_none()
        && std::env::var_os("ENVOY_FAKE_GROUP_READY").is_none()
    {
        return;
    }
    if std::env::var_os("ENVOY_FAKE_ECHO").is_some() {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap();
        print!("stdout:{input}");
        eprint!("stderr:{input}");
        return;
    }
    if let Some(ready) = std::env::var_os("ENVOY_FAKE_GROUP_READY") {
        let heartbeat = std::env::var_os("ENVOY_FAKE_HEARTBEAT").unwrap();
        ProcessCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", "fake_descendant_helper", "--nocapture"])
            .env("ENVOY_FAKE_HEARTBEAT", &heartbeat)
            .spawn()
            .unwrap();
        wait_for_path(Path::new(&heartbeat));
        #[cfg(unix)]
        let identifiers = {
            let pid = std::process::id().to_string();
            let output = ProcessCommand::new("ps")
                .args(["-o", "pid=", "-o", "pgid=", "-o", "sess=", "-p", &pid])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        #[cfg(windows)]
        let identifiers = std::process::id().to_string().into_bytes();
        fs::write(ready, identifiers).unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    if let Some(marker) = std::env::var_os("ENVOY_FAKE_MARKER") {
        fs::write(marker, b"spawned").unwrap();
    }
    if let Some(capture) = std::env::var_os("ENVOY_FAKE_CAPTURE") {
        let arguments = arguments_after_separator();
        fs::write(capture, serde_json::to_vec(&arguments).unwrap()).unwrap();
    }
    let exit_code = std::env::var("ENVOY_FAKE_EXIT")
        .unwrap_or_else(|_| "0".to_owned())
        .parse()
        .unwrap();
    std::process::exit(exit_code);
}

#[test]
fn fake_descendant_helper() {
    let Some(heartbeat) = std::env::var_os("ENVOY_FAKE_HEARTBEAT") else {
        return;
    };
    let mut counter = 0_u64;
    loop {
        fs::write(&heartbeat, counter.to_string()).unwrap();
        counter += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn arguments_after_separator() -> Vec<String> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let separator = arguments
        .iter()
        .rposition(|argument| argument == "--")
        .expect("fake agent separator");
    arguments[separator + 1..]
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct RunFixture {
    root: TempDir,
    common_dir: PathBuf,
    worktree: PathBuf,
}

impl RunFixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        git(root.path(), &["init", "-q", "-b", "main"]);
        git(root.path(), &["config", "user.name", "Envoy Test"]);
        git(
            root.path(),
            &["config", "user.email", "envoy@example.invalid"],
        );
        fs::write(root.path().join("README.md"), "fixture\n").unwrap();
        git(root.path(), &["add", "README.md"]);
        git(root.path(), &["commit", "-qm", "fixture"]);
        let repository = RepositoryContext::discover(root.path(), "origin")
            .expect("discover fixture repository");
        Self {
            root,
            common_dir: repository.common_dir,
            worktree: repository.current_worktree,
        }
    }

    fn store(&self) -> Store {
        Store::new(self.common_dir.join("envoy"))
    }

    fn persist_claim(&self, branch: &str) -> Claim {
        let claim = Claim {
            schema_version: SCHEMA_VERSION.to_owned(),
            claim_id: Uuid::new_v4(),
            repo: "local/fixture".to_owned(),
            issue: NonZeroU64::new(25).unwrap(),
            title: Some("Slice 9 -- Foreground execution boundary".to_owned()),
            branch: branch.to_owned(),
            worktree: self.worktree.clone(),
            base_remote: "origin".to_owned(),
            base_ref: "main".to_owned(),
            base_sha: git_stdout(self.root.path(), &["rev-parse", "HEAD"]),
            base_issue: None,
            base_claim_id: None,
            wait_for: Vec::new(),
            declared_scope: Some(DeclaredScope::default()),
            note: None,
            created_at: Utc::now(),
        };
        self.store().lock().unwrap().create_claim(&claim).unwrap();
        claim
    }

    fn persist_release(&self, claim: &Claim) {
        self.store()
            .lock()
            .unwrap()
            .create_release(&ReleaseMarker {
                schema_version: SCHEMA_VERSION.to_owned(),
                repo: claim.repo.clone(),
                issue: claim.issue,
                claim_id: claim.claim_id,
                reason: ReleaseReason::Manual,
                released_at: Utc::now(),
            })
            .unwrap();
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = ProcessCommand::new("git")
        .current_dir(directory)
        .args(arguments)
        .status()
        .unwrap();
    assert!(status.success());
}

fn git_stdout(directory: &Path, arguments: &[&str]) -> String {
    let output = ProcessCommand::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

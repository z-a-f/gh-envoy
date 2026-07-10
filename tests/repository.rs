use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;

use gh_envoy::command::{CommandOutput, CommandRunner, CommandSpec, RunnerError};
use gh_envoy::git::{GitCli, GithubCli, RepositoryContext};
use tempfile::TempDir;

#[derive(Default)]
struct RecordingRunner {
    calls: Mutex<Vec<CommandSpec>>,
}

impl CommandRunner for RecordingRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
        self.calls
            .lock()
            .expect("recording lock")
            .push(spec.clone());
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: b"ok\n".to_vec(),
            stderr: Vec::new(),
        })
    }
}

#[test]
fn git_and_github_clients_use_structured_commands() {
    let runner = RecordingRunner::default();
    let cwd = Path::new("/tmp/repository");

    GitCli::new(&runner)
        .run(cwd, ["status", "--porcelain=v1"])
        .expect("fake git succeeds");
    GithubCli::new(&runner)
        .run(cwd, ["issue", "view", "7"])
        .expect("fake gh succeeds");

    let calls = runner.calls.lock().expect("recording lock");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].program, OsString::from("git"));
    assert_eq!(calls[0].args, ["status", "--porcelain=v1"]);
    assert_eq!(calls[0].cwd.as_deref(), Some(cwd));
    assert_eq!(calls[1].program, OsString::from("gh"));
    assert_eq!(calls[1].args, ["issue", "view", "7"]);
}

#[test]
fn main_and_linked_worktrees_resolve_the_same_shared_store() {
    let fixture = RepositoryFixture::new();
    let linked = fixture.root.path().join("linked");
    fixture.git(&["worktree", "add", "-q", "-b", "linked", path(&linked)]);

    let main =
        RepositoryContext::discover(fixture.root.path(), "origin").expect("discover main worktree");
    let secondary =
        RepositoryContext::discover(&linked, "origin").expect("discover linked worktree");

    assert_eq!(main.common_dir, secondary.common_dir);
    assert_eq!(main.main_worktree, secondary.main_worktree);
    assert_ne!(main.current_worktree, secondary.current_worktree);
    assert_eq!(main.store_root(), secondary.store_root());
    assert_eq!(main.repository, "z-a-f/fixture");
    assert_eq!(secondary.remote_name, "origin");
}

#[test]
fn repository_discovery_reports_git_failures() {
    let outside_repository = TempDir::new().expect("temporary directory");

    let error = RepositoryContext::discover(outside_repository.path(), "origin")
        .expect_err("non-repository must fail");

    assert!(error.to_string().contains("rev-parse"), "{error}");
}

struct RepositoryFixture {
    root: TempDir,
}

impl RepositoryFixture {
    fn new() -> Self {
        let root = TempDir::new().expect("temporary repository");
        let fixture = Self { root };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Envoy Tests"]);
        fixture.git(&["config", "user.email", "envoy@example.invalid"]);
        fixture.git(&["commit", "--allow-empty", "-qm", "initial"]);
        fixture.git(&[
            "remote",
            "add",
            "origin",
            "git@github.com:z-a-f/fixture.git",
        ]);
        fixture
    }

    fn git(&self, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(self.root.path())
            .args(arguments)
            .output()
            .expect("run fixture git");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn path(value: &Path) -> &str {
    value.to_str().expect("test path is UTF-8")
}

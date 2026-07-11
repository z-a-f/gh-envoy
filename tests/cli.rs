use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn envoy() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("gh-envoy").expect("gh-envoy binary should build")
}

#[test]
fn help_identifies_the_github_extension_entrypoint() {
    let output = envoy().arg("--help").output().expect("run gh-envoy");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    assert!(stdout.contains("Usage: gh-envoy"), "{stdout}");
    for command in ["claim", "list", "status", "doctor", "release"] {
        assert!(stdout.contains(command), "missing {command} in {stdout}");
    }
}

#[test]
fn stack_doctor_reports_a_missing_target_without_creating_store_state() {
    let repository = TempDir::new().expect("temporary repository");
    run_git(repository.path(), &["init", "-q"]);

    let output = envoy()
        .current_dir(repository.path())
        .args(["doctor", "--stack", "1"])
        .output()
        .expect("run stack doctor");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("no active local claim")
    );
    assert!(output.stderr.is_empty());
    assert!(!repository.path().join(".git/envoy").exists());
}

#[test]
fn doctor_runs_read_only_in_human_and_json_modes() {
    let repository = TempDir::new().expect("temporary repository");
    run_git(repository.path(), &["init", "-q"]);

    let human = envoy()
        .current_dir(repository.path())
        .arg("doctor")
        .output()
        .expect("run human doctor");
    assert_eq!(human.status.code(), Some(0));
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .contains("Overall: ok")
    );

    let json = envoy()
        .current_dir(repository.path())
        .args(["--json", "doctor"])
        .output()
        .expect("run JSON doctor");
    assert_eq!(json.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&json.stdout).expect("valid JSON");
    assert_eq!(value["command"], "doctor");
    assert_eq!(value["status"], "ok");
    assert!(!repository.path().join(".git/envoy").exists());
}

#[test]
fn doctor_uses_blocked_exit_for_a_missing_requested_claim() {
    let repository = TempDir::new().expect("temporary repository");
    run_git(repository.path(), &["init", "-q"]);

    let output = envoy()
        .current_dir(repository.path())
        .args(["--json", "doctor", "99"])
        .output()
        .expect("run issue doctor");

    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["status"], "blocked");
    assert_eq!(value["doctor"]["gates"]["integrity"], "blocked");
    assert!(!repository.path().join(".git/envoy").exists());
}

#[test]
fn json_mode_is_global_and_machine_readable() {
    let repository = TempDir::new().expect("temporary repository");
    run_git(repository.path(), &["init", "-q"]);
    for arguments in [vec!["--json", "status"], vec!["status", "--json"]] {
        let output = envoy()
            .current_dir(repository.path())
            .args(arguments)
            .output()
            .expect("run JSON status");

        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
        assert_eq!(value["schema_version"], "0.1");
        assert_eq!(value["command"], "status");
        assert_eq!(value["status"], "success");
        assert_eq!(value["claims"], serde_json::json!([]));
        assert_eq!(value["problems"], serde_json::json!([]));
    }
    assert!(!repository.path().join(".git/envoy").exists());
}

#[test]
fn invalid_arguments_use_the_operational_error_exit_code() {
    for arguments in [
        vec!["claim", "0"],
        vec!["claim", "not-a-number"],
        vec!["doctor", "1", "--stack", "2"],
        vec!["release"],
    ] {
        let output = envoy().args(arguments).output().expect("run invalid CLI");

        assert_eq!(output.status.code(), Some(3));
    }
}

fn run_git(directory: &std::path::Path, arguments: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .status()
        .expect("run git");
    assert!(status.success());
}

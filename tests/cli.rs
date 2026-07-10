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
    for command in ["claim", "status", "doctor", "release"] {
        assert!(stdout.contains(command), "missing {command} in {stdout}");
    }
}

#[test]
fn remaining_command_stubs_fail_explicitly_without_creating_store_state() {
    let repository = TempDir::new().expect("temporary repository");
    run_git(repository.path(), &["init", "-q"]);

    for arguments in [vec!["doctor", "1"], vec!["doctor", "--stack", "1"]] {
        let output = envoy()
            .current_dir(repository.path())
            .args(arguments)
            .output()
            .expect("run command stub");

        assert_eq!(output.status.code(), Some(3));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .expect("stderr is UTF-8")
                .contains("is not implemented yet")
        );
        assert!(!repository.path().join(".git/envoy").exists());
    }
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

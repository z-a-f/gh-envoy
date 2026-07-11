use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use gh_envoy::execution::{ProcessSpec, StdioPolicy, WaitOutcome};
use tempfile::TempDir;

#[test]
fn grouped_child_is_a_leader_and_interrupt_kills_its_descendants() {
    let directory = TempDir::new().unwrap();
    let ready = directory.path().join("ready");
    let heartbeat = directory.path().join("heartbeat");
    let spec = ProcessSpec {
        executable: std::env::current_exe().unwrap().into_os_string(),
        args: ["--exact", "group_agent_helper", "--nocapture"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        cwd: std::env::current_dir().unwrap(),
        env_overrides: vec![
            (
                "ENVOY_GROUP_READY".into(),
                Some(ready.clone().into_os_string()),
            ),
            (
                "ENVOY_GROUP_HEARTBEAT".into(),
                Some(heartbeat.clone().into_os_string()),
            ),
        ],
        stdio: StdioPolicy::Inherit,
    };
    let mut child = spec.spawn_grouped().unwrap();
    wait_for_path(&ready);
    #[cfg(unix)]
    let identifiers = {
        let identifiers = fs::read_to_string(&ready).unwrap();
        let identifiers = identifiers
            .split_whitespace()
            .map(|value| value.parse::<i32>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(identifiers.len(), 3);
        identifiers
    };
    assert_eq!(
        child.wait_interruptibly(&AtomicBool::new(true)).unwrap(),
        WaitOutcome::Interrupted
    );
    let before = fs::read(&heartbeat).unwrap();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(fs::read(&heartbeat).unwrap(), before);
    #[cfg(unix)]
    {
        assert_eq!(
            identifiers[0], identifiers[1],
            "child must lead its process group"
        );
        assert_eq!(
            identifiers[0], identifiers[2],
            "child must lead its session"
        );
    }
}

#[cfg(unix)]
#[test]
fn process_boundary_preserves_non_utf8_argument_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let directory = TempDir::new().unwrap();
    let capture = directory.path().join("raw-arguments");
    let agent = directory.path().join("raw-arg-agent");
    let compile = Command::new("rustc")
        .args(["tests/fixtures/raw_arg_agent.rs", "-o"])
        .arg(&agent)
        .status()
        .unwrap();
    assert!(compile.success());
    let expected = vec![b'a', 0xff, b'z'];
    let spec = ProcessSpec {
        executable: agent.into_os_string(),
        args: vec![OsString::from_vec(expected.clone())],
        cwd: std::env::current_dir().unwrap(),
        env_overrides: vec![(
            "ENVOY_FAKE_RAW_CAPTURE".into(),
            Some(capture.clone().into_os_string()),
        )],
        stdio: StdioPolicy::Inherit,
    };
    let mut child = spec.spawn_grouped().unwrap();
    assert_eq!(
        child.wait_interruptibly(&AtomicBool::new(false)).unwrap(),
        WaitOutcome::Exited(0)
    );
    assert_eq!(fs::read(capture).unwrap(), expected);
}

#[cfg(unix)]
#[test]
fn process_boundary_maps_signal_termination_to_conventional_exit_code() {
    let spec = ProcessSpec {
        executable: "/bin/sh".into(),
        args: ["-c", "kill -TERM $$"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        cwd: std::env::current_dir().unwrap(),
        env_overrides: Vec::new(),
        stdio: StdioPolicy::Inherit,
    };
    let mut child = spec.spawn_grouped().unwrap();
    assert_eq!(
        child.wait_interruptibly(&AtomicBool::new(false)).unwrap(),
        WaitOutcome::Exited(143)
    );
}

#[test]
#[allow(clippy::zombie_processes)] // The group-kill fixture intentionally leaves reaping to Envoy.
fn group_agent_helper() {
    let Some(ready) = std::env::var_os("ENVOY_GROUP_READY") else {
        return;
    };
    let heartbeat = std::env::var_os("ENVOY_GROUP_HEARTBEAT").unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "group_descendant_helper", "--nocapture"])
        .env("ENVOY_GROUP_HEARTBEAT", &heartbeat)
        .spawn()
        .unwrap();
    wait_for_path(Path::new(&heartbeat));
    #[cfg(unix)]
    let identifiers = {
        use nix::unistd::{Pid, getpgid, getsid};

        let pid = Pid::from_raw(i32::try_from(std::process::id()).unwrap());
        let process_group = getpgid(Some(pid)).unwrap();
        let session = getsid(Some(pid)).unwrap();
        format!(
            "{} {} {}",
            pid.as_raw(),
            process_group.as_raw(),
            session.as_raw()
        )
        .into_bytes()
    };
    #[cfg(windows)]
    let identifiers = std::process::id().to_string().into_bytes();
    fs::write(ready, identifiers).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn group_descendant_helper() {
    let Some(heartbeat) = std::env::var_os("ENVOY_GROUP_HEARTBEAT") else {
        return;
    };
    let mut counter = 0_u64;
    loop {
        fs::write(&heartbeat, counter.to_string()).unwrap();
        counter += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
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

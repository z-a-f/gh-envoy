use std::fs;

use gh_envoy::config::Config;
use tempfile::TempDir;

#[test]
fn missing_config_uses_built_in_defaults() {
    let common_dir = TempDir::new().expect("temporary common directory");

    let config = Config::load(common_dir.path()).expect("load defaults");

    assert_eq!(config.base_remote, "origin");
    assert_eq!(config.default_base_ref, None);
    assert_eq!(config.worktree_root, None);
    assert!(config.redact_paths_in_json);
    assert!(!config.risk_paths.is_empty());
    for label in [
        "lockfile",
        "migration",
        "project_config",
        "workflow",
        "test",
    ] {
        assert!(config.risk_paths.values().any(|value| value == label));
    }
}

#[test]
fn common_directory_config_overlays_individual_defaults() {
    let common_dir = TempDir::new().expect("temporary common directory");
    let store = common_dir.path().join("envoy");
    fs::create_dir(&store).expect("create store directory");
    let worktrees = common_dir.path().join("worktrees");
    fs::write(
        store.join("config.yml"),
        format!(
            "base_remote: upstream\ndefault_base_ref: trunk\nworktree_root: {}\nredact_paths_in_json: false\nrisk_paths:\n  Cargo.lock: lockfile\n",
            worktrees.display()
        ),
    )
    .expect("write config");

    let config = Config::load(common_dir.path()).expect("load overlay");

    assert_eq!(config.base_remote, "upstream");
    assert_eq!(config.default_base_ref.as_deref(), Some("trunk"));
    assert_eq!(config.worktree_root.as_deref(), Some(worktrees.as_path()));
    assert!(!config.redact_paths_in_json);
    assert_eq!(config.risk_paths["Cargo.lock"], "lockfile");
    assert!(
        config.risk_paths.len() > 1,
        "configured risk paths extend defaults"
    );
}

#[test]
fn configured_identical_risk_glob_replaces_only_its_default_label() {
    let common_dir = TempDir::new().expect("temporary common directory");
    let store = common_dir.path().join("envoy");
    fs::create_dir(&store).expect("create store directory");
    let pattern = "**/{migration,migrations}/**";
    fs::write(
        store.join("config.yml"),
        format!("risk_paths:\n  '{pattern}': database\n"),
    )
    .expect("write config");

    let config = Config::load(common_dir.path()).expect("load overlay");

    assert_eq!(config.risk_paths[pattern], "database");
    assert!(config.risk_paths.values().any(|label| label == "lockfile"));
}

#[test]
fn config_rejects_invalid_overrides() {
    for yaml in [
        "unknown_setting: true\n",
        "worktree_root: relative/path\n",
        "base_remote: ''\n",
        "default_base_ref: ''\n",
        "risk_paths:\n  '': lockfile\n",
        "risk_paths:\n  Cargo.lock: ''\n",
        "risk_paths:\n  '[': invalid\n",
    ] {
        let common_dir = TempDir::new().expect("temporary common directory");
        let store = common_dir.path().join("envoy");
        fs::create_dir(&store).expect("create store directory");
        fs::write(store.join("config.yml"), yaml).expect("write invalid config");

        let error = Config::load(common_dir.path()).expect_err("config must fail");
        assert!(error.to_string().contains("config.yml"), "{error}");
    }
}

#[test]
fn unreadable_config_path_reports_the_source_path() {
    let common_dir = TempDir::new().expect("temporary common directory");
    let config_path = common_dir.path().join("envoy/config.yml");
    fs::create_dir_all(&config_path).expect("create directory at config path");

    let error = Config::load(common_dir.path()).expect_err("directory is not a YAML file");

    assert!(error.to_string().contains("config.yml"), "{error}");
}

use std::collections::BTreeMap;
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
    assert_eq!(config.risk_paths, BTreeMap::new());
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
            "default_base_ref: trunk\nworktree_root: {}\nrisk_paths:\n  Cargo.lock: lockfile\n",
            worktrees.display()
        ),
    )
    .expect("write config");

    let config = Config::load(common_dir.path()).expect("load overlay");

    assert_eq!(config.base_remote, "origin");
    assert_eq!(config.default_base_ref.as_deref(), Some("trunk"));
    assert_eq!(config.worktree_root.as_deref(), Some(worktrees.as_path()));
    assert_eq!(config.risk_paths["Cargo.lock"], "lockfile");
}

#[test]
fn config_rejects_unknown_keys_and_relative_worktree_roots() {
    for yaml in ["unknown_setting: true\n", "worktree_root: relative/path\n"] {
        let common_dir = TempDir::new().expect("temporary common directory");
        let store = common_dir.path().join("envoy");
        fs::create_dir(&store).expect("create store directory");
        fs::write(store.join("config.yml"), yaml).expect("write invalid config");

        let error = Config::load(common_dir.path()).expect_err("config must fail");
        assert!(error.to_string().contains("config.yml"), "{error}");
    }
}

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::conflict::validate_glob_pattern;

const DEFAULT_RISK_PATHS: &[(&str, &str)] = &[
    (
        "**/{Cargo.lock,package-lock.json,pnpm-lock.yaml,yarn.lock,uv.lock,poetry.lock,Pipfile.lock}",
        "lockfile",
    ),
    ("**/{migration,migrations}/**", "migration"),
    (
        "**/{Cargo.toml,pyproject.toml,package.json,go.mod,pom.xml,build.gradle,build.gradle.kts,*.csproj}",
        "project_config",
    ),
    (".github/workflows/**", "workflow"),
    ("**/{test,tests}/**", "test"),
    ("**/*.{test,spec}.*", "test"),
    ("**/*_{test,spec}.*", "test"),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub base_remote: String,
    pub default_base_ref: Option<String>,
    pub worktree_root: Option<PathBuf>,
    pub redact_paths_in_json: bool,
    pub on_run_event: Option<Vec<String>>,
    pub risk_paths: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_remote: "origin".to_owned(),
            default_base_ref: None,
            worktree_root: None,
            redact_paths_in_json: true,
            on_run_event: None,
            risk_paths: DEFAULT_RISK_PATHS
                .iter()
                .map(|(glob, label)| ((*glob).to_owned(), (*label).to_owned()))
                .collect(),
        }
    }
}

impl Config {
    pub fn load(common_dir: &Path) -> Result<Self, ConfigError> {
        let path = common_dir.join("envoy").join("config.yml");
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        let overlay: ConfigOverlay =
            serde_yaml::from_str(&contents).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
        let mut config = Self::default();
        if let Some(value) = overlay.base_remote {
            config.base_remote = value;
        }
        if let Some(value) = overlay.default_base_ref {
            config.default_base_ref = value;
        }
        if let Some(value) = overlay.worktree_root {
            config.worktree_root = value;
        }
        if let Some(value) = overlay.redact_paths_in_json {
            config.redact_paths_in_json = value;
        }
        if let Some(value) = overlay.on_run_event {
            config.on_run_event = value;
        }
        if let Some(value) = overlay.risk_paths {
            config.risk_paths.extend(value);
        }
        config.validate(&path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        if self.base_remote.trim().is_empty() {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: "base_remote must not be empty".to_owned(),
            });
        }
        if self
            .default_base_ref
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: "default_base_ref must not be empty when set".to_owned(),
            });
        }
        if self
            .worktree_root
            .as_deref()
            .is_some_and(|root| !root.is_absolute())
        {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: "worktree_root must be an absolute path".to_owned(),
            });
        }
        if self
            .risk_paths
            .iter()
            .any(|(glob, label)| glob.trim().is_empty() || label.trim().is_empty())
        {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: "risk path globs and labels must not be empty".to_owned(),
            });
        }
        if let Some(command) = &self.on_run_event
            && (command.is_empty() || command[0].trim().is_empty())
        {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: "on_run_event must contain a non-empty executable".to_owned(),
            });
        }
        for glob in self.risk_paths.keys() {
            if let Err(error) = validate_glob_pattern(glob) {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!("invalid risk path glob {glob:?}: {error}"),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigOverlay {
    base_remote: Option<String>,
    default_base_ref: Option<Option<String>>,
    worktree_root: Option<Option<PathBuf>>,
    redact_paths_in_json: Option<bool>,
    on_run_event: Option<Option<Vec<String>>>,
    risk_paths: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("invalid {path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

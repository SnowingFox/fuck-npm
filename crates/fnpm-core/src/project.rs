//! Locating the project and loading what every command needs: the configuration, the
//! workspace, a registry client and the store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fnpm_config::{CliOverrides, Config, Env, LoadOptions};
use fnpm_registry::RegistryClient;
use fnpm_store::Store;
use fnpm_types::{Event, EventSink};
use fnpm_workspace::Workspace;

use crate::{Error, Result};

/// `cwd` made absolute (the process cwd when empty or relative).
pub fn absolute_cwd(cwd: &Path) -> PathBuf {
    if cwd.as_os_str().is_empty() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(cwd))
            .unwrap_or_else(|_| cwd.to_path_buf())
    }
}

/// A loaded project: where it is and its configuration.
#[derive(Clone)]
pub struct Project {
    /// Where the command was started.
    pub cwd: PathBuf,
    /// The workspace root (the global directory with `-g`).
    pub root_dir: PathBuf,
    /// The effective configuration.
    pub config: Arc<Config>,
    /// The environment the configuration was loaded from.
    pub env: Env,
}

impl Project {
    /// Finds the project root from `cwd` (unless `cli.global`) and loads the configuration.
    /// `create_package_json` writes an empty `package.json` into `cwd` when no project is
    /// found (`fnpm add` in an empty directory, like bun) and into the global directory with
    /// `-g`.
    pub fn load(
        cwd: &Path,
        cli: CliOverrides,
        env: Env,
        create_package_json: bool,
    ) -> Result<Self> {
        Self::load_with_root(cwd, cli, env, create_package_json, false)
    }

    /// [`Project::load`]; with `cwd_is_root` the project is `cwd` itself, whatever is above it
    /// (`fnpm x` installs into a tree below the user's cache and home).
    pub fn load_with_root(
        cwd: &Path,
        cli: CliOverrides,
        env: Env,
        create_package_json: bool,
        cwd_is_root: bool,
    ) -> Result<Self> {
        let cwd = absolute_cwd(cwd);
        let global = cli.global;
        let root = if global {
            None
        } else if cwd_is_root {
            if !cwd.join("package.json").exists() {
                write_new_package_json(&cwd)?;
            }
            Some(cwd.clone())
        } else {
            match Workspace::find_root(&cwd) {
                Ok(root) => Some(root),
                Err(fnpm_workspace::Error::NoProject(_)) if create_package_json => {
                    write_new_package_json(&cwd)?;
                    Some(cwd.clone())
                }
                Err(e) => return Err(e.into()),
            }
        };
        let config = Config::load(&LoadOptions {
            cwd: cwd.clone(),
            cli,
            env: env.clone(),
            project_dir: root,
            node_version: None,
        })?;
        let root_dir = config.project_dir.clone();
        if global {
            std::fs::create_dir_all(&root_dir).map_err(Error::io(&root_dir))?;
            if !root_dir.join("package.json").exists() {
                write_new_package_json(&root_dir)?;
            }
        }
        Ok(Self {
            cwd,
            root_dir,
            config: Arc::new(config),
            env,
        })
    }

    /// [`Project::load`] for commands that also work outside a project (`info <pkg>`,
    /// `whoami`, `pm cache`, `store`, `doctor`, `pm bin`): without a package.json above `cwd`,
    /// the configuration is loaded for `cwd` itself (user and global config files, env).
    pub fn load_anywhere(cwd: &Path, cli: CliOverrides, env: Env) -> Result<Self> {
        match Self::load(cwd, cli.clone(), env.clone(), false) {
            Err(Error::Workspace(fnpm_workspace::Error::NoProject(_))) => {
                let cwd = absolute_cwd(cwd);
                let config = Config::load(&LoadOptions {
                    cwd: cwd.clone(),
                    cli,
                    env: env.clone(),
                    project_dir: Some(cwd.clone()),
                    node_version: None,
                })?;
                Ok(Self {
                    root_dir: config.project_dir.clone(),
                    cwd,
                    config: Arc::new(config),
                    env,
                })
            }
            other => other,
        }
    }

    /// Sends the configuration's warnings to `events`.
    pub fn report_warnings(&self, events: &dyn EventSink) {
        for w in &self.config.warnings {
            events.emit(Event::warn(w.clone(), None));
        }
    }

    /// Loads the workspace; its warnings go to `events`.
    pub fn workspace(&self, events: &dyn EventSink) -> Result<Workspace> {
        let (ws, warnings) = Workspace::load_with(&self.root_dir, &Default::default())?;
        for w in warnings {
            events.emit(Event::warn(w, None));
        }
        Ok(ws)
    }

    /// What fnpm keeps about this project's `node_modules` ([`crate::fast_path::meta_dir`]).
    pub fn meta_dir(&self) -> std::path::PathBuf {
        crate::fast_path::meta_dir(&self.config.cache_dir, &self.root_dir)
    }

    /// A registry client over this configuration and environment.
    pub fn client(&self) -> Result<RegistryClient> {
        Ok(RegistryClient::with_env(
            self.config.clone(),
            self.env.clone(),
        )?)
    }

    /// The store under `config.store_dir`.
    pub fn store(&self) -> Result<Store> {
        Ok(Store::open(&self.config.store_dir)?)
    }
}

fn write_new_package_json(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(Error::io(dir))?;
    let path = dir.join("package.json");
    std::fs::write(&path, "{}\n").map_err(Error::io(&path))
}

/// Writes `bytes` through a temporary file and a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    fnpm_store::fsops::write_atomic(path, bytes).map_err(Error::io(path))
}

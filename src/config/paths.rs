//! Per-user storage. Configuration stays at its original location; disposable data
//! and session state have independent lifetimes. Passwords remain in the OS vault.
use super::{AppConfig, ConfigError, Result, WorkspaceSnapshot, storage};
#[cfg(any(not(windows), test))]
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl AppPaths {
    pub fn discover(custom_config: Option<PathBuf>) -> Result<Self> {
        if let Some(directory) = custom_config {
            // Keep relative CLI paths stable across restarts and isolate every profile.
            let config = std::path::absolute(directory)?;
            return Ok(Self {
                state: config.join("state"),
                cache: config.join("cache"),
                config,
            });
        }
        let config = config_dir()?;
        #[cfg(windows)]
        let state = config.join("state");
        #[cfg(not(windows))]
        let state = xdg_directory("XDG_STATE_HOME", ".local/state")?;
        Ok(Self {
            config,
            state,
            cache: cache_dir()?,
        })
    }

    pub fn logs(&self) -> PathBuf {
        self.state.join("logs")
    }

    /// Call while holding ConfigLock. Keep that lock in the configuration directory
    /// so older versions and instances using the same configuration still exclude us.
    pub fn load_config(&self) -> Result<AppConfig> {
        let mut config = AppConfig::load(self.config.join("config.toml"))?;
        let legacy_workspace = !config.workspace.is_empty();
        if let Some(source) = storage::read_optional(&self.state.join("workspace.toml"))? {
            let workspace: WorkspaceSnapshot = toml::from_str(&source)?;
            workspace.validate()?;
            // An explicitly empty snapshot also wins over old inline session state.
            config.workspace = workspace;
        }
        if legacy_workspace {
            // Save the snapshot before removing it from config.toml. A failed or
            // interrupted migration leaves at least one complete copy to retry.
            self.save_config(&config)?;
        }
        Ok(config)
    }

    pub fn save_config(&self, config: &AppConfig) -> Result<()> {
        config.validate()?;
        let mut preferences = config.clone();
        preferences.workspace = WorkspaceSnapshot::default();
        // Serialize both before touching either file. The config is the credential
        // commit point; a state write must never report that credentials committed.
        let preferences = toml::to_string_pretty(&preferences)?;
        let workspace = toml::to_string_pretty(&config.workspace)?;
        storage::write_atomic(&self.state.join("workspace.toml"), workspace.as_bytes()).map_err(
            |error| match error {
                ConfigError::CommitUncertain(error) => ConfigError::Io(error),
                error => error,
            },
        )?;
        storage::write_atomic(&self.config.join("config.toml"), preferences.as_bytes())
    }

    /// Logs are optional: callers may continue if the log directory is unavailable.
    pub fn prepare_logs(&self) -> Result<()> {
        let logs = self.logs();
        storage::ensure_directory(&self.state)?;
        storage::ensure_directory(&logs)?;
        for name in ["diagnostics.log", "diagnostics.1.log", "diagnostics.2.log"] {
            storage::migrate_file(&self.config.join(name), &logs.join(name))?;
        }
        Ok(())
    }
}

pub fn config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        // Known Folder lookup supports redirected user profiles and does not depend
        // on LOCALAPPDATA being present in the process environment. These settings
        // reference local SSH keys and machine-bound vault entries, so keep them local.
        dirs::config_local_dir()
            .filter(|path| path.is_absolute())
            .map(|path| path.join("RustTerm"))
            .ok_or(ConfigError::MissingConfigDirectory)
    }
    #[cfg(not(windows))]
    xdg_directory("XDG_CONFIG_HOME", ".config")
}

pub fn cache_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(config_dir()?.join("cache"))
    }
    #[cfg(not(windows))]
    xdg_directory("XDG_CACHE_HOME", ".cache")
}

#[cfg(not(windows))]
fn xdg_directory(variable: &str, fallback: &str) -> Result<PathBuf> {
    let value = std::env::var_os(variable).map(PathBuf::from);
    xdg_base(value.as_deref(), dirs::home_dir().as_deref(), fallback)
        .map(|path| path.join("rustterm"))
}

#[cfg(any(not(windows), test))]
fn xdg_base(value: Option<&Path>, home: Option<&Path>, fallback: &str) -> Result<PathBuf> {
    value
        .filter(|path| path.is_absolute())
        .map(Path::to_owned)
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(fallback))
        })
        .ok_or(ConfigError::MissingConfigDirectory)
}

#[cfg(test)]
mod tests;

use crate::i18n::messages as i18n_key;

use super::{Result, invalid, validate_path, validate_text};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            #[cfg(windows)]
            shell: "powershell.exe".into(),
            #[cfg(not(windows))]
            shell: std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/bin/bash".into()),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
        }
    }
}

impl LocalConfig {
    pub fn validate(&self) -> Result<()> {
        validate_text(&self.shell, 4096, i18n_key::CONNECTIONS_SHELL_PATH)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_SHELL))?;
        if self.args.len() > 256 || self.env.len() > 512 {
            return Err(invalid(
                i18n_key::CONNECTIONS_TOO_MANY_ARGUMENTS_OR_ENVIRONMENT_VARIABLES,
            ));
        }
        if self
            .args
            .iter()
            .any(|s| s.len() > 65536 || s.contains('\0'))
        {
            return Err(invalid(
                i18n_key::CONNECTIONS_A_STARTUP_ARGUMENT_IS_TOO_LONG_OR_CONTAINS_NUL,
            ));
        }
        if self.env.iter().any(|(k, v)| {
            k.is_empty()
                || k.contains(['=', '\0'])
                || v.contains('\0')
                || k.len() > 256
                || v.len() > 65536
        }) {
            return Err(invalid(
                i18n_key::CONNECTIONS_AN_ENVIRONMENT_VARIABLE_IS_INVALID_OR_TOO_LONG,
            ));
        }
        if let Some(cwd) = &self.cwd {
            validate_path(cwd, i18n_key::COMMON_WORKING_DIRECTORY)
                .map_err(|error| error.at_field(i18n_key::COMMON_WORKING_DIRECTORY))?;
        }
        Ok(())
    }
}

pub type LocalProfile = LocalConfig;

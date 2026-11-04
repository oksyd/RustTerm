use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug)]
pub struct LocalOptions {
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}
impl Default for LocalOptions {
    fn default() -> Self {
        #[cfg(windows)]
        let shell = "powershell.exe".to_owned();
        #[cfg(not(windows))]
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        Self {
            shell,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
        }
    }
}

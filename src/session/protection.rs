//! Immutable policy for one connection lifetime. Saved enhancement preferences
//! are kept separately so changing the policy never erases them.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionProtection {
    #[default]
    Standard,
    Production,
}

impl SessionProtection {
    pub fn is_production(self) -> bool {
        self == Self::Production
    }

    pub fn allows_enhancements(self) -> bool {
        !self.is_production()
    }

    pub fn requires_paste_review(self, text: &str) -> bool {
        self.is_production() && text.contains(['\r', '\n'])
    }
}

impl From<bool> for SessionProtection {
    fn from(production: bool) -> Self {
        if production {
            Self::Production
        } else {
            Self::Standard
        }
    }
}

/// Client-generated shell commands, distinct from text entered by the user.
pub enum ShellAction {
    EnableIntegration {
        shell: &'static str,
    },
    RestoreDirectory {
        shell: &'static str,
        directory: String,
    },
}

impl ShellAction {
    pub(super) fn command(self) -> Option<String> {
        match self {
            Self::EnableIntegration { shell } => crate::terminal::integration::activation(shell),
            Self::RestoreDirectory { shell, directory } => {
                crate::terminal::integration::directory_command(shell, &directory)
            }
        }
    }
}

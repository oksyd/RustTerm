//! Startup errors must remain visible without a working GPUI renderer or config.
use rustterm::config::{AppPaths, ConfigError};
use rustterm::i18n::{messages as key, tr};

#[derive(Default)]
pub(super) struct StartupContext {
    pub paths: Option<AppPaths>,
    pub logs_ready: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    Incompatible,
    InvalidConfig,
    AlreadyRunning,
    Storage,
    Other,
}

impl Reason {
    fn from_error(error: &anyhow::Error) -> Self {
        error
            .chain()
            .find_map(|cause| {
                let mut config_error = cause.downcast_ref::<ConfigError>()?;
                while let ConfigError::File { source, .. } = config_error {
                    config_error = source;
                }
                Some(match config_error {
                    ConfigError::FutureVersion { .. } | ConfigError::UnsupportedData { .. } => {
                        Self::Incompatible
                    }
                    ConfigError::Decode { .. }
                    | ConfigError::Validation(_)
                    | ConfigError::FieldValidation { .. } => Self::InvalidConfig,
                    ConfigError::AlreadyRunning => Self::AlreadyRunning,
                    ConfigError::Io(_)
                    | ConfigError::Permissions(_)
                    | ConfigError::MissingConfigDirectory
                    | ConfigError::CommitUncertain(_) => Self::Storage,
                    _ => return None,
                })
            })
            .unwrap_or(Self::Other)
    }

    fn messages(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Incompatible => (
                key::STARTUP_INCOMPATIBLE,
                key::STARTUP_INCOMPATIBLE_HELP,
                "incompatible-config",
            ),
            Self::InvalidConfig => (
                key::STARTUP_CONFIG_INVALID,
                key::STARTUP_CONFIG_HELP,
                "invalid-config",
            ),
            Self::AlreadyRunning => (
                key::STARTUP_ALREADY_RUNNING,
                key::STARTUP_ALREADY_RUNNING_HELP,
                "already-running",
            ),
            Self::Storage => (key::STARTUP_STORAGE, key::STARTUP_STORAGE_HELP, "storage"),
            Self::Other => (key::STARTUP_OTHER, key::STARTUP_OTHER_HELP, "other"),
        }
    }
}

fn message(error: &anyhow::Error, context: &StartupContext) -> String {
    let (summary, help, _) = Reason::from_error(error).messages();
    let build = tr(if cfg!(debug_assertions) {
        key::STARTUP_BUILD_DEVELOPMENT
    } else {
        key::STARTUP_BUILD_RELEASE
    });
    let mut parts = vec![
        rustterm::trf!(
            key::STARTUP_VERSION,
            version = env!("CARGO_PKG_VERSION"),
            build = build
        ),
        tr(summary).to_owned(),
        tr(help).to_owned(),
    ];
    if let Some(paths) = &context.paths {
        parts.push(rustterm::trf!(
            key::STARTUP_CONFIG_DIRECTORY,
            path = paths.config.display().to_string()
        ));
    }
    // ConfigError discards TOML source and unrecognized values before this point.
    let details = if let Some(config_error) = error.downcast_ref::<ConfigError>() {
        config_error.to_string()
    } else {
        format!("{error:#}")
    };
    let details = details.chars().take(4096).collect::<String>();
    parts.push(rustterm::trf!(key::STARTUP_DETAILS, error = details));
    if context.logs_ready
        && let Some(paths) = &context.paths
    {
        parts.push(rustterm::trf!(
            key::STARTUP_LOG,
            path = paths.logs().join("diagnostics.log").display().to_string()
        ));
    }
    parts.join("\n\n")
}

pub(super) fn report(error: &anyhow::Error, context: &StartupContext) {
    if context.logs_ready
        && let Some(paths) = &context.paths
    {
        super::diagnostics::startup_failure(&paths.logs(), Reason::from_error(error).messages().2);
    }
    show_native(tr(key::STARTUP_TITLE), &message(error, context));
    // main also prints the original error to stderr and returns a nonzero code.
}

#[cfg(target_os = "linux")]
fn show_native(title: &str, message: &str) {
    use std::process::{Command, Stdio};
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    // Ubuntu packages depend on Zenity. No renderer initialization, shell
    // interpolation or markup interpretation is needed for this error path.
    if Command::new("zenity")
        .args([
            "--error",
            "--no-markup",
            "--width=620",
            "--title",
            title,
            "--text",
            message,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| matches!(status.code(), Some(0 | 1)))
    {
        return;
    }
    // Portable archives may run on desktops without Zenity.
    let _ = Command::new("notify-send")
        .args([
            "--app-name=RustTerm",
            "--urgency=critical",
            "--",
            title,
            message,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn show_native(title: &str, message: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            window: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            style: u32,
        ) -> i32;
    }
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    // SAFETY: Both strings are terminated and alive for the synchronous call.
    // There is no owner window yet. MB_ICONERROR | MB_SETFOREGROUND | MB_OK.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            0x10 | 0x10000,
        );
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn show_native(_title: &str, _message: &str) {}

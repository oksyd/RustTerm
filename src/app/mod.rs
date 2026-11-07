use rustterm::i18n::messages as i18n_key;
mod auth_prompt;
mod command_view;
mod database_view;
mod diagnostics;
mod docker_view;
mod elasticsearch_view;
mod file_transfer;
mod forms;
mod input_capture;
mod interaction;
mod layout;
mod pane;
mod rdp_view;
mod redis_view;
mod resize;
mod resource_monitor;
mod resource_view;
mod terminal_view;
mod text_input;
mod tunnel_panel;
mod ui;
mod updates;
mod window_frame;
mod window_visibility;
mod workspace;

use anyhow::{Context as _, Result, bail};
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use rustterm::config::{AppPaths, ConfigLock, Connection, LocalConfig, Profile};
use rustterm::i18n::{set_language, tr};
use rustterm::trf;
use std::{cell::RefCell, path::PathBuf, rc::Rc};

pub struct Startup {
    profile: Profile,
    panes: usize,
    restore: bool,
    restart: Rc<RefCell<Option<PathBuf>>>,
}

struct Options {
    panes: usize,
    font: Option<String>,
    font_size: Option<f32>,
    shell: Option<String>,
    args: Vec<String>,
    config_dir: Option<PathBuf>,
}
impl Options {
    fn should_restore_workspace(&self) -> bool {
        self.shell.is_none() && self.args.is_empty() && self.panes == 1
    }

    fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>> {
        let mut options = Self {
            panes: 1,
            font: None,
            font_size: None,
            shell: None,
            args: Vec::new(),
            config_dir: None,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => return Ok(None),
                "--panes" => {
                    options.panes = args
                        .next()
                        .context(tr(i18n_key::COMMON_PANES_REQUIRES_1_OR_4))?
                        .parse()?;
                    if !matches!(options.panes, 1 | 4) {
                        bail!(tr(i18n_key::COMMON_PANES_ACCEPTS_ONLY_1_OR_4));
                    }
                }
                "--font" => {
                    options.font = Some(
                        args.next()
                            .context(tr(i18n_key::COMMON_FONT_REQUIRES_A_VALUE))?,
                    )
                }
                "--font-size" => {
                    let value: f32 = args
                        .next()
                        .context(tr(i18n_key::COMMON_FONT_SIZE_REQUIRES_A_VALUE))?
                        .parse()?;
                    if !(8.0..=40.0).contains(&value) {
                        bail!(tr(i18n_key::COMMON_FONT_SIZE_MUST_BE_BETWEEN_8_AND_40));
                    }
                    options.font_size = Some(value);
                }
                "--shell" => {
                    options.shell = Some(
                        args.next()
                            .context(tr(i18n_key::COMMON_SHELL_REQUIRES_A_VALUE))?,
                    )
                }
                "--config-dir" => {
                    options.config_dir = Some(PathBuf::from(
                        args.next()
                            .context(tr(i18n_key::COMMON_CONFIG_DIR_REQUIRES_A_VALUE))?,
                    ))
                }
                "--" => {
                    options.args = args.collect();
                    break;
                }
                _ => bail!(trf!(
                    i18n_key::COMMON_UNKNOWN_ARGUMENT_ARG_USE_HELP_FOR_USAGE,
                    arg = arg
                )),
            }
        }
        Ok(Some(options))
    }
}
pub(crate) fn run() -> Result<()> {
    if std::env::args_os().skip(1).collect::<Vec<_>>() == ["--credential-worker"] {
        return rustterm::config::run_credential_worker().map_err(Into::into);
    }
    if std::env::args().skip(1).eq(["--version"]) {
        println!("RustTerm {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let Some(options) = Options::parse(std::env::args().skip(1))? else {
        println!("{}", tr(i18n_key::COMMON_CLI_HELP));
        return Ok(());
    };
    let restore = options.should_restore_workspace();
    let paths = AppPaths::discover(options.config_dir.clone())?;
    let restart_directory = options.config_dir.as_ref().map(|_| paths.config.clone());
    let lock = ConfigLock::acquire(&paths.config)?;
    let mut config = paths.load_config()?;
    set_language(config.settings.language);
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        bail!(tr(
            i18n_key::COMMON_NO_X11_WAYLAND_SESSION_START_RUSTTERM_FROM_THE_UBUNTU_DESKTOP
        ));
    }
    config.settings.theme.upgrade_legacy_default();
    if let Some(font) = options.font {
        config.settings.font_family = font;
    }
    if let Some(size) = options.font_size {
        config.settings.font_size = size;
    }
    config.validate()?;
    if !paths.config.join("config.toml").exists() {
        paths.save_config(&config)?;
    }
    let initial = if let Some(shell) = options.shell {
        Profile::local(
            tr(i18n_key::COMMON_TEMPORARY_LOCAL_TERMINAL),
            LocalConfig {
                shell,
                args: options.args,
                ..LocalConfig::default()
            },
        )
    } else {
        let mut profile = config
            .profiles
            .iter()
            .find(|p| matches!(p.connection, Connection::Local(_)))
            .cloned()
            .unwrap_or_else(|| {
                Profile::local(tr(i18n_key::COMMON_LOCAL_TERMINAL), LocalConfig::default())
            });
        if !options.args.is_empty()
            && let Connection::Local(local) = &mut profile.connection
        {
            local.args = options.args;
            profile.id = rustterm::config::Uuid::new_v4();
        }
        profile
    };
    let restart = Rc::new(RefCell::new(None::<PathBuf>));
    let startup = Startup {
        profile: initial,
        panes: options.panes,
        restore,
        restart: restart.clone(),
    };
    // A log failure must not prevent users from accessing their connections.
    let _ = paths.prepare_logs();
    let logs = paths.logs();
    diagnostics::record(&logs, "application-start", 0);
    diagnostics::record(
        &logs,
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "backend-wayland"
        } else {
            "backend-x11"
        },
        0,
    );
    let startup_error = Rc::new(RefCell::new(None));
    let window_error = startup_error.clone();
    Application::new()
        .with_assets(ui::Assets)
        .run(move |cx: &mut App| {
            ui::set_theme(config.settings.app_theme, cx);
            ui::prompt::install(cx);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit()
                }
            })
            .detach();
            let bounds = Bounds::centered(
                None,
                size(
                    px(config.settings.window_width),
                    px(config.settings.window_height),
                ),
                cx,
            );
            let result = cx.open_window(
                WindowOptions {
                    app_id: Some("io.rustterm.RustTerm".into()),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_decorations: Some(gpui::WindowDecorations::Client),
                    window_background: gpui::WindowBackgroundAppearance::Transparent,
                    titlebar: Some(TitlebarOptions {
                        title: Some("RustTerm".into()),
                        // Windows uses this flag instead of window_decorations.
                        appears_transparent: cfg!(target_os = "windows"),
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(700.), px(460.))),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| workspace::Workspace::new(config, paths, lock, startup, window, cx))
                },
            );
            if let Err(error) = result {
                *window_error.borrow_mut() = Some(error.to_string());
                cx.quit();
            }
            cx.activate(true);
        });
    if let Some(error) = startup_error.borrow_mut().take() {
        bail!(trf!(
            i18n_key::COMMON_COULD_NOT_CREATE_WINDOW_ERROR,
            error = error
        ));
    }
    if let Some(executable) = restart.borrow_mut().take() {
        let mut command = std::process::Command::new(executable);
        if let Some(directory) = restart_directory {
            command.arg("--config-dir").arg(directory);
        }
        #[cfg(windows)]
        {
            command.spawn().context("Could not restart RustTerm")?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            return Err(command.exec()).context("Could not restart RustTerm");
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_options() {
        for args in [
            vec!["--panes", "3"],
            vec!["--font-size", "NaN"],
            vec!["--shell"],
        ] {
            assert!(Options::parse(args.into_iter().map(String::from)).is_err());
        }
    }
    #[test]
    fn help_needs_no_configuration() {
        assert!(Options::parse(["--help".into()]).unwrap().is_none());
    }
    #[test]
    fn shell_arguments_are_not_interpreted() {
        let o = Options::parse(
            ["--shell", "/bin/sh", "--", "-c", "echo '$HOME'"]
                .into_iter()
                .map(String::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.args, ["-c", "echo '$HOME'"]);
    }
    #[test]
    fn explicit_shell_intent_disables_workspace_restore() {
        for args in [
            vec!["--", "-c", "printf hi"],
            vec!["--shell", "/bin/bash"],
            vec!["--panes", "4"],
        ] {
            let options = Options::parse(args.into_iter().map(str::to_owned))
                .unwrap()
                .unwrap();
            assert!(!options.should_restore_workspace());
        }
        for args in [
            vec![],
            vec!["--panes", "1"],
            vec!["--font-size", "16"],
            vec!["--"],
        ] {
            let options = Options::parse(args.into_iter().map(str::to_owned))
                .unwrap()
                .unwrap();
            assert!(options.should_restore_workspace());
        }
    }
}

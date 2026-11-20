use std::{collections::BTreeMap, io, path::Path};

pub const BASH: &str = include_str!("integration/integration.bash");
pub const ZSH: &str = include_str!("integration/integration.zsh");
pub const FISH: &str = include_str!("integration/integration.fish");

/// Starts an integrated interactive login shell without consuming terminal input
/// or changing user startup files. The bootstrap owns its private temporary files.
pub fn remote_command() -> String {
    let script = include_str!("integration/remote.sh")
        .replace(
            "@BASH_STARTUP@",
            &quote_posix(&format!(
                "{}\n{BASH}",
                include_str!("integration/remote.bash")
            )),
        )
        .replace(
            "@ZSH_ENV@",
            &quote_posix(&format!(
                "typeset -g __rustterm_user_zdotdir=$__RUSTTERM_ORIGINAL_ZDOTDIR\ntypeset -g __rustterm_user_zdotdir_set=$__RUSTTERM_ZDOTDIR_SET\nunset __RUSTTERM_ORIGINAL_ZDOTDIR __RUSTTERM_ZDOTDIR_SET\n{}",
                remote_zsh_stage(".zshenv", false),
            )),
        )
        .replace("@ZSH_PROFILE@", &quote_posix(&remote_zsh_stage(".zprofile", false)))
        .replace("@ZSH_RC@", &quote_posix(&remote_zsh_stage(".zshrc", false)))
        .replace(
            "@ZSH_LOGIN@",
            &quote_posix(&format!(
                "{}\n{ZSH}",
                remote_zsh_stage(".zlogin", true),
            )),
        )
        .replace(
            "@FISH_STARTUP@",
            &quote_posix(&format!(
                "{}\n{FISH}",
                include_str!("integration/remote.fish")
            )),
        );
    // The SSH server invokes this command through the account's shell. Double
    // quoting these characters has the same meaning in Bash, Zsh and Fish.
    let mut command = String::from("exec sh -c \"");
    for ch in script.chars() {
        if matches!(ch, '\\' | '"' | '$' | '`') {
            command.push('\\');
        }
        command.push(ch);
    }
    command.push('"');
    command
}

fn remote_zsh_stage(file: &str, final_stage: bool) -> String {
    include_str!("integration/remote.zsh")
        .replace("@STARTUP_FILE@", file)
        .replace("@FINAL_STAGE@", if final_stage { "yes" } else { "no" })
}

/// Private startup files live as long as the child process. User dotfiles stay untouched.
pub struct ShellIntegration {
    pub directory: tempfile::TempDir,
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
}

pub fn prepare(
    shell: &str,
    arguments: &[String],
    environment: &BTreeMap<String, String>,
) -> io::Result<Option<ShellIntegration>> {
    if !arguments.is_empty() {
        return Ok(None);
    }
    let shell = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !matches!(shell, "bash" | "zsh" | "fish") {
        return Ok(None);
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("rustterm-shell-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir()?;
    let path = directory.path().to_string_lossy().into_owned();
    let mut environment_out = Vec::new();
    let arguments = match shell {
        "bash" => {
            std::fs::write(
                directory.path().join("rc.bash"),
                format!("[[ -r ~/.bashrc ]] && source ~/.bashrc\n{BASH}"),
            )?;
            vec!["--rcfile".into(), format!("{path}/rc.bash"), "-i".into()]
        }
        "zsh" => {
            let original = environment
                .get("ZDOTDIR")
                .cloned()
                .or_else(|| std::env::var("ZDOTDIR").ok())
                .unwrap_or_default();
            environment_out.push(("RUSTTERM_USER_ZDOTDIR".into(), original));
            environment_out.push(("ZDOTDIR".into(), path.clone()));
            std::fs::write(
                directory.path().join(".zshenv"),
                "typeset -g __rustterm_injected_zdotdir=$ZDOTDIR\nZDOTDIR=${RUSTTERM_USER_ZDOTDIR:-$HOME}\n[[ -r $ZDOTDIR/.zshenv ]] && source $ZDOTDIR/.zshenv\ntypeset -g __rustterm_user_zdotdir=$ZDOTDIR\nZDOTDIR=$__rustterm_injected_zdotdir\n",
            )?;
            std::fs::write(
                directory.path().join(".zshrc"),
                format!(
                    "ZDOTDIR=$__rustterm_user_zdotdir\n[[ -r $ZDOTDIR/.zshrc ]] && source $ZDOTDIR/.zshrc\n{ZSH}"
                ),
            )?;
            vec!["-i".into()]
        }
        "fish" => {
            std::fs::write(directory.path().join("init.fish"), FISH)?;
            vec![
                "--interactive".into(),
                "--init-command".into(),
                format!("source {}", quote_fish(&format!("{path}/init.fish"))),
            ]
        }
        _ => return Ok(None),
    };
    Ok(Some(ShellIntegration {
        directory,
        arguments,
        environment: environment_out,
    }))
}

pub fn activation(shell: &str) -> Option<String> {
    match shell {
        "bash" => Some(format!("eval {}\n", quote_posix(BASH))),
        "zsh" => Some(format!("eval {}\n", quote_posix(ZSH))),
        "fish" => Some(format!("eval {}\n", quote_fish(FISH))),
        _ => None,
    }
}
/// The caller must get explicit authorization at a prompt; OSC metadata cannot authorize input.
pub fn directory_command(shell: &str, directory: &str) -> Option<String> {
    if !directory.starts_with('/')
        || directory.len() > 4096
        || directory.chars().any(char::is_control)
    {
        return None;
    }
    let directory = match shell {
        "bash" | "zsh" => quote_posix(directory),
        "fish" => quote_fish(directory),
        _ => return None,
    };
    Some(format!("builtin cd -- {directory}\n"))
}

fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn quote_fish(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(all(test, unix))]
mod test_process;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn interactive_bash(integrated: bool, startup: &str, input: &[u8]) -> std::process::Output {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".bashrc");
        std::fs::write(&rc, format!("unset HISTFILE\nPS1='test> '\n{startup}\n")).unwrap();
        let integration = integrated.then(|| {
            prepare("/bin/bash", &[], &BTreeMap::new())
                .unwrap()
                .unwrap()
        });
        let arguments = integration.as_ref().map_or_else(
            || {
                vec![
                    "--rcfile".into(),
                    rc.to_string_lossy().into_owned(),
                    "-i".into(),
                ]
            },
            |integration| integration.arguments.clone(),
        );
        test_process::output(
            test_process::command("/bin/bash")
                .args(arguments)
                .env("HOME", home.path()),
            input,
            std::time::Duration::from_secs(5),
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn observations(output: &std::process::Output) -> Vec<String> {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("OBSERVED "))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn bash_integration_does_not_repeat_error_traps() {
        for errtrace in [false, true] {
            let startup = format!(
                "{}\ntrap 'builtin printf \"\\nOBSERVED ERR:%s:%s\\n\" \"$?\" \"$BASH_COMMAND\"' ERR",
                if errtrace { "set -E" } else { "set +E" },
            );
            let input = b"false\ntrue | false\nfalse || true\nexit 0\n";
            let plain = interactive_bash(false, &startup, input);
            let integrated = interactive_bash(true, &startup, input);
            assert!(plain.status.success());
            assert_eq!(integrated.status.code(), plain.status.code());
            assert_eq!(
                observations(&plain),
                ["OBSERVED ERR:1:false", "OBSERVED ERR:1:false"]
            );
            assert_eq!(observations(&integrated), observations(&plain));
            assert!(String::from_utf8_lossy(&integrated.stdout).contains("\x1b]133;D;1\x07"));
        }
    }

    #[test]
    #[cfg(unix)]
    fn bash_integration_preserves_prompt_status_and_pipelines() {
        for prompt in [
            r#"PROMPT_COMMAND='builtin printf "\nOBSERVED PROMPT:%s:%s\n" "$?" "${PIPESTATUS[*]}"'"#,
            r#"PROMPT_COMMAND=('builtin printf "\nOBSERVED PROMPT:%s:%s\n" "$?" "${PIPESTATUS[*]}"' 'builtin printf "\nOBSERVED NEXT:%s:%s\n" "$?" "${PIPESTATUS[*]}"')"#,
        ] {
            let input = b"false\nbuiltin printf '\\nOBSERVED STATUS:%s:%s\\n' \"$?\" \"${PIPESTATUS[*]}\"\nfalse | true\ntrue | false\nset -o pipefail\nfalse | true\nexit 7\n";
            let plain = interactive_bash(false, prompt, input);
            let integrated = interactive_bash(true, prompt, input);
            assert_eq!(plain.status.code(), Some(7));
            assert_eq!(integrated.status.code(), plain.status.code());
            let expected = observations(&plain);
            for required in [
                "OBSERVED PROMPT:1:1",
                "OBSERVED PROMPT:0:1 0",
                "OBSERVED PROMPT:1:0 1",
                "OBSERVED PROMPT:1:1 0",
                "OBSERVED STATUS:1:1",
            ] {
                assert!(expected.iter().any(|line| line == required), "{expected:?}");
            }
            assert_eq!(observations(&integrated), expected);
        }
    }

    #[test]
    #[cfg(unix)]
    fn bash_integration_preserves_strict_mode_and_conditional_execution() {
        let startup = r#"set -Eeuo pipefail
trap 'builtin printf "\nOBSERVED ERR:%s:%s\n" "$?" "$BASH_COMMAND"' ERR"#;
        let input = b"false || builtin printf '\\nOBSERVED RECOVERED\\n'\nfalse && builtin printf '\\nOBSERVED UNEXPECTED\\n'\nif false; then builtin printf '\\nOBSERVED UNEXPECTED\\n'; fi\nbuiltin printf '\\nOBSERVED BEFORE_FAILURE\\n'\nfalse\nbuiltin printf '\\nOBSERVED UNEXPECTED\\n'\n";
        let plain = interactive_bash(false, startup, input);
        let integrated = interactive_bash(true, startup, input);
        assert_eq!(plain.status.code(), Some(1));
        assert_eq!(integrated.status.code(), plain.status.code());
        assert_eq!(
            observations(&plain),
            [
                "OBSERVED RECOVERED",
                "OBSERVED BEFORE_FAILURE",
                "OBSERVED ERR:1:false"
            ]
        );
        assert_eq!(observations(&integrated), observations(&plain));
    }

    #[cfg(unix)]
    fn remote_bash(profile: &str, input: &[u8]) -> (std::process::Output, tempfile::TempDir) {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("user ' with spaces");
        let scratch = fixture.path().join("scratch");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(home.join(".bash_profile"), profile).unwrap();
        std::fs::write(
            home.join(".bashrc"),
            "printf 'rc\\n' >> \"$RUSTTERM_TEST_TRACE\"\nPS1='test> '\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".bash_logout"),
            "printf 'logout\\n' >> \"$RUSTTERM_TEST_TRACE\"\n",
        )
        .unwrap();
        let output = test_process::output(
            test_process::command("/bin/sh")
                .args(["-c", &remote_command()])
                .env("SHELL", "/bin/bash")
                .env("HOME", &home)
                .env("TMPDIR", &scratch)
                .env("ENV", "previous-env")
                .env("RUSTTERM_TEST_TRACE", fixture.path().join("trace")),
            input,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(".bash_profile")).unwrap(),
            profile
        );
        assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 0);
        (output, fixture)
    }

    #[test]
    #[cfg(unix)]
    fn remote_bash_preserves_login_configuration_input_and_exit_status() {
        let (output, fixture) = remote_bash(
            "printf 'profile\\n' >> \"$RUSTTERM_TEST_TRACE\"\nsource \"$HOME/.bashrc\"\n",
            b"printf 'ENV:%s\\n' \"$ENV\"; shopt -q login_shell && printf 'LOGIN\\n'; false\nexit 7\n",
        );
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("trace")).unwrap(),
            "profile\nrc\nlogout\n"
        );
        let transcript = [output.stdout, output.stderr].concat();
        let transcript = String::from_utf8_lossy(&transcript);
        assert!(transcript.contains("ENV:previous-env"));
        assert!(transcript.contains("LOGIN"));
        assert!(transcript.contains("\x1b]133;P;RustTerm=bash\x07"));
        assert!(transcript.contains("\x1b]133;D;1\x07"));
    }

    #[test]
    #[cfg(unix)]
    fn remote_bash_preserves_error_traps_and_handled_failures() {
        let startup = r#"PS1='test> '
set -Eeuo pipefail
trap 'builtin printf "\nOBSERVED ERR:%s:%s\n" "$?" "$BASH_COMMAND"' ERR"#;
        let input =
            b"false && true\nbuiltin printf '\\nOBSERVED SURVIVED\\n'\nset +e\nfalse\nexit 7\n";
        let plain = interactive_bash(false, startup, input);
        let (remote, _) = remote_bash(startup, input);
        assert_eq!(plain.status.code(), Some(7));
        assert_eq!(remote.status.code(), plain.status.code());
        assert_eq!(
            observations(&plain),
            ["OBSERVED SURVIVED", "OBSERVED ERR:1:false"]
        );
        assert_eq!(observations(&remote), observations(&plain));
        assert!(String::from_utf8_lossy(&remote.stdout).contains("\x1b]133;D;1\x07"));
    }

    #[test]
    #[cfg(unix)]
    fn remote_startup_cleanup_survives_profile_exit_or_exec() {
        for (profile, status) in [("exit 19\n", 19), ("exec /bin/sh -c 'exit 23'\n", 23)] {
            let (output, _) = remote_bash(profile, b"");
            assert_eq!(output.status.code(), Some(status));
        }
    }
    #[test]
    fn scripts_are_private_and_custom_commands_remain_untouched() {
        assert!(
            prepare(
                "/bin/bash",
                &["-c".into(), "echo hi".into()],
                &BTreeMap::new()
            )
            .unwrap()
            .is_none()
        );
        let integration = prepare("/bin/bash", &[], &BTreeMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(integration.arguments[0], "--rcfile");
        assert!(
            std::fs::read_to_string(integration.directory.path().join("rc.bash"))
                .unwrap()
                .contains("source ~/.bashrc")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(integration.directory.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }
    #[test]
    #[cfg(unix)]
    fn directory_restore_does_not_interpret_shell_metacharacters() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quote' slash\\ $(false); space");
        std::fs::create_dir(&path).unwrap();
        let command = directory_command("bash", path.to_str().unwrap()).unwrap();
        let output = test_process::output(
            test_process::command("/bin/bash").args([
                "--noprofile",
                "--norc",
                "-c",
                &(command + "pwd -P"),
            ]),
            b"",
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim_end(),
            path.to_str().unwrap()
        );
        assert!(directory_command("bash", "/tmp/\nmalformed").is_none());
        assert!(directory_command("powershell", "/tmp").is_none());
    }
}

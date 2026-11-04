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
        _ => unreachable!(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

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
        let mut child = Command::new("/bin/sh")
            .args(["-c", &remote_command()])
            .env("SHELL", "/bin/bash")
            .env("HOME", &home)
            .env("TMPDIR", &scratch)
            .env("ENV", "previous-env")
            .env("RUSTTERM_TEST_TRACE", fixture.path().join("trace"))
            .env_remove("BASH_ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(".bash_profile")).unwrap(),
            profile
        );
        assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 0);
        (output, fixture)
    }

    #[test]
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
    fn directory_restore_does_not_interpret_shell_metacharacters() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quote' slash\\ $(false); space");
        std::fs::create_dir(&path).unwrap();
        let command = directory_command("bash", path.to_str().unwrap()).unwrap();
        let output = std::process::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-c", &(command + "pwd -P")])
            .output()
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

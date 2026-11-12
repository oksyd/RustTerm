use super::*;
use rustterm::terminal::ShellInteraction;
use std::{collections::BTreeMap, fs, process::Command as ProcessCommand};

#[track_caller]
fn wait_mode(session: &Session, expected: ShellInteraction) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let text = fresh_text(session);
        let snapshot = session.snapshot();
        if snapshot.shell.interaction == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected:?}, got {:?}, state {:?}: {text}",
            snapshot.shell.interaction,
            session.state()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn input(session: &Session, bytes: &[u8]) {
    session.send(Command::Input(bytes.to_vec())).unwrap();
}

fn bash(rc: &str, remote: bool) -> (Session, tempfile::TempDir) {
    let fixture = tempfile::tempdir().unwrap();
    fs::write(
        fixture.path().join(".bashrc"),
        format!(
            r#"
PS1='test> '
HISTFILE=/dev/null
HISTSIZE=100
set -o emacs
bind 'set enable-bracketed-paste on'
history -s 'printf rustterm_search_needle'
{rc}
"#
        ),
    )
    .unwrap();
    // The remote bootstrap starts a login shell, just like an SSH connection.
    fs::write(fixture.path().join(".bash_profile"), "source ~/.bashrc\n").unwrap();
    let session = Session::spawn_local(
        LocalOptions {
            shell: if remote { "/bin/sh" } else { "/bin/bash" }.into(),
            args: if remote {
                vec![
                    "-c".into(),
                    rustterm::terminal::integration::remote_command(),
                ]
            } else {
                vec![]
            },
            env: BTreeMap::from([
                ("HOME".into(), fixture.path().to_string_lossy().into_owned()),
                ("SHELL".into(), "/bin/bash".into()),
                ("INPUTRC".into(), "/dev/null".into()),
            ]),
            ..Default::default()
        },
        80,
        24,
        100,
    )
    .unwrap();
    wait_mode(&session, ShellInteraction::Prompt);
    assert_eq!(session.snapshot().shell.shell, Some("bash"));
    (session, fixture)
}

#[test]
fn bash_search_cancel_accept_resize_and_execution_report_actual_modes() {
    let (session, _fixture) = bash("", false);
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    assert!(session.snapshot().shell.history_search_supported);
    assert!(!session.can_insert_command());
    input(&session, b"-no-match");
    wait_until(|| fresh_text(&session).contains("failed reverse-i-search"));
    assert_eq!(
        session.snapshot().shell.interaction,
        ShellInteraction::HistorySearch
    );
    session
        .send(Command::Resize {
            columns: 110,
            rows: 32,
        })
        .unwrap();
    wait_until(|| {
        fresh_text(&session);
        session.snapshot().columns == 110
    });
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x07");
    wait_mode(&session, ShellInteraction::Prompt);
    assert!(session.can_insert_command());
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x1b"); // Accept the result without executing it.
    wait_mode(&session, ShellInteraction::Prompt);
    input(&session, b"\r");
    wait_until(|| {
        fresh_text(&session);
        session
            .snapshot()
            .shell
            .commands
            .iter()
            .any(|command| command.command.contains("rustterm_search_needle"))
    });
    wait_mode(&session, ShellInteraction::Prompt);
    input(&session, b"read -r test_answer\r");
    wait_mode(&session, ShellInteraction::Executing);
    input(&session, b"done\r");
    wait_mode(&session, ShellInteraction::Prompt);
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x03");
    wait_mode(&session, ShellInteraction::Prompt);
    input(&session, b"\x04");
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    wait_mode(&session, ShellInteraction::Unknown);
}

#[test]
fn bash_custom_search_binding_is_preserved_without_interpreting_ctrl_r() {
    let (session, _fixture) = bash(
        r#"
bind '"\C-r": beginning-of-line'
bind '"\C-xr": reverse-search-history'
"#,
        false,
    );
    input(&session, b"text\x12");
    wait_until(|| fresh_text(&session).contains("text") && session.snapshot().cursor.column == 6);
    assert_eq!(
        session.snapshot().shell.interaction,
        ShellInteraction::Prompt
    );
    input(&session, b"\x05\x15\x18rneedle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x07");
    wait_mode(&session, ShellInteraction::Prompt);
    session.close();
}

#[test]
fn bash_multiline_search_survives_resize_and_search_direction_changes() {
    let (session, _fixture) = bash(
        r#"
bind '"\C-xf": forward-search-history'
history -s $'echo first_line\necho a_long_result_with_中文_and_multiline_needle'
"#,
        false,
    );
    input(&session, b"\x12multiline_needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    for (columns, rows) in [(110, 32), (140, 24), (80, 18)] {
        session.send(Command::Resize { columns, rows }).unwrap();
        wait_until(|| {
            fresh_text(&session);
            session.snapshot().columns == columns
        });
        wait_mode(&session, ShellInteraction::HistorySearch);
        assert!(!session.can_insert_command());
    }
    input(&session, b"\x07");
    wait_mode(&session, ShellInteraction::Prompt);
    input(&session, b"\x18f");
    wait_mode(&session, ShellInteraction::HistorySearch);
    assert!(!fresh_text(&session).contains("reverse-i-search"));
    input(&session, b"\x07");
    wait_mode(&session, ShellInteraction::Prompt);
    session.close();
}

#[test]
fn bash_ssh_bootstrap_recognizes_search_with_a_multiline_colored_prompt() {
    let (session, _fixture) = bash(r#"PS1='\[\e[32m\]test host\[\e[0m\]\n$ '"#, true);
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x07");
    wait_mode(&session, ShellInteraction::Prompt);
    input(&session, b"\x12needle\r");
    wait_until(|| {
        fresh_text(&session);
        !session.snapshot().shell.commands.is_empty()
    });
    wait_mode(&session, ShellInteraction::Prompt);
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    session.disconnect();
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    wait_mode(&session, ShellInteraction::Unknown);
}

fn zsh(remapped: bool, remote: bool) -> Option<(Session, tempfile::TempDir)> {
    let explicit = std::env::var("RUSTTERM_TEST_ZSH").ok();
    let shell = explicit.as_deref().unwrap_or("zsh");
    if !ProcessCommand::new(shell)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        assert!(explicit.is_none(), "configured test Zsh is unavailable");
        eprintln!("skipping Zsh PTY test: install zsh or set RUSTTERM_TEST_ZSH");
        return None;
    }
    let fixture = tempfile::tempdir().unwrap();
    // Exercise our startup files without loading the host's global rc files.
    // Ubuntu's global zshrc calls compinit, which can block on a directory trust
    // prompt on CI runners. Keep RCS enabled so the fixture and integration load.
    // Optional search paths allow testing an extracted shell without installing it.
    fs::write(fixture.path().join(".zshenv"), r#"
unsetopt GLOBAL_RCS
[[ -z ${RUSTTERM_TEST_ZSH_FPATH-} ]] || fpath=($RUSTTERM_TEST_ZSH_FPATH $fpath)
[[ -z ${RUSTTERM_TEST_ZSH_MODULE_PATH-} ]] || module_path=($RUSTTERM_TEST_ZSH_MODULE_PATH $module_path)
"#).unwrap();
    let rc = format!(
        r#"
PROMPT='test> '
KEYTIMEOUT=1
HISTSIZE=100
bindkey -e
bindkey -M isearch '^O' accept-search
print -s -- 'printf rustterm_search_needle'
test_search_update() {{ print -r -- update >> "$HOME/hooks"; return 0; }}
test_search_exit() {{ print -r -- exit >> "$HOME/hooks"; return 0; }}
zle -N zle-isearch-update test_search_update
zle -N zle-isearch-exit test_search_exit
{}
"#,
        if remapped {
            "bindkey '^R' beginning-of-line\nbindkey '^Xr' history-incremental-search-backward"
        } else {
            ""
        }
    );
    fs::write(fixture.path().join(".zshrc"), &rc).unwrap();
    let home = fixture.path().to_string_lossy().into_owned();
    let session = Session::spawn_local(
        LocalOptions {
            shell: if remote {
                "/bin/sh".into()
            } else {
                shell.into()
            },
            args: if remote {
                vec![
                    "-c".into(),
                    rustterm::terminal::integration::remote_command(),
                ]
            } else {
                vec![]
            },
            env: BTreeMap::from([
                ("HOME".into(), home.clone()),
                ("ZDOTDIR".into(), home),
                ("SHELL".into(), shell.into()),
            ]),
            ..Default::default()
        },
        80,
        24,
        100,
    )
    .unwrap();
    wait_mode(&session, ShellInteraction::Editing);
    assert!(
        session.snapshot().shell.history_search_supported,
        "{}",
        fresh_text(&session)
    );
    assert_eq!(
        fs::read_to_string(fixture.path().join(".zshrc")).unwrap(),
        rc
    );
    Some((session, fixture))
}

#[test]
fn zsh_search_cancel_accept_resize_and_execution_report_actual_modes() {
    let Some((session, fixture)) = zsh(false, false) else {
        return;
    };
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    assert!(!session.can_insert_command());
    for keys in [&b"\x0c"[..], b"-no-match", b"\x7f"] {
        let output = session.output_revision();
        input(&session, keys);
        wait_until(|| session.output_revision() > output);
        fresh_text(&session);
        assert_eq!(
            session.snapshot().shell.interaction,
            ShellInteraction::HistorySearch
        );
    }
    session
        .send(Command::Resize {
            columns: 110,
            rows: 32,
        })
        .unwrap();
    wait_until(|| {
        fresh_text(&session);
        session.snapshot().columns == 110
    });
    assert_eq!(
        session.snapshot().shell.interaction,
        ShellInteraction::HistorySearch
    );
    input(&session, b"\x07"); // Abort the search, restoring the original line.
    wait_mode(&session, ShellInteraction::Editing);
    assert!(session.can_insert_command());
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x0f"); // The fixture binds accept-search without executing it.
    wait_mode(&session, ShellInteraction::Editing);
    input(&session, b"\r");
    wait_until(|| {
        fresh_text(&session);
        session
            .snapshot()
            .shell
            .commands
            .iter()
            .any(|command| command.command.contains("rustterm_search_needle"))
    });
    wait_mode(&session, ShellInteraction::Editing);
    input(&session, b"read -r test_answer\r");
    wait_mode(&session, ShellInteraction::Executing);
    input(&session, b"done\r");
    wait_mode(&session, ShellInteraction::Editing);
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x03"); // Interrupt search and return to a fresh prompt.
    wait_mode(&session, ShellInteraction::Editing);
    let hooks = fs::read_to_string(fixture.path().join("hooks")).unwrap();
    assert!(
        hooks.contains("update\n") && hooks.contains("exit\n"),
        "{hooks}"
    );
    input(&session, b"\x12");
    wait_mode(&session, ShellInteraction::HistorySearch);
    session.disconnect();
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    wait_mode(&session, ShellInteraction::Unknown);
}

#[test]
fn zsh_custom_search_binding_is_preserved_without_interpreting_ctrl_r() {
    let Some((session, _fixture)) = zsh(true, false) else {
        return;
    };
    input(&session, b"text\x12");
    wait_until(|| fresh_text(&session).contains("text"));
    assert_eq!(
        session.snapshot().shell.interaction,
        ShellInteraction::Editing
    );
    assert_eq!(session.snapshot().cursor.column, 6);
    input(&session, b"\x05\x15\x18rneedle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x07");
    wait_mode(&session, ShellInteraction::Editing);
    session.close();
}

#[test]
fn ssh_bootstrap_installs_the_same_search_hooks_in_the_login_shell() {
    let Some((session, _fixture)) = zsh(false, true) else {
        return;
    };
    input(&session, b"\x12needle");
    wait_mode(&session, ShellInteraction::HistorySearch);
    input(&session, b"\x07");
    wait_mode(&session, ShellInteraction::Editing);
    input(&session, b"exit 0\r");
    wait_until(|| session.state() == SessionState::Disconnected { exit_code: Some(0) });
    wait_mode(&session, ShellInteraction::Unknown);
}

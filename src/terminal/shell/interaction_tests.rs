use super::*;

fn integrated_search() -> Terminal {
    let mut terminal = Terminal::new(40, 4, 100);
    terminal.feed(b"\x1b[?2004h\x1b]133;P;RustTerm=zsh\x07\x1b]133;P;RustTermHistorySearch=1\x07\x1b]133;B\x07echo test\r\n\x1b]133;C\x07test\r\n\x1b]133;D;0\x07\x1b]133;B\x07");
    assert!(terminal.can_insert_command());
    terminal.feed(b"\x1b]133;P;RustTermInput=history-search\x07");
    assert!(!terminal.can_insert_command());
    assert!(terminal.command_draft(1).is_empty());
    assert!(terminal.snapshot().shell.command_insert_blocked);
    terminal
}

#[test]
fn search_requires_shell_capability_and_never_uses_screen_text() {
    let mut terminal = Terminal::new(60, 5, 100);
    for shell in ["bash", "fish", "unknown"] {
        terminal.feed(format!("\x1b]133;P;RustTerm={shell}\x07\x1b]133;B\x07\x1b]133;P;RustTermHistorySearch=1\x07").as_bytes());
        terminal.feed(b"(reverse-i-search)`x': \x1b]133;P;RustTermInput=history-search\x07");
        assert!(!terminal.snapshot().shell.history_search_supported);
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::Prompt
        );
    }
    terminal.feed(
        b"\x1b]133;P;RustTerm=zsh\x07\x1b]133;B\x07\x1b]133;P;RustTermInput=history-search\x07",
    );
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Prompt
    );
    terminal.feed(b"\x1b]133;P;RustTermHistorySearch=1\x07");
    // Chunk boundaries and resize must not drop an active mode.
    for byte in b"\x1b]133;P;RustTermInput=history-search\x1b\\" {
        terminal.feed(&[*byte]);
    }
    terminal.resize(90, 8);
    terminal.feed(b"\r\x1b]133;A\x07test> \x1b]133;B\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::HistorySearch
    );
    terminal.feed(b"\x1b]133;P;RustTermInput=unsupported\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::HistorySearch
    );
    terminal.feed(b"\x1b]133;P;RustTermInput=editing\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Editing
    );
    terminal.resize(60, 5);
    terminal.feed(b"\r\x1b]133;A\x07test> \x1b]133;B\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Editing
    );
    terminal.feed(b"\x1b]133;P;RustTerm=zsh\x07\x1b]133;P;RustTermHistorySearch=1\x07\x1b]133;A\x07test> \x1b]133;B\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Prompt,
        "a capability announcement is not an editor mode notification"
    );
}

#[test]
fn search_cannot_leak_into_execution_fullscreen_reset_or_archive() {
    for end in [
        &b"\x1b]133;P;RustTermInput=finished\x07"[..],
        b"\x1b]133;D;0\x07",
        b"\x1b]133;P;RustTerm=bash\x07",
        b"\x1b]133;P;RustTermHistorySearch=0\x07",
        b"\x1bc",
        b"\x1b[?1049h\x1b]133;P;RustTermInput=editing\x07\x1b[?1049l",
    ] {
        let mut terminal = integrated_search();
        terminal.feed(end);
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::Unknown,
            "{end:?}"
        );
        assert!(!terminal.can_insert_command());
    }
    let mut terminal = integrated_search();
    terminal.feed(b"\x1b]133;C\x07\x1b]133;P;RustTermInput=history-search\x07\x1b]133;P;RustTermInput=editing\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Executing
    );
    terminal.feed(b"\x1b[?1049h\x1b[?1049l");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Executing
    );
    terminal.feed(b"\x1b]133;D;0\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Unknown
    );
    let mut terminal = integrated_search();
    terminal.freeze();
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Unknown
    );
    assert!(!terminal.can_insert_command());
}

#[test]
fn search_and_capability_are_scoped_to_each_terminal() {
    let mut searching = integrated_search();
    let mut editing = Terminal::new(40, 4, 100);
    editing.feed(b"\x1b]133;P;RustTerm=bash\x07\x1b]133;B\x07");
    assert_eq!(
        editing.snapshot().shell.interaction,
        ShellInteraction::Prompt
    );
    assert!(!editing.snapshot().shell.history_search_supported);
    assert_eq!(
        searching.snapshot().shell.interaction,
        ShellInteraction::HistorySearch
    );
    searching.feed(b"\x1b]133;P;RustTermInput=editing\x07");
    assert!(searching.can_insert_command());
    assert!(!searching.command_draft(1).is_empty());
}

#[test]
fn empty_commands_still_end_execution_and_clear_timing() {
    let mut terminal = Terminal::new(40, 4, 100);
    terminal.feed(b"\x1b]133;P;RustTerm=fish\x07\x1b]133;B\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Prompt
    );
    terminal.feed(b"\x1b]133;C\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Executing
    );
    terminal.feed(b"\x1b]133;D;0\x07");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Unknown
    );
    assert!(terminal.shell.started_at.is_none());
    assert!(terminal.last_completed_command().is_none());
}

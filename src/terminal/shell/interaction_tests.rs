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
fn editor_notifications_require_zsh_search_hooks() {
    let mut terminal = Terminal::new(60, 5, 100);
    for shell in ["fish", "unknown"] {
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
    assert!(editing.snapshot().shell.history_search_supported);
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

fn bash_prompt(columns: u16, prompt: &str) -> Terminal {
    let mut terminal = Terminal::new(columns, 6, 100);
    terminal.feed(
        format!("\x1b[?2004h\x1b]133;P;RustTerm=bash\x07\x1b]133;A\x07{prompt}\x1b]133;B\x07")
            .as_bytes(),
    );
    terminal
}

#[test]
fn bash_readline_search_tracks_the_live_wrapped_line_and_preserves_prompt_readiness() {
    for prefix in [
        "reverse-i-search",
        "failed reverse-i-search",
        "i-search",
        "failed i-search",
    ] {
        let mut terminal = bash_prompt(20, "test> ");
        let redraw = format!("\r\x1b[2K({prefix})`中文': echo 中文");
        for byte in redraw.as_bytes() {
            terminal.feed(&[*byte]);
        }
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::HistorySearch,
            "{prefix}"
        );
        assert!(!terminal.can_insert_command());
        terminal.resize(55, 8);
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::HistorySearch
        );
        terminal.feed(b"\r\x1b[2Ktest> echo test");
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::Prompt
        );
        assert!(terminal.can_insert_command());
    }
}

#[test]
fn bash_search_text_requires_an_integrated_prompt_and_cannot_leak_to_other_states() {
    for before in [
        &b"\x1b]133;C\x07"[..],
        b"\x1b]133;D;0\x07",
        b"\x1b]133;P;RustTerm=fish\x07\x1b]133;B\x07",
        b"\x1b[?1049h",
        b"\x1bc",
    ] {
        let mut terminal = bash_prompt(80, "test> ");
        terminal.feed(before);
        terminal.feed(b"\r\x1b[2K(reverse-i-search)`needle': echo needle");
        assert_ne!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::HistorySearch,
            "{before:?}"
        );
    }
    let mut terminal = Terminal::new(80, 6, 100);
    terminal.feed(b"(reverse-i-search)`needle': echo needle");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Unknown
    );
    let mut terminal = bash_prompt(80, "test> ");
    terminal.feed(b"\r\x1b[2K(reverse-i-search)`needle': echo needle");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::HistorySearch
    );
    terminal.freeze();
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Unknown
    );
}

#[test]
fn bash_search_does_not_override_normal_input_literal_prompts_or_accept_zsh_notifications() {
    for prompt in ["test> ", "(reverse-i-search)`custom': "] {
        let mut terminal = bash_prompt(80, prompt);
        terminal.feed(b"(reverse-i-search)`needle': echo needle");
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::Prompt
        );
        terminal.feed(
            b"\x1b]133;P;RustTermHistorySearch=1\x07\x1b]133;P;RustTermInput=history-search\x07",
        );
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::Prompt
        );
    }
}

#[test]
fn bash_search_uses_the_live_prompt_when_scrollback_is_visible() {
    let mut terminal = bash_prompt(80, "test> ");
    terminal.feed(b"\x1b]133;C\x07\r\n(reverse-i-search)`output': text\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n\x1b]133;D;0\x07\x1b]133;A\x07test> \x1b]133;B\x07");
    terminal.scroll(5);
    assert!(terminal.snapshot().display_offset > 0);
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::Prompt
    );
    terminal.feed(b"\r\x1b[2K(reverse-i-search)`needle': echo first\r\necho needle");
    assert_eq!(
        terminal.snapshot().shell.interaction,
        ShellInteraction::HistorySearch
    );
    for columns in [30, 100] {
        terminal.resize(columns, 8);
        assert_eq!(
            terminal.snapshot().shell.interaction,
            ShellInteraction::HistorySearch
        );
    }
}

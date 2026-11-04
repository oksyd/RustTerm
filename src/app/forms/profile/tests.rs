use super::{ProfileDraft, SshAuthentication, display_text, preserve_text, preserved_path};
use crate::app::forms::parse_value;
use rustterm::i18n::messages as i18n_key;
use rustterm::{config::Uuid, i18n::tr};
use std::path::PathBuf;

#[test]
fn raw_drafts_detect_invalid_text_hidden_fields_and_empty_rows() {
    let initial = ProfileDraft {
        fields: vec!["22".into(), "/keys/old".into()],
        ..ProfileDraft::default()
    };
    let mut draft = initial.clone();
    draft.fields[0] = "invalid port".into();
    assert_ne!(draft, initial);
    draft = initial.clone();
    draft.fields[1] = "/keys/new".into();
    assert_ne!(draft, initial); // The key remains part of the draft in password mode.
    draft = initial.clone();
    draft.arguments.push(String::new());
    assert_ne!(draft, initial); // An empty argument is not an absent argument.
    draft.arguments.clear();
    assert_eq!(draft, initial);
    draft.environment.push((String::new(), String::new()));
    assert_ne!(draft, initial); // An incomplete row must still trigger draft protection.
    draft = initial.clone();
    draft.ssh_auth = SshAuthentication::PrivateKey;
    assert_ne!(draft, initial);
    draft = initial.clone();
    draft.shell_integration = !initial.shell_integration;
    assert_ne!(draft, initial);
    draft = initial.clone();
    draft.database_tls = true;
    assert_ne!(draft, initial);
    draft = initial.clone();
    draft.sqlite_read_only = true;
    assert_ne!(draft, initial);
    draft = initial.clone();
    draft.ssh_profile = Some(Uuid::from_u128(23));
    assert_ne!(draft, initial);
}

#[test]
fn imported_control_characters_are_not_rewritten_by_an_unrelated_edit() {
    let original = "line one\nline two\tvalue\u{1b}";
    let shown = display_text(original);
    assert_eq!(preserve_text(&shown, original), original);
    assert_eq!(preserve_text("replacement", original), "replacement");
    let path = PathBuf::from("/tmp/directory\nname");
    assert_eq!(
        preserved_path(&display_text(&path.display().to_string()), Some(&path)),
        path
    );
}

#[test]
fn malformed_numbers_name_the_field() {
    let error = parse_value::<u16>(i18n_key::WORKSPACE_PORT, "not-a-port").unwrap_err();
    assert_eq!(
        error.to_string(),
        rustterm::trf!(
            i18n_key::CONNECTIONS_INVALID_VALUE_FOR_FIELD,
            field = tr(i18n_key::WORKSPACE_PORT)
        )
    );
    assert!(parse_value::<u16>(i18n_key::WORKSPACE_PORT, "65536").is_err());
    assert_eq!(
        parse_value::<u16>(i18n_key::WORKSPACE_PORT, " 2222 ").unwrap(),
        2222
    );
}

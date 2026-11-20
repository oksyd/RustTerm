use super::{Result, invalid};
use crate::i18n::{messages as key, tr};
use gpui::{Keystroke, Modifiers};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutAction {
    QuickOpen,
    Palette,
    Search,
    Sidebar,
}

impl ShortcutAction {
    pub const ALL: [Self; 4] = [Self::QuickOpen, Self::Palette, Self::Search, Self::Sidebar];

    pub fn label(self) -> &'static str {
        match self {
            Self::QuickOpen => key::WORKSPACE_QUICK_OPEN_CONNECTION,
            Self::Palette => key::WORKSPACE_COMMAND_PALETTE,
            Self::Search => key::WORKSPACE_SEARCH_OUTPUT,
            Self::Sidebar => key::SETTINGS_TOGGLE_CONNECTION_SIDEBAR,
        }
    }

    pub fn default_binding(self) -> ShortcutChord {
        let key = match self {
            Self::QuickOpen => "space",
            Self::Palette => "p",
            Self::Search => "f",
            Self::Sidebar => "b",
        };
        ShortcutChord::new(
            key,
            Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutChord {
    key: String,
    modifiers: Modifiers,
    label: String,
}

fn binding_key(key: &str) -> &str {
    match key {
        "return" => "enter",
        "page-up" => "pageup",
        "page-down" => "pagedown",
        other => other,
    }
}

impl ShortcutChord {
    fn new(key: &str, modifiers: Modifiers) -> Self {
        let key = binding_key(key).to_ascii_lowercase();
        let mut label = String::new();
        for (active, name) in [
            (modifiers.control, "Ctrl+"),
            (modifiers.alt, "Alt+"),
            (modifiers.platform, "Super+"),
            (modifiers.shift, "Shift+"),
        ] {
            if active {
                label.push_str(name);
            }
        }
        let mut chars = key.chars();
        if let Some(first) = chars.next() {
            label.extend(first.to_uppercase());
        }
        label.push_str(chars.as_str());
        Self {
            key,
            modifiers,
            label,
        }
    }

    pub fn capture(stroke: &Keystroke) -> Option<Self> {
        let mods = stroke.modifiers;
        let key = binding_key(&stroke.key);
        let function_key = key
            .strip_prefix('f')
            .filter(|number| !number.starts_with('0'))
            .and_then(|s| s.parse::<u8>().ok())
            .is_some_and(|n| (1..=24).contains(&n));
        if mods.function
            || (!mods.control && !mods.alt && !mods.platform && !function_key)
            || (mods.control && mods.alt && !mods.platform && stroke.key_char.is_some())
        {
            return None;
        }
        let supported = key.chars().count() == 1 && !key.chars().any(char::is_control)
            || function_key
            || matches!(
                key,
                "space"
                    | "enter"
                    | "tab"
                    | "backspace"
                    | "delete"
                    | "insert"
                    | "home"
                    | "end"
                    | "pageup"
                    | "pagedown"
                    | "up"
                    | "down"
                    | "left"
                    | "right"
                    | "escape"
            );
        supported.then(|| Self::new(key, mods))
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn reserved_action(&self) -> Option<&'static str> {
        Some(match self.label() {
            "Ctrl+," => key::COMMON_SETTINGS_DIALOG,
            "Ctrl+Shift+L" => key::SETTINGS_LOCK_RUSTTERM,
            "Ctrl+Shift+R" => key::TERMINAL_RECONNECT_ENDED_SESSION,
            "Ctrl+Tab" | "Ctrl+Shift+Tab" => key::WORKSPACE_NEXT_TAB,
            "Ctrl+Shift+T" => key::WORKSPACE_NEW_LOCAL_TERMINAL,
            "Ctrl+Shift+W" => key::WORKSPACE_CLOSE_CURRENT_PANE,
            "Ctrl+Shift+D" => key::WORKSPACE_DUPLICATE_CONNECTION_TO_THE_RIGHT,
            "Ctrl+Shift+E" => key::WORKSPACE_DUPLICATE_CONNECTION_BELOW,
            "Ctrl+Shift+J" => key::WORKSPACE_NEXT_PANE,
            "Ctrl+Shift+Enter" => key::WORKSPACE_ZOOM_RESTORE_PANE,
            "Ctrl+Shift+S" => key::WORKSPACE_SFTP_FILE_MANAGER,
            "Ctrl+C" | "Ctrl+Shift+C" | "Ctrl+Insert" => key::WORKSPACE_COPY_SELECTION,
            "Ctrl+V" | "Ctrl+Shift+V" => key::COMMON_PASTE,
            "Ctrl+A" | "Ctrl+Shift+A" => key::COMMON_SELECT_ALL,
            "Ctrl++" | "Ctrl+=" | "Ctrl+-" | "Ctrl+0" | "Ctrl+Shift++" | "Ctrl+Shift+="
            | "Ctrl+Shift+-" | "Ctrl+Shift+0" => key::WORKSPACE_INCREASE_DECREASE_RESET_FONT_SIZE,
            "Ctrl+S" => key::COMMON_SAVE,
            "Ctrl+X" | "Ctrl+Z" | "Ctrl+Shift+Z" | "Ctrl+Y" => key::SETTINGS_TEXT_EDITING,
            "Ctrl+Enter" => key::DATABASE_RUN_SQL,
            "F2" => key::TERMINAL_RENAME_F2,
            "F3" => key::TERMINAL_NEXT_MATCH,
            "Shift+F3" => key::TERMINAL_PREVIOUS_MATCH,
            "F5" => key::TERMINAL_REFRESH_F5,
            "Shift+F10" => key::WORKSPACE_PANE_ACTIONS,
            "Ctrl+Shift+Up" => key::TERMINAL_PREVIOUS_COMMAND,
            "Ctrl+Shift+Down" => key::TERMINAL_NEXT_COMMAND,
            _ => return None,
        })
    }

    fn matches(&self, stroke: &Keystroke) -> bool {
        self.modifiers == stroke.modifiers
            && self.key.eq_ignore_ascii_case(binding_key(&stroke.key))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
struct Binding(Option<ShortcutChord>);

impl TryFrom<String> for Binding {
    type Error = &'static str;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value.is_empty() {
            return Ok(Self(None));
        }
        let stroke = Keystroke::parse(&value).map_err(|_| "invalid shortcut")?;
        ShortcutChord::capture(&stroke)
            .map(|k| Self(Some(k)))
            .ok_or("invalid shortcut")
    }
}
impl From<Binding> for String {
    fn from(binding: Binding) -> Self {
        binding
            .0
            .map(|k| {
                Keystroke {
                    key: k.key,
                    modifiers: k.modifiers,
                    key_char: None,
                }
                .unparse()
            })
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shortcuts {
    quick_open: Binding,
    palette: Binding,
    search: Binding,
    sidebar: Binding,
}
impl Default for Shortcuts {
    fn default() -> Self {
        let [quick_open, palette, search, sidebar] =
            ShortcutAction::ALL.map(|a| Binding(Some(a.default_binding())));
        Self {
            quick_open,
            palette,
            search,
            sidebar,
        }
    }
}

impl Shortcuts {
    fn slot(&self, action: ShortcutAction) -> &Binding {
        match action {
            ShortcutAction::QuickOpen => &self.quick_open,
            ShortcutAction::Palette => &self.palette,
            ShortcutAction::Search => &self.search,
            ShortcutAction::Sidebar => &self.sidebar,
        }
    }
    pub fn binding(&self, action: ShortcutAction) -> Option<&ShortcutChord> {
        self.slot(action).0.as_ref()
    }
    pub fn label(&self, action: ShortcutAction) -> &str {
        self.binding(action).map_or("", ShortcutChord::label)
    }
    pub fn hint(&self, action: ShortcutAction, label: &'static str) -> String {
        let text = tr(label);
        self.binding(action)
            .map_or_else(|| text.to_owned(), |k| format!("{text} · {}", k.label()))
    }
    pub fn owner(&self, chord: &ShortcutChord) -> Option<ShortcutAction> {
        ShortcutAction::ALL
            .into_iter()
            .find(|a| self.binding(*a) == Some(chord))
    }
    pub fn action(&self, stroke: &Keystroke) -> Option<ShortcutAction> {
        if stroke.modifiers.control
            && stroke.modifiers.alt
            && !stroke.modifiers.platform
            && stroke.key_char.is_some()
        {
            return None;
        }
        ShortcutAction::ALL
            .into_iter()
            .find(|a| self.binding(*a).is_some_and(|k| k.matches(stroke)))
    }
    pub fn assign(&mut self, action: ShortcutAction, chord: Option<ShortcutChord>) {
        if let Some(other) = chord.as_ref().and_then(|k| self.owner(k))
            && other != action
        {
            self.set(other, None);
        }
        self.set(action, chord);
    }
    fn set(&mut self, action: ShortcutAction, chord: Option<ShortcutChord>) {
        let slot = match action {
            ShortcutAction::QuickOpen => &mut self.quick_open,
            ShortcutAction::Palette => &mut self.palette,
            ShortcutAction::Search => &mut self.search,
            ShortcutAction::Sidebar => &mut self.sidebar,
        };
        slot.0 = chord;
    }
    pub(super) fn is_default(&self) -> bool {
        *self == Self::default()
    }
    pub(super) fn validate(&self) -> Result<()> {
        for a in ShortcutAction::ALL {
            if let Some(k) = self.binding(a)
                && (k.reserved_action().is_some() || self.owner(k) != Some(a))
            {
                return Err(invalid(key::SETTINGS_SHORTCUT_INVALID));
            }
        }
        Ok(())
    }
}

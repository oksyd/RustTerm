//! Shared chrome tokens and embedded vector icons for the compact native UI.
//! Terminal ANSI colors remain owned by the configurable terminal theme.

use rustterm::i18n::messages as i18n_key;
mod scroll_area;
mod scrollbar;
mod theme;
pub use theme::{Colors, colors, set_theme};

pub use scroll_area::scrollable;
pub use scrollbar::{
    SCROLLBAR_WIDTH, ScrollAxis, ScrollMetrics, list_scrollbar, scrollbar, scrollbars,
};

use anyhow::Result;
use gpui::{
    App, AssetSource, ClipboardItem, Context, Div, ElementId, MouseButton, RenderOnce,
    SharedString, Stateful, Svg, Task, Timer, Window, div, prelude::*, px, rgb, svg,
};
use rustterm::config::{Connection, SqlEngine};
use rustterm::i18n::tr;
use std::{borrow::Cow, time::Duration};

/// Keep native wheel axes distinct instead of GPUI's single-axis fallback.
pub fn preserve_scroll_axes<E: Styled>(mut element: E) -> E {
    element.style().restrict_scroll_to_axis = Some(true);
    element.style().allow_concurrent_scroll = Some(true);
    element
}

// GPUI resolves font family names literally; CSS's generic "monospace" is not
// a portable font selection on the Linux text system.
pub const MONOSPACE_FONT: &str = "DejaVu Sans Mono";

pub const TITLE_HEIGHT: f32 = 32.0;
pub const TAB_HEIGHT: f32 = 36.0;
pub const PANE_HEADER_HEIGHT: f32 = 44.0;
pub const STATUS_HEIGHT: f32 = 24.0;
pub const SEARCH_HEIGHT: f32 = 30.0;

// Keep enum, path lookup and embedded bytes in one catalog so a new icon cannot
// be named without supplying the corresponding asset.
macro_rules! icons {
    ($($variant:ident => $file:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Icon { $($variant),+ }

        impl Icon {
            pub const fn path(self) -> &'static str {
                match self { $(Self::$variant => concat!("icons/", $file, ".svg")),+ }
            }
        }

        const ICON_ASSETS: &[(&str, &[u8])] = &[
            $((Icon::$variant.path(), include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/", $file, ".svg")))),+
        ];
    };
}

icons! {
    Logo => "logo",
    Menu => "menu",
    Sidebar => "sidebar",
    Terminal => "terminal",
    Database => "database",
    Redis => "redis",
    Mysql => "mysql",
    Postgres => "postgres",
    Sqlite => "sqlite",
    Table => "table",
    View => "view",
    Desktop => "desktop",
    Search => "search",
    Plus => "plus",
    Close => "close",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    ChevronLeft => "chevron-left",
    ChevronUp => "chevron-up",
    SplitHorizontal => "split-horizontal",
    SplitVertical => "split-vertical",
    Settings => "settings",
    Shield => "shield",
    Folder => "folder",
    File => "file",
    Link => "link",
    Edit => "edit",
    Copy => "copy",
    Trash => "trash",
    More => "more",
    Check => "check",
    Key => "key",
    Eye => "eye",
    EyeOff => "eye-off",
    Enter => "enter",
    Minimize => "minimize",
    Maximize => "maximize",
    Restore => "restore",
    ExitFullscreen => "exit-fullscreen",
    Refresh => "refresh",
    Resume => "resume",
    Stop => "stop",
    Warning => "warning",
    Disconnect => "disconnect",
    Pulse => "pulse",
    Pin => "pin",
    Unpin => "unpin",
}

impl Icon {
    pub fn for_connection(connection: &Connection) -> Self {
        match connection {
            Connection::Redis(_) => Self::Redis,
            Connection::Mysql(_) => Self::Mysql,
            Connection::Postgres(_) => Self::Postgres,
            Connection::Sqlite(_) => Self::Sqlite,
            Connection::Rdp(_) => Self::Desktop,
            Connection::Local(_) | Connection::Ssh(_) | Connection::Telnet(_) => Self::Terminal,
        }
    }

    pub const fn for_sql_engine(engine: SqlEngine) -> Self {
        match engine {
            SqlEngine::Mysql => Self::Mysql,
            SqlEngine::Postgres => Self::Postgres,
            SqlEngine::Sqlite => Self::Sqlite,
        }
    }

    fn color(self) -> u32 {
        match self {
            Self::Redis => colors().danger,
            Self::Mysql => colors().warning,
            Self::Postgres => colors().blue,
            Self::Sqlite | Self::Desktop => colors().teal,
            _ => colors().muted,
        }
    }
}

/// A 14 px monochrome mask. GPUI 0.2.2 only paints SVGs with an explicit color;
/// unlike text, an SVG does not inherit its parent's text color.
pub fn icon(glyph: Icon) -> Svg {
    svg()
        .path(glyph.path())
        .size(px(14.0))
        .flex_shrink_0()
        .text_color(rgb(glyph.color()))
}

/// Compact action chrome. The caller attaches the action and supplies a label
/// that includes the shortcut, for example "New terminal · Ctrl+Shift+T".
pub fn icon_button(id: impl Into<ElementId>, glyph: Icon, label: &'static str) -> Stateful<Div> {
    styled_icon_button(id, glyph, label, ButtonStyle::Neutral)
}

pub fn icon_button_enabled(
    id: impl Into<ElementId>,
    glyph: Icon,
    label: &'static str,
    enabled: bool,
) -> Stateful<Div> {
    icon_button_control(id, glyph, label, ButtonStyle::Neutral, enabled)
        .tab_index(0)
        .tab_stop(enabled)
}

#[derive(Clone, Copy)]
pub enum ButtonStyle {
    Neutral,
    Primary,
    Danger,
}

impl ButtonStyle {
    fn hover_colors(self) -> (u32, u32) {
        match self {
            Self::Neutral => (colors().hover, colors().text),
            Self::Primary => (colors().accent_hover, colors().on_accent),
            Self::Danger => (0x793e48, 0xffffff),
        }
    }
}

/// GPUI allows only one hover refinement per element. Choose the variant here,
/// before applying hover, instead of overriding a pre-styled button at the call site.
pub fn styled_icon_button(
    id: impl Into<ElementId>,
    glyph: Icon,
    label: &'static str,
    style: ButtonStyle,
) -> Stateful<Div> {
    icon_button_control(id, glyph, label, style, true)
}

fn icon_button_control(
    id: impl Into<ElementId>,
    glyph: Icon,
    label: &'static str,
    style: ButtonStyle,
    enabled: bool,
) -> Stateful<Div> {
    let (hover_bg, hover_fg) = style.hover_colors();
    let foreground = match style {
        ButtonStyle::Primary => colors().accent,
        _ => colors().muted,
    };
    div()
        .id(id)
        .group("icon-button")
        .size(px(28.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(2.0))
        .text_color(rgb(foreground))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(rgb(hover_bg)).text_color(rgb(hover_fg)))
        })
        .when(!enabled, disabled_button)
        .tooltip(move |_, cx| cx.new(|_| Tooltip(tr(label).to_owned())).into())
        .child(
            icon(glyph)
                .text_color(rgb(foreground))
                .when(enabled, |icon| {
                    icon.group_hover("icon-button", |s| s.text_color(rgb(hover_fg)))
                }),
        )
}

pub fn text_button(
    id: impl Into<ElementId>,
    label: &'static str,
    style: ButtonStyle,
) -> Stateful<Div> {
    text_button_control(id, label, style, true)
}

pub fn text_button_enabled(
    id: impl Into<ElementId>,
    label: &'static str,
    style: ButtonStyle,
    enabled: bool,
) -> Stateful<Div> {
    text_button_control(id, label, style, enabled)
        .tab_index(0)
        .tab_stop(enabled)
}

fn disabled_button(button: Stateful<Div>) -> Stateful<Div> {
    button
        .opacity(0.4)
        .cursor(gpui::CursorStyle::Arrow)
        .capture_any_mouse_down(|_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .capture_any_mouse_up(|_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .capture_key_down(|event, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "return" | "space") {
                window.prevent_default();
                cx.stop_propagation();
            }
        })
        .capture_key_up(|event, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "return" | "space") {
                window.prevent_default();
                cx.stop_propagation();
            }
        })
}

fn text_button_control(
    id: impl Into<ElementId>,
    label: &'static str,
    style: ButtonStyle,
    enabled: bool,
) -> Stateful<Div> {
    let (hover_bg, hover_fg) = match style {
        ButtonStyle::Neutral => (colors().selected, colors().text),
        _ => style.hover_colors(),
    };
    let (background, foreground) = match style {
        ButtonStyle::Primary => (colors().accent, colors().on_accent),
        _ => (colors().hover, colors().text),
    };
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(rgb(background))
        .text_color(rgb(foreground))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(rgb(hover_bg)).text_color(rgb(hover_fg)))
        })
        .when(!enabled, disabled_button)
        .child(tr(label))
}

pub fn connection_address(
    id: impl Into<ElementId>,
    glyph: Icon,
    protocol: &'static str,
    address: String,
    detail: String,
) -> Stateful<Div> {
    let address = match protocol {
        "SSH" => format!("ssh://{address}"),
        "Telnet" => format!("telnet://{address}"),
        "RDP" => format!("rdp://{address}"),
        _ => address,
    };
    let tooltip_address = address.clone();
    let copy_address = address.clone();
    div()
        .id(id)
        .h(px(28.))
        .flex_1()
        .min_w_0()
        .px(px(4.))
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .tooltip(move |_, cx| {
            cx.new(|_| Tooltip(format!("{detail}\n{tooltip_address}")))
                .into()
        })
        .child(icon(glyph))
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(MONOSPACE_FONT)
                .text_color(rgb(colors().text))
                .child(address),
        )
        .child(CopyAddress {
            address: copy_address,
        })
}

#[derive(IntoElement)]
struct CopyAddress {
    address: String,
}

#[derive(Default)]
struct CopyAddressState {
    address: String,
    copied: bool,
    reset: Option<Task<()>>,
}

impl RenderOnce for CopyAddress {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_state(cx, |_, _| CopyAddressState::default());
        state.update(cx, |state, cx| {
            if state.address != self.address {
                state.address = self.address;
                state.copied = false;
                state.reset = None;
                cx.notify();
            }
        });
        state
    }
}

impl gpui::Render for CopyAddressState {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        icon_button(
            "copy-connection-address",
            if self.copied { Icon::Check } else { Icon::Copy },
            if self.copied {
                i18n_key::COMMON_ADDRESS_COPIED
            } else {
                i18n_key::WORKSPACE_COPY_CONNECTION_ADDRESS
            },
        )
        .tab_index(0)
        .size(px(24.))
        .when(self.copied, |button| button.bg(rgb(colors().selected)))
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_click(cx.listener(|this, _, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(this.address.clone()));
            this.copied = true;
            this.reset = Some(cx.spawn(async move |this, cx| {
                Timer::after(Duration::from_secs(2)).await;
                let _ = this.update(cx, |this, cx| {
                    this.copied = false;
                    cx.notify();
                });
            }));
            cx.notify();
            cx.stop_propagation();
        }))
    }
}

pub struct Tooltip(pub String);

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .max_w(px(480.0))
            .rounded(px(3.0))
            .border_1()
            .border_color(rgb(colors().border))
            .bg(rgb(colors().raised))
            .text_color(rgb(colors().text))
            .text_size(px(11.0))
            .whitespace_normal()
            .child(self.0.clone())
    }
}

/// Assets are part of the executable; installed builds need no working-directory
/// convention or runtime filesystem access to render their controls.
pub struct Assets;

pub const APP_ICON: &str = "io.rustterm.RustTerm.svg";
const APP_ICON_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/io.rustterm.RustTerm.svg"
));

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == APP_ICON {
            return Ok(Some(Cow::Borrowed(APP_ICON_BYTES)));
        }
        Ok(ICON_ASSETS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        match path.trim_matches('/') {
            "" => Ok(std::iter::once(APP_ICON)
                .chain(ICON_ASSETS.iter().map(|(name, _)| *name))
                .map(SharedString::from)
                .collect()),
            "icons" => Ok(ICON_ASSETS
                .iter()
                .map(|(name, _)| SharedString::from(*name))
                .collect()),
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These constructors execute GPUI's hover assertion without a window,
    /// display server, graphics device or application event loop.
    #[test]
    fn button_variants_construct_without_duplicate_hover_styles() {
        let _ = icon_button("new-tab", Icon::Plus, "New terminal");
        let _ = styled_icon_button(
            "window-close",
            Icon::Close,
            i18n_key::WORKSPACE_CLOSE_WINDOW,
            ButtonStyle::Danger,
        )
        .w(px(38.))
        .h_full();
        let _ = styled_icon_button(
            ("close", 1usize),
            Icon::Close,
            i18n_key::TERMINAL_CLOSE_PANE,
            ButtonStyle::Danger,
        )
        .w_auto()
        .min_w(px(24.));
        let _ = text_button("save-modal", i18n_key::COMMON_SAVE, ButtonStyle::Primary);
        let _ = text_button(
            "cancel-modal",
            i18n_key::COMMON_CANCEL,
            ButtonStyle::Neutral,
        );
        for enabled in [false, true] {
            let _ = icon_button_enabled("copy-state", Icon::Copy, i18n_key::COMMON_COPY, enabled);
            let _ = text_button_enabled(
                "run-state",
                i18n_key::DATABASE_RUN_SQL,
                ButtonStyle::Primary,
                enabled,
            );
        }
    }
}

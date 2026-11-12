use super::{LibraryItem, LibraryRow, Workspace, library_sections};
use crate::app::text_input::{InputEvent, TextInput};
use crate::app::ui;
use gpui::{AnyElement, div, prelude::*, px, rgb};
use gpui::{App, Context, Entity, Subscription, Window};
use rustterm::config::Uuid;
use rustterm::i18n::{messages as i18n_key, tr};
use std::ops::Range;

pub(in crate::app::workspace) struct LibrarySearch {
    pub(in crate::app::workspace) query: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl LibrarySearch {
    pub(in crate::app::workspace) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let query = cx.new(|cx| {
            let mut input = TextInput::new("", false, cx);
            input.set_borderless(true, cx);
            input.set_compact(true, cx);
            input
        });
        let subscriptions = vec![
            cx.subscribe(&query, |this, _, event, cx| {
                if matches!(event, InputEvent::Changed) && !this.lock_screen.locked {
                    this.update_library_match(cx);
                }
            }),
            // Preedit also reveals the temporary field; only committed text
            // changes the selection, so IME candidate navigation stays intact.
            cx.observe(&query, |_, _, cx| cx.notify()),
            cx.on_blur(&query.read(cx).focus_handle(), window, |this, _, cx| {
                this.clear_library_search(cx);
            }),
        ];
        Self {
            query,
            _subscriptions: subscriptions,
        }
    }
}

impl Workspace {
    pub(super) fn render_library_search(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = self.library_search.query.read(cx);
        let active = input.focus_handle().is_focused(window)
            && (!input.value().is_empty() || input.is_composing());
        let rows = if active {
            self.library_rows(cx)
        } else {
            Vec::new()
        };
        let matches = matching_rows(&rows, input.value().trim());
        let position = matches
            .iter()
            .position(|&index| Some(rows[index].item) == self.library_selection)
            .map_or(0, |index| index + 1);
        // Keep an IME input handler mounted while the tree owns focus. Its
        // invisible one-pixel surface accepts the very first composed character.
        // The temporary field floats over the heading without moving tree rows.
        div()
            .absolute()
            .top(px(6.))
            .left_2()
            .flex()
            .items_center()
            .when(active, |d| {
                d.w(px(self.sidebar_width - 16.))
                    .h(px(32.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(if matches.is_empty() {
                        ui::colors().danger
                    } else {
                        ui::colors().accent
                    }))
                    .bg(rgb(ui::colors().input))
                    .shadow_md()
                    .occlude()
            })
            .when(!active, |d| {
                d.w(px(40.)).h(px(1.)).opacity(0.).overflow_hidden()
            })
            .when(active, |d| {
                d.child(div().pl_2().child(ui::icon(ui::Icon::Search).size(px(12.))))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.library_search.query.clone()),
            )
            .when(active, |d| {
                d.child(
                    div()
                        .id("library-speed-search-count")
                        .pr_2()
                        .text_size(px(11.))
                        .text_color(rgb(if matches.is_empty() {
                            ui::colors().danger
                        } else {
                            ui::colors().muted
                        }))
                        .tooltip(|_, cx| {
                            cx.new(|_| ui::Tooltip(tr(i18n_key::WORKSPACE_SPEED_SEARCH).to_owned()))
                                .into()
                        })
                        .child(format!("{position}/{}", matches.len())),
                )
            })
            .into_any_element()
    }

    pub(super) fn library_rows(&self, cx: &App) -> Vec<LibraryRow> {
        let query = self.session_filter.read(cx).value().trim().to_lowercase();
        visible_rows(&self.config, &self.collapsed, &query, self.connection_type)
    }

    pub(in crate::app::workspace) fn focus_library(&self, window: &mut Window, cx: &App) {
        self.library_search.query.read(cx).focus(window);
    }

    pub(in crate::app::workspace) fn clear_library_search(&mut self, cx: &mut Context<Self>) {
        self.library_search
            .query
            .update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    pub(in crate::app::workspace) fn selected_library_profile(&self) -> Option<Uuid> {
        match self.library_selection {
            Some(LibraryItem::Profile(id)) => Some(id),
            _ => None,
        }
    }

    pub(in crate::app::workspace) fn selected_library_group(&self) -> Option<Uuid> {
        match self.library_selection {
            Some(LibraryItem::Group(id)) => id,
            Some(LibraryItem::Profile(id)) => self
                .config
                .profiles
                .iter()
                .find(|p| p.id == id)
                .and_then(|p| p.group),
            None => None,
        }
    }

    pub(super) fn update_library_match(&mut self, cx: &mut Context<Self>) {
        let query = self.library_search.query.read(cx).value().trim();
        if query.is_empty() {
            cx.notify();
            return;
        }
        let rows = self.library_rows(cx);
        let matches = matching_rows(&rows, query);
        let selected = matches
            .iter()
            .copied()
            .find(|&index| Some(rows[index].item) == self.library_selection)
            .or_else(|| matches.first().copied());
        if let Some(index) = selected {
            self.library_selection = Some(rows[index].item);
            self.sidebar_scroll.scroll_to_item(index);
        }
        cx.notify();
    }
}

fn visible_rows(
    config: &rustterm::config::AppConfig,
    collapsed: &std::collections::HashSet<Uuid>,
    query: &str,
    kind: Option<super::ConnectionKind>,
) -> Vec<LibraryRow> {
    let mut rows = Vec::new();
    for section in library_sections(config, collapsed, query, kind) {
        rows.push(LibraryRow {
            item: LibraryItem::Group(section.id),
            parent: section.parent,
            name: if query.is_empty() {
                section.name
            } else {
                section.path
            },
        });
        if !section.collapsed {
            rows.extend(section.profiles.into_iter().map(|(_, profile)| LibraryRow {
                item: LibraryItem::Profile(profile.id),
                parent: profile.group,
                name: profile.name.clone(),
            }));
        }
    }
    rows
}

pub(super) fn matching_rows(rows: &[LibraryRow], query: &str) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            (query.is_empty() || !match_ranges(&row.name, query).is_empty()).then_some(index)
        })
        .collect()
}

pub(super) fn highlighted(name: String, query: &str) -> gpui::StyledText {
    let ranges = match_ranges(&name, query);
    gpui::StyledText::new(name).with_highlights(ranges.into_iter().map(|range| {
        (
            range,
            gpui::HighlightStyle {
                color: Some(gpui::rgb(ui::colors().accent).into()),
                background_color: Some(gpui::rgba((ui::colors().accent << 8) | 0x30).into()),
                font_weight: Some(gpui::FontWeight::SEMIBOLD),
                ..Default::default()
            },
        )
    }))
}

// Map lowercase matches back to the original UTF-8 text. A lowercase mapping
// can expand a character (for example, İ), so byte offsets cannot be reused.
fn match_ranges(name: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut folded = String::new();
    let mut original = Vec::new();
    for (start, ch) in name.char_indices() {
        for lower in ch.to_lowercase() {
            folded.push(lower);
            original.extend(std::iter::repeat_n(
                start..start + ch.len_utf8(),
                lower.len_utf8(),
            ));
        }
    }
    let query = query.to_lowercase();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (start, _) in folded.match_indices(&query) {
        let range = original[start].start..original[start + query.len() - 1].end;
        if let Some(previous) = ranges.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            ranges.push(range);
        }
    }
    ranges
}

pub(super) fn next_match(matches: &[usize], current: usize, backwards: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let Some(index) = matches.iter().position(|index| *index == current) else {
        return if backwards {
            matches.last()
        } else {
            matches.first()
        }
        .copied();
    };
    Some(
        matches[if backwards {
            (index + matches.len() - 1) % matches.len()
        } else {
            (index + 1) % matches.len()
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::{LibraryItem, match_ranges, matching_rows, next_match, visible_rows};
    use std::collections::HashSet;

    #[test]
    fn speed_search_navigates_visible_names_without_filtering_or_expanding_the_tree() {
        let config = super::super::tests::library();
        let collapsed = HashSet::from([config.groups[0].id]);
        let rows = visible_rows(&config, &collapsed, "", None);
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["Company A", "Company B", "Project", "Database"]
        );
        assert_eq!(matching_rows(&rows, "COMPANY"), vec![0, 1]);
        assert_eq!(matching_rows(&rows, "project"), vec![2]);
        assert_eq!(rows[2].item, LibraryItem::Group(Some(config.groups[3].id)));
        assert_eq!(matching_rows(&rows, "database"), vec![3]);
        assert_eq!(rows[3].item, LibraryItem::Profile(config.profiles[1].id));
        assert!(matching_rows(&rows, "App server").is_empty());
        assert_eq!(collapsed, HashSet::from([config.groups[0].id]));
        assert_eq!(
            rows.len(),
            4,
            "quick locate must keep non-matching rows visible"
        );
    }

    #[test]
    fn speed_search_respects_the_existing_text_and_connection_type_filters() {
        let mut config = super::super::tests::library();
        config.profiles[0].connection = rustterm::config::Connection::Ssh(Default::default());
        let rows = visible_rows(
            &config,
            &HashSet::new(),
            "app",
            Some(super::super::ConnectionKind::Ssh),
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(matching_rows(&rows, "server"), vec![2]);
        assert!(matching_rows(&rows, "database").is_empty());
        assert!(
            matching_rows(&rows, "ssh").is_empty(),
            "quick locate matches displayed names, not hidden metadata"
        );
    }

    #[test]
    fn names_and_paths_match_without_case_sensitivity() {
        let path = "Company / Payments / Production";
        assert_eq!(match_ranges(path, "PAY"), vec![10..13]);
        assert_eq!(match_ranges(path, "company / pay"), vec![0..13]);
        assert!(match_ranges(path, "staging").is_empty());
        assert!(match_ranges(path, "").is_empty());
    }

    #[test]
    fn highlights_preserve_original_unicode_boundaries() {
        for (name, query, expected) in [
            ("公司 / 项目甲 / 项目乙", "项目", vec!["项目", "项目"]),
            ("🦀 / Équipe / İSTANBUL", "équipe", vec!["Équipe"]),
            ("🦀 / Équipe / İSTANBUL", "istanbul", vec![]),
            ("🦀 / İSTANBUL", "i", vec!["İ"]),
            ("İİ", "i", vec!["İİ"]),
            ("App / APP", "app", vec!["App", "APP"]),
        ] {
            let ranges = match_ranges(name, query);
            let actual: Vec<_> = ranges.iter().map(|range| &name[range.clone()]).collect();
            assert_eq!(actual, expected, "{name}: {query}");
        }
    }

    #[test]
    fn navigation_wraps_only_through_matches_and_handles_no_match() {
        let matches = [1, 4, 7];
        assert_eq!(next_match(&matches, 1, false), Some(4));
        assert_eq!(next_match(&matches, 7, false), Some(1));
        assert_eq!(next_match(&matches, 1, true), Some(7));
        assert_eq!(next_match(&matches, usize::MAX, false), Some(1));
        assert_eq!(next_match(&matches, usize::MAX, true), Some(7));
        assert_eq!(next_match(&[4], 4, false), Some(4));
        assert_eq!(next_match(&[], 4, false), None);
    }
}

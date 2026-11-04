use super::*;
use rustterm::i18n::messages as i18n_key;
use std::collections::HashMap;

impl FileTransferView {
    pub(super) fn cancel_navigation(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.listing_task = None;
        self.loading = false;
        self.pending_directory = None;
        self.path_input
            .update(cx, |input, cx| input.set_value(&self.directory, cx));
        self.path_dirty = false;
        cx.notify();
    }

    pub(super) fn navigate(&mut self, requested: String, cx: &mut Context<Self>) {
        if self.closed
            || !self.client.is_connected()
            || self.mutating
            || self.native_pending
            || self.prompts.is_blocked()
        {
            return;
        }
        if self.preview.is_some() || self.editor.is_some() {
            self.error = Some(i18n_key::SFTP_FINISH_EDITING_BEFORE_CHANGING_FOLDERS.into());
            cx.notify();
            return;
        }
        if requested.is_empty() || requested.len() > 16 * 1024 || requested.contains('\0') {
            self.error = Some(i18n_key::TERMINAL_ENTER_A_VALID_REMOTE_PATH.into());
            cx.notify();
            return;
        }
        self.pending_directory = None;
        self.path_dirty = false;
        self.path_input
            .update(cx, |input, cx| input.set_value(&requested, cx));
        self.load_directory(requested, cx);
    }

    pub(super) fn refresh_directory(&mut self, cx: &mut Context<Self>) {
        if self.closed
            || !self.client.is_connected()
            || self.mutating
            || self.native_pending
            || self.preview.is_some()
            || self.editor.is_some()
            || self.prompts.is_blocked()
        {
            return;
        }
        self.load_directory(self.directory.clone(), cx);
    }

    fn load_directory(&mut self, requested: String, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        self.notice = None;
        let client = self.client.clone();
        let path = resolve_path(&self.directory, &requested);
        let task = cx.background_executor().spawn(async move {
            let directory = client.canonicalize(&path).await?;
            let entries = client.list_dir(&directory).await?;
            Ok::<_, rustterm::session::SftpError>((directory, entries))
        });
        self.listing_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || generation != this.generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok((directory, entries)) => {
                        if this.path_input.read(cx).value() == requested
                            && !this.path_input.read(cx).is_composing()
                        {
                            this.path_input
                                .update(cx, |input, cx| input.set_value(&directory, cx));
                        }
                        this.replace_listing(directory, entries);
                        this.directory_ready = this.client.is_connected();
                    }
                    Err(error) => {
                        if this.directory_ready
                            && this.path_input.read(cx).value() == requested
                            && !this.path_input.read(cx).is_composing()
                        {
                            this.path_input
                                .update(cx, |input, cx| input.set_value(&this.directory, cx));
                        }
                        this.error = Some(rustterm::i18n::Message::with_arg(
                            i18n_key::SFTP_COULD_NOT_READ_DIRECTORY_ERROR,
                            "error",
                            rustterm::i18n::Message::from_error(error),
                        ));
                    }
                }
                this.path_dirty = this.path_input.read(cx).value() != this.directory;
                this.refresh_completed_uploads(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn replace_listing(&mut self, directory: String, entries: Vec<SftpEntry>) {
        if directory == self.directory {
            let mut scroll = self.scroll.0.borrow_mut();
            let offset = scroll.base_handle.offset();
            let top = (-f32::from(offset.y) / FILE_ROW_HEIGHT).max(0.).floor() as usize;
            let mut position = ListingPosition {
                selected: std::mem::take(&mut self.selected),
                cursor: self.cursor,
                anchor: self.anchor,
                top,
            };
            position.remap(&self.entries, &entries);
            self.selected = position.selected;
            self.cursor = position.cursor;
            self.anchor = position.anchor;
            scroll.deferred_scroll_to_item = None;
            scroll.base_handle.set_offset(gpui::point(
                offset.x,
                offset.y + px((top as f32 - position.top as f32) * FILE_ROW_HEIGHT),
            ));
        } else {
            self.selected.clear();
            self.cursor = None;
            self.anchor = None;
            self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        self.directory = directory;
        self.entries = entries;
    }
}

fn resolve_path(directory: &str, requested: &str) -> String {
    if requested.starts_with('/') || directory == "." {
        requested.to_owned()
    } else {
        remote_join(directory, requested)
    }
}

struct ListingPosition {
    selected: BTreeSet<usize>,
    cursor: Option<usize>,
    anchor: Option<usize>,
    top: usize,
}

impl ListingPosition {
    fn remap(&mut self, previous: &[SftpEntry], entries: &[SftpEntry]) {
        let positions: HashMap<_, _> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.path.as_str(), index))
            .collect();
        let retained = |index: usize| {
            previous
                .get(index)
                .and_then(|entry| positions.get(entry.path.as_str()))
                .copied()
        };
        self.selected = self
            .selected
            .iter()
            .filter_map(|&index| retained(index))
            .collect();
        self.cursor = self.cursor.and_then(retained);
        self.anchor = self.anchor.and_then(retained);
        self.top =
            retained(self.top).unwrap_or_else(|| self.top.min(entries.len().saturating_sub(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> SftpEntry {
        SftpEntry {
            name: name.into(),
            path: format!("/data/{name}"),
            is_dir: false,
            is_symlink: false,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn refresh_tracks_selection_and_scroll_by_path_not_row_number() {
        let previous = [entry("a"), entry("b"), entry("c"), entry("d")];
        let entries = [entry("new"), entry("a"), entry("c"), entry("d")];
        let mut position = ListingPosition {
            selected: BTreeSet::from([1, 3]),
            cursor: Some(3),
            anchor: Some(1),
            top: 2,
        };
        position.remap(&previous, &entries);
        assert_eq!(position.selected, BTreeSet::from([3]));
        assert_eq!(position.cursor, Some(3));
        assert_eq!(position.anchor, None);
        assert_eq!(position.top, 2);
        let reordered = [entry("d"), entry("new"), entry("a"), entry("c")];
        position.remap(&entries, &reordered);
        assert_eq!(position.selected, BTreeSet::from([0]));
        assert_eq!(position.cursor, Some(0));
        assert_eq!(position.top, 3);
        position.remap(&reordered, &[]);
        assert!(position.selected.is_empty());
        assert_eq!(position.cursor, None);
        assert_eq!(position.top, 0);
    }

    #[test]
    fn relative_paths_use_the_current_remote_folder_without_shell_expansion() {
        assert_eq!(resolve_path(".", "."), ".");
        assert_eq!(resolve_path("/srv/app", "logs"), "/srv/app/logs");
        assert_eq!(resolve_path("/srv/app", "../logs"), "/srv/app/../logs");
        assert_eq!(resolve_path("/srv/app", "/var/log"), "/var/log");
        assert_eq!(resolve_path("/", "tmp"), "/tmp");
        assert_eq!(
            resolve_path("/srv", "$(echo x) '报告'"),
            "/srv/$(echo x) '报告'"
        );
    }
}

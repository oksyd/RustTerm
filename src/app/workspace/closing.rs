use super::Workspace;
use crate::app::pane::Pane;
use gpui::{App, Context, PromptLevel, Window};
use rustterm::config::Connection;
use rustterm::i18n::{messages as key, tr};

struct ClosingPane {
    name: String,
    running: bool,
    remote: bool,
    warnings: Vec<&'static str>,
}

struct ClosePrompt {
    title: String,
    detail: Option<String>,
    confirm: &'static str,
}

impl ClosePrompt {
    fn for_panes(panes: &[ClosingPane]) -> Option<Self> {
        let running = panes.iter().filter(|pane| pane.running).count();
        if running == 0 && panes.iter().all(|pane| pane.warnings.is_empty()) {
            return None;
        }
        let mut details = Vec::new();
        let (title, confirm) = if let [pane] = panes {
            let (title, confirm) = if pane.running && pane.remote {
                (
                    key::WORKSPACE_DISCONNECT_NAMED_SESSION,
                    key::COMMON_DISCONNECT,
                )
            } else {
                (
                    key::WORKSPACE_CLOSE_NAMED_SESSION,
                    if pane.running {
                        key::WORKSPACE_END_AND_CLOSE
                    } else {
                        key::COMMON_CLOSE
                    },
                )
            };
            (rustterm::trf!(title, name = pane.name), confirm)
        } else {
            if running > 0 {
                details.push(rustterm::trf!(
                    key::WORKSPACE_CLOSE_RUNNING_SESSIONS,
                    count = running
                ));
            }
            (
                rustterm::trf!(key::WORKSPACE_CLOSE_PANES_CONFIRMATION, count = panes.len()),
                if running > 0 {
                    key::WORKSPACE_END_AND_CLOSE
                } else {
                    key::COMMON_CLOSE
                },
            )
        };
        for pane in panes.iter().filter(|pane| !pane.warnings.is_empty()) {
            let warnings = pane
                .warnings
                .iter()
                .map(|key| tr(key))
                .collect::<Vec<_>>()
                .join("\n");
            details.push(if panes.len() == 1 {
                warnings
            } else {
                format!("{}\n{}", pane.name, warnings)
            });
        }
        Some(Self {
            title,
            detail: (!details.is_empty()).then(|| details.join("\n\n")),
            confirm,
        })
    }
}

impl Workspace {
    fn closing_pane(&self, id: usize, cx: &App) -> Option<ClosingPane> {
        let pane = self.panes.get(&id)?;
        let mut warnings = Vec::new();
        let name = match pane {
            Pane::Terminal(view) => {
                let view = view.read(cx);
                if view.is_container() && view.is_running() {
                    warnings.push(key::DOCKER_CONSOLE_CLOSE_HINT);
                }
                warnings.extend(view.active_transfer_warning());
                view.display_name().to_owned()
            }
            _ => self
                .pane_display_name(id)
                .unwrap_or_else(|| tr(key::WORKSPACE_SESSION).to_owned()),
        };
        if let Pane::Database(view) = pane {
            let database = view.read(cx);
            if database.has_pending_transaction() {
                warnings.push(key::WORKSPACE_CLOSE_TRANSACTION);
            }
            if database.has_unsaved_documents() {
                warnings.push(key::WORKSPACE_CLOSE_UNSAVED_EDITS);
            }
        } else if pane.has_unsaved_changes(cx) {
            warnings.push(key::WORKSPACE_CLOSE_UNSAVED_EDITS);
        }
        if let Pane::Rdp(view) = pane
            && view.read(cx).receiving_files()
        {
            warnings.push(key::WORKSPACE_CLOSE_DOWNLOADS);
        }
        if let Some(files) = self.files.get(&id) {
            files.read(cx).append_close_warnings(&mut warnings, cx);
        }
        let remote =
            self.pane_profiles.get(&id).is_some_and(|p| {
                !matches!(p.connection, Connection::Local(_) | Connection::Sqlite(_))
            }) || matches!(pane, Pane::Terminal(view) if view.read(cx).is_container());
        Some(ClosingPane {
            name,
            running: pane.is_running(cx),
            remote,
            warnings,
        })
    }

    pub(super) fn confirm_close(
        &mut self,
        ids: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming_close {
            return;
        }
        let panes: Vec<_> = ids
            .iter()
            .filter_map(|id| self.closing_pane(*id, cx))
            .collect();
        let Some(prompt) = ClosePrompt::for_panes(&panes) else {
            self.remove_panes(&ids, window, cx);
            return;
        };
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        self.confirming_close = true;
        let response = window.prompt(
            PromptLevel::Warning,
            &prompt.title,
            prompt.detail.as_deref(),
            &[tr(key::COMMON_CANCEL), tr(prompt.confirm)],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let yes = response.await == Ok(1);
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    let valid = permit.is_valid();
                    drop(permit);
                    this.confirming_close = false;
                    if yes && valid {
                        this.remove_panes(&ids, window, cx);
                    }
                    cx.notify();
                })
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_prompt_names_the_session_and_only_describes_actual_losses() {
        let mut ssh = ClosingPane {
            name: "web:2".into(),
            running: true,
            remote: true,
            warnings: Vec::new(),
        };
        let prompt = ClosePrompt::for_panes(std::slice::from_ref(&ssh)).unwrap();
        assert_eq!(
            prompt.title,
            rustterm::trf!(key::WORKSPACE_DISCONNECT_NAMED_SESSION, name = "web:2")
        );
        assert!(prompt.detail.is_none());
        ssh.running = false;
        assert!(ClosePrompt::for_panes(std::slice::from_ref(&ssh)).is_none());
        ssh.warnings.push(key::WORKSPACE_CLOSE_UPLOADS);
        let prompt = ClosePrompt::for_panes(std::slice::from_ref(&ssh)).unwrap();
        assert_eq!(
            prompt.detail.as_deref(),
            Some(tr(key::WORKSPACE_CLOSE_UPLOADS))
        );
        let database = ClosingPane {
            name: "inventory".into(),
            running: true,
            remote: true,
            warnings: vec![key::WORKSPACE_CLOSE_TRANSACTION],
        };
        let detail = ClosePrompt::for_panes(&[ssh, database])
            .unwrap()
            .detail
            .unwrap();
        for text in [
            "web:2",
            "inventory",
            tr(key::WORKSPACE_CLOSE_UPLOADS),
            tr(key::WORKSPACE_CLOSE_TRANSACTION),
        ] {
            assert!(detail.contains(text));
        }
        assert!(!detail.contains(tr(key::WORKSPACE_CLOSE_DOWNLOADS)));
    }
}

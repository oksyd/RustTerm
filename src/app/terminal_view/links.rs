use super::{TerminalKind, TerminalView};
use crate::app::pane::PaneEvent;
use gpui::{Context, Modifiers, Pixels, Point};
use rustterm::terminal::TerminalLink;

pub(super) struct LinkPress {
    target: TerminalLink,
    position: Point<Pixels>,
    cancelled: bool,
}

pub(super) fn link_modifier(modifiers: Modifiers) -> bool {
    modifiers.control && !modifiers.alt && !modifiers.shift && !modifiers.platform
}

impl LinkPress {
    pub(super) fn new(target: TerminalLink, position: Point<Pixels>) -> Self {
        Self {
            target,
            position,
            cancelled: false,
        }
    }

    pub(super) fn update(&mut self, position: Point<Pixels>, modifiers: Modifiers) {
        self.cancelled |= !link_modifier(modifiers) || (position - self.position).magnitude() > 2.;
    }

    fn complete(
        mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        target: Option<&TerminalLink>,
    ) -> bool {
        self.update(position, modifiers);
        !self.cancelled && target == Some(&self.target)
    }
}

impl TerminalView {
    pub(super) fn link_at_position(
        &self,
        position: Point<Pixels>,
        cx: &gpui::App,
    ) -> Option<TerminalLink> {
        if self.chrome_blocked(cx) || self.show_final_alternate || !self.visible {
            return None;
        }
        let (column, row) = self.mouse_position(position)?;
        let link = self
            .snapshot
            .as_ref()?
            .link_at(column as usize, row as usize)?;
        if matches!(link, TerminalLink::RemotePath(_))
            && (self.kind != TerminalKind::Ssh
                || self.viewed_run + 1 != self.runs.len()
                || !self.is_running())
        {
            return None;
        }
        Some(link)
    }

    pub(super) fn begin_link(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        self.link_press = None;
        if !link_modifier(modifiers) {
            return false;
        }
        let Some(target) = self.link_at_position(position, cx) else {
            return false;
        };
        self.link_press = Some(LinkPress::new(target, position));
        self.begin_selection(position, rustterm::terminal::SelectionKind::Character, cx);
        true
    }

    pub(super) fn finish_link(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(press) = self.link_press.take() else {
            return;
        };
        let target = self.link_at_position(position, cx);
        if !press.complete(position, modifiers, target.as_ref()) {
            return;
        }
        match target {
            Some(TerminalLink::Url(url)) => cx.open_url(&url),
            Some(TerminalLink::RemotePath(path)) => {
                let Some(run) = self.runs.get(self.viewed_run) else {
                    return;
                };
                cx.emit(PaneEvent::Focused(self.index));
                cx.emit(PaneEvent::RevealRemotePath {
                    index: self.index,
                    run: run.id(),
                    path,
                });
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    #[test]
    fn link_click_requires_release_on_the_original_target_without_a_drag_or_modifier_change() {
        let target = TerminalLink::Url("https://example.org".into());
        let origin = point(px(10.), px(10.));
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let press = || LinkPress::new(target.clone(), origin);
        assert!(press().complete(origin + point(px(1.), px(0.)), ctrl, Some(&target)));
        assert!(!press().complete(origin, ctrl, None));
        assert!(!press().complete(origin, Modifiers::default(), Some(&target)));
        assert!(!press().complete(
            origin,
            ctrl,
            Some(&TerminalLink::Url("https://example.net".into()))
        ));
        let mut drag = press();
        drag.update(origin + point(px(10.), px(0.)), ctrl);
        assert!(
            !drag.complete(origin, ctrl, Some(&target)),
            "moving back must not reactivate a cancelled click"
        );
        let mut changed = press();
        changed.update(
            origin,
            Modifiers {
                shift: true,
                ..ctrl
            },
        );
        assert!(!changed.complete(origin, ctrl, Some(&target)));
    }
}

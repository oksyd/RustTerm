use super::{SCROLLBAR_WIDTH, scrollbars};
use gpui::{
    App, Div, Element, ElementId, IntoElement, Overflow, RenderOnce, ScrollHandle, Stateful,
    Window, prelude::*, px,
};

/// Adds persistent, draggable scrollbars without changing the container's layout.
pub fn scrollable(mut content: Stateful<Div>, handle: Option<ScrollHandle>) -> ScrollArea {
    let horizontal = content.style().overflow.x == Some(Overflow::Scroll);
    let vertical = content.style().overflow.y == Some(Overflow::Scroll);
    ScrollArea {
        id: Element::id(&content).expect("a stateful scroll container has an ID"),
        content,
        handle,
        horizontal,
        vertical,
    }
}

#[derive(IntoElement)]
pub struct ScrollArea {
    id: ElementId,
    content: Stateful<Div>,
    handle: Option<ScrollHandle>,
    horizontal: bool,
    vertical: bool,
}

impl RenderOnce for ScrollArea {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let handle = self.handle.unwrap_or_else(|| {
            window.with_id(self.id.clone(), |window| {
                window
                    .use_state(cx, |_, _| ScrollHandle::new())
                    .read(cx)
                    .clone()
            })
        });
        scrollbars(
            self.id,
            self.horizontal.then(|| handle.clone()),
            self.vertical.then(|| handle.clone()),
        )
        .decorate(
            self.content
                .track_scroll(&handle)
                .scrollbar_width(px(SCROLLBAR_WIDTH)),
        )
    }
}

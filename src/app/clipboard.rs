//! Order asynchronous copies against every clipboard write in this application.
use gpui::{App, ClipboardItem};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Default)]
struct CopyOrder(Rc<Cell<u64>>);
impl gpui::Global for CopyOrder {}

pub(super) struct CopyTicket {
    order: CopyOrder,
    number: u64,
}

impl CopyOrder {
    fn start(&self) -> CopyTicket {
        let number = self.0.get().wrapping_add(1);
        self.0.set(number);
        CopyTicket {
            order: self.clone(),
            number,
        }
    }
}

impl CopyTicket {
    pub(super) fn is_current(&self) -> bool {
        self.number == self.order.0.get()
    }
}

pub(super) fn request(cx: &mut App) -> CopyTicket {
    cx.default_global::<CopyOrder>().start()
}

pub(super) fn write(cx: &mut App, item: ClipboardItem) {
    let _ = request(cx);
    cx.write_to_clipboard(item);
}

#[cfg(test)]
mod copy_tests {
    use super::*;

    #[test]
    fn explicit_copies_from_different_sessions_complete_in_request_order() {
        let clipboard = CopyOrder::default();
        let other_session = clipboard.clone();
        let first = clipboard.start();
        assert!(first.is_current());
        let second = other_session.start();
        assert!(second.is_current());
        assert!(
            !first.is_current(),
            "an older late result must not overwrite the latest copy"
        );
        drop(second);
        assert!(
            !first.is_current(),
            "cancelling the latest copy must not revive an old request"
        );
    }
}

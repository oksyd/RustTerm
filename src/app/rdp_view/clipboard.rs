use rustterm::i18n::messages as i18n_key;
use rustterm::{
    i18n::tr,
    rdp::{RdpClipboardEvent, RdpError},
};
use tokio_util::sync::CancellationToken;

pub(super) type ClipboardRead = Result<Option<String>, &'static str>;

#[derive(Default)]
enum Origin {
    #[default]
    Unobserved,
    Local,
    Remote {
        local: ClipboardRead,
        sequence: Option<u32>,
    },
    RemoteFiles(CancellationToken),
    RemoteContent {
        ownership: CancellationToken,
        text: Option<String>,
    },
    Files {
        selected: Option<ClipboardRead>,
        sequence: Option<u32>,
    },
}

pub(super) enum Feedback {
    ReadingFiles,
    Waiting,
    Requesting,
    RemoteEmpty,
    Message(&'static str),
    Rejected(RdpError),
}

impl Feedback {
    pub(super) fn text(&self) -> String {
        let message = match self {
            Self::ReadingFiles => i18n_key::RDP_READING_FILES_FROM_THE_LOCAL_CLIPBOARD,
            Self::Waiting => i18n_key::RDP_WAITING_FOR_REMOTE_CLIPBOARD_CONFIRMATION,
            Self::Requesting => i18n_key::RDP_FETCHING_REMOTE_CLIPBOARD,
            Self::RemoteEmpty => i18n_key::RDP_CLIPBOARD_UNAVAILABLE_DETAIL,
            Self::Message(message) => message,
            Self::Rejected(error) => return error.to_string(),
        };
        tr(message).to_owned()
    }

    fn remote_request(&self) -> bool {
        matches!(self, Self::Requesting | Self::RemoteEmpty)
    }
}

#[derive(Clone, Copy)]
pub(super) enum ContentKind {
    Unknown,
    Text,
    Image,
    Files,
}

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Send(ContentKind),
    Receive(ContentKind),
}

#[derive(Default)]
pub(super) struct ClipboardState {
    revision: u64,
    pub(super) operation: Option<Operation>,
    origin: Origin,
    pub(super) remote: Option<String>,
    pub(super) remote_content: Option<std::sync::Arc<rustrdp::ClipboardContent>>,
    pub(super) feedback: Option<Feedback>,
    remote_offer_seen: bool,
    export: Option<Export>,
    files_ready: bool,
}

struct Export {
    local: ClipboardRead,
    sequence: Option<u32>,
    stage: ExportStage,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportStage {
    AwaitingOffer,
    // A menu request can receive data directly from the current format list.
    Requested,
    AwaitingData,
    Ready,
}

impl ExportStage {
    fn awaits_offer(self) -> bool {
        matches!(self, Self::AwaitingOffer | Self::Requested)
    }
}

impl ClipboardState {
    pub(super) fn feedback_text(&self) -> Option<String> {
        let feedback = self.feedback.as_ref()?;
        if matches!(feedback, Feedback::Message(key) if matches!(*key,
            i18n_key::RDP_REMOTE_CLIPBOARD_CONFIRMED_PRESS_CTRL_V_IN_THE_REMOTE_DESKTOP
            | i18n_key::RDP_PASTE_SHORTCUT_SENT_TO_THE_REMOTE_DESKTOP
            | i18n_key::RDP_REMOTE_TEXT_COPIED_TO_THE_LOCAL_CLIPBOARD))
        {
            return None;
        }
        let Some(operation) = self.operation else {
            return Some(feedback.text());
        };
        let (direction, kind) = match operation {
            Operation::Send(kind) => (i18n_key::RDP_CLIPBOARD_SEND_DIRECTION, kind),
            Operation::Receive(kind) => (i18n_key::RDP_CLIPBOARD_RECEIVE_DIRECTION, kind),
        };
        let kind = match kind {
            ContentKind::Unknown => i18n_key::RDP_CLIPBOARD_CONTENT,
            ContentKind::Text => i18n_key::RDP_CLIPBOARD_TEXT,
            ContentKind::Image => i18n_key::RDP_CLIPBOARD_IMAGE,
            ContentKind::Files => i18n_key::RDP_CLIPBOARD_FILES,
        };
        Some(format!(
            "{} · {} · {}",
            tr(direction),
            tr(kind),
            feedback.text()
        ))
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn begin(&mut self) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        self.feedback = None;
        self.operation = None;
        self.export = None;
        self.revision
    }

    pub(super) fn publish(&mut self) {
        self.operation
            .get_or_insert(Operation::Send(ContentKind::Text));
        self.origin = Origin::Local;
        self.feedback = Some(Feedback::Waiting);
    }

    pub(super) fn local_image(&mut self, sequence: Option<u32>) {
        if sequence.is_some()
            && matches!(&self.origin,
            Origin::Remote { sequence: previous, .. } | Origin::Files { sequence: previous, .. }
                if *previous == sequence)
        {
            return; // No new local copy since the remote gesture or file selection.
        }
        if matches!(&self.origin, Origin::RemoteContent { ownership, .. } if !ownership.is_cancelled())
        {
            return; // Our own native export still belongs to the remote copy.
        }
        // An image and an empty clipboard both have no plain text. Observing
        // native image data supersedes any earlier text-based ownership guess.
        self.origin = Origin::Local;
        self.export = None;
    }

    pub(super) fn image_text(&self) -> Option<String> {
        match &self.origin {
            Origin::RemoteContent { ownership, text } if !ownership.is_cancelled() => text.clone(),
            _ => None,
        }
    }

    pub(super) fn publish_files(&mut self, selected: Option<ClipboardRead>) {
        self.operation = Some(Operation::Send(ContentKind::Files));
        self.origin = Origin::Files {
            selected,
            sequence: None,
        };
        self.files_ready = false;
        self.export = None;
        self.feedback = Some(Feedback::Waiting);
    }

    pub(super) fn has_file_offer(&self) -> bool {
        matches!(self.origin, Origin::Files { .. })
    }

    pub(super) fn has_selected_files(&self) -> bool {
        matches!(
            self.origin,
            Origin::Files {
                selected: Some(_),
                ..
            }
        )
    }

    pub(super) fn prefers_files(&self, local: &ClipboardRead) -> bool {
        matches!(&self.origin, Origin::Files { selected: Some(snapshot), .. }
            if snapshot.is_ok() && snapshot == local)
    }

    pub(super) fn files_ready(&self) -> bool {
        self.has_file_offer() && self.files_ready
    }

    pub(super) fn clear_files(&mut self) {
        if self.has_file_offer() {
            self.origin = Origin::Unobserved;
        }
        self.files_ready = false;
    }

    pub(super) fn remote_copy(&mut self, local: ClipboardRead) {
        self.origin = Origin::Remote {
            local,
            sequence: None,
        };
    }

    pub(super) fn remember_native_copy(&mut self, sequence: Option<u32>) {
        if let Some(export) = &mut self.export {
            export.sequence = sequence;
        }
        match &mut self.origin {
            Origin::Remote {
                sequence: previous, ..
            }
            | Origin::Files {
                sequence: previous, ..
            } => *previous = sequence,
            _ => {}
        }
    }

    pub(super) fn begin_control(&mut self) {
        // Text equality cannot detect copying the same text again in a host app.
        // A new interaction with this desktop must use the current host text,
        // rather than inheriting a previous remote-copy gesture. Remote files
        // have their own native ownership token and keep that authority.
        if matches!(self.origin, Origin::Remote { .. }) {
            self.origin = Origin::Unobserved;
        }
    }

    pub(super) fn expect_remote_copy(&mut self, local: ClipboardRead) {
        self.operation = Some(Operation::Receive(ContentKind::Unknown));
        self.remote_copy(local.clone());
        self.export = Some(Export {
            local,
            sequence: None,
            stage: ExportStage::AwaitingOffer,
        });
    }

    pub(super) fn remote_offer(&mut self, revision: u64) -> Option<bool> {
        self.remote = None;
        self.remote_content = None;
        let first = !std::mem::replace(&mut self.remote_offer_seen, true);
        if revision != self.revision {
            return None;
        }
        self.clear_files();
        let requested =
            if let Some(export) = self.export.as_mut().filter(|e| e.stage.awaits_offer()) {
                export.stage = ExportStage::AwaitingData;
                true
            } else {
                self.cancel_export();
                false
            };
        Some(!first || requested)
    }

    pub(super) fn has_export(&self) -> bool {
        self.export.is_some()
    }

    pub(super) fn awaiting_copy_offer(&self) -> bool {
        self.export
            .as_ref()
            .is_some_and(|export| export.stage.awaits_offer())
    }

    pub(super) fn local_changed_since_copy(&self, local: &ClipboardRead) -> bool {
        self.export.as_ref().is_some_and(|export| {
            matches!((&export.local, local), (Ok(before), Ok(current)) if before != current)
        })
    }

    pub(super) fn export_ready(&self) -> bool {
        self.export
            .as_ref()
            .is_some_and(|export| export.stage == ExportStage::Ready)
    }

    pub(super) fn capture_export(&mut self, local: ClipboardRead, sequence: Option<u32>) {
        self.export = Some(Export {
            local,
            sequence,
            stage: ExportStage::AwaitingData,
        });
    }

    pub(super) fn request_export(&mut self, local: ClipboardRead, sequence: Option<u32>) {
        self.export = Some(Export {
            local,
            sequence,
            stage: ExportStage::Requested,
        });
        self.operation = Some(Operation::Receive(ContentKind::Unknown));
        self.feedback = Some(Feedback::Requesting);
    }

    pub(super) fn cancel_export(&mut self) {
        self.export = None;
        if matches!(self.feedback, Some(Feedback::Requesting)) {
            self.feedback = None;
        }
    }

    pub(super) fn take_export(
        &mut self,
        local: &ClipboardRead,
        sequence: Option<u32>,
    ) -> Option<String> {
        if !self.export_ready() || self.remote_content.is_some() {
            return None;
        }
        let export = self.export.take()?;
        (export.local.is_ok() && &export.local == local && export.sequence == sequence)
            .then(|| self.remote.clone())
            .flatten()
    }

    pub(super) fn remote_content(&mut self, ownership: CancellationToken, text: Option<String>) {
        self.origin = Origin::RemoteContent { ownership, text };
    }

    pub(super) fn remote_files(&mut self, ownership: CancellationToken) {
        self.origin = Origin::RemoteFiles(ownership);
    }

    pub(super) fn prefers_remote(&self, local: &ClipboardRead) -> bool {
        match &self.origin {
            Origin::Remote {
                local: snapshot, ..
            } => snapshot.is_ok() && snapshot == local,
            Origin::RemoteFiles(ownership) => !ownership.is_cancelled() && *local == Ok(None),
            Origin::RemoteContent { ownership, text } => {
                !ownership.is_cancelled() && local.as_ref().is_ok_and(|local| local == text)
            }
            _ => false,
        }
    }

    pub(super) fn receive(&mut self, revision: u64, event: RdpClipboardEvent) -> bool {
        let current = revision == self.revision;
        match event {
            RdpClipboardEvent::Offer => {
                let _ = self.remote_offer(revision);
            }
            RdpClipboardEvent::Text(text) => {
                // Cache events describe the worker's clipboard in FIFO order,
                // even if the user has already begun another local operation.
                self.remote_content = None;
                self.remote = Some(text);
                if current {
                    self.data_received();
                }
            }
            RdpClipboardEvent::Content(content) => {
                self.remote = content.text.clone();
                self.remote_content = Some(std::sync::Arc::new(content));
                if current {
                    self.data_received();
                }
            }
            RdpClipboardEvent::Unavailable => {
                self.remote_content = None;
                self.remote = None;
                if current && self.feedback.as_ref().is_some_and(Feedback::remote_request) {
                    if self
                        .export
                        .as_ref()
                        .is_some_and(|e| e.stage == ExportStage::Requested)
                    {
                        self.cancel_export();
                    }
                    self.feedback = Some(Feedback::RemoteEmpty);
                }
            }
            event if current => {
                if self.has_file_offer() {
                    if matches!(
                        event,
                        RdpClipboardEvent::Published | RdpClipboardEvent::PasteSent
                    ) {
                        self.files_ready = true;
                    } else if matches!(event, RdpClipboardEvent::Rejected(_)) {
                        self.clear_files();
                    }
                }
                self.feedback = Some(match event {
                    RdpClipboardEvent::Published => Feedback::Message(
                        i18n_key::RDP_REMOTE_CLIPBOARD_CONFIRMED_PRESS_CTRL_V_IN_THE_REMOTE_DESKTOP,
                    ),
                    RdpClipboardEvent::PasteSent => {
                        Feedback::Message(i18n_key::RDP_PASTE_SHORTCUT_SENT_TO_THE_REMOTE_DESKTOP)
                    }
                    RdpClipboardEvent::PasteCancelled => Feedback::Message(
                        i18n_key::RDP_PASTE_CANCELLED_BECAUSE_FOCUS_OR_INPUT_CHANGED_TRY_AGAIN,
                    ),
                    RdpClipboardEvent::Rejected(error) => {
                        self.cancel_export();
                        Feedback::Rejected(error)
                    }
                    RdpClipboardEvent::Offer
                    | RdpClipboardEvent::Text(_)
                    | RdpClipboardEvent::Content(_)
                    | RdpClipboardEvent::Unavailable => return current,
                });
            }
            _ => {}
        }
        current
    }

    fn data_received(&mut self) {
        if let Some(export) = &mut self.export
            && matches!(
                export.stage,
                ExportStage::AwaitingData | ExportStage::Requested
            )
        {
            export.stage = ExportStage::Ready;
        }
        if self.feedback.as_ref().is_some_and(Feedback::remote_request) {
            self.feedback = self.export_ready().then_some(Feedback::Requesting);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::rdp::RdpEvent;
    use tokio::sync::mpsc;

    #[test]
    fn routine_success_is_quiet_but_failures_keep_the_transfer_direction() {
        let mut clipboard = ClipboardState::default();
        let revision = clipboard.begin();
        clipboard.publish();
        assert!(clipboard.feedback_text().unwrap().contains("→"));
        clipboard.receive(revision, RdpClipboardEvent::Published);
        assert!(clipboard.feedback_text().is_none());
        clipboard.receive(
            revision,
            RdpClipboardEvent::Rejected(RdpError::ClipboardUnavailable),
        );
        assert!(clipboard.feedback_text().unwrap().contains("→"));
        clipboard.feedback = Some(Feedback::Requesting);
        clipboard.operation = Some(Operation::Receive(ContentKind::Unknown));
        assert!(clipboard.feedback_text().unwrap().contains("→"));
    }

    #[test]
    fn existing_local_images_do_not_interrupt_remote_copy_or_selected_files() {
        let mut clipboard = ClipboardState::default();
        clipboard.expect_remote_copy(Ok(None));
        clipboard.remember_native_copy(Some(41));
        clipboard.local_image(Some(41));
        assert!(clipboard.prefers_remote(&Ok(None))); // Ctrl+C then immediate Ctrl+V remotely.
        clipboard.local_image(Some(42));
        assert!(!clipboard.prefers_remote(&Ok(None))); // A new host copy takes priority.

        clipboard.publish_files(Some(Ok(None)));
        clipboard.remember_native_copy(Some(43));
        clipboard.local_image(Some(43));
        assert!(clipboard.prefers_files(&Ok(None)));
        clipboard.local_image(Some(44));
        assert!(!clipboard.prefers_files(&Ok(None)));

        clipboard.remote_content(CancellationToken::new(), Some("image caption".into()));
        clipboard.local_image(Some(45));
        assert!(clipboard.prefers_remote(&Ok(clipboard.image_text())));
    }

    #[test]
    fn copying_a_local_image_supersedes_remote_and_selected_file_offers() {
        let mut clipboard = ClipboardState::default();
        clipboard.expect_remote_copy(Ok(None));
        assert!(clipboard.prefers_remote(&Ok(None)));
        clipboard.local_image(None);
        assert!(!clipboard.prefers_remote(&Ok(None)));
        assert!(!clipboard.has_export());

        clipboard.publish_files(Some(Ok(None)));
        assert!(clipboard.prefers_files(&Ok(None)));
        clipboard.local_image(None);
        assert!(!clipboard.prefers_files(&Ok(None)));
        assert!(!clipboard.has_selected_files());

        clipboard.remote_files(CancellationToken::new());
        clipboard.local_image(None);
        assert!(!clipboard.prefers_remote(&Ok(None)));
        let ownership = CancellationToken::new();
        clipboard.remote_content(ownership.clone(), None);
        clipboard.local_image(None);
        assert!(clipboard.prefers_remote(&Ok(None)));
        ownership.cancel();
        clipboard.local_image(None);
        assert!(!clipboard.prefers_remote(&Ok(None)));
    }

    #[test]
    fn replacing_remote_files_with_local_files_revokes_remote_paste_preference() {
        let mut clipboard = ClipboardState::default();
        let ownership = CancellationToken::new();
        clipboard.remote_files(ownership.clone());
        assert!(clipboard.prefers_remote(&Ok(None)));
        assert!(!clipboard.prefers_remote(&Ok(Some("new local text".into()))));
        ownership.cancel();
        assert!(!clipboard.prefers_remote(&Ok(None)));
    }

    #[test]
    fn remote_offer_keeps_the_copy_time_baseline_and_new_local_intent_revokes_it() {
        let before = Ok(Some("before remote copy".into()));
        let newer = Ok(Some("new local copy".into()));
        let mut clipboard = ClipboardState::default();
        let revision = clipboard.begin();
        clipboard.expect_remote_copy(before.clone());
        assert!(clipboard.awaiting_copy_offer());
        assert!(clipboard.local_changed_since_copy(&newer));

        assert_eq!(clipboard.remote_offer(revision), Some(true));
        assert!(!clipboard.awaiting_copy_offer());
        assert!(clipboard.has_export());
        assert!(clipboard.local_changed_since_copy(&newer));
        assert!(!clipboard.local_changed_since_copy(&before));

        clipboard.begin();
        assert!(!clipboard.has_export());
        assert_eq!(clipboard.remote_offer(revision), None);
    }

    fn receive_next(clipboard: &mut ClipboardState, events: &mut mpsc::Receiver<RdpEvent>) -> bool {
        let RdpEvent::Clipboard { revision, event } = events.try_recv().unwrap() else {
            panic!("expected a clipboard event");
        };
        clipboard.receive(revision, event)
    }

    #[test]
    fn chosen_files_wait_for_their_own_confirmation_and_keep_the_remote_offer() {
        let mut clipboard = ClipboardState::default();
        let revision = clipboard.begin();
        clipboard.publish_files(Some(Ok(Some("existing local text".into()))));
        assert!(clipboard.prefers_files(&Ok(Some("existing local text".into()))));
        assert!(!clipboard.prefers_files(&Ok(Some("new local copy".into()))));
        assert!(!clipboard.prefers_files(&Ok(None)));
        assert!(!clipboard.prefers_files(&Err(i18n_key::RDP_CLIPBOARD_TEXT_IS_LIMITED_TO_1_MIB)));
        assert!(!clipboard.files_ready());
        assert!(!clipboard.receive(revision - 1, RdpClipboardEvent::Published));
        assert!(!clipboard.files_ready());
        assert_eq!(clipboard.remote_offer(revision - 1), None);
        assert!(clipboard.prefers_files(&Ok(Some("existing local text".into()))));
        assert!(clipboard.receive(revision, RdpClipboardEvent::Published));
        assert!(clipboard.files_ready());
        assert!(clipboard.prefers_files(&Ok(Some("existing local text".into()))));

        // A new remote copy supersedes this offer, even while an older file read finishes.
        clipboard.remote_offer(revision);
        assert!(!clipboard.has_file_offer());
        assert!(!clipboard.files_ready());
    }

    #[test]
    fn clipboard_files_are_refreshed_on_the_next_paste_and_rejection_revokes_them() {
        let mut clipboard = ClipboardState::default();
        let revision = clipboard.begin();
        clipboard.publish_files(None);
        clipboard.receive(revision, RdpClipboardEvent::PasteSent);
        assert!(clipboard.has_file_offer());
        assert!(clipboard.files_ready());
        assert!(!clipboard.prefers_files(&Ok(Some("existing local text".into()))));
        clipboard.receive(revision, RdpClipboardEvent::Rejected(RdpError::FileChanged));
        assert!(!clipboard.has_file_offer());
        assert!(!clipboard.files_ready());
    }

    #[test]
    fn text_publication_and_explicit_cancellation_replace_selected_files() {
        let mut clipboard = ClipboardState::default();
        clipboard.publish_files(Some(Ok(Some("existing local text".into()))));
        clipboard.publish();
        assert!(!clipboard.has_file_offer());
        assert!(!clipboard.files_ready());
        clipboard.publish_files(Some(Ok(Some("existing local text".into()))));
        clipboard.clear_files();
        assert!(!clipboard.prefers_files(&Ok(Some("existing local text".into()))));
        assert!(!clipboard.files_ready());
    }

    #[test]
    fn initial_remote_contents_do_not_replace_local_text_but_explicit_copy_does() {
        let local = Ok(Some("Ubuntu text ready to paste".into()));
        let mut clipboard = ClipboardState::default();
        assert_eq!(clipboard.remote_offer(0), Some(false));
        clipboard.receive(0, RdpClipboardEvent::Text("old Windows text".into()));
        assert!(!clipboard.export_ready());
        assert_eq!(clipboard.take_export(&local, None), None);

        let mut clipboard = ClipboardState::default();
        let revision = clipboard.begin();
        clipboard.expect_remote_copy(local.clone());
        assert_eq!(clipboard.remote_offer(revision), Some(true));
        clipboard.receive(revision, RdpClipboardEvent::Unavailable);
        clipboard.receive(
            revision,
            RdpClipboardEvent::Text("copied Windows 中文".into()),
        );
        assert_eq!(
            clipboard.take_export(&local, None).as_deref(),
            Some("copied Windows 中文")
        );
        assert_eq!(clipboard.take_export(&local, None), None);
    }

    #[test]
    fn a_new_remote_offer_supports_mouse_copy_and_never_overwrites_newer_local_text() {
        let original = Ok(Some("previous Ubuntu text".into()));
        let mut clipboard = ClipboardState::default();
        assert_eq!(clipboard.remote_offer(0), Some(false));
        clipboard.receive(0, RdpClipboardEvent::Unavailable);
        assert_eq!(clipboard.remote_offer(0), Some(true));
        clipboard.capture_export(original.clone(), None);
        clipboard.receive(0, RdpClipboardEvent::Unavailable);
        clipboard.receive(0, RdpClipboardEvent::Text("right-click copy".into()));
        assert!(clipboard.export_ready());
        assert_eq!(
            clipboard.take_export(&original, None).as_deref(),
            Some("right-click copy")
        );

        assert_eq!(clipboard.remote_offer(0), Some(true));
        clipboard.capture_export(original.clone(), None);
        clipboard.receive(0, RdpClipboardEvent::Text("late Windows reply".into()));
        assert_eq!(
            clipboard.take_export(&Ok(Some("new Ubuntu copy".into())), None),
            None
        );
        assert_eq!(clipboard.take_export(&original, None), None);
    }

    #[test]
    fn stale_offers_cannot_arm_or_cancel_a_new_copy_and_new_local_actions_revoke_export() {
        let local = Ok(Some("Ubuntu".into()));
        let mut clipboard = ClipboardState::default();
        let current = clipboard.begin();
        clipboard.expect_remote_copy(local.clone());
        assert_eq!(clipboard.remote_offer(0), None);
        assert!(clipboard.has_export());
        clipboard.receive(0, RdpClipboardEvent::Text("stale".into()));
        assert!(!clipboard.export_ready());
        assert_eq!(clipboard.remote_offer(current), Some(true));
        clipboard.receive(current, RdpClipboardEvent::Text("current".into()));
        assert!(clipboard.export_ready());
        clipboard.begin();
        assert_eq!(clipboard.take_export(&local, None), None);
    }

    #[test]
    fn every_new_offer_replaces_the_previous_exports_local_snapshot() {
        let mut clipboard = ClipboardState::default();
        assert_eq!(clipboard.remote_offer(0), Some(false));
        assert_eq!(clipboard.remote_offer(0), Some(true));
        clipboard.capture_export(Ok(Some("old Ubuntu text".into())), None);
        // An image-only or rejected offer never produces Unicode text.
        clipboard.receive(0, RdpClipboardEvent::Unavailable);
        assert_eq!(clipboard.remote_offer(0), Some(true));
        assert!(!clipboard.has_export());
        let local = Ok(Some("new Ubuntu text".into()));
        clipboard.capture_export(local.clone(), None);
        clipboard.receive(0, RdpClipboardEvent::Text("new Windows copy".into()));
        assert_eq!(
            clipboard.take_export(&local, None).as_deref(),
            Some("new Windows copy")
        );

        clipboard.expect_remote_copy(local.clone());
        let _ = clipboard.remote_offer(0);
        // A later offer while the application is inactive gets no new permit.
        let _ = clipboard.remote_offer(0);
        assert!(!clipboard.has_export());
        clipboard.receive(0, RdpClipboardEvent::Text("background change".into()));
        assert_eq!(clipboard.take_export(&local, None), None);
    }

    #[test]
    fn queued_remote_text_cannot_change_a_new_local_paste_source() {
        let (events, mut receive) = mpsc::channel(4);
        let mut clipboard = ClipboardState::default();
        events
            .try_send(RdpEvent::Clipboard {
                revision: 0,
                event: RdpClipboardEvent::Text("old Windows text".into()),
            })
            .unwrap();

        // The worker has not consumed the new paste when the UI receives its
        // previously queued clipboard offer. A second Ctrl+V must still publish
        // Ubuntu text, rather than cancelling it by sending a remote V key.
        let revision = clipboard.begin();
        clipboard.publish();
        let current = receive_next(&mut clipboard, &mut receive);
        assert!(!clipboard.prefers_remote(&Ok(Some("new Ubuntu 文本".into()))));
        assert!(!current);
        assert!(matches!(clipboard.feedback, Some(Feedback::Waiting)));
        assert_eq!(clipboard.remote.as_deref(), Some("old Windows text"));

        events
            .try_send(RdpEvent::Clipboard {
                revision,
                event: RdpClipboardEvent::Unavailable,
            })
            .unwrap();
        events
            .try_send(RdpEvent::Clipboard {
                revision,
                event: RdpClipboardEvent::PasteSent,
            })
            .unwrap();
        assert!(receive_next(&mut clipboard, &mut receive));
        assert!(clipboard.remote.is_none());
        assert!(receive_next(&mut clipboard, &mut receive));
        assert!(matches!(
            clipboard.feedback,
            Some(Feedback::Message(
                i18n_key::RDP_PASTE_SHORTCUT_SENT_TO_THE_REMOTE_DESKTOP
            ))
        ));
        assert!(!clipboard.prefers_remote(&Ok(Some("new Ubuntu 文本".into()))));
    }

    #[test]
    fn queued_results_cannot_overwrite_a_new_request_or_local_read_failure() {
        for event in [
            RdpClipboardEvent::Published,
            RdpClipboardEvent::PasteSent,
            RdpClipboardEvent::PasteCancelled,
            RdpClipboardEvent::Rejected(RdpError::ClipboardUnavailable),
        ] {
            let (events, mut receive) = mpsc::channel(2);
            let mut clipboard = ClipboardState::default();
            let previous = clipboard.begin();
            clipboard.publish();
            events
                .try_send(RdpEvent::Clipboard {
                    revision: previous,
                    event,
                })
                .unwrap();

            let current = clipboard.begin();
            clipboard.feedback = Some(Feedback::Message(
                i18n_key::RDP_THE_LOCAL_CLIPBOARD_CONTAINS_NO_TEXT,
            ));
            assert!(!receive_next(&mut clipboard, &mut receive));
            assert!(matches!(
                clipboard.feedback,
                Some(Feedback::Message(
                    i18n_key::RDP_THE_LOCAL_CLIPBOARD_CONTAINS_NO_TEXT
                ))
            ));

            clipboard.publish();
            events
                .try_send(RdpEvent::Clipboard {
                    revision: previous,
                    event: RdpClipboardEvent::Rejected(RdpError::ClipboardUnavailable),
                })
                .unwrap();
            events
                .try_send(RdpEvent::Clipboard {
                    revision: current,
                    event: RdpClipboardEvent::Published,
                })
                .unwrap();
            assert!(!receive_next(&mut clipboard, &mut receive));
            assert!(matches!(clipboard.feedback, Some(Feedback::Waiting)));
            assert!(receive_next(&mut clipboard, &mut receive));
            assert!(matches!(
                clipboard.feedback,
                Some(Feedback::Message(
                    i18n_key::RDP_REMOTE_CLIPBOARD_CONFIRMED_PRESS_CTRL_V_IN_THE_REMOTE_DESKTOP
                ))
            ));
        }
    }

    #[test]
    fn copying_remote_text_keeps_queued_cache_updates_without_replacing_feedback() {
        let (events, mut receive) = mpsc::channel(2);
        let mut clipboard = ClipboardState::default();
        clipboard.receive(0, RdpClipboardEvent::Text("first remote".into()));
        events
            .try_send(RdpEvent::Clipboard {
                revision: 0,
                event: RdpClipboardEvent::Text("new remote".into()),
            })
            .unwrap();

        let copied = clipboard.remote.clone();
        let revision = clipboard.begin();
        clipboard.remote_copy(Ok(copied.clone()));
        clipboard.feedback = Some(Feedback::Message(
            i18n_key::RDP_REMOTE_TEXT_COPIED_TO_THE_LOCAL_CLIPBOARD,
        ));
        assert!(!receive_next(&mut clipboard, &mut receive));
        assert_eq!(clipboard.remote.as_deref(), Some("new remote"));
        assert!(clipboard.prefers_remote(&Ok(copied)));
        assert!(matches!(
            clipboard.feedback,
            Some(Feedback::Message(
                i18n_key::RDP_REMOTE_TEXT_COPIED_TO_THE_LOCAL_CLIPBOARD
            ))
        ));

        // Cancel is a queue barrier, not a permanent ban on remote updates.
        assert!(clipboard.receive(revision, RdpClipboardEvent::Text("after barrier".into())));
        assert_eq!(clipboard.remote.as_deref(), Some("after barrier"));
    }

    #[test]
    fn failed_menu_copy_cannot_publish_a_late_reply() {
        let local = Ok(Some("original".into()));
        for failure in [
            RdpClipboardEvent::Unavailable,
            RdpClipboardEvent::Rejected(RdpError::ClipboardUnavailable),
        ] {
            let mut clipboard = ClipboardState::default();
            let revision = clipboard.begin();
            clipboard.request_export(local.clone(), None);
            clipboard.receive(revision, failure);
            assert!(!clipboard.has_export());
            clipboard.receive(revision, RdpClipboardEvent::Text("late".into()));
            assert_eq!(clipboard.take_export(&local, None), None);
        }
    }

    #[test]
    fn one_click_copies_the_reply_with_or_without_a_new_format_offer() {
        let local = Ok(Some("original".into()));
        for announces in [false, true] {
            let mut clipboard = ClipboardState::default();
            let revision = clipboard.begin();
            clipboard.request_export(local.clone(), None);
            if announces {
                assert_eq!(clipboard.remote_offer(revision), Some(true));
                clipboard.receive(revision, RdpClipboardEvent::Unavailable);
            }
            clipboard.receive(revision, RdpClipboardEvent::Text("remote".into()));
            assert_eq!(
                clipboard.take_export(&local, None).as_deref(),
                Some("remote")
            );
            assert_eq!(clipboard.take_export(&local, None), None);
        }
    }

    #[test]
    fn a_menu_copy_keeps_all_remote_formats_together_for_native_publication() {
        let mut clipboard = ClipboardState::default();
        let local = Ok(None);
        let revision = clipboard.begin();
        clipboard.request_export(local.clone(), None);
        let selection = rustrdp::ClipboardContent {
            text: Some("selection".into()),
            html: Some("<b>selection</b>".into()),
            ..Default::default()
        };
        clipboard.receive(revision, RdpClipboardEvent::Content(selection.clone()));
        assert!(clipboard.export_ready());
        assert_eq!(clipboard.remote_content.as_deref(), Some(&selection));
        assert_eq!(clipboard.take_export(&local, None), None);
        assert!(clipboard.export_ready());
    }

    #[test]
    fn a_new_local_image_supersedes_a_pending_remote_text_copy() {
        let mut clipboard = ClipboardState::default();
        let local = Ok(None);
        let revision = clipboard.begin();
        clipboard.request_export(local.clone(), Some(14));
        clipboard.receive(revision, RdpClipboardEvent::Text("late".into()));
        assert_eq!(clipboard.take_export(&local, Some(15)), None);
    }

    #[test]
    fn a_new_offer_invalidates_cached_data_before_the_request_can_reuse_it() {
        let mut clipboard = ClipboardState::default();
        clipboard.receive(
            0,
            RdpClipboardEvent::Content(rustrdp::ClipboardContent {
                text: Some("old".into()),
                png: Some(vec![1, 2]),
                ..Default::default()
            }),
        );
        clipboard.remote_offer(0);
        assert!(clipboard.remote_content.is_none());
        assert!(clipboard.remote.is_none());
    }

    #[test]
    fn current_remote_text_response_cannot_take_over_a_local_paste_source() {
        let local = Ok(Some("Ubuntu 文本".to_owned()));
        let mut clipboard = ClipboardState::default();
        clipboard.receive(0, RdpClipboardEvent::Text("initial Windows text".into()));
        assert!(!clipboard.prefers_remote(&local));
        let revision = clipboard.begin();
        clipboard.publish();
        for remote in ["late Windows text", "Ubuntu 文本", ""] {
            assert!(clipboard.receive(revision, RdpClipboardEvent::Text(remote.into())));
            assert_eq!(clipboard.remote.as_deref(), Some(remote));
            assert!(!clipboard.prefers_remote(&local));
        }
        // Retrying with unchanged Ubuntu text must publish it again instead of
        // sending Ctrl+V against unrelated remote clipboard contents.
        clipboard.begin();
        assert!(!clipboard.prefers_remote(&local));
    }

    #[test]
    fn explicit_remote_copy_is_preferred_until_the_local_clipboard_changes() {
        let local = Ok(Some("Ubuntu 文本".to_owned()));
        let mut clipboard = ClipboardState::default();
        let revision = clipboard.begin();
        clipboard.expect_remote_copy(local.clone());
        assert!(clipboard.prefers_remote(&local));
        assert_eq!(clipboard.remote_offer(revision), Some(true));
        clipboard.receive(revision, RdpClipboardEvent::Unavailable);
        // FORMAT_LIST invalidates the text cache before the new data arrives.
        // Remote images and files can also be pasted without a text response.
        assert!(clipboard.prefers_remote(&local));
        clipboard.receive(
            revision,
            RdpClipboardEvent::Text("copied in Windows".into()),
        );
        assert!(clipboard.prefers_remote(&local));
        assert!(!clipboard.prefers_remote(&Ok(Some("new Ubuntu text".into()))));
    }

    #[test]
    fn returning_from_host_uses_local_text_even_when_the_contents_are_unchanged() {
        let local = Ok(Some("copied again in Ubuntu".into()));
        let mut clipboard = ClipboardState::default();
        clipboard.expect_remote_copy(local.clone());
        assert!(clipboard.prefers_remote(&local));
        clipboard.begin_control();
        assert!(!clipboard.prefers_remote(&local));

        clipboard.remote_copy(local.clone());
        assert!(clipboard.prefers_remote(&local));
        clipboard.begin_control();
        assert!(!clipboard.prefers_remote(&local));
    }

    #[test]
    fn returning_to_remote_keeps_native_file_clipboard_ownership() {
        let mut clipboard = ClipboardState::default();
        let ownership = CancellationToken::new();
        clipboard.remote_files(ownership.clone());
        clipboard.begin_control();
        assert!(clipboard.prefers_remote(&Ok(None)));
        ownership.cancel();
        assert!(!clipboard.prefers_remote(&Ok(None)));
    }

    #[test]
    fn remote_text_is_preferred_only_after_export_is_written_locally() {
        let local = Ok(Some("Ubuntu 文本".to_owned()));
        let mut clipboard = ClipboardState::default();
        assert_eq!(clipboard.remote_offer(0), Some(false));
        assert_eq!(clipboard.remote_offer(0), Some(true));
        clipboard.capture_export(local.clone(), None);
        clipboard.receive(0, RdpClipboardEvent::Text("copied in Windows".into()));
        assert!(!clipboard.prefers_remote(&local));
        let text = clipboard.take_export(&local, None).unwrap();
        assert_eq!(text, "copied in Windows");
        assert!(!clipboard.prefers_remote(&local));

        let exported = Ok(Some(text));
        clipboard.remote_copy(exported.clone());
        assert!(clipboard.prefers_remote(&exported));
        assert!(!clipboard.prefers_remote(&local));
        assert!(!clipboard.prefers_remote(&Ok(None)));

        clipboard.remote_copy(Ok(None));
        assert!(clipboard.prefers_remote(&Ok(None)));
        let rejected = Err(i18n_key::RDP_CLIPBOARD_TEXT_IS_LIMITED_TO_1_MIB);
        clipboard.remote_copy(rejected.clone());
        assert!(!clipboard.prefers_remote(&rejected));
    }
}

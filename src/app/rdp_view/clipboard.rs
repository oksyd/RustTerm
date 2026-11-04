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
    Remote(ClipboardRead),
    RemoteFiles(CancellationToken),
    RemoteContent {
        ownership: CancellationToken,
        text: Option<String>,
    },
    Files {
        selected: Option<ClipboardRead>,
    },
}

pub(super) enum Feedback {
    ReadingFiles,
    Waiting,
    Requesting,
    RemoteEmpty,
    RemoteReady,
    Message(&'static str),
    Rejected(RdpError),
}

impl Feedback {
    pub(super) fn text(&self) -> String {
        let message = match self {
            Self::ReadingFiles => i18n_key::RDP_READING_FILES_FROM_THE_LOCAL_CLIPBOARD,
            Self::Waiting => i18n_key::RDP_WAITING_FOR_REMOTE_CLIPBOARD_CONFIRMATION,
            Self::Requesting => {
                i18n_key::RDP_REMOTE_CLIPBOARD_REQUESTED_USE_COPY_FROM_REMOTE_WHEN_TEXT_IS
            }
            Self::RemoteEmpty => i18n_key::RDP_THE_REMOTE_CLIPBOARD_HAS_NO_TEXT_AVAILABLE,
            Self::RemoteReady => {
                i18n_key::RDP_REMOTE_CLIPBOARD_IS_READY_USE_COPY_FROM_REMOTE_TO_COPY
            }
            Self::Message(message) => message,
            Self::Rejected(error) => return error.to_string(),
        };
        tr(message).to_owned()
    }

    fn remote_request(&self) -> bool {
        matches!(
            self,
            Self::Requesting | Self::RemoteEmpty | Self::RemoteReady
        )
    }
}

#[derive(Default)]
pub(super) struct ClipboardState {
    revision: u64,
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
    stage: ExportStage,
}

#[derive(PartialEq, Eq)]
enum ExportStage {
    AwaitingOffer,
    AwaitingText,
    Ready,
}

impl ClipboardState {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn begin(&mut self) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        self.feedback = None;
        self.export = None;
        self.revision
    }

    pub(super) fn publish(&mut self) {
        self.origin = Origin::Local;
        self.feedback = Some(Feedback::Waiting);
    }

    pub(super) fn publish_files(&mut self, selected: Option<ClipboardRead>) {
        self.origin = Origin::Files { selected };
        self.files_ready = false;
        self.export = None;
        self.feedback = Some(Feedback::Waiting);
    }

    pub(super) fn has_file_offer(&self) -> bool {
        matches!(self.origin, Origin::Files { .. })
    }

    pub(super) fn has_selected_files(&self) -> bool {
        matches!(self.origin, Origin::Files { selected: Some(_) })
    }

    pub(super) fn prefers_files(&self, local: &ClipboardRead) -> bool {
        matches!(&self.origin, Origin::Files { selected: Some(snapshot) }
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
        self.origin = Origin::Remote(local);
    }

    pub(super) fn begin_control(&mut self) {
        // Text equality cannot detect copying the same text again in a host app.
        // A new interaction with this desktop must use the current host text,
        // rather than inheriting a previous remote-copy gesture. Remote files
        // have their own native ownership token and keep that authority.
        if matches!(self.origin, Origin::Remote(_)) {
            self.origin = Origin::Unobserved;
        }
    }

    pub(super) fn expect_remote_copy(&mut self, local: ClipboardRead) {
        self.origin = Origin::Remote(local.clone());
        self.export = Some(Export {
            local,
            stage: ExportStage::AwaitingOffer,
        });
    }

    pub(super) fn remote_offer(&mut self, revision: u64) -> Option<bool> {
        let first = !std::mem::replace(&mut self.remote_offer_seen, true);
        if revision != self.revision {
            return None;
        }
        self.clear_files();
        let requested = match &mut self.export {
            Some(export) if export.stage == ExportStage::AwaitingOffer => {
                export.stage = ExportStage::AwaitingText;
                true
            }
            _ => {
                self.export = None;
                false
            }
        };
        Some(!first || requested)
    }

    pub(super) fn has_export(&self) -> bool {
        self.export.is_some()
    }

    pub(super) fn awaiting_copy_offer(&self) -> bool {
        self.export
            .as_ref()
            .is_some_and(|export| export.stage == ExportStage::AwaitingOffer)
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
            && self.remote.is_some()
    }

    pub(super) fn capture_export(&mut self, local: ClipboardRead) {
        self.export = Some(Export {
            local,
            stage: ExportStage::AwaitingText,
        });
    }

    pub(super) fn cancel_export(&mut self) {
        self.export = None;
    }

    pub(super) fn take_export(&mut self, local: &ClipboardRead) -> Option<String> {
        if !self.export_ready() {
            return None;
        }
        let export = self.export.take()?;
        (export.local.is_ok() && &export.local == local)
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
            Origin::Remote(snapshot) => snapshot.is_ok() && snapshot == local,
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
                    if let Some(export) = &mut self.export
                        && export.stage == ExportStage::AwaitingText
                    {
                        export.stage = ExportStage::Ready;
                    }
                    // A response updates the remote cache, not clipboard ownership.
                    // It can arrive after a local publish, with the same revision.
                    // Only a remote copy gesture or a completed local export can
                    // make subsequent pastes prefer the remote clipboard.
                    if self.feedback.as_ref().is_some_and(Feedback::remote_request) {
                        self.feedback = Some(Feedback::RemoteReady);
                    }
                }
            }
            RdpClipboardEvent::Content(content) => {
                self.remote = content.text.clone();
                self.remote_content = Some(std::sync::Arc::new(content));
                if current && self.feedback.as_ref().is_some_and(Feedback::remote_request) {
                    self.feedback = Some(Feedback::RemoteReady);
                }
            }
            RdpClipboardEvent::Unavailable => {
                self.remote_content = None;
                self.remote = None;
                if current && self.feedback.as_ref().is_some_and(Feedback::remote_request) {
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
                    RdpClipboardEvent::Rejected(error) => Feedback::Rejected(error),
                    RdpClipboardEvent::Offer
                    | RdpClipboardEvent::Text(_)
                    | RdpClipboardEvent::Content(_)
                    | RdpClipboardEvent::Unavailable => unreachable!(),
                });
            }
            _ => {}
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::rdp::RdpEvent;
    use tokio::sync::mpsc;

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
        assert_eq!(clipboard.take_export(&local), None);

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
            clipboard.take_export(&local).as_deref(),
            Some("copied Windows 中文")
        );
        assert_eq!(clipboard.take_export(&local), None);
    }

    #[test]
    fn a_new_remote_offer_supports_mouse_copy_and_never_overwrites_newer_local_text() {
        let original = Ok(Some("previous Ubuntu text".into()));
        let mut clipboard = ClipboardState::default();
        assert_eq!(clipboard.remote_offer(0), Some(false));
        clipboard.receive(0, RdpClipboardEvent::Unavailable);
        assert_eq!(clipboard.remote_offer(0), Some(true));
        clipboard.capture_export(original.clone());
        clipboard.receive(0, RdpClipboardEvent::Unavailable);
        clipboard.receive(0, RdpClipboardEvent::Text("right-click copy".into()));
        assert!(clipboard.export_ready());
        assert_eq!(
            clipboard.take_export(&original).as_deref(),
            Some("right-click copy")
        );

        assert_eq!(clipboard.remote_offer(0), Some(true));
        clipboard.capture_export(original.clone());
        clipboard.receive(0, RdpClipboardEvent::Text("late Windows reply".into()));
        assert_eq!(
            clipboard.take_export(&Ok(Some("new Ubuntu copy".into()))),
            None
        );
        assert_eq!(clipboard.take_export(&original), None);
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
        assert_eq!(clipboard.take_export(&local), None);
    }

    #[test]
    fn every_new_offer_replaces_the_previous_exports_local_snapshot() {
        let mut clipboard = ClipboardState::default();
        assert_eq!(clipboard.remote_offer(0), Some(false));
        assert_eq!(clipboard.remote_offer(0), Some(true));
        clipboard.capture_export(Ok(Some("old Ubuntu text".into())));
        // An image-only or rejected offer never produces Unicode text.
        clipboard.receive(0, RdpClipboardEvent::Unavailable);
        assert_eq!(clipboard.remote_offer(0), Some(true));
        assert!(!clipboard.has_export());
        let local = Ok(Some("new Ubuntu text".into()));
        clipboard.capture_export(local.clone());
        clipboard.receive(0, RdpClipboardEvent::Text("new Windows copy".into()));
        assert_eq!(
            clipboard.take_export(&local).as_deref(),
            Some("new Windows copy")
        );

        clipboard.expect_remote_copy(local.clone());
        let _ = clipboard.remote_offer(0);
        // A later offer while the application is inactive gets no new permit.
        let _ = clipboard.remote_offer(0);
        assert!(!clipboard.has_export());
        clipboard.receive(0, RdpClipboardEvent::Text("background change".into()));
        assert_eq!(clipboard.take_export(&local), None);
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
    fn requesting_remote_text_survives_format_invalidation_before_the_data_reply() {
        let mut clipboard = ClipboardState::default();
        let revision = clipboard.begin();
        clipboard.feedback = Some(Feedback::Requesting);
        clipboard.receive(revision, RdpClipboardEvent::Unavailable);
        assert!(matches!(clipboard.feedback, Some(Feedback::RemoteEmpty)));
        clipboard.receive(revision, RdpClipboardEvent::Text("remote".into()));
        assert!(matches!(clipboard.feedback, Some(Feedback::RemoteReady)));
        assert_eq!(clipboard.remote.as_deref(), Some("remote"));
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
        clipboard.capture_export(local.clone());
        clipboard.receive(0, RdpClipboardEvent::Text("copied in Windows".into()));
        assert!(!clipboard.prefers_remote(&local));
        let text = clipboard.take_export(&local).unwrap();
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

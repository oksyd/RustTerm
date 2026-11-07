use super::Workspace;
use gpui::Context;
use rustterm::config::CredentialStore;
use rustterm::config::{CredentialRef, ProcessCredentialStore, Profile};
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
use std::collections::HashSet;

impl Workspace {
    pub(super) fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            self.save_again = true;
            return;
        }
        self.saving = true;
        self.save_failed = false;
        let config = self.config.clone();
        let paths = self.paths.clone();
        let saved = config.clone();
        let task = cx
            .background_executor()
            .spawn(async move { paths.save_config(&config) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        if this
                            .last_save_error
                            .as_ref()
                            .is_some_and(|previous| this.error.as_ref() == Some(previous))
                        {
                            this.error = None;
                        }
                        this.last_save_error = None;
                        this.persisted_credentials = saved
                            .profiles
                            .iter()
                            .filter_map(profile_reference)
                            .collect();
                        this.cleanup_credentials(cx);
                    }
                    Err(error) => {
                        if error.was_committed() {
                            this.persisted_credentials
                                .extend(saved.profiles.iter().filter_map(profile_reference));
                        }
                        this.save_failed = true;
                        let message = rustterm::msg!(
                            i18n_key::WORKSPACE_CONFIGURATION_NEEDS_ATTENTION_ERROR,
                            error = Message::from_error(error)
                        );
                        this.last_save_error = Some(message.clone());
                        this.error = Some(message);
                        // Do not remove either old or new secrets after an uncertain commit.
                    }
                }
                if this.save_again {
                    this.save_again = false;
                    this.save(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn cleanup_credentials(&mut self, cx: &mut Context<Self>) {
        let removable: Vec<_> = self
            .credential_cleanup
            .iter()
            .copied()
            .filter(|reference| {
                cleanup_allowed(
                    *reference,
                    &self.persisted_credentials,
                    &self.config.profiles,
                )
            })
            .take(4usize.saturating_sub(self.pending_credentials))
            .collect();
        for reference in removable {
            self.credential_cleanup.remove(&reference);
            self.pending_credentials += 1;
            let task = cx
                .background_executor()
                .spawn(async move { ProcessCredentialStore::new().delete(reference) });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.pending_credentials = this.pending_credentials.saturating_sub(1);
                    if let Err(error) = result {
                        let message = rustterm::msg!(
                            i18n_key::CONNECTIONS_CREDENTIAL_CLEANUP_COULD_NOT_BE_CONFIRMED_ERROR,
                            error = Message::from_error(error)
                        );
                        this.error = Some(match this.error.take() {
                            Some(previous) => Message::join([previous, message], "\n"),
                            None => message,
                        });
                    }
                    this.cleanup_credentials(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }
}
fn cleanup_allowed(
    reference: CredentialRef,
    committed: &HashSet<CredentialRef>,
    current: &[Profile],
) -> bool {
    !committed.contains(&reference)
        && !current
            .iter()
            .any(|p| profile_reference(p) == Some(reference))
}
pub(super) fn profile_reference(profile: &Profile) -> Option<CredentialRef> {
    profile.connection.credential_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::AuthMethod;
    #[test]
    fn credentials_survive_unsaved_removal_and_shared_references() {
        let reference = CredentialRef::new();
        let committed = HashSet::from([reference]);
        // In-memory deletion is insufficient: the on-disk config can still need this secret.
        assert!(!cleanup_allowed(reference, &committed, &[]));
        let shared = Profile::ssh(
            "other profile",
            rustterm::config::SshConfig {
                host: "example.invalid".into(),
                username: "fixture".into(),
                auth: AuthMethod::Password {
                    credential: Some(reference),
                },
                ..Default::default()
            },
        );
        assert!(!cleanup_allowed(reference, &HashSet::new(), &[shared]));
        assert!(cleanup_allowed(reference, &HashSet::new(), &[]));
    }
}

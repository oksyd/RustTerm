use super::*;
use std::collections::HashMap;

/// Workspace ownership keeps forwarding independent of individual terminal tabs.
#[derive(Clone, Default)]
pub struct ForwardRegistry(Arc<Registry>);

#[derive(Default)]
struct Registry {
    entries: Mutex<HashMap<Uuid, Entry>>,
    acquisition: tokio::sync::Mutex<()>,
    stop: CancellationToken,
}
struct Entry {
    identity: String,
    controller: ForwardController,
    stop: CancellationToken,
    task: JoinHandle<()>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl ForwardRegistry {
    pub(in crate::session::ssh) async fn acquire(
        &self,
        id: Uuid,
        mut options: SshOptions,
    ) -> ForwardController {
        // Never retain the registry in the actor's options: that would form a
        // cycle and keep listeners alive after their workspace is destroyed.
        options.forwarding_registry = None;
        let _acquisition = self.0.acquisition.lock().await;
        let identity = format!(
            "{}\0{}\0{}\0{:?}",
            options.host, options.port, options.username, options.jump_hosts
        );
        let old = {
            let mut entries = self.0.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = entries.get(&id)
                && entry.identity == identity
                && !entry.task.is_finished()
            {
                // New terminal authentication can refresh credentials without
                // interrupting established forwarded connections.
                let _ = entry
                    .controller
                    .control
                    .try_send(Control::Credentials(Box::new(options)));
                return entry.controller.clone();
            }
            entries.remove(&id)
        };
        if let Some(mut entry) = old {
            entry.stop.cancel();
            if tokio::time::timeout(Duration::from_secs(5), &mut entry.task)
                .await
                .is_err()
            {
                entry.task.abort();
                let _ = (&mut entry.task).await;
            }
        }
        let stop = self.0.stop.child_token();
        let (controller, task) = start(options, stop.clone());
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                Entry {
                    identity,
                    controller: controller.clone(),
                    stop,
                    task,
                },
            );
        controller
    }

    pub fn get(&self, id: Uuid) -> Option<ForwardController> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .map(|e| e.controller.clone())
    }
    pub fn matches_target(&self, id: Uuid, config: &crate::config::SshConfig) -> bool {
        let identity = format!(
            "{}\0{}\0{}\0{:?}",
            config.host, config.port, config.username, config.jump_hosts
        );
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .is_some_and(|entry| entry.identity == identity)
    }
    pub fn connections(&self) -> Vec<(Uuid, ForwardController)> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, e)| (*id, e.controller.clone()))
            .collect()
    }
    pub fn stop_all(&self) {
        self.0.stop.cancel();
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn remove(&self, id: Uuid) {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

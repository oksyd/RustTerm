use crate::i18n::messages as i18n_key;

use super::{
    SshOptions,
    broker::{self, TcpRequest},
    connection::{HostVerifier, Ui, connect_authenticated},
};
use crate::config::{ForwardKind, PortForward};
use crate::session::{SendError, transport::IO_TIMEOUT};
use russh::client;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardStatus {
    pub rule: PortForward,
    pub state: ForwardState,
}
#[derive(Clone)]
pub struct ForwardController {
    control: mpsc::Sender<Control>,
    states: Arc<Mutex<Vec<ForwardStatus>>>,
    remote: Arc<Mutex<BTreeMap<(String, u16), RemoteRoute>>>,
    slots: Arc<Semaphore>,
}
enum Control {
    SetEnabled(usize, bool),
    Retry(usize),
}
#[derive(Clone)]
struct RemoteRoute {
    rule: PortForward,
    stop: CancellationToken,
}
pub(super) enum ForwardRequest {
    Open(TcpRequest),
    Remote {
        rule: PortForward,
        enabled: bool,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
}
impl ForwardController {
    pub fn statuses(&self) -> Vec<ForwardStatus> {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn set_enabled(&self, index: usize, enabled: bool) -> std::result::Result<(), SendError> {
        self.send(index, Control::SetEnabled(index, enabled))
    }
    pub fn retry(&self, index: usize) -> std::result::Result<(), SendError> {
        self.send(index, Control::Retry(index))
    }
    fn send(&self, index: usize, control: Control) -> std::result::Result<(), SendError> {
        if index >= self.states.lock().unwrap_or_else(|e| e.into_inner()).len() {
            return Err(SendError::Closed);
        }
        self.control.try_send(control).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => SendError::Busy,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }
    pub(super) fn incoming(
        &self,
        channel: russh::Channel<client::Msg>,
        address: &str,
        port: u32,
        reply: client::ChannelOpenHandle,
    ) {
        let Ok(port) = u16::try_from(port) else {
            return;
        };
        let route = self
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(address.to_owned(), port))
            .cloned();
        let Some(route) = route else { return };
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            return;
        };
        tokio::spawn(async move {
            let _slot = slot;
            let target = tokio::select! {
                _ = route.stop.cancelled() => return,
                result = tokio::time::timeout(
                    Duration::from_secs(10),
                    TcpStream::connect((
                        route.rule.destination_host.as_str(),
                        route.rule.destination_port
                    ))
                ) => match result {
                    Ok(Ok(stream)) => stream,
                    _ => return
                }
            };
            if route.stop.is_cancelled() {
                return;
            }
            tokio::select! {
                biased;
                _ = route.stop.cancelled() => return,
                _ = reply.accept() => {},
            }
            let mut remote = channel.into_stream();
            let mut target = target;
            tokio::select! {
                _ = route.stop.cancelled() => {},
                _ = tokio::io::copy_bidirectional(&mut remote, &mut target) => {}
            }
        });
    }
}

enum Activation {
    Remote,
    Local(TcpListener),
}

#[derive(Default)]
enum RulePhase {
    #[default]
    Stopped,
    Starting(CancellationToken),
    Running(CancellationToken),
    Stopping(CancellationToken),
    Failed,
}

impl RulePhase {
    fn cancel(&self) {
        if let Self::Starting(token) | Self::Running(token) | Self::Stopping(token) = self {
            token.cancel();
        }
    }
}

struct RuleRuntime {
    desired: bool,
    generation: u64,
    phase: RulePhase,
}

struct ForwardRuntime {
    rules: Vec<RuleRuntime>,
    activations: JoinSet<(usize, u64, Result<Activation, String>)>,
    deactivations: JoinSet<(usize, u64, Result<(), String>)>,
    listeners: JoinSet<(usize, u64, Option<String>)>,
}

impl ForwardRuntime {
    fn new(rules: &[PortForward]) -> Self {
        Self {
            rules: rules
                .iter()
                .map(|rule| RuleRuntime {
                    desired: rule.enabled,
                    generation: 0,
                    phase: RulePhase::Stopped,
                })
                .collect(),
            activations: JoinSet::new(),
            deactivations: JoinSet::new(),
            listeners: JoinSet::new(),
        }
    }

    fn enabled(&self) -> bool {
        self.rules.iter().any(|rule| rule.desired)
    }

    fn control(&mut self, control: Control) -> Option<(usize, bool)> {
        let (Control::SetEnabled(index, _) | Control::Retry(index)) = control;
        let rule = self.rules.get_mut(index)?;
        if let Control::SetEnabled(_, enabled) = control {
            rule.desired = enabled;
        }
        Some((index, rule.desired))
    }

    fn finish_start(
        &mut self,
        index: usize,
        generation: u64,
        result: Result<Activation, String>,
        rule: &PortForward,
        controller: &ForwardController,
        connection: &ForwardConnection,
    ) {
        if self.rules[index].generation != generation {
            return;
        }
        let phase = std::mem::take(&mut self.rules[index].phase);
        match phase {
            RulePhase::Stopping(token) if rule.kind != ForwardKind::Remote => {
                token.cancel();
                // A completed bind still owns its port until the result is dropped.
                drop(result);
                if self.rules[index].desired {
                    activate(index, rule, controller, connection, self);
                } else {
                    set_state(controller, index, ForwardState::Stopped);
                }
            }
            RulePhase::Starting(token) | RulePhase::Stopping(token) => {
                match result {
                    Ok(Activation::Remote) if token.is_cancelled() => {
                        // A pending enable must be acknowledged before its disable.
                        // Dropping that reply would hand cleanup to the broker and
                        // make our cancel race its reserved endpoint.
                        self.rules[index].phase = RulePhase::Stopping(token);
                        stop_remote(index, rule, connection, self);
                    }
                    Ok(Activation::Remote) => {
                        self.rules[index].phase = RulePhase::Running(token);
                        set_state(controller, index, ForwardState::Running);
                    }
                    Ok(Activation::Local(listener)) => {
                        self.rules[index].phase = RulePhase::Running(token);
                        listen(
                            index, generation, listener, rule, controller, connection, self,
                        );
                        set_state(controller, index, ForwardState::Running);
                    }
                    Err(error) => {
                        token.cancel();
                        if rule.kind == ForwardKind::Remote {
                            controller
                                .remote
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&(rule.bind_host.clone(), rule.bind_port));
                        }
                        self.rules[index].phase = RulePhase::Failed;
                        set_state(controller, index, ForwardState::Failed(error));
                    }
                }
            }
            phase => self.rules[index].phase = phase,
        }
    }

    fn finish_stop(
        &mut self,
        index: usize,
        generation: u64,
        result: Result<(), String>,
        rule: &PortForward,
        controller: &ForwardController,
        connection: &ForwardConnection,
    ) {
        if self.rules[index].generation != generation {
            return;
        }
        std::mem::take(&mut self.rules[index].phase).cancel();
        match result {
            Ok(()) if self.rules[index].desired => {
                activate(index, rule, controller, connection, self)
            }
            Ok(()) => set_state(controller, index, ForwardState::Stopped),
            Err(error) => {
                self.rules[index].phase = RulePhase::Failed;
                set_state(controller, index, ForwardState::Failed(error));
            }
        }
    }

    fn finish_listener(
        &mut self,
        index: usize,
        generation: u64,
        error: Option<String>,
        rule: &PortForward,
        controller: &ForwardController,
        connection: &ForwardConnection,
    ) {
        if self.rules[index].generation != generation {
            return;
        }
        let phase = std::mem::take(&mut self.rules[index].phase);
        phase.cancel();
        if matches!(phase, RulePhase::Stopping(_)) && self.rules[index].desired {
            activate(index, rule, controller, connection, self);
        } else if let Some(error) = error {
            self.rules[index].phase = RulePhase::Failed;
            set_state(controller, index, ForwardState::Failed(error));
        } else {
            set_state(controller, index, ForwardState::Stopped);
        }
    }

    async fn stop_all(&mut self, controller: &ForwardController) {
        for rule in &mut self.rules {
            rule.generation = rule.generation.wrapping_add(1);
            std::mem::take(&mut rule.phase).cancel();
        }
        controller
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.activations.shutdown().await;
        self.deactivations.shutdown().await;
        self.listeners.shutdown().await;
    }
}

struct ForwardConnection {
    stop: CancellationToken,
    closed: CancellationToken,
    requests: mpsc::Sender<ForwardRequest>,
    task: JoinHandle<()>,
}

impl ForwardConnection {
    fn new(
        mut handle: client::Handle<HostVerifier>,
        stop: CancellationToken,
        closed: CancellationToken,
        timeout: Duration,
    ) -> Self {
        let (requests, receiver) = mpsc::channel(64);
        let task_stop = stop.clone();
        let task_closed = closed.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = task_stop.cancelled() => {},
                _ = broker::run_forwarding(&handle, receiver, &task_closed, timeout) => {},
            }
            task_stop.cancel();
            task_closed.cancel();
            let _ = tokio::time::timeout(IO_TIMEOUT, &mut handle).await;
        });
        Self {
            stop,
            closed,
            requests,
            task,
        }
    }

    async fn close(mut self) {
        self.stop.cancel();
        self.closed.cancel();
        let _ = tokio::time::timeout(IO_TIMEOUT, &mut self.task).await;
    }
}

impl Drop for ForwardConnection {
    fn drop(&mut self) {
        self.stop.cancel();
        self.closed.cancel();
        self.task.abort();
    }
}

pub(super) fn start(
    mut options: SshOptions,
    stop: CancellationToken,
) -> (ForwardController, JoinHandle<()>) {
    let rules = std::mem::take(&mut options.forwards);
    let (control, mut controls) = mpsc::channel(32);
    let controller = ForwardController {
        control,
        states: Arc::new(Mutex::new(
            rules
                .iter()
                .cloned()
                .map(|rule| ForwardStatus {
                    rule,
                    state: ForwardState::Stopped,
                })
                .collect(),
        )),
        remote: Arc::new(Mutex::new(BTreeMap::new())),
        slots: Arc::new(Semaphore::new(64)),
    };
    let active = controller.clone();
    let task = tokio::spawn(async move {
        if rules.is_empty() {
            return;
        }
        let timeout = Duration::from_secs(options.timeout_secs.clamp(1, 600));
        let mut runtime = ForwardRuntime::new(&rules);
        // Retry only at startup or on an explicit Start, never because desired
        // remains true after an authentication/transport failure.
        let mut connect_requested = runtime.enabled();
        loop {
            if stop.is_cancelled() {
                break;
            }
            if !connect_requested || !runtime.enabled() {
                let event = tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    event = controls.recv() => event,
                };
                let Some(event) = event else { break };
                let Some((index, enabled)) = runtime.control(event) else {
                    continue;
                };
                if enabled {
                    connect_requested = true;
                } else {
                    set_state(&active, index, ForwardState::Stopped);
                }
                continue;
            }
            connect_requested = false;
            let connection_stop = stop.child_token();
            let closed = CancellationToken::new();
            for (index, state) in runtime.rules.iter().enumerate() {
                if state.desired {
                    set_state(&active, index, ForwardState::Starting);
                }
            }
            let handle = {
                let connecting = connect_authenticated(
                    options.clone(),
                    Ui::Forwarding {
                        controller: active.clone(),
                        closed: closed.clone(),
                    },
                    connection_stop.clone(),
                );
                tokio::pin!(connecting);
                loop {
                    tokio::select! {
                        biased;
                        _ = stop.cancelled() => break None,
                        event = controls.recv() => {
                            let Some(event) = event else { break None };
                            let Some((index, enabled)) = runtime.control(event) else { continue };
                            set_state(&active, index, if enabled {
                                ForwardState::Starting
                            } else {
                                ForwardState::Stopped
                            });
                            if !runtime.enabled() {
                                break None;
                            }
                        }
                        result = &mut connecting => match result {
                            Ok(handle) => break Some(handle),
                            Err((_, error)) => {
                                for (index, state) in runtime.rules.iter().enumerate() {
                                    if state.desired {
                                        set_state(&active, index, ForwardState::Failed(error.clone()));
                                    }
                                }
                                break None;
                            }
                        },
                    }
                }
            };
            let Some(handle) = handle else {
                connection_stop.cancel();
                continue;
            };
            let connection = ForwardConnection::new(handle, connection_stop, closed, timeout);
            for (index, rule) in rules.iter().enumerate() {
                if runtime.rules[index].desired {
                    activate(index, rule, &active, &connection, &mut runtime);
                }
            }
            let failure = loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break None,
                    _ = connection.closed.cancelled() => {
                        break Some(crate::i18n::tr(i18n_key::CONNECTIONS_THE_SSH_FORWARDING_CONNECTION_CLOSED).to_owned());
                    }
                    event = controls.recv() => {
                        let Some(event) = event else { break None };
                        let Some((index, enabled)) = runtime.control(event) else { continue };
                        let rule = &rules[index];
                        if !runtime.enabled() {
                            set_state(&active, index, ForwardState::Stopping);
                            break None;
                        }
                        if enabled {
                            if matches!(runtime.rules[index].phase, RulePhase::Stopped | RulePhase::Failed) {
                                activate(index, rule, &active, &connection, &mut runtime);
                            } else if matches!(runtime.rules[index].phase, RulePhase::Stopping(_)) {
                                set_state(&active, index, ForwardState::Starting);
                            }
                        } else {
                            disable(index, rule, &active, &connection, &mut runtime);
                        }
                    }
                    result = runtime.activations.join_next(), if !runtime.activations.is_empty() => {
                        match result {
                            Some(Ok((index, generation, result))) => {
                                runtime.finish_start(index, generation, result, &rules[index], &active, &connection);
                            }
                            Some(Err(error)) => break Some(error.to_string()),
                            None => {},
                        }
                    }
                    result = runtime.deactivations.join_next(), if !runtime.deactivations.is_empty() => {
                        match result {
                            Some(Ok((index, generation, result))) => {
                                runtime.finish_stop(index, generation, result, &rules[index], &active, &connection);
                            }
                            Some(Err(error)) => break Some(error.to_string()),
                            None => {},
                        }
                    }
                    result = runtime.listeners.join_next(), if !runtime.listeners.is_empty() => {
                        match result {
                            Some(Ok((index, generation, error))) => {
                                runtime.finish_listener(index, generation, error, &rules[index], &active, &connection);
                            }
                            Some(Err(error)) => break Some(error.to_string()),
                            None => {},
                        }
                    }
                }
            };
            connection.stop.cancel();
            runtime.stop_all(&active).await;
            connection.close().await;
            for (index, state) in runtime.rules.iter().enumerate() {
                set_state(
                    &active,
                    index,
                    match (&failure, state.desired) {
                        (Some(error), true) => ForwardState::Failed(error.clone()),
                        _ => ForwardState::Stopped,
                    },
                );
            }
        }
        runtime.stop_all(&active).await;
        for index in 0..rules.len() {
            set_state(&active, index, ForwardState::Stopped);
        }
    });
    (controller, task)
}

fn set_state(controller: &ForwardController, index: usize, state: ForwardState) {
    if let Some(status) = controller
        .states
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(index)
    {
        status.state = state;
    }
}

fn activate(
    index: usize,
    rule: &PortForward,
    controller: &ForwardController,
    connection: &ForwardConnection,
    runtime: &mut ForwardRuntime,
) {
    runtime.rules[index].generation = runtime.rules[index].generation.wrapping_add(1);
    let generation = runtime.rules[index].generation;
    set_state(controller, index, ForwardState::Starting);
    let token = connection.stop.child_token();
    runtime.rules[index].phase = RulePhase::Starting(token.clone());
    if rule.kind == ForwardKind::Remote {
        controller
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (rule.bind_host.clone(), rule.bind_port),
                RemoteRoute {
                    rule: rule.clone(),
                    stop: token.clone(),
                },
            );
    }
    let rule = rule.clone();
    let requests = connection.requests.clone();
    let connection_stop = connection.stop.clone();
    runtime.activations.spawn(async move {
        let result = if rule.kind == ForwardKind::Remote {
            remote_request(&requests, rule, true, &connection_stop).await.map(|()| Activation::Remote)
        } else {
            tokio::select! {
                biased;
                _ = token.cancelled() => Err(crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into()),
                result = tokio::time::timeout(Duration::from_secs(10), TcpListener::bind((rule.bind_host.as_str(), rule.bind_port))) => match result {
                    Ok(Ok(listener)) => Ok(Activation::Local(listener)),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(error.to_string()),
                },
            }
        };
        (index, generation, result)
    });
}

fn disable(
    index: usize,
    rule: &PortForward,
    controller: &ForwardController,
    connection: &ForwardConnection,
    runtime: &mut ForwardRuntime,
) {
    if rule.kind == ForwardKind::Remote {
        controller
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(rule.bind_host.clone(), rule.bind_port));
    }
    let phase = std::mem::take(&mut runtime.rules[index].phase);
    phase.cancel();
    match phase {
        RulePhase::Starting(token) | RulePhase::Stopping(token) => {
            // Activation/listener/disable completion owns the next transition.
            runtime.rules[index].phase = RulePhase::Stopping(token);
        }
        RulePhase::Running(token) => {
            runtime.rules[index].phase = RulePhase::Stopping(token);
            if rule.kind == ForwardKind::Remote {
                stop_remote(index, rule, connection, runtime);
            }
        }
        RulePhase::Failed if rule.kind == ForwardKind::Remote => {
            let token = connection.stop.child_token();
            token.cancel();
            runtime.rules[index].phase = RulePhase::Stopping(token);
            stop_remote(index, rule, connection, runtime);
        }
        RulePhase::Stopped | RulePhase::Failed => {
            set_state(controller, index, ForwardState::Stopped);
        }
    }
    if matches!(runtime.rules[index].phase, RulePhase::Stopping(_)) {
        set_state(
            controller,
            index,
            if runtime.rules[index].desired {
                ForwardState::Starting
            } else {
                ForwardState::Stopping
            },
        );
    }
}

fn stop_remote(
    index: usize,
    rule: &PortForward,
    connection: &ForwardConnection,
    runtime: &mut ForwardRuntime,
) {
    let generation = runtime.rules[index].generation;
    let requests = connection.requests.clone();
    let stop = connection.stop.clone();
    let rule = rule.clone();
    runtime.deactivations.spawn(async move {
        let result = remote_request(&requests, rule, false, &stop).await;
        (index, generation, result)
    });
}

fn listen(
    index: usize,
    generation: u64,
    listener: TcpListener,
    rule: &PortForward,
    controller: &ForwardController,
    connection: &ForwardConnection,
    runtime: &mut ForwardRuntime,
) {
    let RulePhase::Running(token) = &runtime.rules[index].phase else {
        unreachable!("only running local rules own listeners");
    };
    let token = token.clone();
    let rule = rule.clone();
    let requests = connection.requests.clone();
    let slots = controller.slots.clone();
    runtime.listeners.spawn(async move {
        let mut clients = JoinSet::new();
        let error = loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => break None,
                _ = clients.join_next(), if !clients.is_empty() => {},
                incoming = listener.accept() => match incoming {
                    Ok((stream, _)) => {
                        let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
                        let request = requests.clone();
                        let rule = rule.clone();
                        let stop = token.clone();
                        clients.spawn(async move {
                            let _slot = slot;
                            let _ = forward_client(stream, rule, request, stop).await;
                        });
                    }
                    Err(error) => break Some(error.to_string()),
                }
            }
        };
        clients.shutdown().await;
        (index, generation, error)
    });
}
async fn remote_request(
    requests: &mpsc::Sender<ForwardRequest>,
    rule: PortForward,
    enabled: bool,
    stop: &CancellationToken,
) -> std::result::Result<(), String> {
    let (reply, result) = oneshot::channel();
    tokio::select! {
        _ = stop.cancelled() => Err(crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into()),
        result = async {
            requests
                .send(ForwardRequest::Remote { rule, enabled, reply })
                .await
                .map_err(|e| e.to_string())?;
            result.await.map_err(|e| e.to_string())?
        } => result
    }
}
async fn open_channel(
    requests: &mpsc::Sender<ForwardRequest>,
    host: String,
    port: u16,
    stop: &CancellationToken,
) -> std::result::Result<russh::ChannelStream<client::Msg>, String> {
    let (reply, response) = oneshot::channel();
    tokio::select! {
        _ = stop.cancelled() => Err(crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into()),
        result = async {
            requests
                .send(ForwardRequest::Open(TcpRequest { host, port, reply }))
                .await
                .map_err(|e| e.to_string())?;
            response.await.map_err(|e| e.to_string())?.map_err(|(_, message)| message)
        } => result
    }
}
async fn forward_client(
    mut local: TcpStream,
    rule: PortForward,
    requests: mpsc::Sender<ForwardRequest>,
    stop: CancellationToken,
) -> std::result::Result<(), String> {
    let destination = if rule.kind == ForwardKind::Dynamic {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            result = tokio::time::timeout(
                Duration::from_secs(10),
                socks_request(&mut local)
            ) => result.map_err(|e| e.to_string())??
        }
    } else {
        (rule.destination_host, rule.destination_port)
    };
    let remote = open_channel(&requests, destination.0, destination.1, &stop).await;
    if rule.kind == ForwardKind::Dynamic {
        let code = if remote.is_ok() { 0 } else { 5 };
        let reply = [5, code, 0, 1, 0, 0, 0, 0, 0, 0];
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            result = tokio::time::timeout(
                Duration::from_secs(2),
                local.write_all(&reply)
            ) => {
                result.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
            }
        }
    }
    let mut remote = remote?;
    tokio::select! {
        _ = stop.cancelled() => Ok(()),
        result = tokio::io::copy_bidirectional(&mut local, &mut remote) =>
            result.map(|_| ()).map_err(|e| e.to_string())
    }
}
async fn socks_request(stream: &mut TcpStream) -> std::result::Result<(String, u16), String> {
    let mut greeting = [0; 2];
    stream
        .read_exact(&mut greeting)
        .await
        .map_err(|e| e.to_string())?;
    if greeting[0] != 5 || greeting[1] == 0 {
        return Err(crate::i18n::tr(i18n_key::CONNECTIONS_UNSUPPORTED_SOCKS_REQUEST).into());
    }
    let mut methods = vec![0; usize::from(greeting[1])];
    stream
        .read_exact(&mut methods)
        .await
        .map_err(|e| e.to_string())?;
    if !methods.contains(&0) {
        let _ = stream.write_all(&[5, 255]).await;
        return Err(crate::i18n::tr(
            i18n_key::CONNECTIONS_SOCKS_AUTHENTICATION_METHODS_ARE_NOT_SUPPORTED,
        )
        .into());
    }
    stream.write_all(&[5, 0]).await.map_err(|e| e.to_string())?;
    let mut request = [0; 4];
    stream
        .read_exact(&mut request)
        .await
        .map_err(|e| e.to_string())?;
    if request[0] != 5 || request[1] != 1 || request[2] != 0 {
        let _ = stream.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await;
        return Err(crate::i18n::tr(i18n_key::CONNECTIONS_ONLY_SOCKS5_CONNECT_IS_SUPPORTED).into());
    }
    let host = match request[3] {
        1 => {
            let mut ip = [0; 4];
            stream
                .read_exact(&mut ip)
                .await
                .map_err(|e| e.to_string())?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        4 => {
            let mut ip = [0; 16];
            stream
                .read_exact(&mut ip)
                .await
                .map_err(|e| e.to_string())?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        3 => {
            let length = stream.read_u8().await.map_err(|e| e.to_string())?;
            let mut host = vec![0; usize::from(length)];
            stream
                .read_exact(&mut host)
                .await
                .map_err(|e| e.to_string())?;
            String::from_utf8(host).map_err(|e| e.to_string())?
        }
        _ => {
            let _ = stream.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            return Err(
                crate::i18n::tr(i18n_key::CONNECTIONS_UNSUPPORTED_SOCKS_ADDRESS_TYPE).into(),
            );
        }
    };
    let host = crate::config::known_hosts::normalize_host(&host).map_err(|e| e.to_string())?;
    let port = stream.read_u16().await.map_err(|e| e.to_string())?;
    if port == 0 {
        return Err(crate::i18n::tr(i18n_key::CONNECTIONS_INVALID_SSH_ENDPOINT).into());
    }
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(kind: ForwardKind) -> PortForward {
        PortForward {
            kind,
            bind_host: "127.0.0.1".into(),
            bind_port: 2222,
            destination_host: "127.0.0.1".into(),
            destination_port: 22,
            enabled: true,
        }
    }

    fn options(port: u16, rules: Vec<PortForward>) -> SshOptions {
        SshOptions {
            host: "127.0.0.1".into(),
            port,
            username: "test".into(),
            auth: super::super::SshAuth::Password("test".into()),
            known_hosts_path: std::path::PathBuf::new(),
            timeout_secs: 1,
            keepalive_secs: 0,
            keepalive_max: 1,
            shell_integration: false,
            jump_hosts: Vec::new(),
            forwards: rules,
            authentication: None,
        }
    }

    fn fixture(
        rules: &[PortForward],
    ) -> (
        ForwardController,
        ForwardRuntime,
        ForwardConnection,
        mpsc::Receiver<ForwardRequest>,
    ) {
        let (control, _) = mpsc::channel(1);
        let controller = ForwardController {
            control,
            states: Arc::new(Mutex::new(
                rules
                    .iter()
                    .cloned()
                    .map(|rule| ForwardStatus {
                        rule,
                        state: ForwardState::Stopped,
                    })
                    .collect(),
            )),
            remote: Arc::new(Mutex::new(BTreeMap::new())),
            slots: Arc::new(Semaphore::new(64)),
        };
        let (requests, receiver) = mpsc::channel(8);
        let stop = CancellationToken::new();
        let waiting = stop.clone();
        let connection = ForwardConnection {
            stop,
            closed: CancellationToken::new(),
            requests,
            task: tokio::spawn(async move { waiting.cancelled().await }),
        };
        (controller, ForwardRuntime::new(rules), connection, receiver)
    }

    #[tokio::test]
    async fn failed_remote_stop_can_be_retried_without_a_live_route() {
        tokio::time::timeout(Duration::from_secs(1), async {
            let rule = rule(ForwardKind::Remote);
            let (mut controller, mut runtime, connection, mut requests) =
                fixture(std::slice::from_ref(&rule));
            let (controls, mut control_events) = mpsc::channel(2);
            controller.control = controls;
            runtime.rules[0].phase = RulePhase::Failed;
            for (attempt, response) in [Err("temporary failure".to_owned()), Ok(())]
                .into_iter()
                .enumerate()
            {
                if attempt == 0 {
                    controller.set_enabled(0, false).unwrap();
                } else {
                    controller.retry(0).unwrap();
                }
                assert_eq!(
                    runtime.control(control_events.recv().await.unwrap()),
                    Some((0, false))
                );
                disable(0, &rule, &controller, &connection, &mut runtime);
                let Some(ForwardRequest::Remote {
                    enabled: false,
                    reply,
                    ..
                }) = requests.recv().await
                else {
                    panic!("listener cancellation was not requested");
                };
                let expected = match &response {
                    Ok(()) => ForwardState::Stopped,
                    Err(error) => ForwardState::Failed(error.clone()),
                };
                reply.send(response).unwrap();
                let (index, generation, result) =
                    runtime.deactivations.join_next().await.unwrap().unwrap();
                runtime.finish_stop(index, generation, result, &rule, &controller, &connection);
                assert_eq!(controller.statuses()[0].state, expected);
                assert!(controller.remote.lock().unwrap().is_empty());
            }
            connection.close().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn remote_start_stop_serializes_acknowledgements_before_restarting() {
        tokio::time::timeout(Duration::from_secs(1), async {
            for rejected in [false, true] {
                let remote = rule(ForwardKind::Remote);
                let background = rule(ForwardKind::Local);
                let (controller, mut runtime, connection, mut requests) =
                    fixture(&[remote.clone(), background]);
                activate(0, &remote, &controller, &connection, &mut runtime);
                let Some(ForwardRequest::Remote {
                    enabled: true,
                    reply: enable,
                    ..
                }) = requests.recv().await
                else {
                    panic!("remote enable was not requested");
                };
                runtime.control(Control::SetEnabled(0, false));
                assert!(
                    runtime.enabled(),
                    "the background rule keeps the auxiliary connection"
                );
                disable(0, &remote, &controller, &connection, &mut runtime);
                assert_eq!(controller.statuses()[0].state, ForwardState::Stopping);
                assert!(controller.remote.lock().unwrap().is_empty());
                assert!(
                    !enable.is_closed(),
                    "Stop must retain the outstanding enable reply"
                );
                assert!(runtime.deactivations.is_empty());
                assert!(
                    requests.try_recv().is_err(),
                    "cancel must wait for enable acknowledgement"
                );

                if rejected {
                    enable.send(Err("enable rejected".into())).unwrap();
                    let (index, generation, result) =
                        runtime.activations.join_next().await.unwrap().unwrap();
                    runtime.finish_start(
                        index,
                        generation,
                        result,
                        &remote,
                        &controller,
                        &connection,
                    );
                    assert_eq!(
                        controller.statuses()[0].state,
                        ForwardState::Failed("enable rejected".into())
                    );
                    assert!(!runtime.rules[0].desired);
                    assert!(runtime.deactivations.is_empty());
                    connection.close().await;
                    continue;
                }

                runtime.control(Control::SetEnabled(0, true));
                assert!(matches!(runtime.rules[0].phase, RulePhase::Stopping(_)));
                enable.send(Ok(())).unwrap();
                let (index, generation, result) =
                    runtime.activations.join_next().await.unwrap().unwrap();
                runtime.finish_start(index, generation, result, &remote, &controller, &connection);
                let Some(ForwardRequest::Remote {
                    enabled: false,
                    reply: disable,
                    ..
                }) = requests.recv().await
                else {
                    panic!("acknowledged enable was not cancelled before restart");
                };
                assert!(runtime.activations.is_empty());
                assert!(requests.try_recv().is_err());
                disable.send(Ok(())).unwrap();
                let (index, generation, result) =
                    runtime.deactivations.join_next().await.unwrap().unwrap();
                runtime.finish_stop(index, generation, result, &remote, &controller, &connection);
                let Some(ForwardRequest::Remote {
                    enabled: true,
                    reply: restart,
                    ..
                }) = requests.recv().await
                else {
                    panic!("the latest Start intent was lost");
                };
                restart.send(Ok(())).unwrap();
                let (index, generation, result) =
                    runtime.activations.join_next().await.unwrap().unwrap();
                runtime.finish_start(index, generation, result, &remote, &controller, &connection);
                assert_eq!(controller.statuses()[0].state, ForwardState::Running);
                assert_eq!(controller.remote.lock().unwrap().len(), 1);
                runtime.stop_all(&controller).await;
                connection.close().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stopping_local_rule_preserves_remote_route_at_same_endpoint() {
        for kind in [ForwardKind::Local, ForwardKind::Dynamic] {
            let local = rule(kind);
            let remote = rule(ForwardKind::Remote);
            let (controller, mut runtime, connection, _) =
                fixture(&[local.clone(), remote.clone()]);
            let route_stop = connection.stop.child_token();
            controller.remote.lock().unwrap().insert(
                (remote.bind_host.clone(), remote.bind_port),
                RemoteRoute {
                    rule: remote,
                    stop: route_stop.clone(),
                },
            );
            runtime.rules[0].phase = RulePhase::Running(connection.stop.child_token());
            disable(0, &local, &controller, &connection, &mut runtime);
            assert_eq!(controller.remote.lock().unwrap().len(), 1);
            assert!(!route_stop.is_cancelled());
            assert!(
                matches!(&runtime.rules[0].phase, RulePhase::Stopping(token) if token.is_cancelled())
            );
            connection.close().await;
        }
    }

    #[tokio::test]
    async fn forwarding_connects_only_on_start_and_can_cancel_or_retry_authentication() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (empty, worker) = start(options(22, Vec::new()), CancellationToken::new());
            worker.await.unwrap();
            assert!(empty.statuses().is_empty());

            let server = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = server.local_addr().unwrap().port();
            let mut rule = rule(ForwardKind::Remote);
            rule.enabled = false;
            let stop = CancellationToken::new();
            let (controller, worker) = start(options(port, vec![rule]), stop.clone());
            assert!(
                tokio::time::timeout(Duration::from_millis(20), server.accept())
                    .await
                    .is_err()
            );
            for _ in 0..3 {
                controller.set_enabled(0, true).unwrap();
            }
            let (mut first, _) = server.accept().await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(20), server.accept())
                    .await
                    .is_err()
            );
            controller.set_enabled(0, false).unwrap();
            tokio::io::copy(&mut first, &mut tokio::io::sink())
                .await
                .unwrap();
            assert_eq!(controller.statuses()[0].state, ForwardState::Stopped);

            controller.set_enabled(0, true).unwrap();
            let (second, _) = server.accept().await.unwrap();
            drop(second);
            while !matches!(controller.statuses()[0].state, ForwardState::Failed(_)) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(20), server.accept())
                    .await
                    .is_err()
            );
            controller.set_enabled(0, true).unwrap();
            let (mut third, _) = server.accept().await.unwrap();
            stop.cancel();
            tokio::io::copy(&mut third, &mut tokio::io::sink())
                .await
                .unwrap();
            worker.await.unwrap();
            assert_eq!(controller.statuses()[0].state, ForwardState::Stopped);
        })
        .await
        .unwrap();
    }
}

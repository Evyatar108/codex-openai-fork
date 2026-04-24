mod client_tracker;
mod enroll;
mod protocol;
mod websocket;

use crate::transport::remote_control::websocket::RemoteControlWebsocket;
use crate::transport::remote_control::websocket::RemoteControlWebsocketOptions;

pub use self::protocol::ClientId;
use self::protocol::RemoteControlTarget;
use self::protocol::ServerEvent;
use self::protocol::StreamId;
#[allow(unused_imports)] // SANDBOX PATCH: unused after force-disable of remote_control
use self::protocol::normalize_remote_control_url;
use super::CHANNEL_CAPACITY;
use super::TransportEvent;
use super::next_connection_id;
use codex_login::AuthManager;
use codex_state::StateRuntime;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(super) struct QueuedServerEnvelope {
    pub(super) event: ServerEvent,
    pub(super) client_id: ClientId,
    pub(super) stream_id: StreamId,
    pub(super) write_complete_tx: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub(crate) struct RemoteControlHandle {
    enabled_tx: Arc<watch::Sender<bool>>,
}

impl RemoteControlHandle {
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled_tx.send_if_modified(|state| {
            let changed = *state != enabled;
            *state = enabled;
            changed
        });
    }
}

pub(crate) struct RemoteControlStartOptions {
    pub(crate) remote_control_url: String,
    pub(crate) state_db: Option<Arc<StateRuntime>>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) transport_event_tx: mpsc::Sender<TransportEvent>,
    pub(crate) shutdown_token: CancellationToken,
    pub(crate) app_server_client_name_rx: Option<oneshot::Receiver<String>>,
    pub(crate) initial_enabled: bool,
}

#[cfg(test)]
pub(crate) async fn start_remote_control(
    remote_control_url: String,
    state_db: Option<Arc<StateRuntime>>,
    auth_manager: Arc<AuthManager>,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
    app_server_client_name_rx: Option<oneshot::Receiver<String>>,
    initial_enabled: bool,
) -> io::Result<(JoinHandle<()>, RemoteControlHandle)> {
    start_remote_control_with_options(RemoteControlStartOptions {
        remote_control_url,
        state_db,
        auth_manager,
        transport_event_tx,
        shutdown_token,
        app_server_client_name_rx,
        initial_enabled,
    })
    .await
}

pub(crate) async fn start_remote_control_with_options(
    options: RemoteControlStartOptions,
) -> io::Result<(JoinHandle<()>, RemoteControlHandle)> {
    let RemoteControlStartOptions {
        remote_control_url,
        state_db,
        auth_manager,
        transport_event_tx,
        shutdown_token,
        app_server_client_name_rx,
        initial_enabled,
    } = options;
    // SANDBOX PATCH: remote_control is ChatGPT-only (protocol::is_allowed_chatgpt_host
    // restricts the endpoint to chatgpt.com / chatgpt-staging.com / localhost, and the
    // enroll + websocket paths attach ChatGPT-specific auth). Copilot sessions have no
    // ChatGPT OAuth, so enabling this here would leak the `chatgpt-account-id` header
    // to chatgpt.com and/or fail silently. Force-disable regardless of the
    // `features.remote_control` flag or the `initial_enabled` argument: set target to
    // None unconditionally so the websocket stays in the "disabled" branch.
    let _ = initial_enabled;
    let initial_enabled = false;
    let remote_control_target: Option<RemoteControlTarget> = None;
    let (enabled_tx, enabled_rx) = watch::channel(initial_enabled);
    let join_handle = tokio::spawn(async move {
        RemoteControlWebsocket::from_options(RemoteControlWebsocketOptions {
            remote_control_url,
            remote_control_target,
            state_db,
            auth_manager,
            transport_event_tx,
            shutdown_token,
            enabled_rx,
        })
        .run(app_server_client_name_rx)
        .await;
    });

    Ok((
        join_handle,
        RemoteControlHandle {
            enabled_tx: Arc::new(enabled_tx),
        },
    ))
}

#[cfg(test)]
mod tests;

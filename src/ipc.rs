// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::cell::RefCell;
use std::ffi::c_char;
use std::rc::Rc;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use crossbeam_channel::{Sender as ConfigJobSender, TrySendError, bounded};
use serde::Serialize;
use tracing::{error, info, trace};

pub mod cli_exec;
pub mod protocol;
pub mod subscriptions;

#[cfg(test)]
mod tests;

pub use protocol::{WmCommand, WmRequest, WmResponse};
pub use wm_client::{ClientError as WmMachClientError, WmMachClient, WmMachSubscription};

use crate::actor::config as config_actor;
use crate::actor::reactor::{self, Event, LayoutSaveKind};
use crate::ipc::subscriptions::{PortLease, SharedServerState};
use crate::model::window_store::ClaimOwner;
use crate::sys::mach::{
    OwnedMachReply, is_mach_server_registered, mach_msg_header_t, mach_server_install,
    send_mach_reply,
};

type ClientPort = u32;

struct ConfigJob {
    request: WmRequest,
    destination: OwnedMachReply,
}

const CONFIG_QUEUE_CAPACITY: usize = 8;
const MAX_RESPONSE_SIZE: usize = 262_144;

pub struct InstallRequest {
    config_tx: config_actor::Sender,
    reactor_tx: reactor::Sender,
    response: std::sync::mpsc::SyncSender<Result<SharedServerState, String>>,
}

/// A dead client's claims, released on the reactor's own turn if it was busy.
pub struct ReleaseClaims {
    port: ClientPort,
    server_state: SharedServerState,
}

impl std::fmt::Debug for ReleaseClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReleaseClaims({})", self.port)
    }
}

impl ReleaseClaims {
    /// The claims lease outlives the claims, so the port name cannot be reused before this.
    pub(crate) fn apply(self, reactor: &mut reactor::Reactor) {
        reactor.release_claims(ClaimOwner(self.port));
        self.server_state.set_lease(self.port, PortLease::Claims, false);
    }
}

impl std::fmt::Debug for InstallRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstallRequest(..)")
    }
}

pub fn run_mach_server(
    reactor: reactor::ReactorHandle,
    config_tx: config_actor::Sender,
) -> Result<SharedServerState, String> {
    if is_mach_server_registered() {
        return Err(
            "Another Wm instance is already running; quit it before starting another.".into(),
        );
    }
    let (response, result) = sync_channel(1);
    reactor
        .try_send(Event::InstallIpc(InstallRequest {
            config_tx,
            reactor_tx: reactor.sender(),
            response,
        }))
        .map_err(|_| "Reactor is unavailable".to_string())?;
    result.recv().map_err(|_| "Reactor stopped while installing IPC".to_string())?
}

pub(crate) fn install_mach_server(reactor: Rc<RefCell<reactor::Reactor>>, request: InstallRequest) {
    let InstallRequest {
        config_tx,
        reactor_tx,
        response,
    } = request;
    let result = (|| {
        if is_mach_server_registered() {
            return Err(
                "Another Wm instance is already running; quit it before starting another.".into(),
            );
        }

        let server_state: SharedServerState =
            std::sync::Arc::new(crate::ipc::subscriptions::ServerState::new());
        let config_handler = ConfigRequestHandler { config_tx };
        let (config_jobs, jobs) = bounded::<ConfigJob>(CONFIG_QUEUE_CAPACITY);
        // Config has a different state owner and can wait for disk I/O. Keep
        // that exceptional path bounded and off the reactor run loop.
        std::thread::Builder::new()
            .name("ipc-config".into())
            .spawn(move || {
                while let Ok(mut job) = jobs.recv() {
                    let response = config_handler.handle_request(job.request);
                    send_encoded_response(job.destination.header_mut(), &response);
                }
            })
            .map_err(|error| format!("Failed to spawn IPC config worker: {error}"))?;

        let handler = Box::new(IpcRequestHandler {
            reactor,
            reactor_tx,
            server_state: server_state.clone(),
            config_jobs,
        });
        let context = Box::into_raw(handler);
        if !unsafe {
            mach_server_install(
                context.cast(),
                handle_mach_request_c,
                handle_mach_client_disconnect_c,
            )
        } {
            unsafe { drop(Box::from_raw(context)) };
            return Err("Failed to install Mach IPC on the reactor run loop".into());
        }
        info!("Installed Mach IPC on the reactor run loop");
        Ok(server_state)
    })();
    let _ = response.send(result);
}

struct IpcRequestHandler {
    // CFMachPort invokes this handler on the reactor's own CFRunLoop thread.
    // RefCell enforces local borrow exclusivity without a mutex or state mirror.
    reactor: Rc<RefCell<reactor::Reactor>>,
    reactor_tx: reactor::Sender,
    server_state: SharedServerState,
    config_jobs: ConfigJobSender<ConfigJob>,
}

impl IpcRequestHandler {
    /// Claims last as long as the client's port: its lease follows the owner's claim set.
    fn update_claims(
        &self,
        port: ClientPort,
        change: impl FnOnce(&mut reactor::Reactor, ClaimOwner) -> Result<(), &'static str>,
        data: serde_json::Value,
    ) -> Vec<u8> {
        let Ok(mut reactor) = self.reactor.try_borrow_mut() else {
            return encode_error(serde_json::json!({ "message": "Reactor is busy" }));
        };
        let owner = ClaimOwner(port);
        let result = change(&mut reactor, owner);
        if reactor.state.windows.owner_claims(owner).is_empty() {
            self.server_state.set_lease(port, PortLease::Claims, false);
        }
        match result {
            Ok(()) => encode_success(data),
            Err(message) => encode_error(serde_json::json!({ "message": message })),
        }
    }

    fn handle_message(
        &self,
        payload: &[u8],
        client_port: ClientPort,
        header: &mut mach_msg_header_t,
    ) {
        let message = match std::str::from_utf8(payload) {
            Ok(message) => message,
            Err(error) => {
                error!("Invalid UTF-8 in IPC request: {error}");
                send_error_response(header, "IPC request is not valid UTF-8");
                return;
            }
        };
        let request = match serde_json::from_str(message) {
            Ok(request) => request,
            Err(error) => {
                error!("Failed to parse IPC request: {error}");
                send_error_response(header, &format!("Invalid request format: {error}"));
                return;
            }
        };
        trace!(?request, client_port, "Handling IPC request");

        let response = match request {
            request @ (WmRequest::GetConfig
            | WmRequest::ExecuteCommand {
                command: wm_protocol::WmCommand::Config(_),
            }) => {
                self.dispatch_config_request(request, header);
                return;
            }
            WmRequest::Subscribe { event } => {
                if self.server_state.subscribe_client(client_port, event.to_string()) {
                    encode_success(serde_json::json!({ "subscribed": event.to_string() }))
                } else {
                    encode_error(serde_json::json!({
                        "message": "Failed to monitor subscription port"
                    }))
                }
            }
            WmRequest::Unsubscribe { event } => {
                self.server_state.unsubscribe_client(client_port, event.to_string());
                encode_success(serde_json::json!({ "unsubscribed": event.to_string() }))
            }
            WmRequest::SubscribeCli { event, command, args } => {
                self.server_state
                    .subscribe_cli(event.to_string(), command.clone(), args.clone());
                encode_success(serde_json::json!({
                    "cli_subscribed": event.to_string(),
                    "command": command,
                    "args": args
                }))
            }
            WmRequest::UnsubscribeCli { event } => {
                self.server_state.unsubscribe_cli(event.to_string());
                encode_success(serde_json::json!({ "cli_unsubscribed": event.to_string() }))
            }
            WmRequest::ListCliSubscriptions => {
                encode_success(self.server_state.list_cli_subscriptions())
            }
            WmRequest::ClaimWindow { window_id, flags } => self.update_claims(
                client_port,
                |reactor, owner| {
                    // A claim may only exist while its owner's port is watched.
                    if !self.server_state.set_lease(client_port, PortLease::Claims, true) {
                        return Err("Failed to monitor management port");
                    }
                    reactor.claim_window(window_id_from(window_id)?, owner, flags)
                },
                serde_json::json!({ "claimed": window_id }),
            ),
            WmRequest::ReleaseWindow { window_id } => self.update_claims(
                client_port,
                |reactor, owner| reactor.release_window(window_id_from(window_id)?, owner),
                serde_json::json!({ "released": window_id }),
            ),
            request => match self.reactor.try_borrow_mut() {
                Ok(mut reactor) => encode_reactor_response(&mut reactor, request),
                Err(_) => encode_error(serde_json::json!({ "message": "Reactor is busy" })),
            },
        };
        send_encoded_response(header, &response);
    }

    fn dispatch_config_request(&self, request: WmRequest, header: &mut mach_msg_header_t) {
        let Some(destination) = (unsafe { OwnedMachReply::retain(header) }) else {
            send_error_response(header, "Failed to retain IPC reply port");
            return;
        };
        let job = ConfigJob { request, destination };
        match self.config_jobs.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(mut job)) => send_error_response(
                job.destination.header_mut(),
                "Too many config requests in flight; try again",
            ),
            Err(TrySendError::Disconnected(mut job)) => send_error_response(
                job.destination.header_mut(),
                "Config request service is unavailable",
            ),
        }
    }
}

fn encode_reactor_response(reactor: &mut reactor::Reactor, request: WmRequest) -> Vec<u8> {
    match request {
        WmRequest::GetWorkspaces { space_id } => {
            let workspaces =
                reactor.query_workspaces(space_id.map(crate::sys::screen::SpaceId::new));
            encode_success(
                workspaces.into_iter().map(wm_protocol::WorkspaceData::from).collect::<Vec<_>>(),
            )
        }

        WmRequest::GetWorkspacesForDisplay { display_uuid } => {
            let space = match query_space_for_display(reactor, &display_uuid) {
                Ok(space) => space,
                Err(response) => return response,
            };
            let workspaces = reactor.query_workspaces(Some(space));
            encode_success(
                workspaces.into_iter().map(wm_protocol::WorkspaceData::from).collect::<Vec<_>>(),
            )
        }

        WmRequest::GetDisplays => {
            let displays = reactor.query_displays();
            encode_success(
                displays.into_iter().map(wm_protocol::DisplayData::from).collect::<Vec<_>>(),
            )
        }

        WmRequest::GetWindows { space_id } => {
            let windows = reactor.query_windows(space_id.map(crate::sys::screen::SpaceId::new));
            encode_success(
                windows.into_iter().map(wm_protocol::WindowData::from).collect::<Vec<_>>(),
            )
        }

        WmRequest::GetWindowsForDisplay { display_uuid } => {
            let space = match query_space_for_display(reactor, &display_uuid) {
                Ok(space) => space,
                Err(response) => return response,
            };
            let windows = reactor.query_windows(Some(space));
            encode_success(
                windows.into_iter().map(wm_protocol::WindowData::from).collect::<Vec<_>>(),
            )
        }

        WmRequest::GetWindowInfo { window_id } => {
            match window_id_from(window_id).ok().and_then(|id| reactor.query_window_info(id)) {
                Some(window) => encode_success(wm_protocol::WindowData::from(window)),
                None => encode_error(serde_json::json!({ "message": "Window not found" })),
            }
        }

        WmRequest::GetLayoutState { space_id, workspace_id } => {
            match reactor.query_layout_state(space_id, workspace_id) {
                Some(layout_state) => encode_success(layout_state),
                None => {
                    encode_error(serde_json::json!({ "message": "Space or workspace not found" }))
                }
            }
        }

        WmRequest::GetLayoutStateForDisplay { display_uuid, workspace_id } => {
            let space = match query_space_for_display(reactor, &display_uuid) {
                Ok(space) => space,
                Err(response) => return response,
            };
            match reactor.query_layout_state(Some(space.get()), workspace_id) {
                Some(layout_state) => encode_success(layout_state),
                None => encode_error(serde_json::json!({ "message": "Workspace not found" })),
            }
        }

        WmRequest::GetWorkspaceLayouts { space_id, workspace_id } => {
            let workspace_layouts = reactor.query_workspace_layouts(
                space_id.map(crate::sys::screen::SpaceId::new),
                workspace_id,
            );
            encode_success(workspace_layouts)
        }

        WmRequest::GetWorkspaceLayoutsForDisplay { display_uuid, workspace_id } => {
            let space = match query_space_for_display(reactor, &display_uuid) {
                Ok(space) => space,
                Err(response) => return response,
            };
            encode_success(reactor.query_workspace_layouts(Some(space), workspace_id))
        }

        WmRequest::GetApplications => {
            let applications = reactor.query_applications();
            encode_success(applications)
        }

        WmRequest::GetBindingMode => encode_success(&reactor.binding_mode),
        WmRequest::GetMetrics => encode_success(reactor.query_metrics()),

        WmRequest::GetConfig => {
            unreachable!("config requests run on config workers")
        }

        WmRequest::CheckpointService { pid } => {
            checkpoint_service(reactor, pid, crate::common::config::restore_file())
        }

        WmRequest::ExecuteCommand { command } => match command {
            wm_protocol::WmCommand::Config(_) => {
                unreachable!("config requests run on config workers")
            }
            wm_protocol::WmCommand::Layout(command) => {
                handle_reactor_command(reactor, crate::model::reactor::Command::Layout(command))
            }
            wm_protocol::WmCommand::Metrics(command) => {
                handle_reactor_command(reactor, crate::model::reactor::Command::Metrics(command))
            }
            wm_protocol::WmCommand::Reactor(command) => {
                handle_reactor_command(reactor, crate::model::reactor::Command::Reactor(command))
            }
        },
        _ => encode_error(serde_json::json!({ "message": "Unsupported request" })),
    }
}

fn window_id_from(id: wm_protocol::WindowId) -> Result<crate::actor::app::WindowId, &'static str> {
    std::num::NonZeroU32::new(id.idx)
        .map(|idx| crate::actor::app::WindowId { pid: id.pid, idx })
        .ok_or("Window not found")
}

fn query_space_for_display(
    reactor: &reactor::Reactor,
    display_uuid: &str,
) -> Result<crate::sys::screen::SpaceId, Vec<u8>> {
    reactor.query_space_for_display(display_uuid).ok_or_else(|| {
        encode_error(serde_json::json!({
            "message": format!("Display not found: {display_uuid}")
        }))
    })
}

fn checkpoint_service(
    reactor: &mut reactor::Reactor,
    pid: u32,
    master: std::path::PathBuf,
) -> Vec<u8> {
    if pid != std::process::id() {
        return encode_error(serde_json::json!({
            "message": format!("IPC reached wm PID {}, but launchd reported PID {pid}", std::process::id())
        }));
    }
    save_layout_response(reactor.save_layout(master, LayoutSaveKind::Master))
}

fn save_layout_response(result: std::io::Result<()>) -> Vec<u8> {
    match result {
        Ok(()) => encode_success("Layout saved"),
        Err(error) => encode_error(serde_json::json!({
            "message": format!("Could not save layout: {error}"),
        })),
    }
}

fn handle_reactor_command(
    reactor: &mut reactor::Reactor,
    command: crate::model::reactor::Command,
) -> Vec<u8> {
    if let crate::model::reactor::Command::Reactor(wm_protocol::ReactorCommand::SaveLayout {
        path,
    }) = command
    {
        return save_layout_response(reactor.save_layout(path, LayoutSaveKind::Portable));
    }
    reactor.handle_ipc_command(command);
    encode_success("Command executed successfully")
}

#[derive(Clone)]
struct ConfigRequestHandler {
    config_tx: config_actor::Sender,
}

impl ConfigRequestHandler {
    fn perform<T>(
        &self,
        make_event: impl FnOnce(SyncSender<T>) -> config_actor::Event,
    ) -> Result<T, String>
    where
        T: Send + 'static,
    {
        // Buffer one reply so the actor never waits for this worker to receive it.
        let (response, result) = sync_channel(1);
        self.config_tx
            .try_send(make_event(response))
            .map_err(|error| format!("Failed to send config query: {error}"))?;
        result.recv_timeout(Duration::from_secs(5)).map_err(|error| match error {
            RecvTimeoutError::Timeout => "Failed to get response: config actor timed out".into(),
            RecvTimeoutError::Disconnected => {
                "Failed to get response: config actor disconnected before replying".into()
            }
        })
    }

    fn handle_request(&self, request: WmRequest) -> Vec<u8> {
        match request {
            WmRequest::GetConfig => match self.perform(config_actor::Event::QueryConfig) {
                Ok(config) => encode_success(config),
                Err(error) => encode_error(serde_json::json!({
                    "message": "Failed to get config response",
                    "details": error,
                })),
            },
            WmRequest::ExecuteCommand {
                command: wm_protocol::WmCommand::Config(command),
            } => match self
                .perform(|response| config_actor::Event::ApplyConfig { cmd: command, response })
            {
                Ok(Ok(())) => encode_success("Config applied successfully"),
                Ok(Err(message)) => encode_error(serde_json::json!({ "message": message })),
                Err(error) => encode_error(serde_json::json!({
                    "message": format!("Failed to apply config: {error}"),
                })),
            },
            _ => unreachable!("only config requests are queued to config workers"),
        }
    }
}

unsafe extern "C" fn handle_mach_request_c(
    context: *mut std::ffi::c_void,
    message: *mut c_char,
    len: u32,
    original_msg: *mut mach_msg_header_t,
) {
    if context.is_null() {
        error!("Invalid context pointer");
        return;
    }
    if message.is_null() || len == 0 || original_msg.is_null() {
        error!("Invalid Mach request pointers");
        return;
    }

    let handler = unsafe { &*(context as *const IpcRequestHandler) };
    let message_slice = unsafe { std::slice::from_raw_parts(message as *const u8, len as usize) };

    let trimmed_slice = if let Some(pos) = message_slice.iter().position(|&b| b == 0) {
        &message_slice[..pos]
    } else {
        message_slice
    };

    let client_port = unsafe { (*original_msg).msgh_remote_port };
    handler.handle_message(trimmed_slice, client_port, unsafe { &mut *original_msg });
}

unsafe extern "C" fn handle_mach_client_disconnect_c(
    context: *mut std::ffi::c_void,
    client_port: ClientPort,
) {
    if context.is_null() {
        error!("Invalid context pointer for Mach client disconnect");
        return;
    }

    let handler = unsafe { &*(context as *const IpcRequestHandler) };
    handler.server_state.remove_client(client_port);
    // Subscription-only clients never claimed anything; leave the reactor alone.
    if !handler.server_state.holds_lease(client_port, PortLease::Claims) {
        return;
    }
    let release = ReleaseClaims {
        port: client_port,
        server_state: handler.server_state.clone(),
    };
    match handler.reactor.try_borrow_mut() {
        Ok(mut reactor) => release.apply(&mut reactor),
        Err(_) => handler.reactor_tx.send(Event::ReleaseClaims(release)),
    }
}

fn send_error_response(header: &mut mach_msg_header_t, message: &str) {
    let response = encode_error(serde_json::json!({ "message": message }));
    send_encoded_response(header, &response);
}

fn encode_success<T: Serialize>(data: T) -> Vec<u8> {
    encode_response(&WmResponse::Success { data })
}

fn encode_error(error: serde_json::Value) -> Vec<u8> {
    encode_response::<serde_json::Value>(&WmResponse::Error { error })
}

fn encode_response<T: Serialize>(response: &WmResponse<T>) -> Vec<u8> {
    let mut response_json = match serde_json::to_vec(response) {
        Ok(response) => response,
        Err(error) => {
            error!("Failed to encode IPC response: {error}");
            br#"{"error":{"message":"Failed to encode IPC response"}}"#.to_vec()
        }
    };

    if response_json.last().copied() != Some(0) {
        response_json.push(0);
    }
    response_json
}

fn send_encoded_response(original_msg: *mut mach_msg_header_t, response_json: &[u8]) {
    // An unsendable reply would leave the client waiting; report the size instead.
    if response_json.len() > MAX_RESPONSE_SIZE {
        let message = format!("Response exceeds the {MAX_RESPONSE_SIZE}-byte IPC limit");
        return send_encoded_response(
            original_msg,
            &encode_error(serde_json::json!({ "message": message })),
        );
    }
    unsafe {
        if !send_mach_reply(
            original_msg,
            response_json.as_ptr() as *mut c_char,
            response_json.len() as u32,
        ) {
            error!(
                "Failed to send mach reply for message id {}",
                if original_msg.is_null() {
                    -1
                } else {
                    (*original_msg).msgh_id
                }
            );
        }
    }
}

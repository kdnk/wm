// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
//! A small, synchronous Rust client for Wm's Mach IPC API.
//!
//! The client talks to the same bootstrap service as `wm-cli` and includes
//! the public request and response wire types.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
compile_error!("wm-client only supports macOS");

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem::{MaybeUninit, size_of};
use std::ptr::{addr_of_mut, copy_nonoverlapping};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
pub use wm_protocol::*;

const MAX_MESSAGE_SIZE: usize = 262_144;

type KernReturn = c_int;
type MachPort = u32;
type MachMessageSize = u32;
type MachMessageOption = u32;

const KERN_SUCCESS: KernReturn = 0;
const MACH_SEND_MSG: MachMessageOption = 0x0000_0001;
const MACH_RCV_MSG: MachMessageOption = 0x0000_0002;
const MACH_SEND_TIMEOUT: MachMessageOption = 0x0000_0010;
const MACH_RCV_TIMEOUT: MachMessageOption = 0x0000_0100;
const MACH_SEND_TIMED_OUT: KernReturn = 0x1000_0004;
const MACH_RCV_TIMED_OUT: KernReturn = 0x1000_4003;
// The config worker can wait five seconds for disk I/O; leave time for its error reply.
const REQUEST_TIMEOUT_MS: u32 = if cfg!(test) { 500 } else { 10_000 };
/// Session request ids carry this bit so that, even after the sequence wraps, they stay
/// clear of the small ids Mach reserves for kernel notifications (64-77) and MIG.
const SESSION_REQUEST_ID_TAG: u32 = 0x4000_0000;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_PORT_RIGHT_RECEIVE: c_int = 1;
const MACH_PORT_LIMITS_INFO: c_int = 1;
const MACH_PORT_LIMITS_INFO_COUNT: u32 = 1;
const MACH_PORT_QLIMIT_LARGE: u32 = 1024;
const TASK_BOOTSTRAP_PORT: c_int = 4;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("Wm's Mach service is not registered")]
    ServiceUnavailable,
    #[error("Wm request timed out")]
    RequestTimedOut,
    #[error("invalid Mach service name")]
    InvalidServiceName,
    #[error("Mach operation {operation} failed with code {code}")]
    Mach {
        operation: &'static str,
        code: KernReturn,
    },
    #[error("IPC payload exceeds the {MAX_MESSAGE_SIZE}-byte limit")]
    MessageTooLarge,
    #[error("Wm returned an empty response")]
    EmptyResponse,
    #[error("{kind} payload is missing its NUL terminator")]
    MissingTerminator { kind: &'static str },
    #[error("failed to encode request: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("failed to decode {kind}: {source}")]
    Decode {
        kind: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("Wm rejected the subscription: {0}")]
    SubscriptionRejected(Value),
    #[error("Wm returned an error: {0}")]
    Server(Value),
    #[error("Wm returned an unknown response shape")]
    UnknownResponse,
}

/// A handle for issuing synchronous requests to the running Wm process.
#[derive(Clone, Copy, Debug, Default)]
pub struct WmMachClient;

impl WmMachClient {
    /// Creates a client handle.
    ///
    /// Service discovery happens when a request is sent, allowing callers to
    /// construct the client before Wm has finished starting.
    pub fn connect() -> Result<Self, ClientError> { Ok(Self) }

    /// Returns whether Wm's bootstrap service is currently registered.
    pub fn is_available(&self) -> bool {
        let service_port = service_name()
            .ok()
            .and_then(|name| unsafe { lookup_service(&name).ok() })
            .map(ServicePort::new);
        service_port.is_some()
    }

    /// Sends one request with a ten-second limit on each send and reply wait.
    pub fn send_request(&self, request: &WmRequest) -> Result<JsonWmResponse, ClientError> {
        self.send_typed_request(request)
    }

    /// Sends one request and decodes its response payload into caller-provided
    /// types. Sending and waiting for the reply each time out after ten seconds.
    pub fn send_typed_request<T: DeserializeOwned>(
        &self,
        request: &WmRequest,
    ) -> Result<WmResponse<T>, ClientError> {
        let request_json = serde_json::to_vec(request).map_err(ClientError::Encode)?;
        let response = unsafe { send_request(&request_json, None)? };
        parse_json_payload(&response, "response")
    }

    /// Sends a one-shot request with a shared send/reply deadline and no discovery retries.
    pub fn send_typed_request_with_timeout<T: DeserializeOwned>(
        &self,
        request: &WmRequest,
        timeout: Duration,
    ) -> Result<WmResponse<T>, ClientError> {
        let request_json = serde_json::to_vec(request).map_err(ClientError::Encode)?;
        let deadline = Instant::now() + timeout.min(Duration::from_millis(u32::MAX.into()));
        let response = unsafe { send_request_until(&request_json, None, Some(deadline))? }.1;
        parse_json_payload(&response, "response")
    }

    /// Lists virtual workspaces, optionally for a specific macOS space.
    pub fn get_workspaces(&self, space_id: Option<u64>) -> Result<Vec<WorkspaceData>, ClientError> {
        self.request(WmRequest::GetWorkspaces { space_id })
    }

    /// Lists virtual workspaces for a display's current macOS space.
    pub fn get_workspaces_for_display(
        &self,
        display_uuid: impl Into<String>,
    ) -> Result<Vec<WorkspaceData>, ClientError> {
        self.request(WmRequest::GetWorkspacesForDisplay {
            display_uuid: display_uuid.into(),
        })
    }

    /// Lists managed windows, optionally filtered by a macOS space.
    pub fn get_windows(&self, space_id: Option<u64>) -> Result<Vec<WindowData>, ClientError> {
        self.request(WmRequest::GetWindows { space_id })
    }

    /// Lists managed windows for a display's current macOS space.
    pub fn get_windows_for_display(
        &self,
        display_uuid: impl Into<String>,
    ) -> Result<Vec<WindowData>, ClientError> {
        self.request(WmRequest::GetWindowsForDisplay {
            display_uuid: display_uuid.into(),
        })
    }

    /// Lists connected displays.
    pub fn get_displays(&self) -> Result<Vec<DisplayData>, ClientError> {
        self.request(WmRequest::GetDisplays)
    }

    /// Returns information about a managed window.
    pub fn get_window_info(&self, window_id: WindowId) -> Result<WindowData, ClientError> {
        self.request(WmRequest::GetWindowInfo { window_id })
    }

    /// Returns the layout state for a macOS space.
    pub fn get_layout_state(&self, space_id: Option<u64>) -> Result<LayoutStateData, ClientError> {
        self.get_workspace_layout_state(space_id, None)
    }

    /// Returns layout state for an optional macOS space and workspace index.
    /// Omitted selectors independently default to the active space/workspace.
    pub fn get_workspace_layout_state(
        &self,
        space_id: Option<u64>,
        workspace_id: Option<usize>,
    ) -> Result<LayoutStateData, ClientError> {
        self.request(WmRequest::GetLayoutState { space_id, workspace_id })
    }

    /// Returns layout state for a display's current macOS space and an optional
    /// workspace index.
    pub fn get_workspace_layout_state_for_display(
        &self,
        display_uuid: impl Into<String>,
        workspace_id: Option<usize>,
    ) -> Result<LayoutStateData, ClientError> {
        self.request(WmRequest::GetLayoutStateForDisplay {
            display_uuid: display_uuid.into(),
            workspace_id,
        })
    }

    /// Returns layout modes for workspaces in a macOS space.
    pub fn get_workspace_layouts(
        &self,
        space_id: Option<u64>,
        workspace_id: Option<usize>,
    ) -> Result<Vec<WorkspaceLayoutData>, ClientError> {
        self.request(WmRequest::GetWorkspaceLayouts { space_id, workspace_id })
    }

    /// Returns layout modes for workspaces in a display's current macOS space.
    pub fn get_workspace_layouts_for_display(
        &self,
        display_uuid: impl Into<String>,
        workspace_id: Option<usize>,
    ) -> Result<Vec<WorkspaceLayoutData>, ClientError> {
        self.request(WmRequest::GetWorkspaceLayoutsForDisplay {
            display_uuid: display_uuid.into(),
            workspace_id,
        })
    }

    /// Lists running applications known to Wm.
    pub fn get_applications(&self) -> Result<Vec<ApplicationData>, ClientError> {
        self.request(WmRequest::GetApplications)
    }

    /// Returns the active binding mode name.
    pub fn get_binding_mode(&self) -> Result<String, ClientError> {
        self.request(WmRequest::GetBindingMode)
    }

    /// Returns the current metrics payload.
    pub fn get_metrics(&self) -> Result<Value, ClientError> { self.request(WmRequest::GetMetrics) }

    /// Returns the current configuration as JSON until the config model is
    /// moved into `wm-protocol`.
    pub fn get_config(&self) -> Result<Value, ClientError> { self.request(WmRequest::GetConfig) }

    /// Executes a typed Wm command.
    pub fn execute(&self, command: WmCommand) -> Result<Value, ClientError> {
        self.request(WmRequest::ExecuteCommand { command })
    }

    /// Executes a typed Wm command.
    pub fn execute_command(&self, command: WmCommand) -> Result<Value, ClientError> {
        self.execute(command)
    }

    /// Subscribes to an event and returns a handle that blocks for future events.
    pub fn subscribe(&self, event: EventKind) -> Result<WmMachSubscription, ClientError> {
        self.start_subscription::<Value>(WmRequest::Subscribe { event })
            .map(|(_, subscription)| subscription)
    }

    fn start_subscription<T: DeserializeOwned>(
        &self,
        request: WmRequest,
    ) -> Result<(T, WmMachSubscription), ClientError> {
        let reply_port = ReplyPort::allocate(MACH_PORT_QLIMIT_LARGE)?;
        let request_json = serde_json::to_vec(&request).map_err(ClientError::Encode)?;
        let deadline = std::time::Instant::now() + Duration::from_millis(REQUEST_TIMEOUT_MS.into());
        let mut payload = unsafe { send_request(&request_json, Some(reply_port.name))? };
        let mut pending = VecDeque::new();
        loop {
            let value: Value = parse_json_payload(&payload, "response")?;
            // Broadcasts and the reply share a receive port. The event dispatcher
            // can win the race with the config worker, including for ordinary subscribe.
            if value.get("type").is_some() {
                pending.push_back(payload);
                if std::time::Instant::now() >= deadline {
                    return Err(ClientError::RequestTimedOut);
                }
                let timeout = deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis()
                    .min(u32::MAX.into()) as u32;
                payload = unsafe { receive_message(reply_port.name, Some(timeout))?.1 };
                continue;
            }
            return match parse_json_payload::<WmResponse<T>>(&payload, "response")? {
                WmResponse::Success { data } => Ok((data, WmMachSubscription {
                    reply_port,
                    pending: Mutex::new(pending),
                })),
                WmResponse::Error { error } => Err(ClientError::SubscriptionRejected(error)),
                _ => Err(ClientError::UnknownResponse),
            };
        }
    }

    /// Opens a session for claiming windows from Wm. Claims end when they are
    /// released or when the session is dropped (including process exit).
    pub fn window_management_session(&self) -> Result<WindowManagementSession, ClientError> {
        Ok(WindowManagementSession {
            reply_port: ReplyPort::allocate(MACH_PORT_QLIMIT_LARGE)?,
            sequence: 0,
        })
    }

    fn request<T: DeserializeOwned>(&self, request: WmRequest) -> Result<T, ClientError> {
        match self.send_typed_request(&request)? {
            WmResponse::Success { data } => Ok(data),
            WmResponse::Error { error } => Err(ClientError::Server(error)),
            _ => Err(ClientError::UnknownResponse),
        }
    }
}

/// A persistent Mach receive right that identifies its owner's window claims.
/// Dropping it ends every claim it holds.
#[derive(Debug)]
pub struct WindowManagementSession {
    reply_port: ReplyPort,
    sequence: u32,
}

impl WindowManagementSession {
    /// Excludes a window from Wm's management until it is released.
    pub fn claim_window(&mut self, window_id: WindowId) -> Result<(), ClientError> {
        self.claim_window_with_flags(window_id, WindowClaimFlags::default())
    }

    /// Claims a window, or updates the flags of a claim this session holds.
    pub fn claim_window_with_flags(
        &mut self,
        window_id: WindowId,
        flags: WindowClaimFlags,
    ) -> Result<(), ClientError> {
        self.request(WmRequest::ClaimWindow { window_id, flags })
    }

    /// Returns a claimed window to Wm. Releasing an unclaimed window succeeds.
    pub fn release_window(&mut self, window_id: WindowId) -> Result<(), ClientError> {
        self.request(WmRequest::ReleaseWindow { window_id })
    }

    /// Claim and release are idempotent, so a request may be retried after a
    /// timeout. Replies carry the request id; a late reply to an abandoned
    /// request is discarded instead of being taken for the current one.
    fn request(&mut self, request: WmRequest) -> Result<(), ClientError> {
        let id = self.next_request_id();
        let payload = serde_json::to_vec(&request).map_err(ClientError::Encode)?;
        let port = self.reply_port.name;
        let response = unsafe {
            let first = send_request_with_id(&payload, Some((port, id)))?;
            receive_reply(port, id, first)?
        };
        match parse_json_payload::<WmResponse>(&response, "response")? {
            WmResponse::Success { .. } => Ok(()),
            WmResponse::Error { error } => Err(ClientError::Server(error)),
            _ => Err(ClientError::UnknownResponse),
        }
    }

    fn next_request_id(&mut self) -> i32 {
        self.sequence = self.sequence.wrapping_add(1);
        (self.sequence | SESSION_REQUEST_ID_TAG) as i32
    }
}

/// A live event subscription. Dropping it releases its Mach receive right.
#[derive(Debug)]
pub struct WmMachSubscription {
    reply_port: ReplyPort,
    pending: Mutex<VecDeque<Vec<u8>>>,
}

impl WmMachSubscription {
    /// Blocks until the next event arrives on this subscription.
    pub fn recv_event(&self) -> Result<WmEvent, ClientError> { self.recv_event_as() }

    /// Blocks until the next event arrives and returns its raw JSON payload.
    ///
    /// This is useful for compatibility with clients that intentionally handle
    /// newer event variants without upgrading their protocol types.
    pub fn recv_event_value(&self) -> Result<Value, ClientError> { self.recv_event_as() }

    /// Blocks until the next event arrives and decodes it into the requested
    /// type.
    pub fn recv_event_as<T: DeserializeOwned>(&self) -> Result<T, ClientError> {
        let pending = self.pending.lock().expect("subscription pending events").pop_front();
        let payload = match pending {
            Some(payload) => payload,
            None => unsafe { receive_message(self.reply_port.name, None)?.1 },
        };
        parse_json_payload(&payload, "event")
    }
}

fn parse_json_payload<T: DeserializeOwned>(
    payload: &[u8],
    kind: &'static str,
) -> Result<T, ClientError> {
    if payload.is_empty() {
        return Err(ClientError::EmptyResponse);
    }
    let bytes = CStr::from_bytes_until_nul(payload)
        .map_err(|_| ClientError::MissingTerminator { kind })?
        .to_bytes();
    serde_json::from_slice(bytes).map_err(|source| ClientError::Decode { kind, source })
}

fn service_name() -> Result<CString, ClientError> {
    let name = std::env::var("WM_BS_NAME").unwrap_or_else(|_| DEFAULT_SERVICE_NAME.to_owned());
    CString::new(name).map_err(|_| ClientError::InvalidServiceName)
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct MachMessageHeader {
    bits: u32,
    size: MachMessageSize,
    remote_port: MachPort,
    local_port: MachPort,
    voucher_port: MachPort,
    id: i32,
}

#[repr(C)]
struct InlineMessage {
    header: MachMessageHeader,
    data: [u8; MAX_MESSAGE_SIZE],
}

#[repr(C)]
struct ReceiveBuffer {
    message: InlineMessage,
    trailer: [u8; 512],
}

/// Initializes only the inline message bytes declared in the Mach header.
/// Keeping the maximum-sized storage uninitialized avoids clearing 256 KiB
/// for every usually-small client request without introducing an allocation.
unsafe fn prepare_inline_send(
    storage: &mut MaybeUninit<InlineMessage>,
    mut header: MachMessageHeader,
    payload: &[u8],
) -> *mut MachMessageHeader {
    debug_assert!(payload.len() <= MAX_MESSAGE_SIZE);

    let aligned_len = (payload.len() + 3) & !3;
    header.size = (size_of::<MachMessageHeader>() + aligned_len) as u32;

    let message = storage.as_mut_ptr();
    let header_ptr = unsafe { addr_of_mut!((*message).header) };
    unsafe { header_ptr.write(header) };

    let data = unsafe { addr_of_mut!((*message).data) }.cast::<u8>();
    unsafe { copy_nonoverlapping(payload.as_ptr(), data, payload.len()) };
    if aligned_len > payload.len() {
        unsafe {
            std::ptr::write_bytes(
                data.add(payload.len()),
                0,
                aligned_len.saturating_sub(payload.len()),
            )
        };
    }

    header_ptr
}

#[repr(C)]
struct MachPortLimits {
    queue_limit: u32,
}

#[link(name = "System", kind = "framework")]
unsafe extern "C" {
    fn mach_task_self() -> MachPort;
    fn task_get_special_port(task: MachPort, which: c_int, port: *mut MachPort) -> KernReturn;
    fn mach_port_allocate(task: MachPort, right: c_int, name: *mut MachPort) -> KernReturn;
    fn mach_port_insert_right(
        task: MachPort,
        name: MachPort,
        poly: MachPort,
        disposition: c_int,
    ) -> KernReturn;
    fn mach_port_mod_refs(task: MachPort, name: MachPort, right: c_int, delta: c_int)
    -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn mach_port_set_attributes(
        task: MachPort,
        name: MachPort,
        flavor: c_int,
        info: *const c_void,
        count: u32,
    ) -> KernReturn;
    fn mach_msg(
        message: *mut MachMessageHeader,
        option: MachMessageOption,
        send_size: MachMessageSize,
        receive_size: MachMessageSize,
        receive_name: MachPort,
        timeout: u32,
        notify: MachPort,
    ) -> KernReturn;
    fn mach_msg_destroy(message: *mut MachMessageHeader) -> KernReturn;
    fn bootstrap_look_up(
        bootstrap_port: MachPort,
        service_name: *const c_char,
        service_port: *mut MachPort,
    ) -> KernReturn;
}

#[derive(Debug)]
struct ReplyPort {
    name: MachPort,
}

struct ServicePort {
    name: MachPort,
}

impl ServicePort {
    fn new(name: MachPort) -> Self { Self { name } }
}

impl Drop for ServicePort {
    fn drop(&mut self) {
        unsafe {
            let _ = mach_port_deallocate(mach_task_self(), self.name);
        }
    }
}

impl ReplyPort {
    fn allocate(queue_limit: u32) -> Result<Self, ClientError> {
        unsafe {
            let task = mach_task_self();
            let mut name = 0;
            let result = mach_port_allocate(task, MACH_PORT_RIGHT_RECEIVE, &mut name);
            if result != KERN_SUCCESS {
                return Err(ClientError::Mach {
                    operation: "mach_port_allocate",
                    code: result,
                });
            }

            let limits = MachPortLimits { queue_limit };
            let _ = mach_port_set_attributes(
                task,
                name,
                MACH_PORT_LIMITS_INFO,
                &limits as *const _ as *const c_void,
                MACH_PORT_LIMITS_INFO_COUNT,
            );

            let result = mach_port_insert_right(task, name, name, MACH_MSG_TYPE_MAKE_SEND as c_int);
            if result != KERN_SUCCESS {
                let _ = mach_port_mod_refs(task, name, MACH_PORT_RIGHT_RECEIVE, -1);
                let _ = mach_port_deallocate(task, name);
                return Err(ClientError::Mach {
                    operation: "mach_port_insert_right",
                    code: result,
                });
            }
            Ok(Self { name })
        }
    }
}

impl Drop for ReplyPort {
    fn drop(&mut self) {
        unsafe {
            let task = mach_task_self();
            let _ = mach_port_mod_refs(task, self.name, MACH_PORT_RIGHT_RECEIVE, -1);
            let _ = mach_port_deallocate(task, self.name);
        }
    }
}

unsafe fn lookup_service(name: &CStr) -> Result<MachPort, ClientError> {
    let mut bootstrap_port = 0;
    let result = unsafe {
        task_get_special_port(mach_task_self(), TASK_BOOTSTRAP_PORT, &mut bootstrap_port)
    };
    if result != KERN_SUCCESS {
        return Err(ClientError::Mach {
            operation: "task_get_special_port",
            code: result,
        });
    }

    let mut service_port = 0;
    let result = unsafe { bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut service_port) };
    let _ = unsafe { mach_port_deallocate(mach_task_self(), bootstrap_port) };
    if result != KERN_SUCCESS {
        return Err(ClientError::ServiceUnavailable);
    }
    Ok(service_port)
}

unsafe fn find_service_once() -> Result<ServicePort, ClientError> {
    #[cfg(test)]
    if let port @ 1.. = tests::SERVICE.get() {
        unsafe { mach_port_mod_refs(mach_task_self(), port, tests::MACH_PORT_RIGHT_SEND, 1) };
        return Ok(ServicePort::new(port));
    }
    unsafe { lookup_service(&service_name()?).map(ServicePort::new) }
}

unsafe fn find_service_with_retry() -> Result<ServicePort, ClientError> {
    for attempt in 0..5 {
        match unsafe { find_service_once() } {
            Ok(port) => return Ok(port),
            Err(ClientError::InvalidServiceName) => return Err(ClientError::InvalidServiceName),
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(50 * (1 << attempt)));
    }
    Err(ClientError::ServiceUnavailable)
}

unsafe fn send_request(
    payload: &[u8],
    subscription_port: Option<MachPort>,
) -> Result<Vec<u8>, ClientError> {
    let reply = subscription_port.map(|port| (port, port as i32));
    Ok(unsafe { send_request_with_id(payload, reply)? }.1)
}

/// Sends on a persistent `(port, id)` reply destination, or a one-shot port.
unsafe fn send_request_with_id(
    payload: &[u8],
    persistent: Option<(MachPort, i32)>,
) -> Result<(i32, Vec<u8>), ClientError> {
    unsafe { send_request_until(payload, persistent, None) }
}

fn request_timeout(deadline: Option<Instant>) -> Result<u32, ClientError> {
    let Some(deadline) = deadline else {
        return Ok(REQUEST_TIMEOUT_MS);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ClientError::RequestTimedOut);
    }
    Ok(remaining.as_millis().saturating_add(1).min(u32::MAX.into()) as u32)
}

unsafe fn send_request_until(
    payload: &[u8],
    persistent: Option<(MachPort, i32)>,
    deadline: Option<Instant>,
) -> Result<(i32, Vec<u8>), ClientError> {
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(ClientError::MessageTooLarge);
    }

    let service_port = if deadline.is_some() {
        unsafe { find_service_once()? }
    } else {
        unsafe { find_service_with_retry()? }
    };
    let owned_reply_port;
    let (reply_port, id) = match persistent {
        Some(reply) => reply,
        None => {
            owned_reply_port = ReplyPort::allocate(1)?;
            (owned_reply_port.name, owned_reply_port.name as i32)
        }
    };

    let header = MachMessageHeader {
        bits: message_bits(
            MACH_MSG_TYPE_COPY_SEND,
            if persistent.is_some() {
                MACH_MSG_TYPE_COPY_SEND
            } else {
                MACH_MSG_TYPE_MAKE_SEND
            },
        ),
        size: 0,
        remote_port: service_port.name,
        local_port: reply_port,
        voucher_port: 0,
        id,
    };
    let mut storage = MaybeUninit::<InlineMessage>::uninit();
    let header_ptr = unsafe { prepare_inline_send(&mut storage, header, payload) };
    let send_size = unsafe { (*header_ptr).size };

    let result = unsafe {
        mach_msg(
            header_ptr,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            send_size,
            0,
            0,
            request_timeout(deadline)?,
            0,
        )
    };
    if result == MACH_SEND_TIMED_OUT {
        // The kernel returns an unsent message's rights: mach_msg_destroy releases the
        // destination (remote) right but not the reply (local) right.
        unsafe {
            let _ = mach_port_deallocate(mach_task_self(), (*header_ptr).local_port);
            mach_msg_destroy(header_ptr);
        }
        return Err(ClientError::RequestTimedOut);
    }
    if result != KERN_SUCCESS {
        return Err(ClientError::Mach {
            operation: "mach_msg(send)",
            code: result,
        });
    }

    unsafe { receive_message(reply_port, Some(request_timeout(deadline)?)) }
}

/// Returns the message id (echoed from the request by replies) and payload.
unsafe fn receive_message(
    reply_port: MachPort,
    timeout: Option<u32>,
) -> Result<(i32, Vec<u8>), ClientError> {
    // The kernel initializes the received header, payload, and trailer. Leave
    // the maximum-sized backing storage untouched until then.
    let mut buffer = MaybeUninit::<ReceiveBuffer>::uninit();
    let buffer_ptr = buffer.as_mut_ptr();
    let header_ptr = unsafe { addr_of_mut!((*buffer_ptr).message.header) };
    let result = unsafe {
        mach_msg(
            header_ptr,
            MACH_RCV_MSG
                | if timeout.is_some() {
                    MACH_RCV_TIMEOUT
                } else {
                    0
                },
            0,
            size_of::<ReceiveBuffer>() as u32,
            reply_port,
            timeout.unwrap_or(0),
            0,
        )
    };
    if result == MACH_RCV_TIMED_OUT {
        return Err(ClientError::RequestTimedOut);
    }
    if result != KERN_SUCCESS {
        return Err(ClientError::Mach {
            operation: "mach_msg(receive)",
            code: result,
        });
    }

    let payload_len = unsafe { (*header_ptr).size }
        .saturating_sub(size_of::<MachMessageHeader>() as u32) as usize;
    if payload_len > MAX_MESSAGE_SIZE {
        unsafe { mach_msg_destroy(header_ptr) };
        return Err(ClientError::MessageTooLarge);
    }
    let data = unsafe { addr_of_mut!((*buffer_ptr).message.data) }.cast::<u8>();
    let payload = unsafe { std::slice::from_raw_parts(data, payload_len) }.to_vec();
    let id = unsafe { (*header_ptr).id };
    unsafe { mach_msg_destroy(header_ptr) };
    Ok((id, payload))
}

/// Receives until the reply to request `id`, discarding replies to abandoned requests.
unsafe fn receive_reply(
    port: MachPort,
    id: i32,
    (mut reply_id, mut payload): (i32, Vec<u8>),
) -> Result<Vec<u8>, ClientError> {
    let deadline = std::time::Instant::now() + Duration::from_millis(REQUEST_TIMEOUT_MS.into());
    while reply_id != id {
        let timeout = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .min(u32::MAX.into()) as u32;
        (reply_id, payload) = unsafe { receive_message(port, Some(timeout))? };
    }
    Ok(payload)
}

const fn message_bits(remote: u32, local: u32) -> u32 { remote | (local << 8) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_service_name_uses_the_wm_namespace() {
        let expected = std::env::var("WM_BS_NAME").unwrap_or_else(|_| "git.kdnk.wm".to_owned());
        assert_eq!(service_name().unwrap().to_str().unwrap(), expected);
    }

    #[test]
    fn inline_send_initializes_header_payload_and_padding() {
        let payload = b"hello";
        let header = MachMessageHeader {
            bits: 7,
            size: 0,
            remote_port: 11,
            local_port: 13,
            voucher_port: 0,
            id: 17,
        };
        let mut storage = MaybeUninit::<InlineMessage>::uninit();
        let header_ptr = unsafe { prepare_inline_send(&mut storage, header, payload) };

        unsafe {
            assert_eq!((*header_ptr).bits, 7);
            assert_eq!((*header_ptr).remote_port, 11);
            assert_eq!((*header_ptr).local_port, 13);
            assert_eq!((*header_ptr).id, 17);
            assert_eq!((*header_ptr).size as usize, size_of::<MachMessageHeader>() + 8);

            let data = addr_of_mut!((*storage.as_mut_ptr()).data).cast::<u8>();
            assert_eq!(std::slice::from_raw_parts(data, 8), b"hello\0\0\0");
        }
    }

    pub(super) const MACH_PORT_RIGHT_SEND: c_int = 0;
    const OK: &[u8] = b"{\"data\":null}\0";

    thread_local! {
        /// Requests from this thread go to this port instead of Wm's bootstrap service.
        pub(super) static SERVICE: std::cell::Cell<MachPort> = const { std::cell::Cell::new(0) };
    }

    /// Sends a message to `port` as a Wm reply would.
    fn deliver(port: MachPort, id: i32, payload: &[u8]) {
        let header = MachMessageHeader {
            bits: message_bits(MACH_MSG_TYPE_COPY_SEND, 0),
            size: 0,
            remote_port: port,
            local_port: 0,
            voucher_port: 0,
            id,
        };
        let mut storage = MaybeUninit::<InlineMessage>::uninit();
        unsafe {
            let message = prepare_inline_send(&mut storage, header, payload);
            assert_eq!(
                mach_msg(message, MACH_SEND_MSG, (*message).size, 0, 0, 0, 0),
                KERN_SUCCESS
            );
        }
    }

    /// Serves `count` requests on a fake Wm service port for this thread's clients.
    fn serve(
        count: usize,
        mut respond: impl FnMut(i32, MachPort, WmRequest) + Send + 'static,
    ) -> (ReplyPort, thread::JoinHandle<()>) {
        let service = ReplyPort::allocate(MACH_PORT_QLIMIT_LARGE).unwrap();
        let port = service.name;
        SERVICE.set(port);
        let server = thread::spawn(move || {
            for _ in 0..count {
                let mut buffer = [0u64; 512];
                let header = buffer.as_mut_ptr().cast::<MachMessageHeader>();
                let options = MACH_RCV_MSG | MACH_RCV_TIMEOUT;
                let size = size_of_val(&buffer) as u32;
                assert_eq!(
                    unsafe { mach_msg(header, options, 0, size, port, 2_000, 0) },
                    KERN_SUCCESS
                );
                let (id, reply, len) =
                    unsafe { ((*header).id, (*header).remote_port, (*header).size) };
                let body = unsafe {
                    std::slice::from_raw_parts(
                        header.add(1).cast::<u8>(),
                        len as usize - size_of::<MachMessageHeader>(),
                    )
                };
                let json = body.split(|&byte| byte == 0).next().unwrap();
                respond(id, reply, serde_json::from_slice(json).unwrap());
                unsafe { mach_msg_destroy(header) };
            }
        });
        (service, server)
    }

    #[test]
    fn window_claim_api_matches_pr_519() {
        type Session = WindowManagementSession;
        let _: fn(&WmMachClient) -> Result<Session, ClientError> =
            WmMachClient::window_management_session;
        let _: fn(&mut Session, WindowId) -> Result<(), ClientError> = Session::claim_window;
        let _: fn(&mut Session, WindowId, WindowClaimFlags) -> Result<(), ClientError> =
            Session::claim_window_with_flags;
        let _: fn(&mut Session, WindowId) -> Result<(), ClientError> = Session::release_window;
        let flags: WindowClaimFlags = WindowClaimFlags::empty()
            | WindowClaimFlags::from_bits_retain(WindowClaimFlags::default().bits());
        assert!(
            flags.is_supported() && !flags.contains(WindowClaimFlags::SUPPRESS_FOCUS_FOLLOWS_MOUSE)
        );
        assert!(matches!(
            ClientError::RequestTimedOut,
            ClientError::RequestTimedOut
        ));
        let _ = |window: WindowData| window.externally_managed;
    }

    #[test]
    fn bounded_request_times_out_without_poisoning_the_next_reply() {
        let mut first = true;
        let (_service, server) = serve(2, move |id, reply, request| {
            assert!(matches!(request, WmRequest::GetWorkspaces { space_id: None }));
            if first {
                first = false;
            } else {
                deliver(reply, id, b"{\"data\":true}\0");
            }
        });
        let request = WmRequest::GetWorkspaces { space_id: None };
        let timed_out: Result<WmResponse<bool>, _> =
            WmMachClient.send_typed_request_with_timeout(&request, Duration::from_millis(5));
        assert!(matches!(timed_out, Err(ClientError::RequestTimedOut)));
        let result: WmResponse<bool> = WmMachClient
            .send_typed_request_with_timeout(&request, Duration::from_secs(1))
            .unwrap();
        assert_eq!(result.into_result(), Ok(true));
        server.join().unwrap();
    }

    #[test]
    fn exhausted_deadline_does_not_send_a_request() {
        let (_service, server) = serve(1, |id, reply, _| deliver(reply, id, OK));
        let request = WmRequest::GetWorkspaces { space_id: None };
        let timed_out: Result<WmResponse, _> =
            WmMachClient.send_typed_request_with_timeout(&request, Duration::ZERO);
        assert!(matches!(timed_out, Err(ClientError::RequestTimedOut)));
        WmMachClient.send_request(&request).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn session_request_ids_never_collide_with_notification_ids() {
        let mut session = WmMachClient.window_management_session().unwrap();
        for sequence in [
            0,
            63,
            71,
            72,
            77,
            0x3FFF_FFBF,
            0x7FFF_FFFF,
            0xBFFF_FFFF,
            u32::MAX - 1,
        ] {
            session.sequence = sequence;
            let id = session.next_request_id() as u32;
            assert!(id >= SESSION_REQUEST_ID_TAG, "sequence {sequence} -> id {id:#x}");
        }
        // Wrapping continues with distinct ids.
        session.sequence = u32::MAX - 1;
        let ids = [(); 3].map(|_| session.next_request_id());
        assert_eq!(ids, [
            -1,
            SESSION_REQUEST_ID_TAG as i32,
            SESSION_REQUEST_ID_TAG as i32 + 1
        ]);
    }

    #[test]
    fn one_session_serves_hundreds_of_requests_across_wraparound() {
        const REQUESTS: usize = 300;
        let (_service, server) = serve(REQUESTS, |id, reply, request| {
            // A server that recognized notifications by id alone would drop this request.
            assert!(
                !(64..=77).contains(&id),
                "request id {id} is a Mach notification id"
            );
            assert!(matches!(
                request,
                WmRequest::ClaimWindow { .. } | WmRequest::ReleaseWindow { .. }
            ));
            deliver(reply, id, OK);
        });
        let mut session = WmMachClient.window_management_session().unwrap();
        session.sequence = u32::MAX - (REQUESTS / 2) as u32;
        let window = WindowId { pid: 1, idx: 1 };
        for _ in 0..REQUESTS / 2 {
            session.claim_window(window).unwrap();
            session.release_window(window).unwrap();
        }
        server.join().unwrap();
    }

    #[test]
    fn late_reply_after_timeout_is_not_taken_for_the_next_request() {
        let mut abandoned = None;
        let (_service, server) = serve(2, move |id, reply, _| {
            // Answer the first request only after the client has given up on it.
            if let Some(late) = abandoned.replace(id) {
                deliver(reply, late, b"{\"error\":{\"message\":\"stale\"}}\0");
                deliver(reply, id, OK);
            }
        });
        let mut session = WmMachClient.window_management_session().unwrap();
        let window = WindowId { pid: 1, idx: 1 };
        assert!(matches!(
            session.claim_window(window),
            Err(ClientError::RequestTimedOut)
        ));
        session.claim_window(window).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn full_service_queue_times_out_without_leaking_rights() {
        unsafe extern "C" {
            fn mach_port_get_refs(
                task: MachPort,
                name: MachPort,
                right: c_int,
                refs: *mut u32,
            ) -> KernReturn;
        }
        let refs = |name| {
            let mut refs = 0;
            unsafe { mach_port_get_refs(mach_task_self(), name, MACH_PORT_RIGHT_SEND, &mut refs) };
            refs
        };
        let service = ReplyPort::allocate(1).unwrap();
        SERVICE.set(service.name);
        deliver(service.name, 0, OK);
        let mut session = WmMachClient.window_management_session().unwrap();
        let before = (refs(service.name), refs(session.reply_port.name));
        assert!(matches!(
            session.claim_window(WindowId { pid: 1, idx: 1 }),
            Err(ClientError::RequestTimedOut)
        ));
        assert_eq!((refs(service.name), refs(session.reply_port.name)), before);
    }

    #[test]
    fn session_replies_are_matched_to_their_request() {
        let session = WmMachClient.window_management_session().unwrap();
        let port = session.reply_port.name;
        // A reply to a timed-out request arrives before the current one.
        deliver(port, 1, b"late\0");
        deliver(port, 2, b"current\0");
        deliver(port, 1, b"later\0");
        unsafe {
            let first = receive_message(port, Some(0)).unwrap();
            assert_eq!(receive_reply(port, 2, first).unwrap(), b"current\0");
            let first = receive_message(port, Some(0)).unwrap();
            deliver(port, 3, b"next\0");
            assert_eq!(receive_reply(port, 3, first).unwrap(), b"next\0\0\0\0");
        }
    }

    #[test]
    fn parses_nul_terminated_response_with_alignment_padding() {
        let response: WmResponse =
            parse_json_payload(b"{\"data\":true}\0\0\0", "response").unwrap();
        assert_eq!(response.into_result(), Ok(Value::Bool(true)));
    }

    #[test]
    fn rejects_payload_without_nul() {
        let result = parse_json_payload::<WmResponse>(b"{\"data\":true}", "response");
        assert!(matches!(result, Err(ClientError::MissingTerminator { .. })));
    }

    #[test]
    fn request_uses_the_existing_wire_format() {
        let request = WmRequest::GetWindows { space_id: Some(7) };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({ "get_windows": { "space_id": 7 } })
        );
    }

    #[test]
    fn response_is_untagged() {
        let response = WmResponse::Success {
            data: serde_json::json!({ "ok": true }),
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::json!({ "data": { "ok": true } })
        );
    }
}

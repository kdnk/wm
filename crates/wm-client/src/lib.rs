// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
//! A small, synchronous Rust client for Wm's Mach IPC API.
//!
//! The client talks to the same bootstrap service as `wm` and includes
//! the public request and response wire types.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
compile_error!("wm-client only supports macOS");

use std::collections::VecDeque;
use std::ffi::CStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
pub use wm_protocol::*;

mod mach;

use mach::{MAX_MESSAGE_SIZE, REQUEST_TIMEOUT_MS, ReplyPort};

/// Session request ids carry this bit so that, even after the sequence wraps, they stay
/// clear of the small ids Mach reserves for kernel notifications (64-77) and MIG.
const SESSION_REQUEST_ID_TAG: u32 = 0x4000_0000;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("Wm's Mach service is not registered")]
    ServiceUnavailable,
    #[error("Wm request timed out")]
    RequestTimedOut,
    #[error("invalid Mach service name")]
    InvalidServiceName,
    #[error("Mach operation {operation} failed with code {code}")]
    Mach { operation: &'static str, code: i32 },
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
    pub fn is_available(&self) -> bool { mach::is_service_available() }

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
        let response = mach::request(&request_json, None)?;
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
        let response = mach::request(&request_json, Some(deadline))?;
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
        let reply_port = ReplyPort::persistent()?;
        let request_json =
            serde_json::to_vec(&WmRequest::Subscribe { event }).map_err(ClientError::Encode)?;
        let deadline = Instant::now() + Duration::from_millis(REQUEST_TIMEOUT_MS.into());
        let mut payload = reply_port.send_subscription_request(&request_json)?;
        let mut pending = VecDeque::new();
        loop {
            let value: Value = parse_json_payload(&payload, "response")?;
            // Broadcasts and the reply share a receive port. The event dispatcher
            // can win the race with the config worker, including for ordinary subscribe.
            if value.get("type").is_some() {
                pending.push_back(payload);
                if Instant::now() >= deadline {
                    return Err(ClientError::RequestTimedOut);
                }
                payload = reply_port.receive_payload(Some(deadline))?;
                continue;
            }
            return match parse_json_payload::<JsonWmResponse>(&payload, "response")? {
                WmResponse::Success { .. } => Ok(WmMachSubscription {
                    reply_port,
                    pending: Mutex::new(pending),
                }),
                WmResponse::Error { error } => Err(ClientError::SubscriptionRejected(error)),
                _ => Err(ClientError::UnknownResponse),
            };
        }
    }

    /// Opens a session for claiming windows from Wm. Claims end when they are
    /// released or when the session is dropped (including process exit).
    pub fn window_management_session(&self) -> Result<WindowManagementSession, ClientError> {
        Ok(WindowManagementSession {
            reply_port: ReplyPort::persistent()?,
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
        let response = self.reply_port.request(&payload, id)?;
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
            None => self.reply_port.receive_payload(None)?,
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

#[cfg(test)]
mod tests {
    use super::*;

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

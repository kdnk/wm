// Modified by kdnk for wm on 2026-11-26. Original attribution is preserved in LICENSE.
use std::ffi::{CString, c_char, c_int, c_void};
use std::mem::{MaybeUninit, size_of};
use std::ptr::{addr_of_mut, copy_nonoverlapping};
use std::thread;
use std::time::{Duration, Instant};

use wm_protocol::DEFAULT_SERVICE_NAME;

use crate::ClientError;

pub(super) const MAX_MESSAGE_SIZE: usize = 262_144;

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
pub(super) const REQUEST_TIMEOUT_MS: u32 = if cfg!(test) { 500 } else { 10_000 };
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_PORT_RIGHT_RECEIVE: c_int = 1;
const MACH_PORT_LIMITS_INFO: c_int = 1;
const MACH_PORT_LIMITS_INFO_COUNT: u32 = 1;
const MACH_PORT_QLIMIT_LARGE: u32 = 1024;
const TASK_BOOTSTRAP_PORT: c_int = 4;

pub(super) fn is_service_available() -> bool { ServicePort::lookup().is_ok() }

pub(super) fn request(payload: &[u8], deadline: Option<Instant>) -> Result<Vec<u8>, ClientError> {
    Ok(send_request(payload, None, deadline)?.payload)
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

struct ReceivedMessage {
    id: i32,
    payload: Vec<u8>,
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
pub(super) struct ReplyPort {
    name: MachPort,
}

struct ServicePort {
    name: MachPort,
}

impl ServicePort {
    fn lookup() -> Result<Self, ClientError> {
        let service_name = service_name()?;
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

        let mut name = 0;
        let result = unsafe { bootstrap_look_up(bootstrap_port, service_name.as_ptr(), &mut name) };
        let _ = unsafe { mach_port_deallocate(mach_task_self(), bootstrap_port) };
        if result != KERN_SUCCESS {
            return Err(ClientError::ServiceUnavailable);
        }
        Ok(Self { name })
    }
}

impl Drop for ServicePort {
    fn drop(&mut self) {
        unsafe {
            let _ = mach_port_deallocate(mach_task_self(), self.name);
        }
    }
}

impl ReplyPort {
    pub(super) fn persistent() -> Result<Self, ClientError> {
        Self::allocate(MACH_PORT_QLIMIT_LARGE)
    }

    pub(super) fn send_subscription_request(&self, payload: &[u8]) -> Result<Vec<u8>, ClientError> {
        Ok(send_request(payload, Some((self, self.name as i32)), None)?.payload)
    }

    pub(super) fn request(&self, payload: &[u8], id: i32) -> Result<Vec<u8>, ClientError> {
        let first = send_request(payload, Some((self, id)), None)?;
        self.receive_matching_reply(id, first)
    }

    pub(super) fn receive_payload(
        &self,
        deadline: Option<Instant>,
    ) -> Result<Vec<u8>, ClientError> {
        Ok(self.receive(deadline.map(remaining_timeout))?.payload)
    }

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

    fn receive(&self, timeout: Option<u32>) -> Result<ReceivedMessage, ClientError> {
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
                self.name,
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
            .saturating_sub(size_of::<MachMessageHeader>() as u32)
            as usize;
        if payload_len > MAX_MESSAGE_SIZE {
            unsafe { mach_msg_destroy(header_ptr) };
            return Err(ClientError::MessageTooLarge);
        }
        let data = unsafe { addr_of_mut!((*buffer_ptr).message.data) }.cast::<u8>();
        let payload = unsafe { std::slice::from_raw_parts(data, payload_len) }.to_vec();
        let id = unsafe { (*header_ptr).id };
        unsafe { mach_msg_destroy(header_ptr) };
        Ok(ReceivedMessage { id, payload })
    }

    fn receive_matching_reply(
        &self,
        id: i32,
        mut message: ReceivedMessage,
    ) -> Result<Vec<u8>, ClientError> {
        let deadline = Instant::now() + Duration::from_millis(REQUEST_TIMEOUT_MS.into());
        while message.id != id {
            message = self.receive(Some(remaining_timeout(deadline)))?;
        }
        Ok(message.payload)
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

fn find_service_once() -> Result<ServicePort, ClientError> {
    #[cfg(test)]
    if let port @ 1.. = tests::SERVICE.get() {
        unsafe { mach_port_mod_refs(mach_task_self(), port, tests::MACH_PORT_RIGHT_SEND, 1) };
        return Ok(ServicePort { name: port });
    }
    ServicePort::lookup()
}

fn find_service_with_retry() -> Result<ServicePort, ClientError> {
    for attempt in 0..5 {
        match find_service_once() {
            Ok(port) => return Ok(port),
            Err(ClientError::InvalidServiceName) => return Err(ClientError::InvalidServiceName),
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(50 * (1 << attempt)));
    }
    Err(ClientError::ServiceUnavailable)
}

fn remaining_timeout(deadline: Instant) -> u32 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(u32::MAX.into()) as u32
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

fn send_request(
    payload: &[u8],
    persistent: Option<(&ReplyPort, i32)>,
    deadline: Option<Instant>,
) -> Result<ReceivedMessage, ClientError> {
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(ClientError::MessageTooLarge);
    }

    let service_port = if deadline.is_some() {
        find_service_once()?
    } else {
        find_service_with_retry()?
    };
    let owned_reply_port;
    let (reply_port, id, disposition) = match persistent {
        Some((reply, id)) => (reply, id, MACH_MSG_TYPE_COPY_SEND),
        None => {
            owned_reply_port = ReplyPort::allocate(1)?;
            (
                &owned_reply_port,
                owned_reply_port.name as i32,
                MACH_MSG_TYPE_MAKE_SEND,
            )
        }
    };

    let header = MachMessageHeader {
        bits: message_bits(MACH_MSG_TYPE_COPY_SEND, disposition),
        size: 0,
        remote_port: service_port.name,
        local_port: reply_port.name,
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

    reply_port.receive(Some(request_timeout(deadline)?))
}

const fn message_bits(remote: u32, local: u32) -> u32 { remote | (local << 8) }

#[cfg(test)]
mod tests;

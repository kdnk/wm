// Modified by kdnk for wm on 2026-11-26. Original attribution is preserved in LICENSE.
use super::*;
use crate::{
    EventKind, SESSION_REQUEST_ID_TAG, WindowId, WmEvent, WmMachClient, WmRequest, WmResponse,
};

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
            let (id, reply, len) = unsafe { ((*header).id, (*header).remote_port, (*header).size) };
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
fn subscription_keeps_events_received_before_acknowledgement_in_order() {
    let (_service, server) = serve(1, |id, reply, request| {
        assert!(matches!(request, WmRequest::Subscribe {
            event: EventKind::ConfigChanged
        }));
        let event = |revision| {
            let payload = format!("{{\"type\":\"config_changed\",\"revision\":{revision}}}\0");
            deliver(reply, 0, payload.as_bytes());
        };
        event(1);
        event(2);
        deliver(reply, id, OK);
        // Keep receives bounded even if the client drops both early events.
        for revision in 3..=5 {
            event(revision);
        }
    });
    let subscription = WmMachClient.subscribe(EventKind::ConfigChanged).unwrap();
    server.join().unwrap();
    for revision in 1..=3 {
        assert_eq!(subscription.recv_event().unwrap(), WmEvent::ConfigChanged {
            revision
        });
    }
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
    let port = &session.reply_port;
    deliver(port.name, 1, b"late\0");
    deliver(port.name, 2, b"current\0");
    deliver(port.name, 1, b"later\0");
    let first = port.receive(Some(0)).unwrap();
    assert_eq!(port.receive_matching_reply(2, first).unwrap(), b"current\0");
    let first = port.receive(Some(0)).unwrap();
    deliver(port.name, 3, b"next\0");
    assert_eq!(port.receive_matching_reply(3, first).unwrap(), b"next\0\0\0\0");
}

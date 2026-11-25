// Modified by kdnk for wm on 2026-11-18. Original attribution is preserved in LICENSE.
use std::path::Path;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use serde_json::{Value, json};

use super::*;
use crate::actor::app::WindowId;
use crate::actor::reactor::testing::{
    Apps, fullscreen_startup_space_state, make_windows, space_state_event, test_context,
    test_context_with_workspace_count, test_reactor,
};
use crate::actor::reactor::{Event, Reactor};
use crate::common::config::WorkspaceSelector;
use crate::layout_engine::{LayoutCommand, LayoutEngine};
use crate::sys::mach::{
    MACH_RCV_MSG, mach_allocate_reply_port, mach_deallocate_reply_port, mach_msg,
};
use crate::sys::screen::SpaceId;

#[test]
fn config_ipc_queries_and_commands_complete_while_the_reactor_is_busy() {
    struct ReplyPort(u32);
    impl Drop for ReplyPort {
        fn drop(&mut self) { unsafe { mach_deallocate_reply_port(self.0) } }
    }

    let directory = tempfile::tempdir().unwrap();
    let (reactor_tx, _reactor_events) = crate::actor::channel();
    let (events, _broadcasts) = crate::actor::channel();
    let mut config = crate::common::config::Config::default();
    config.settings.animate = true;
    let config_tx = config_actor::ConfigActor::spawn_with_path(
        config,
        reactor_tx.clone(),
        events,
        directory.path().join("config.toml"),
    );
    let handler = IpcRequestHandler {
        reactor: Rc::new(RefCell::new(test_reactor())),
        reactor_tx,
        server_state: std::sync::Arc::new(subscriptions::ServerState::new()),
        config_worker: ConfigWorker::spawn(config_tx).unwrap(),
    };
    let _busy = handler.reactor.borrow_mut();
    let port = ReplyPort(unsafe { mach_allocate_reply_port() }.unwrap());
    let request = |json: &str| -> Value {
        let mut header = mach_msg_header_t {
            msgh_bits: 0,
            msgh_size: 0,
            msgh_remote_port: port.0,
            msgh_local_port: 0,
            msgh_voucher_port: 0,
            msgh_id: 0,
        };
        handler.handle_message(json.as_bytes(), port.0, &mut header);
        const MACH_RCV_TIMEOUT: u32 = 0x100;
        let mut buffer = [0u64; 4096];
        let received = buffer.as_mut_ptr().cast::<mach_msg_header_t>();
        let status = unsafe {
            mach_msg(
                received,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                0,
                size_of_val(&buffer) as u32,
                port.0,
                7000,
                0,
            )
        };
        assert_eq!(status, 0, "No Mach reply for {json}");
        let bytes = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size_of_val(&buffer))
        };
        let size_offset = std::mem::offset_of!(mach_msg_header_t, msgh_size);
        let (size, _) = bytes[size_offset..].split_first_chunk().unwrap();
        let payload = &bytes[size_of::<mach_msg_header_t>()..u32::from_ne_bytes(*size) as usize];
        serde_json::from_slice(std::ffi::CStr::from_bytes_until_nul(payload).unwrap().to_bytes())
            .unwrap()
    };
    let query = || request(r#""get_config""#);
    assert_eq!(query()["data"]["settings"]["animate"], true);
    assert_eq!(
        request(r#"{"execute_command":{"command":{"config":{"set_animate":false}}}}"#),
        json!({"data": "Config applied successfully"})
    );
    assert_eq!(query()["data"]["settings"]["animate"], false);
    assert_eq!(
        request(r#"{"execute_command":{"command":"{\"Config\":\"get_config\"}"}}"#),
        json!({"data": "Config applied successfully"})
    );
    assert_eq!(
        request(r#"{"execute_command":{"command":{"config":{"set_animation_fps":241}}}}"#),
        json!({"error": {"message": "Invalid animation_fps value: 241. Must be between 0 and 240"}})
    );
    assert_eq!(
        request(r#""get_binding_mode""#),
        json!({"error": {"message": "Reactor is busy"}})
    );
}

fn save(reactor: &mut Reactor, path: &Path) -> WmResponse {
    let response = encode_reactor_response(reactor, WmRequest::ExecuteCommand {
        command: WmCommand::Reactor(wm_protocol::ReactorCommand::SaveLayout {
            path: path.to_owned(),
        }),
    });
    serde_json::from_slice(response.strip_suffix(&[0]).unwrap()).unwrap()
}

#[test]
fn only_the_launchd_process_can_checkpoint_its_master_layout() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1200., 800.));
    apps.make_app_and_settle_on_screen(&mut reactor, screen, SpaceId::new(1), 1, make_windows(1));
    let directory = tempfile::tempdir().unwrap();
    let master = directory.path().join("layout.ron");
    std::fs::write(&master, "previous snapshot").unwrap();
    let mut checkpoint = |pid| {
        let bytes = checkpoint_service(&mut reactor, pid, master.clone());
        serde_json::from_slice::<WmResponse>(bytes.strip_suffix(&[0]).unwrap()).unwrap()
    };
    let pid = std::process::id();
    assert!(matches!(checkpoint(pid + 1), WmResponse::Error { .. }));
    assert_eq!(std::fs::read_to_string(&master).unwrap(), "previous snapshot");
    assert_eq!(checkpoint(pid), WmResponse::Success {
        data: json!("Layout saved")
    });
    LayoutEngine::validate_file(master).unwrap();
}

#[test]
fn save_layout_acknowledges_only_a_durable_snapshot() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1200., 800.));
    apps.make_app_and_settle_on_screen(&mut reactor, screen, SpaceId::new(1), 1, make_windows(2));
    let directory = tempfile::tempdir().unwrap();
    let blocker = directory.path().join("file");
    std::fs::write(&blocker, "previous snapshot").unwrap();
    let blocked_path = blocker.join("layout.ron");
    assert!(matches!(
        save(&mut reactor, &blocked_path),
        WmResponse::Error { .. }
    ));
    let response = checkpoint_service(&mut reactor, std::process::id(), blocked_path);
    let WmResponse::<Value>::Error { error } =
        serde_json::from_slice(response.strip_suffix(&[0]).unwrap()).unwrap()
    else {
        panic!("checkpoint unexpectedly succeeded with a file blocking its parent directory");
    };
    assert_ne!(error["retryable"], true);
    assert_eq!(std::fs::read_to_string(&blocker).unwrap(), "previous snapshot");

    let snapshot = directory.path().join("layout.ron");
    assert_eq!(save(&mut reactor, &snapshot), WmResponse::Success {
        data: json!("Layout saved")
    });
    LayoutEngine::validate_file(snapshot).unwrap();
}

#[test]
fn saving_before_display_discovery_preserves_the_previous_snapshot() {
    let mut reactor = test_reactor();
    let snapshot = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(snapshot.path(), "previous snapshot").unwrap();
    assert!(matches!(
        save(&mut reactor, snapshot.path()),
        WmResponse::Error { .. }
    ));
    let response = checkpoint_service(&mut reactor, std::process::id(), snapshot.path().into());
    let WmResponse::<Value>::Error { error } =
        serde_json::from_slice(response.strip_suffix(&[0]).unwrap()).unwrap()
    else {
        panic!("checkpoint unexpectedly succeeded before display discovery");
    };
    assert_eq!(error["retryable"], true);
    assert_eq!(
        std::fs::read_to_string(snapshot.path()).unwrap(),
        "previous snapshot"
    );
}

#[test]
fn service_checkpoint_preserves_the_desktop_while_the_only_display_is_fullscreen() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1200., 800.));
    let desktop = SpaceId::new(1);
    let windows = make_windows(2);
    apps.make_app_and_settle_on_screen(&mut reactor, screen, desktop, 1, windows.clone());
    let before = workspace_snapshot(&reactor, 0);

    reactor.handle_events(vec![fullscreen_startup_space_state(
        screen,
        "test-display-0".into(),
        desktop,
        SpaceId::new(0x400000001),
    )]);
    let snapshot = tempfile::NamedTempFile::new().unwrap();
    let response = checkpoint_service(&mut reactor, std::process::id(), snapshot.path().into());
    let response: WmResponse =
        serde_json::from_slice(response.strip_suffix(&[0]).unwrap()).unwrap();
    assert_eq!(response, WmResponse::Success { data: json!("Layout saved") });

    let layout =
        LayoutEngine::deserialize_from_str(&std::fs::read_to_string(snapshot.path()).unwrap())
            .unwrap();
    let mut restarted = Reactor::new_for_test(layout);
    Apps::new().make_app_and_settle_on_screen(&mut restarted, screen, desktop, 1, windows);
    assert_eq!(workspace_snapshot(&restarted, 0), before);
}

fn workspace_snapshot(reactor: &Reactor, index: usize) -> Value {
    fn tree(reactor: &Reactor, node: &wm_protocol::ContainerTreeNode) -> Value {
        json!({
            "frame": node.frame,
            "window": node.window_id.map(|id| {
                reactor.query_window_info(WindowId::new(id.pid, id.idx)).unwrap_or_else(|| {
                    panic!("missing restored window {id:?}; live windows: {:?}", reactor.query_windows(None))
                }).info.title
            }),
            "children": node.children.iter().map(|child| tree(reactor, child)).collect::<Vec<_>>(),
        })
    }
    let layout = reactor.query_layout_state(Some(1), Some(index)).unwrap();
    json!({
        "active": layout.is_active_workspace,
        "tree": tree(reactor, &layout.container_tree),
    })
}

#[test]
fn saved_session_restores_geometry_order_and_workspaces_with_new_window_identities() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(2);
    let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1200., 800.));
    let space = SpaceId::new(1);
    let windows = make_windows(4)
        .into_iter()
        .map(|mut window| {
            window.bundle_id = Some("com.testapp1".into());
            window.frame.size.width = 700.;
            window
        })
        .collect::<Vec<_>>();
    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, windows.clone());
    reactor.handle_test_layout_command(LayoutCommand::ResizeWindowBy { amount: 0.17 });
    reactor.handle_test_layout_command(LayoutCommand::SwapWindows(
        WindowId::new(1, 1).into(),
        WindowId::new(1, 3).into(),
    ));
    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: Some(4),
    });
    reactor.handle_test_layout_command(LayoutCommand::ScrollStrip { delta: 1. });
    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));
    apps.simulate_until_quiet(&mut reactor);
    let before = [
        workspace_snapshot(&reactor, 0),
        workspace_snapshot(&reactor, 1),
    ];
    assert_eq!(before[0]["active"], false);
    assert_eq!(before[1]["active"], true);
    let columns = before[0]["tree"]["children"].as_array().unwrap();
    assert!(
        columns
            .iter()
            .any(|column| column["frame"]["origin"]["x"].as_f64().unwrap() < 0.)
    );
    assert!(
        columns
            .iter()
            .any(|column| { column["frame"]["size"]["width"].as_f64().unwrap() != 700. })
    );

    let snapshot = tempfile::NamedTempFile::new().unwrap();
    assert!(matches!(
        save(&mut reactor, snapshot.path()),
        WmResponse::Success { .. }
    ));
    let layout =
        LayoutEngine::deserialize_from_str(&std::fs::read_to_string(snapshot.path()).unwrap())
            .unwrap();
    let mut restarted = Reactor::new_for_test(layout);
    restarted.handle_events(vec![space_state_event(vec![screen], vec![Some(space)])]);
    let mut restarted_apps = Apps::new();
    let mut discovery = restarted_apps.make_app_with_opts(
        2,
        windows.into_iter().rev().collect(),
        None,
        false,
        true,
    );
    for event in &mut discovery {
        if let Event::ApplicationLaunched { info, .. } = event {
            info.bundle_id = Some("com.testapp1".into());
        }
    }
    restarted.handle_events(discovery);
    restarted_apps.simulate_until_quiet(&mut restarted);
    assert_eq!(
        [
            workspace_snapshot(&restarted, 0),
            workspace_snapshot(&restarted, 1)
        ],
        before,
    );
}

// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::future::Future;
use std::path::Path;
use std::{io, process};

use clap::Parser;
use objc2::MainThreadMarker;
use objc2_application_services::AXUIElement;
use tokio::join;
use wm::actor::config::ConfigActor;
use wm::actor::config_watcher::ConfigWatcher;
use wm::actor::input::Input;
use wm::actor::menu_bar::Menu;
use wm::actor::mission_control::MissionControlActor;
use wm::actor::mission_control_observer::NativeMissionControl;
use wm::actor::notification_center::NotificationCenter;
use wm::actor::process::ProcessActor;
use wm::actor::reactor::{self, Reactor};
use wm::actor::spaces::SpacesActor;
use wm::actor::stack_line::StackLine;
use wm::actor::window_notify as window_notify_actor;
use wm::actor::wm_controller::{self, WmController};
use wm::cli::Cli;
use wm::common::config::{Config, config_file, restore_file};
use wm::common::log;
use wm::common::util::execute_startup_commands;
use wm::ipc;
use wm::layout_engine::LayoutEngine;
use wm::model::tx_store::WindowTxStore;
use wm::sys::accessibility::ensure_accessibility_permission;
use wm::sys::executor::Executor;
use wm::sys::mach::init_window_sub_level_server_port;
use wm::sys::screen::{CoordinateConverter, displays_have_separate_spaces};
use wm::sys::skylight::{
    CGEnableEventStateCombining, CGSEventType, CGSetLocalEventsSuppressionInterval, KnownCGSEvent,
    SLSWindowManagementBridgeSetDelegate,
};

embed_plist::embed_info_plist!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/Info.plist"));

/// Actors cannot recover after exiting; panic so the service can restart.
async fn supervise(name: &'static str, fut: impl Future<Output = ()>) {
    fut.await;
    panic!("{name} exited");
}

fn spawn_supervised<F: Future<Output = ()> + 'static>(
    name: &'static str,
    task: impl FnOnce() -> F + Send + 'static,
) {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || Executor::run(supervise(name, task())))
        .unwrap_or_else(|error| panic!("failed to spawn {name} thread: {error}"));
}

fn read_startup_config(path: &Path, allow_missing: bool) -> anyhow::Result<Config> {
    match Config::read(path) {
        Err(error)
            if allow_missing
                && error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(Config::default())
        }
        result => result,
    }
}

fn main() {
    let mut opt = Cli::parse();
    if opt.run_command() {
        return;
    }

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: We are single threaded at this point.
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }
    log::init_logging();
    install_panic_hook();

    let mtm = MainThreadMarker::new().unwrap();
    {
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
        let app = NSApplication::sharedApplication(mtm);
        let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        NSApplication::load();
    }

    unsafe { SLSWindowManagementBridgeSetDelegate(std::ptr::null_mut()) };

    wm::sys::hotkey::refresh_virtual_keymap(mtm);

    let config_path = opt.config.clone().unwrap_or_else(config_file);
    let mut config = match read_startup_config(&config_path, opt.config.is_none()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "Could not load config file at {}: {error:#}",
                config_path.display()
            );
            process::exit(1);
        }
    };
    config.settings.animate &= !opt.no_animate;
    config.settings.default_disable |= opt.default_disable;

    ensure_accessibility_permission();
    init_window_sub_level_server_port();

    if !displays_have_separate_spaces() {
        eprintln!(
            "Wm detected that the macOS setting \"Displays have separate Spaces\" \
is disabled. Wm currently requires this setting to be enabled. \
Enable it in System Settings > Desktop & Dock (Mission Control) and restart Wm."
        );
        std::process::exit(1);
    }

    let startup_commands = config.settings.run_on_start.clone();

    let (broadcast_tx, broadcast_rx) = wm::actor::channel();

    let path = restore_file();
    let mut layout = match LayoutEngine::load_for_startup_restore(path.clone()) {
        Ok(layout) => layout,
        Err(error) => {
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                eprintln!(
                    "Could not restore master file at {}; starting with a fresh layout: {error}",
                    path.display()
                );
            }
            LayoutEngine::new(
                &config.virtual_workspaces,
                &config.settings.layout,
                Some(broadcast_tx.clone()),
            )
        }
    };
    if let Err(error) = layout.finish_loading(
        &config.virtual_workspaces,
        &config.settings.layout,
        Some(broadcast_tx.clone()),
    ) {
        eprintln!("Could not apply workspace settings to restored layout: {error}");
        process::exit(1);
    }
    let (input_tx, input_rx) = wm::actor::channel();
    let (menu_tx, menu_rx) = wm::actor::channel();
    let (stack_line_tx, stack_line_rx) = wm::actor::channel();
    let (wnd_tx, wnd_rx) = wm::actor::channel();
    let (borders_tx, borders_rx) = wm::actor::window_borders::channel();
    let window_tx_store = WindowTxStore::new();
    let native_motion_active: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
    let (reactor, startup_ready) = Reactor::spawn(
        config.clone(),
        layout,
        reactor::Record::new(opt.record.as_deref()),
        input_tx.clone(),
        broadcast_tx.clone(),
        menu_tx.clone(),
        stack_line_tx.clone(),
        borders_tx.clone(),
        Some((wnd_tx.clone(), window_tx_store.clone())),
        opt.one,
        native_motion_active.clone(),
    );
    let events_tx = reactor.sender();

    let config_tx = ConfigActor::spawn_with_path(
        config.clone(),
        events_tx.clone(),
        broadcast_tx.clone(),
        config_path.clone(),
    );

    ConfigWatcher::spawn(config_tx.clone(), config_path.clone());

    let server_state = match ipc::run_mach_server(reactor.clone(), config_tx.clone()) {
        Ok(state) => state,
        Err(err) => {
            eprintln!("{}", err);
            process::exit(1);
        }
    };

    let _window_recovery =
        wm::sys::window_recovery::start().expect("could not start window recovery");

    let mach_bridge_rx = broadcast_rx;

    let server_state_for_bridge = server_state.clone();
    std::thread::spawn(move || {
        let mut rx = mach_bridge_rx;
        let server_state = server_state_for_bridge;
        loop {
            match rx.blocking_recv() {
                Some((_span, event)) => {
                    server_state.publish(event);
                }
                None => {
                    break;
                }
            }
        }
    });

    let wm_config = wm_controller::Config {
        restore_file: restore_file(),
        config: config.clone(),
    };
    let mc_channel =
        wm::actor::mission_control::channel_if_enabled(&config.settings.ui.mission_control);
    let mc_tx = mc_channel.as_ref().map(|(tx, _)| tx.clone());
    let (_mc_native_tx, mc_native_rx) = wm::actor::channel();
    let (wm_controller, wm_controller_sender) = WmController::new(
        wm_config,
        config_tx.clone(),
        events_tx.clone(),
        input_tx.clone(),
        stack_line_tx.clone(),
        mc_tx.clone(),
        Some(window_tx_store.clone()),
    );

    let (spaces_actor, spaces_tx) =
        SpacesActor::new(events_tx.clone(), wm_controller_sender.clone());
    events_tx.send(reactor::Event::RegisterSenders {
        wm: wm_controller_sender.clone(),
        spaces: spaces_tx.clone(),
        topology_guard: spaces_actor.topology_guard(),
    });
    let wn_actor = window_notify_actor::WindowNotify::new(
        events_tx.clone(),
        spaces_tx.clone(),
        wnd_rx,
        &[
            // this event seems bugged
            //CGSEventType::Known(KnownCGSEvent::SpaceCurrentChanged),
            // repl for aboce
            CGSEventType::Known(KnownCGSEvent::WorkspaceDidChange),
            CGSEventType::Known(KnownCGSEvent::ManagedSpaceMembershipUpdated),
            CGSEventType::Known(KnownCGSEvent::SpaceWindowManagementCapabilitiesChanged),
            CGSEventType::Known(KnownCGSEvent::SpaceWindowDestroyed),
            CGSEventType::Known(KnownCGSEvent::SpaceWindowCreated),
            CGSEventType::Known(KnownCGSEvent::WindowClosed),
            // Native focus wakeups. Payload identities are racy, so adjacent
            // events are coalesced before re-querying WindowServer key focus.
            CGSEventType::Known(KnownCGSEvent::WindowReordered),
            CGSEventType::Known(KnownCGSEvent::WindowUnhidden),
            CGSEventType::Known(KnownCGSEvent::WindowHidden),
            CGSEventType::Known(KnownCGSEvent::WindowManagerSpaceFrontConnectionChanged),
            CGSEventType::Known(KnownCGSEvent::WindowManagerGlobalFrontConnectionChanged),
            CGSEventType::Known(KnownCGSEvent::SpaceCreated),
            CGSEventType::Known(KnownCGSEvent::SpaceDestroyed),
        ],
        borders_tx,
        Some(window_tx_store.clone()),
    );

    let notification_center =
        NotificationCenter::new(wm_controller_sender.clone(), spaces_tx.clone());

    let process_actor = ProcessActor::new(wm_controller_sender.clone(), spaces_tx);

    let menu = Menu::new(
        config.clone(),
        menu_rx,
        events_tx.clone(),
        config_tx.clone(),
        mtm,
    );
    let stack_line = StackLine::new(
        config.clone(),
        stack_line_rx,
        stack_line_tx.clone(),
        mtm,
        events_tx.clone(),
        CoordinateConverter::default(),
    );

    let mission_control = mc_channel.map(|(tx, rx)| {
        MissionControlActor::new(
            config.settings.ui.mission_control.clone(),
            config.virtual_workspaces.mode == wm::common::config::WorkspaceMode::Global,
            rx,
            tx,
            reactor.clone(),
            mtm,
            input_tx.clone(),
            stack_line_tx.clone(),
        )
    });
    let mission_control_native = NativeMissionControl::new(events_tx.clone(), mc_native_rx);

    if config.settings.default_disable {
        println!(
            "NOTICE: by default wm starts in a deactivated state.
            you must activate it by using the toggle_spaces_activated command.
            by default this is bound to Alt+Z but can be changed in the config file."
        );
    }

    unsafe { AXUIElement::new_system_wide().set_messaging_timeout(1.0) };

    CGSetLocalEventsSuppressionInterval(0.0);
    CGEnableEventStateCombining(false);

    // Construct the unified HID tap on its dedicated input CFRunLoop thread.
    let input_config = config.clone();
    let input_wm_sender = wm_controller_sender.clone();
    let input_mc_tx = mc_tx;
    spawn_supervised("input", move || {
        Input::new(
            input_config,
            events_tx,
            input_rx,
            input_wm_sender,
            input_mc_tx,
            native_motion_active,
        )
        .run()
    });

    Executor::run_main(mtm, async move {
        join!(
            async move {
                if startup_ready.await.is_ok() {
                    execute_startup_commands(&startup_commands);
                }
            },
            supervise("wm_controller", wm_controller.run()),
            supervise(
                "notification_center",
                notification_center.watch_for_notifications()
            ),
            supervise("spaces", spaces_actor.run()),
            supervise("menu", menu.run()),
            supervise("stack_line", stack_line.run()),
            supervise("window_borders", borders_rx.run(wnd_tx)),
            supervise("window_notify", wn_actor.run()),
            supervise("mc_native", mission_control_native.run()),
            async move {
                if let Some(actor) = mission_control {
                    actor.run().await;
                }
            },
            supervise("process_actor", process_actor.run()),
        );
    });
}

#[cfg(panic = "unwind")]
fn install_panic_hook() {
    // Abort on panic instead of propagating panics to the main thread.
    // See Cargo.toml for why we don't use panic=abort everywhere.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        original_hook(info);
        std::process::abort();
    }));
}

#[cfg(not(panic = "unwind"))]
fn install_panic_hook() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_uses_defaults_only_for_a_missing_default_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        assert!(read_startup_config(&path, true).is_ok());
        assert!(read_startup_config(&path, false).is_err());

        std::fs::write(&path, "invalid config").unwrap();
        assert!(read_startup_config(&path, true).is_err());
        assert!(read_startup_config(&path, false).is_err());
    }
}

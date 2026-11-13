use std::path::Path;
use std::time::Duration;

use cgs::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationOptions, NSMenu, NSRunningApplication};
use objc2_core_foundation::CGRect;
use tokio::sync::mpsc::UnboundedSender;

use crate::actor::app::WindowId;
use crate::actor::{config, reactor};
use crate::common::config::{Config, ConfigCommand};
use crate::layout_engine::LayoutCommand;
use crate::sys::screen::SpaceId;
use crate::sys::timer::Timer;
use crate::ui::menu_bar::{MenuAction, MenuIcon};
use crate::{actor, common};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MenuLayoutPosition {
    pub column_id: crate::layout_engine::ColumnId,
    pub column: usize,
    pub row: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MenuWindow {
    pub id: WindowId,
    pub app_name: String,
    pub bundle_id: Option<String>,
    pub title: String,
    pub focused: bool,
    pub layout_position: Option<MenuLayoutPosition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MenuColumn {
    pub id: crate::layout_engine::ColumnId,
    pub members: Vec<WindowId>,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub columns: Vec<MenuColumn>,
    pub key: crate::layout_engine::VirtualWorkspaceId,
    pub id: String,
    pub index: usize,
    pub name: String,
    pub layout_mode: String,
    pub is_active: bool,
    pub window_count: usize,
    pub windows: Vec<CGRect>,
    pub window_list: Vec<MenuWindow>,
    pub has_fullscreen: bool,
}

/// Menu-bar-only projection; workspace indices remain local to each space.
#[derive(Debug, Clone)]
pub struct DisplayWorkspaces {
    pub screen_id: crate::sys::screen::ScreenId,
    pub is_builtin: bool,
    pub display_uuid: String,
    pub space: SpaceId,
    pub is_active_context: bool,
    pub space_is_activated: bool,
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub active_space_is_activated: bool,
    pub displays: Vec<DisplayWorkspaces>,
}

impl Update {
    pub fn context_workspaces(&self) -> &[Workspace] {
        self.displays
            .iter()
            .find(|display| display.is_active_context)
            .map(|display| display.workspaces.as_slice())
            .unwrap_or_default()
    }
}

pub enum Event {
    OpenSettings,
    BindingModeChanged(String),
    Update(Update),
    ConfigUpdated(Box<Config>),
}

const LAYOUT_REFRESH_DELAY: Duration = Duration::from_millis(150);
const MENU_UPDATE_DELAY: Duration = Duration::from_millis(150);

fn focused_window(update: &Update) -> Option<WindowId> {
    update
        .displays
        .iter()
        .flat_map(|display| &display.workspaces)
        .flat_map(|workspace| &workspace.window_list)
        .find(|window| window.focused)
        .map(|window| window.id)
}

fn queue_update(
    last: Option<&Update>,
    pending: &mut Option<Update>,
    timer: &Timer,
    update: Update,
) -> Option<Update> {
    if last.is_none_or(|last| focused_window(last) != focused_window(&update)) {
        *pending = None;
        timer.set_next_fire(Duration::MAX);
        return Some(update);
    }
    if pending.is_none() {
        timer.set_next_fire(MENU_UPDATE_DELAY);
    }
    *pending = Some(update);
    None
}

pub struct Menu {
    config: Config,
    rx: Receiver,
    reactor_tx: reactor::Sender,
    config_tx: config::Sender,
    action_tx: UnboundedSender<MenuAction>,
    action_rx: tokio::sync::mpsc::UnboundedReceiver<MenuAction>,
    icon: Option<MenuIcon>,
    /// The `rift --settings` process; checked only when Settings is requested.
    settings: Option<std::process::Child>,
    mtm: MainThreadMarker,
    config_path: std::path::PathBuf,
    last_signature: Option<u64>,
    last_update: Option<Update>,
    binding_mode: String,
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

impl Menu {
    pub fn new(
        config: Config,
        rx: Receiver,
        reactor_tx: reactor::Sender,
        config_tx: config::Sender,
        mtm: MainThreadMarker,
        config_path: std::path::PathBuf,
    ) -> Self {
        // AppKit only dispatches the status menu's key equivalents (⌘, for Settings…)
        // while it tracks if the application has a main menu, even an empty one.
        let app = NSApplication::sharedApplication(mtm);
        if app.mainMenu().is_none() {
            app.setMainMenu(Some(&NSMenu::new(mtm)));
        }
        let (action_tx, action_rx) = tokio::sync::mpsc::unbounded_channel();
        let layout_folder = config.settings.ui.menu_bar.resolved_layout_folder();
        let mut icon = config
            .settings
            .ui
            .menu_bar
            .enabled
            .then(|| MenuIcon::new(mtm, action_tx.clone(), reactor_tx.clone(), &layout_folder));
        if let Some(icon) = &mut icon {
            icon.update_config(&config.settings.ui.menu_bar, &config.keys);
        }
        Self {
            icon,
            settings: None,
            config,
            rx,
            reactor_tx,
            config_tx,
            action_tx,
            action_rx,
            mtm,
            config_path,
            last_signature: None,
            last_update: None,
            binding_mode: "default".into(),
        }
    }

    pub async fn run(mut self) {
        let mut pending: Option<Update> = None;
        // Reusable main-run-loop timers; idle while disarmed, so they cost no thread or wakeup.
        let mut debounce = Timer::manual();
        // Nothing waits on a layout-folder rescan, so it may coalesce with other wakeups.
        let mut refresh_layouts = Timer::manual().with_tolerance(Duration::from_millis(50));

        let mut presentation = Timer::manual();
        let mut presentation_armed = false;
        loop {
            if !presentation_armed
                && self.icon.as_ref().is_some_and(|icon| icon.needs_presentation_refresh())
            {
                presentation.set_next_fire(Duration::from_millis(250));
                presentation_armed = true;
            }
            tokio::select! {
                maybe_action = self.action_rx.recv() => {
                    if let Some(action) = maybe_action {
                        if matches!(action, MenuAction::SaveLayout(_)) {
                            // Give the reactor time to write the file before rereading the folder.
                            refresh_layouts.set_next_fire(LAYOUT_REFRESH_DELAY);
                        }
                        self.handle_action(action);
                    }
                }
                Some(()) = debounce.next(), if pending.is_some() => {
                    if let Some(update) = pending.take() {
                        self.handle_update(update);
                    }
                }
                Some(()) = presentation.next(), if presentation_armed => {
                    presentation_armed = false;
                    if let (Some(icon), Some(update)) = (&mut self.icon, &self.last_update) {
                        icon.update_status_icon(&update.displays, &self.config.settings.ui.menu_bar, &self.binding_mode);
                    }
                }
                Some(()) = refresh_layouts.next() => {
                    self.handle_action(MenuAction::RefreshLayoutFiles);
                }

                maybe = self.rx.recv() => {
                    match maybe {
                        Some((span, event)) => {
                            let _enter = span.enter();
                            match event {
                                Event::Update(update) => {
                                    if let Some(update) = queue_update(self.last_update.as_ref(), &mut pending, &debounce, update) {
                                        self.handle_update(update);
                                    }
                                }
                                Event::BindingModeChanged(mode) => {
                                    self.binding_mode = mode;
                                    if let (Some(icon), Some(update)) = (&mut self.icon, &self.last_update) {
                                        icon.update_status_icon(
                                            &update.displays,
                                            &self.config.settings.ui.menu_bar,
                                            &self.binding_mode,
                                        );
                                    }
                                }
                                Event::ConfigUpdated(cfg) => self.handle_config_updated(*cfg),
                                Event::OpenSettings => self.open_settings(),
                            }
                        }
                        None => {
                            if let Some(update) = pending.take() {
                                self.handle_update(update);
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Focus the running Settings process, or start one. Its exit is noticed here, lazily.
    fn open_settings(&mut self) {
        if let Some(child) = &mut self.settings {
            if let Ok(None) = child.try_wait() {
                // A child still starting has no application yet; it activates itself when ready.
                if let Some(app) =
                    NSRunningApplication::runningApplicationWithProcessIdentifier(child.id() as i32)
                {
                    app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
                }
                return;
            }
            self.settings = None;
        }
        let timing = crate::ui::settings::timing::start();
        let child = std::env::current_exe().and_then(|exe| {
            let mut command = std::process::Command::new(exe);
            if timing.is_some() {
                command.env(
                    "RIFT_SETTINGS_SPAWN_TIME_NS",
                    crate::ui::settings::timing::spawn_time_ns().to_string(),
                );
            }
            command.arg("--settings").arg("--config").arg(&self.config_path).spawn()
        });
        crate::ui::settings::timing::finish(timing, "parent process spawn");
        match child {
            Ok(child) => {
                // Reap on exit so a closed Settings leaves no zombie behind.
                crate::sys::dispatch::reap_on_exit_proc(child.id() as i32);
                self.settings = Some(child);
            }
            Err(error) => tracing::error!(%error, "Could not start Settings"),
        }
    }

    fn handle_update(&mut self, update: Update) {
        self.apply_update(&update);
        self.last_update = Some(update);
    }

    fn apply_update(&mut self, update: &Update) {
        let Some(icon) = &mut self.icon else { return };

        let sig = sig(update);
        if self.last_signature == Some(sig) {
            return;
        }
        self.last_signature = Some(sig);

        icon.sync_workspace_topology(update.context_workspaces(), &self.config.keys);
        icon.update_menu_state(update.active_space_is_activated, update.context_workspaces());
        icon.update_status_icon(
            &update.displays,
            &self.config.settings.ui.menu_bar,
            &self.binding_mode,
        );
    }

    fn handle_config_updated(&mut self, new_config: Config) {
        let should_enable = new_config.settings.ui.menu_bar.enabled;

        self.config = new_config;

        if should_enable && self.icon.is_none() {
            let layout_folder = self.config.settings.ui.menu_bar.resolved_layout_folder();
            self.icon = Some(MenuIcon::new(
                self.mtm,
                self.action_tx.clone(),
                self.reactor_tx.clone(),
                &layout_folder,
            ));
        } else if !should_enable && self.icon.is_some() {
            self.icon = None;
        }

        if let Some(icon) = &mut self.icon {
            icon.update_config(&self.config.settings.ui.menu_bar, &self.config.keys);
        }

        self.last_signature = None;
        if let Some(update) = self.last_update.take() {
            self.handle_update(update);
        }
    }

    fn handle_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::ScopedLayout { space, command } => {
                self.reactor_tx.send(reactor::Event::MenuBarLayoutCommand { space, command })
            }
            MenuAction::ScopedToggle(space) => {
                self.reactor_tx.send(reactor::Event::MenuBarToggleSpace { space })
            }
            MenuAction::ScopedRestore { space, path, scope, source } => self
                .reactor_tx
                .send(reactor::Event::MenuBarRestoreLayout { space, path, scope, source }),
            MenuAction::FocusWindow(window) => {
                self.reactor_tx.send(reactor::Event::MenuBarFocusWindow(window));
            }
            MenuAction::SetLayout(mode) => self
                .send_layout_command(LayoutCommand::SetWorkspaceLayout { workspace: None, mode }),
            MenuAction::NextWorkspace => {
                self.send_layout_command(LayoutCommand::NextWorkspace(None));
            }
            MenuAction::PrevWorkspace => {
                self.send_layout_command(LayoutCommand::PrevWorkspace(None));
            }
            MenuAction::SwitchToWorkspace(workspace) => {
                self.send_layout_command(LayoutCommand::SwitchToWorkspace(workspace));
            }
            MenuAction::RestoreLayout { path, scope, source } => {
                self.reactor_tx.send(reactor::Event::Command(reactor::Command::Reactor(
                    reactor::ReactorCommand::RestoreLayout { path, scope, source },
                )));
            }
            MenuAction::RestoreMasterFile(scope) => {
                self.reactor_tx.send(reactor::Event::Command(reactor::Command::Reactor(
                    reactor::ReactorCommand::RestoreLayout {
                        path: common::config::restore_file(),
                        scope,
                        source: crate::layout_engine::RestoreSource::CurrentSpace,
                    },
                )));
            }
            MenuAction::SaveLayout(path) => {
                self.reactor_tx.send(reactor::Event::Command(reactor::Command::Reactor(
                    reactor::ReactorCommand::SaveLayout { path },
                )));
            }
            MenuAction::SaveMasterFile => {
                self.reactor_tx.send(reactor::Event::Command(reactor::Command::Reactor(
                    reactor::ReactorCommand::SaveLayout {
                        path: common::config::restore_file(),
                    },
                )));
            }
            MenuAction::ToggleSpaceActivated => {
                self.reactor_tx.send(reactor::Event::Command(reactor::Command::Reactor(
                    reactor::ReactorCommand::ToggleSpaceActivated,
                )));
            }
            MenuAction::OpenGitHub => {
                Self::open_path_or_url("https://github.com/acsandmann/rift");
            }
            MenuAction::OpenDocumentation => {
                Self::open_path_or_url("https://acsandmann.github.io/rift-docs/");
            }
            MenuAction::OpenMatrix => {
                Self::open_path_or_url("https://matrix.to/#/#rift:matrix.org");
            }
            MenuAction::OpenSponsor => {
                Self::open_path_or_url("https://github.com/sponsors/acsandmann");
            }
            MenuAction::OpenSettings => self.open_settings(),
            MenuAction::OpenConfig => {
                Self::open_path_or_url(common::config::config_file());
            }
            MenuAction::ReloadConfig => self.reload_config(),
            MenuAction::RefreshLayoutFiles => {
                if let Some(icon) = &mut self.icon {
                    icon.refresh_layout_library();
                }
            }
            MenuAction::QuitRift => {
                self.reactor_tx.send(reactor::Event::Command(reactor::Command::Reactor(
                    reactor::ReactorCommand::SaveAndExit,
                )));
            }
        }
    }

    fn send_layout_command(&self, command: LayoutCommand) {
        self.reactor_tx.send(reactor::Event::Command(reactor::Command::Layout(command)));
    }

    fn open_path_or_url(target: impl AsRef<Path>) {
        cgs::app::Application::open(&target.as_ref().to_string_lossy());
    }

    fn reload_config(&self) {
        let (response, _result) = std::sync::mpsc::sync_channel(1);
        let msg = config::Event::ApplyConfig {
            cmd: ConfigCommand::ReloadConfig,
            response,
        };
        self.config_tx.send(msg);
    }
}

pub(crate) fn sig(update: &Update) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    update.active_space_is_activated.hash(&mut hash);
    update.displays.len().hash(&mut hash);
    for display in &update.displays {
        display.display_uuid.hash(&mut hash);
        display.space.hash(&mut hash);
        display.screen_id.hash(&mut hash);
        display.is_builtin.hash(&mut hash);
        display.is_active_context.hash(&mut hash);
        display.space_is_activated.hash(&mut hash);
        display.workspaces.len().hash(&mut hash);
        for workspace in &display.workspaces {
            workspace.id.hash(&mut hash);
            workspace.index.hash(&mut hash);
            workspace.name.hash(&mut hash);
            workspace.layout_mode.hash(&mut hash);
            workspace.is_active.hash(&mut hash);
            workspace.window_count.hash(&mut hash);
            workspace.window_list.hash(&mut hash);
            workspace.columns.hash(&mut hash);
            workspace.key.hash(&mut hash);
            workspace.has_fullscreen.hash(&mut hash);
            workspace.windows.len().hash(&mut hash);
            for window in &workspace.windows {
                let frame = *window;
                [
                    frame.origin.x.to_bits(),
                    frame.origin.y.to_bits(),
                    frame.size.width.to_bits(),
                    frame.size.height.to_bits(),
                ]
                .hash(&mut hash);
            }
        }
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(layout_mode: &str) -> Workspace {
        Workspace {
            key: Default::default(),
            columns: Vec::new(),
            id: "VirtualWorkspaceId(1v1)".to_string(),
            index: 0,
            name: "main".to_string(),
            layout_mode: layout_mode.to_string(),
            is_active: true,
            window_count: 1,
            windows: Vec::new(),
            window_list: Vec::new(),
            has_fullscreen: false,
        }
    }

    fn update(workspaces: Vec<Workspace>) -> Update {
        Update {
            active_space_is_activated: true,
            displays: vec![DisplayWorkspaces {
                screen_id: crate::sys::screen::ScreenId::new(0),
                is_builtin: false,
                display_uuid: "a".into(),
                space: SpaceId::new(1),
                is_active_context: true,
                space_is_activated: true,
                workspaces,
            }],
        }
    }

    #[test]
    fn grouped_signature_and_command_context() {
        let mut base = update(vec![workspace("bsp")]);
        base.displays.push(DisplayWorkspaces {
            screen_id: crate::sys::screen::ScreenId::new(0),
            is_builtin: false,
            display_uuid: "b".into(),
            space: SpaceId::new(2),
            is_active_context: false,
            space_is_activated: true,
            workspaces: vec![workspace("stack"), workspace("floating")],
        });
        let before = sig(&base);
        assert_eq!(before, sig(&base.clone()));
        assert_eq!(base.context_workspaces().len(), 1);
        let mut changed = base.clone();
        changed.displays[1].workspaces[0].is_active = false;
        assert_ne!(before, sig(&changed));
        changed = base.clone();
        changed.displays.reverse();
        assert_ne!(before, sig(&changed));
        assert_eq!(changed.context_workspaces().len(), 1);
        changed.displays[0].is_active_context = true;
        changed.displays[1].is_active_context = false;
        assert_eq!(changed.context_workspaces().len(), 2);
        changed = base.clone();
        changed.displays.pop();
        assert_ne!(before, sig(&changed));
    }

    #[test]
    fn signature_changes_when_workspace_layout_mode_changes() {
        let base = vec![workspace("bsp")];
        let changed = vec![workspace("master_stack")];

        let before = sig(&update(base));
        let after = sig(&update(changed));

        assert_ne!(before, after);
    }

    #[test]
    fn signature_changes_when_workspace_fullscreen_changes() {
        let base = vec![workspace("bsp")];
        let mut changed = base.clone();
        changed[0].has_fullscreen = true;

        assert_ne!(sig(&update(base)), sig(&update(changed)));
    }
    #[test]
    fn menu_bar_window_signature_tracks_metadata_identity_focus_and_order() {
        let mut base = update(vec![workspace("scrolling")]);
        base.displays[0].workspaces[0].window_list = vec![
            MenuWindow {
                id: WindowId::new(1, 1),
                app_name: "Editor".into(),
                bundle_id: Some("app.editor".into()),
                title: "first".into(),
                focused: true,
                layout_position: None,
            },
            MenuWindow {
                id: WindowId::new(1, 2),
                app_name: "Editor".into(),
                bundle_id: Some("app.editor".into()),
                title: "second".into(),
                focused: false,
                layout_position: None,
            },
        ];
        let before = sig(&base);
        for field in 0..7 {
            let mut changed = base.clone();
            let windows = &mut changed.displays[0].workspaces[0].window_list;
            match field {
                0 => windows[0].title.push_str(" changed"),
                1 => windows[0].focused = false,
                2 => windows[0].id = WindowId::new(2, 1),
                3 => windows[0].app_name.push_str(" changed"),
                4 => windows[0].bundle_id = None,
                5 => {
                    windows[0].layout_position = Some(MenuLayoutPosition {
                        column_id: crate::layout_engine::ColumnId(1),
                        column: 1,
                        row: 0,
                    })
                }
                _ => windows.reverse(),
            }
            assert_ne!(before, sig(&changed), "field {field}");
        }
    }

    #[test]
    fn menu_bar_focus_updates_bypass_metadata_delay_and_discard_stale_pending() {
        use crate::sys::executor::Executor;
        Executor::run(async {
            let mut pending = None;
            let timer = Timer::manual();
            let mut last = update(vec![workspace("scrolling")]);
            last.displays[0].workspaces[0].window_list = (1..=2)
                .map(|idx| MenuWindow {
                    id: WindowId::new(1, idx),
                    app_name: "Editor".into(),
                    bundle_id: None,
                    title: format!("window {idx}"),
                    focused: idx == 1,
                    layout_position: None,
                })
                .collect();
            let initial = queue_update(None, &mut pending, &timer, last.clone()).unwrap();
            assert_eq!(focused_window(&initial), Some(WindowId::new(1, 1)));
            for target in [Some(2), Some(1), None, Some(2)] {
                let mut metadata = last.clone();
                metadata.displays[0].workspaces[0].window_list[0].title.push_str(" updated");
                assert!(
                    queue_update(Some(&last), &mut pending, &timer, metadata.clone()).is_none()
                );
                assert!(pending.is_some());
                for window in &mut metadata.displays[0].workspaces[0].window_list {
                    window.focused = Some(window.id.idx.get()) == target;
                }
                let shown = queue_update(Some(&last), &mut pending, &timer, metadata)
                    .expect("focus must publish without waiting for the metadata timer");
                assert_eq!(focused_window(&shown), target.map(|idx| WindowId::new(1, idx)));
                assert!(
                    pending.is_none(),
                    "an older metadata update must never restore the previous focus"
                );
                last = shown;
            }
        });
    }

    #[test]
    fn menu_bar_publishes_latest_update_during_continuous_title_changes() {
        use crate::sys::executor::Executor;
        Executor::run(async {
            let mut pending = None;
            let mut timer = Timer::manual();
            let mut changes =
                Timer::repeating(Duration::from_millis(100), Duration::from_millis(100));
            let mut timeout = Timer::sleep(Duration::from_millis(450));
            let mut generation = 0;
            let last = update(vec![workspace("initial")]);
            assert!(queue_update(Some(&last), &mut pending, &timer, last.clone()).is_none());
            loop {
                tokio::select! {
                    _ = timer.next() => {
                        assert!(generation >= 1);
                        let shown = pending.take().unwrap();
                        assert_eq!(shown.context_workspaces()[0].name, generation.to_string());
                        break;
                    }
                    _ = changes.next() => {
                        generation += 1;
                        let mut changed = workspace("scrolling");
                        changed.name = generation.to_string();
                        assert!(queue_update(Some(&last), &mut pending, &timer, update(vec![changed])).is_none());
                    }
                    _ = &mut timeout => panic!("continuous updates starved menu publication"),
                }
            }
        });
    }
}

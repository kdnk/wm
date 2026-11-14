// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::path::Path;
use std::time::Duration;

use cgs::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSMenu};
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
    BindingModeChanged(String),
    Update(Update),
    ConfigUpdated(Box<Config>),
}

const LAYOUT_REFRESH_DELAY: Duration = Duration::from_millis(150);
const MENU_UPDATE_DELAY: Duration = Duration::from_millis(150);

fn same_interaction(a: &Update, b: &Update) -> bool {
    a.active_space_is_activated == b.active_space_is_activated
        && a.displays.len() == b.displays.len()
        && a.displays.iter().zip(&b.displays).all(|(a, b)| {
            a.display_uuid == b.display_uuid
                && a.screen_id == b.screen_id
                && a.is_builtin == b.is_builtin
                && a.space == b.space
                && a.is_active_context == b.is_active_context
                && a.space_is_activated == b.space_is_activated
                && a.workspaces.len() == b.workspaces.len()
                && a.workspaces.iter().zip(&b.workspaces).all(|(a, b)| {
                    a.key == b.key
                        && a.id == b.id
                        && a.index == b.index
                        && a.is_active == b.is_active
                        && a.layout_mode == b.layout_mode
                        && a.has_fullscreen == b.has_fullscreen
                        && a.window_count == b.window_count
                        && a.columns == b.columns
                        && a.windows.len() == b.windows.len()
                        && a.window_list.len() == b.window_list.len()
                        && a.window_list.iter().zip(&b.window_list).all(|(a, b)| {
                            (a.id, &a.bundle_id, a.focused, a.layout_position)
                                == (b.id, &b.bundle_id, b.focused, b.layout_position)
                        })
                })
        })
}

fn queue_update(
    last: Option<&Update>,
    pending: &mut Option<Update>,
    timer: &Timer,
    update: Update,
) -> Option<Update> {
    if last.is_none_or(|last| !same_interaction(last, &update)) {
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
    mtm: MainThreadMarker,
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
    ) -> Self {
        // AppKit requires a main menu to dispatch status-menu key equivalents.
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
            config,
            rx,
            reactor_tx,
            config_tx,
            action_tx,
            action_rx,
            mtm,
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
                Self::open_path_or_url("https://github.com/kdnk/wm");
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
            MenuAction::OpenConfig => {
                Self::open_path_or_url(common::config::config_file());
            }
            MenuAction::ReloadConfig => self.reload_config(),
            MenuAction::RefreshLayoutFiles => {
                if let Some(icon) = &mut self.icon {
                    icon.refresh_layout_library();
                }
            }
            MenuAction::QuitWm => {
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

    fn focused_window(update: &Update) -> Option<WindowId> {
        update
            .displays
            .iter()
            .flat_map(|display| &display.workspaces)
            .flat_map(|workspace| &workspace.window_list)
            .find(|window| window.focused)
            .map(|window| window.id)
    }

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
    fn menu_bar_actions_with_unchanged_focus_publish_the_latest_state_immediately() {
        use crate::sys::executor::Executor;
        Executor::run(async {
            let mut last = update(vec![workspace("scrolling"), workspace("bsp")]);
            last.displays[0].workspaces[1].is_active = false;
            last.displays[0].workspaces[1].id = "second".into();
            last.displays[0].workspaces[1].index = 1;
            last.displays[0].workspaces[0].window_list = (1..=2)
                .map(|idx| MenuWindow {
                    id: WindowId::new(1, idx),
                    app_name: "Editor".into(),
                    bundle_id: Some("app.editor".into()),
                    title: format!("window {idx}"),
                    focused: idx == 1,
                    layout_position: None,
                })
                .collect();
            let cases: &[(&str, fn(&mut Update))] = &[
                ("layout mode", |u| {
                    u.displays[0].workspaces[0].layout_mode = "bsp".into()
                }),
                ("fullscreen", |u| {
                    u.displays[0].workspaces[0].has_fullscreen = true
                }),
                ("window order", |u| {
                    u.displays[0].workspaces[0].window_list.reverse()
                }),
                ("window replacement", |u| {
                    u.displays[0].workspaces[0].window_list[1].id = WindowId::new(2, 1)
                }),
                ("application grouping", |u| {
                    u.displays[0].workspaces[0].window_list[1].bundle_id = None
                }),
                ("column assignment", |u| {
                    u.displays[0].workspaces[0].window_list[1].layout_position =
                        Some(MenuLayoutPosition {
                            column_id: crate::layout_engine::ColumnId(1),
                            column: 1,
                            row: 0,
                        })
                }),
                ("empty column", |u| {
                    u.displays[0].workspaces[0].columns.push(MenuColumn {
                        id: crate::layout_engine::ColumnId(1),
                        members: Vec::new(),
                    })
                }),
                ("window removal", |u| {
                    let workspace = &mut u.displays[0].workspaces[0];
                    workspace.window_list.pop();
                    workspace.window_count = 1;
                }),
                ("preview membership", |u| {
                    u.displays[0].workspaces[0].windows.push(CGRect::ZERO)
                }),
                ("workspace order", |u| u.displays[0].workspaces.reverse()),
                ("workspace removal", |u| {
                    u.displays[0].workspaces.pop();
                }),
                ("display attachment", |u| {
                    let mut display = u.displays[0].clone();
                    display.display_uuid = "b".into();
                    display.screen_id = crate::sys::screen::ScreenId::new(1);
                    display.is_active_context = false;
                    u.displays.push(display);
                }),
                ("native space change", |u| u.displays[0].space = SpaceId::new(2)),
                ("active context", |u| u.displays[0].is_active_context = false),
            ];
            for (name, mutate) in cases {
                let timer = Timer::manual();
                let mut pending = None;
                let mut changed = last.clone();
                changed.displays[0].workspaces[0].window_list[0].title = "latest title".into();
                assert!(queue_update(Some(&last), &mut pending, &timer, changed.clone()).is_none());
                mutate(&mut changed);
                let expected = sig(&changed);
                let shown = queue_update(Some(&last), &mut pending, &timer, changed)
                    .unwrap_or_else(|| panic!("{name} waited for metadata timer"));
                assert_eq!(
                    sig(&shown),
                    expected,
                    "{name} must publish the complete latest update"
                );
                assert_eq!(focused_window(&shown), focused_window(&last), "{name}");
                assert!(pending.is_none(), "{name} left stale metadata queued");
            }
        });
    }

    #[test]
    fn menu_bar_empty_workspace_navigation_and_activation_publish_immediately() {
        use crate::sys::executor::Executor;
        Executor::run(async {
            let mut last = update(vec![workspace("bsp"), workspace("bsp")]);
            last.displays[0].workspaces[0].window_count = 0;
            last.displays[0].workspaces[1].window_count = 0;
            last.displays[0].workspaces[1].is_active = false;
            let cases: &[(&str, fn(&mut Update))] = &[
                ("empty workspace navigation", |u| {
                    u.displays[0].workspaces[0].is_active = false;
                    u.displays[0].workspaces[1].is_active = true;
                }),
                ("active space activation", |u| {
                    u.active_space_is_activated = false
                }),
                ("display space activation", |u| {
                    u.displays[0].space_is_activated = false
                }),
            ];
            for (name, mutate) in cases {
                let timer = Timer::manual();
                let mut pending = None;
                let mut changed = last.clone();
                changed.displays[0].workspaces[0].name = "renamed".into();
                assert!(queue_update(Some(&last), &mut pending, &timer, changed.clone()).is_none());
                mutate(&mut changed);
                let expected = sig(&changed);
                let shown = queue_update(Some(&last), &mut pending, &timer, changed)
                    .unwrap_or_else(|| panic!("{name} waited for metadata timer"));
                assert_eq!(sig(&shown), expected, "{name}");
                assert!(focused_window(&shown).is_none());
                assert!(pending.is_none(), "{name} left stale metadata queued");
            }
        });
    }

    #[test]
    fn menu_bar_metadata_changes_still_coalesce() {
        use crate::sys::executor::Executor;
        Executor::run(async {
            let timer = Timer::manual();
            let mut pending = None;
            let mut last = update(vec![workspace("bsp")]);
            last.displays[0].workspaces[0].windows.push(CGRect::ZERO);
            last.displays[0].workspaces[0].window_list.push(MenuWindow {
                id: WindowId::new(1, 1),
                app_name: "Editor".into(),
                bundle_id: None,
                title: "initial".into(),
                focused: true,
                layout_position: None,
            });
            let mut changed = last.clone();
            let workspace = &mut changed.displays[0].workspaces[0];
            workspace.name = "renamed".into();
            workspace.window_list[0].app_name = "renamed app".into();
            workspace.window_list[0].title = "latest title".into();
            workspace.windows[0].origin.x = 42.0;
            workspace.windows[0].size.width = 500.0;
            let expected = sig(&changed);
            assert!(queue_update(Some(&last), &mut pending, &timer, changed).is_none());
            assert_eq!(sig(pending.as_ref().unwrap()), expected);
        });
    }

    #[test]
    fn menu_bar_publishes_latest_update_during_continuous_title_changes() {
        use std::future::poll_fn;

        use crate::sys::executor::Executor;
        Executor::run(async {
            let mut pending = None;
            let mut timer = Timer::manual();
            let mut generation = 0;
            let mut last = update(vec![workspace("scrolling")]);
            last.displays[0].workspaces[0].window_list.push(MenuWindow {
                id: WindowId::new(1, 1),
                app_name: "Editor".into(),
                bundle_id: None,
                title: "initial".into(),
                focused: true,
                layout_position: None,
            });
            tokio::select! {
                biased;
                _ = poll_fn(|cx| {
                    generation += 1;
                    let mut changed = last.clone();
                    changed.displays[0].workspaces[0].window_list[0].title = generation.to_string();
                    assert!(queue_update(Some(&last), &mut pending, &timer, changed).is_none());
                    std::pin::pin!(timer.next()).poll(cx)
                }) => {
                    assert!(generation >= 2);
                    let shown = pending.take().unwrap();
                    assert_eq!(shown.context_workspaces()[0].window_list[0].title, generation.to_string());
                }
                _ = async { Timer::sleep(Duration::from_millis(450)).await } => {
                    panic!("continuous updates starved menu publication")
                }
            }
        });
    }
}

// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::actor::{self, reactor};
use crate::common::config::{Config, ConfigCommand, ConfigDocument, MAX_WORKSPACES};
use crate::model::broadcast::{BroadcastEvent, BroadcastSender};

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

#[derive(Serialize, Deserialize)]
pub enum Event {
    #[serde(skip)]
    QueryConfig(SyncSender<Config>),
    #[serde(skip)]
    ApplyConfig {
        cmd: ConfigCommand,
        #[serde(skip)]
        response: SyncSender<Result<(), String>>,
    },
}

pub struct ConfigActor {
    config: Config,
    document: ConfigDocument,
    /// Exact file contents this actor last read or wrote; `None` when the file was absent.
    /// Saves compare against it so external edits are never silently overwritten.
    disk: Option<String>,
    source_revision: u64,
    reactor_tx: reactor::Sender,
    events: BroadcastSender,
    config_path: PathBuf,
}

impl ConfigActor {
    pub fn spawn(config: Config, reactor_tx: reactor::Sender, events: BroadcastSender) -> Sender {
        Self::spawn_with_path(config, reactor_tx, events, crate::common::config::config_file())
    }

    pub fn spawn_with_path(
        config: Config,
        reactor_tx: reactor::Sender,
        events: BroadcastSender,
        config_path: PathBuf,
    ) -> Sender {
        let (tx, rx) = actor::channel();
        std::thread::Builder::new()
            .name("config".to_string())
            .spawn(move || Self::load(config, reactor_tx, events, config_path).run(rx))
            .unwrap();
        tx
    }

    fn load(
        config: Config,
        reactor_tx: reactor::Sender,
        events: BroadcastSender,
        config_path: PathBuf,
    ) -> Self {
        let disk = read_disk(&config_path).unwrap_or_else(|error| {
            tracing::warn!(%error, "Could not read config file");
            None
        });
        // An unreadable startup file stays unknown, so later saves refuse to replace it.
        let (document, disk) = match disk.as_deref().map(ConfigDocument::parse) {
            Some(Ok(document)) => (document, disk),
            Some(Err(error)) => {
                tracing::warn!(%error, "Config file changed before the config actor started");
                (ConfigDocument::default(), None)
            }
            None => (ConfigDocument::default(), None),
        };
        ConfigActor {
            document,
            disk,
            source_revision: 0,
            config,
            reactor_tx,
            events,
            config_path,
        }
    }

    fn run(mut self, mut events: Receiver) {
        while let Some((_span, event)) = events.blocking_recv() {
            match event {
                Event::QueryConfig(resp) => {
                    let _ = resp.send(self.config.clone());
                }
                Event::ApplyConfig { cmd, response } => {
                    let res = self.handle_config_command(cmd);
                    let _ = response.send(res);
                }
            }
        }
    }

    fn publish(&mut self) {
        self.source_revision += 1;
        self.reactor_tx.send(reactor::Event::ConfigUpdated(self.config.clone()));
        self.events
            .send(BroadcastEvent::ConfigChanged { revision: self.source_revision });
    }

    /// Atomically saves `document` only if the file still holds the last version this actor
    /// knew. A missing file has nothing to lose and is recreated.
    fn persist(&mut self, document: &ConfigDocument) -> Result<(), String> {
        let current = read_disk(&self.config_path).map_err(|e| e.to_string())?;
        if let Some(current) = current
            && Some(&current) != self.disk.as_ref()
        {
            return Err(self.reject_stale(current));
        }
        document.save(&self.config_path).map_err(|e| e.to_string())?;
        self.disk = Some(document.to_string());
        Ok(())
    }

    /// Adopts a newer valid file so callers resynchronize; an invalid file is left untouched.
    fn reject_stale(&mut self, text: String) -> String {
        match parse_valid(&text) {
            Ok((document, config)) => {
                self.document = document;
                self.config = config;
                self.disk = Some(text);
                self.publish();
                "The config file changed on disk, so this change was not saved. Wm loaded \
                 the newer file; review it and try again."
                    .into()
            }
            Err(error) => format!(
                "The config file changed on disk and is invalid ({error}). This change was not \
                 saved; fix the file to continue."
            ),
        }
    }

    fn handle_config_command(&mut self, cmd: ConfigCommand) -> Result<(), String> {
        debug!("Applying config command: {:?}", cmd);

        match &cmd {
            ConfigCommand::GetConfig => {
                info!(
                    "Current config:\n{}",
                    serde_json::to_string_pretty(&self.config).map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            ConfigCommand::SaveConfig => return self.persist(&self.document.clone()),
            ConfigCommand::ReloadConfig => {
                let text = read_disk(&self.config_path)
                    .map_err(|e| e.to_string())?
                    .ok_or("Config file not found")?;
                // The watcher also reports Wm's own saves; unchanged files publish nothing.
                if self.disk.as_ref() == Some(&text) && self.document.to_string() == text {
                    return Ok(());
                }
                (self.document, self.config) = parse_valid(&text)?;
                self.disk = Some(text);
            }
            ConfigCommand::Set { key, value } => {
                self.config = self.document.set(key, value).map_err(|e| e.to_string())?;
            }
            _ => {
                let range = match &cmd {
                    ConfigCommand::SetAnimationDuration(v) => {
                        Some(("animation_duration", *v, 0.0, 5.0))
                    }
                    ConfigCommand::SetAnimationFps(v) => Some(("animation_fps", *v, 0.0, 240.0)),
                    _ => None,
                };
                if let Some((name, value, min, max)) = range
                    && !(min..=max).contains(&value)
                {
                    return Err(format!(
                        "Invalid {name} value: {value}. Must be between {min} and {max}"
                    ));
                }
                if let ConfigCommand::SetWorkspaceNames(names) = &cmd
                    && names.len() > MAX_WORKSPACES
                {
                    return Err(format!(
                        "Too many workspace names provided. Maximum is {MAX_WORKSPACES}"
                    ));
                }
                self.config = self
                    .document
                    .update(|source| {
                        let settings = &mut source.settings;
                        match cmd {
                            ConfigCommand::SetAnimate(v) => settings.animate = v,
                            ConfigCommand::SetAnimationDuration(v) => {
                                settings.animation_duration = v
                            }
                            ConfigCommand::SetAnimationFps(v) => settings.animation_fps = v,
                            ConfigCommand::SetAnimationEasing(v) => settings.animation_easing = v,
                            ConfigCommand::SetMouseFollowsFocus(v) => {
                                settings.mouse_follows_focus = v
                            }
                            ConfigCommand::SetMouseHidesOnFocus(v) => {
                                settings.mouse_hides_on_focus = v
                            }
                            ConfigCommand::SetFocusFollowsMouse(v) => {
                                settings.focus_follows_mouse = v
                            }

                            ConfigCommand::SetOuterGaps { top, left, bottom, right } => {
                                settings.layout.gaps.outer =
                                    crate::common::config::OuterGaps { top, left, bottom, right };
                            }
                            ConfigCommand::SetInnerGaps { horizontal, vertical } => {
                                settings.layout.gaps.inner =
                                    crate::common::config::InnerGaps { horizontal, vertical };
                            }
                            ConfigCommand::SetWorkspaceNames(names) => {
                                source.virtual_workspaces.workspace_names = names
                            }
                            _ => unreachable!(),
                        }
                    })
                    .map_err(|e| e.to_string())?;
            }
        }
        self.publish();
        Ok(())
    }
}

fn read_disk(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse_valid(text: &str) -> Result<(ConfigDocument, Config), String> {
    let (document, config) = ConfigDocument::load(text).map_err(|e| e.to_string())?;
    let issues = config.validate();
    if !issues.is_empty() {
        return Err(issues.join("; "));
    }
    Ok((document, config))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    use super::*;

    #[test]
    fn reload_rejects_semantic_errors_without_changing_runtime_or_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let text = "[settings]\nanimate = false\n[keys]\n";
        std::fs::write(&path, text).unwrap();
        let (reactor_tx, mut updates) = actor::channel();
        let mut actor = ConfigActor::load(
            Config::read(&path).unwrap(),
            reactor_tx,
            actor::channel().0,
            path.clone(),
        );
        let original = serde_json::to_value(&actor.config).unwrap();
        for (invalid, field) in [
            ("[settings]\nanimation_fps = 0\n[keys]\n", "animation_fps"),
            ("[settings]\nanimation_duration = -1\n[keys]\n", "animation_duration"),
            (
                "[settings]\n[virtual_workspaces]\ndefault_workspace_count = 0\n[keys]\n",
                "default_workspace_count",
            ),
        ] {
            std::fs::write(&path, invalid).unwrap();
            let error = actor.handle_config_command(ConfigCommand::ReloadConfig).unwrap_err();
            assert!(error.contains(field), "{error}");
            assert_eq!(serde_json::to_value(&actor.config).unwrap(), original);
            assert_eq!(actor.document.to_string(), text);
            assert_eq!(actor.disk.as_deref(), Some(text));
            assert!(updates.try_recv().is_err());
        }
        std::fs::write(&path, text.replace("false", "true")).unwrap();
        actor.handle_config_command(ConfigCommand::ReloadConfig).unwrap();
        let (_, update) = updates.try_recv().unwrap();
        assert!(matches!(update, reactor::Event::ConfigUpdated(config) if config.settings.animate));
    }

    #[test]
    fn commands_save_and_reload_the_source_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let text = include_str!("../../wm.default.toml");
        std::fs::write(&path, text).unwrap();
        let (reactor_tx, _updates) = actor::channel();
        let mut actor =
            ConfigActor::load(Config::default(), reactor_tx, actor::channel().0, path.clone());
        actor.handle_config_command(ConfigCommand::SetAnimate(true)).unwrap();
        actor.handle_config_command(ConfigCommand::SaveConfig).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text.replace("animate = false", "animate = true")
        );
        let external = format!("# external edit\n{text}");
        std::fs::write(&path, &external).unwrap();
        actor.handle_config_command(ConfigCommand::ReloadConfig).unwrap();
        actor
            .handle_config_command(ConfigCommand::Set {
                key: "settings.animation_duration".into(),
                value: serde_json::json!(0.2),
            })
            .unwrap();
        actor.handle_config_command(ConfigCommand::SaveConfig).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            external.replace("animation_duration = 0.3", "animation_duration = 0.2")
        );
        assert_eq!(Config::read(&path).unwrap().settings.animation_duration, 0.2);
    }

    fn save_toggled_animate(actor: &mut ConfigActor) -> Result<bool, String> {
        actor.handle_config_command(ConfigCommand::SetAnimate(!actor.config.settings.animate))?;
        actor.handle_config_command(ConfigCommand::SaveConfig)?;
        Ok(actor.config.settings.animate)
    }

    #[test]
    fn stale_saves_never_overwrite_external_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[settings]\nanimate = false\n[keys]\n").unwrap();
        let (reactor_tx, mut updates) = actor::channel();
        let mut actor = ConfigActor::load(
            Config::read(&path).unwrap(),
            reactor_tx,
            actor::channel().0,
            path.clone(),
        );

        // Own saves succeed, and the watcher's echo of them publishes nothing.
        assert!(save_toggled_animate(&mut actor).unwrap());
        assert!(updates.try_recv().is_ok());
        let revision = actor.source_revision;
        actor.handle_config_command(ConfigCommand::ReloadConfig).unwrap();
        assert_eq!(actor.source_revision, revision);
        assert!(updates.try_recv().is_err());

        let external = "# edited elsewhere\n[settings]\nanimate = true\nfocus_follows_mouse = false\n[keys]\n\"Alt + Y\" = \"close_window\"\n";
        std::fs::write(&path, external).unwrap();
        let error = actor.handle_config_command(ConfigCommand::SaveConfig).unwrap_err();
        assert!(error.contains("changed on disk"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), external);
        assert!(!actor.config.settings.focus_follows_mouse);
        assert!(
            actor.source_revision > revision,
            "subscribers receive the newer file"
        );
        assert!(updates.try_recv().is_ok());
        // Retrying against the adopted file preserves its comments and custom bindings.
        assert!(!save_toggled_animate(&mut actor).unwrap());
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("# edited elsewhere\n") && saved.contains("\"Alt + Y\""));
        assert!(saved.contains("animate = false"));
        assert!(updates.try_recv().is_ok());

        let invalid = "[settings\nanimate = true\n";
        std::fs::write(&path, invalid).unwrap();
        let revision = actor.source_revision;
        assert!(
            actor
                .handle_config_command(ConfigCommand::SaveConfig)
                .unwrap_err()
                .contains("invalid")
        );
        actor.handle_config_command(ConfigCommand::SetAnimate(true)).unwrap();
        assert!(actor.handle_config_command(ConfigCommand::SaveConfig).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        assert!(actor.handle_config_command(ConfigCommand::ReloadConfig).is_err());
        assert!(
            actor.handle_config_command(ConfigCommand::SaveConfig).is_err(),
            "rejection must persist until fixed"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        assert_eq!(actor.source_revision, revision + 1, "only the CLI set published");

        // Once fixed, the next save adopts the file instead of reverting the repair.
        std::fs::write(&path, "[settings]\nanimate = false\n[keys]\n").unwrap();
        assert!(actor.handle_config_command(ConfigCommand::SaveConfig).is_err());
        assert!(!actor.config.settings.animate);
        assert!(save_toggled_animate(&mut actor).unwrap());
    }

    #[test]
    fn saves_recreate_missing_files_and_write_through_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("dotfiles.toml");
        let path = directory.path().join("config.toml");
        let (reactor_tx, _updates) = actor::channel();
        let mut actor =
            ConfigActor::load(Config::default(), reactor_tx, actor::channel().0, path.clone());
        save_toggled_animate(&mut actor).unwrap();
        assert!(Config::read(&path).is_ok(), "an absent file is created");
        std::fs::remove_file(&path).unwrap();
        save_toggled_animate(&mut actor).unwrap();
        assert!(
            path.exists(),
            "a deleted file has nothing to lose and is recreated"
        );

        std::fs::rename(&path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        actor.handle_config_command(ConfigCommand::ReloadConfig).unwrap();
        save_toggled_animate(&mut actor).unwrap();
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        let mut external = std::fs::read_to_string(&target).unwrap();
        external.insert_str(0, "# edited through the link target\n");
        std::fs::write(&target, &external).unwrap();
        assert!(save_toggled_animate(&mut actor).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), external);
    }

    #[test]
    fn unread_and_dropped_replies_do_not_stall_actor() {
        let (reactor_tx, _updates) = actor::channel();
        let config = Config::default();
        let config_tx = ConfigActor::spawn(config.clone(), reactor_tx, actor::channel().0);
        let (response, unread) = sync_channel(1);
        config_tx.try_send(Event::QueryConfig(response)).unwrap();
        let (response, dropped) = sync_channel(1);
        drop(dropped);
        config_tx
            .try_send(Event::ApplyConfig {
                cmd: ConfigCommand::GetConfig,
                response,
            })
            .unwrap();
        let (response, result) = sync_channel(1);
        config_tx.try_send(Event::QueryConfig(response)).unwrap();
        assert_eq!(
            serde_json::to_value(result.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap(),
            serde_json::to_value(&config).unwrap()
        );
        assert_eq!(
            serde_json::to_value(unread.recv().unwrap()).unwrap(),
            serde_json::to_value(&config).unwrap()
        );
    }
}

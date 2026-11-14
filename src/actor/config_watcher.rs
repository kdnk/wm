// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;
use std::{fs, thread};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

use crate::actor::config::{self as config_actor, Event as ConfigEvent};
use crate::common::config::{self, ConfigCommand};

const DEBOUNCE: Duration = Duration::from_millis(250);

pub struct ConfigWatcher {
    file: PathBuf,
    real_file: Option<PathBuf>,
    real_file_id: Option<(u64, u64)>,
    parents: HashSet<PathBuf>,
    last_text: Option<String>,
    config_tx: config_actor::Sender,
}

impl ConfigWatcher {
    pub fn spawn(config_tx: config_actor::Sender, config_path: PathBuf) {
        thread::Builder::new()
            .name("config-watcher".to_string())
            .spawn(move || {
                if let Err(e) = Self::run(config_tx, config_path) {
                    warn!("config-watcher: error: {e:?}");
                }
            })
            .expect("failed to spawn config-watcher thread");
    }

    fn watch(
        config_tx: config_actor::Sender,
        file: PathBuf,
    ) -> notify::Result<(Self, RecommendedWatcher, Receiver<Event>)> {
        // FSEvents reports absolute paths; preserve the final symlink itself.
        let file = file
            .parent()
            .and_then(|p| fs::canonicalize(p).ok())
            .zip(file.file_name())
            .map(|(parent, name)| parent.join(name))
            .unwrap_or(file);
        let mut actor = Self {
            file,
            real_file: None,
            real_file_id: None,
            parents: HashSet::new(),
            last_text: None,
            config_tx,
        };
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |event| match event {
            Ok(event) => {
                let _ = tx.send(event);
            }
            Err(error) => warn!("config-watcher: notification error: {error}"),
        })?;
        actor.refresh_watches(&mut watcher)?;
        Ok((actor, watcher, rx))
    }

    fn run(config_tx: config_actor::Sender, file: PathBuf) -> notify::Result<()> {
        let (mut actor, mut watcher, rx) = Self::watch(config_tx, file)?;
        while let Ok(event) = rx.recv() {
            actor.handle_event(&mut watcher, &rx, event);
            if actor.config_tx.is_closed() {
                break;
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, watcher: &mut dyn Watcher, rx: &Receiver<Event>, event: Event) {
        if !self.is_relevant(&event) {
            return;
        }
        self.settle(rx);
        // Watch parents rather than files: atomic saves replace the inode, and a
        // deleted file must remain discoverable. Retargeted symlinks need new parents.
        // A failed refresh must not end hot reload; keep the existing watches.
        if let Err(error) = self.refresh_watches(watcher) {
            warn!("config-watcher: failed to refresh watches: {error}");
        }
        self.reload_if_changed();
    }

    fn settle(&self, rx: &Receiver<Event>) {
        let mut deadline = std::time::Instant::now() + DEBOUNCE;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(event) if self.is_relevant(&event) => {
                    deadline = std::time::Instant::now() + DEBOUNCE;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn refresh_watches(&mut self, watcher: &mut dyn Watcher) -> notify::Result<()> {
        // Keep the last target when it disappears so recreation is still observed.
        let real_file = fs::canonicalize(&self.file).ok().or_else(|| self.real_file.clone());
        let parents: HashSet<_> = self
            .file
            .parent()
            .into_iter()
            .chain(real_file.as_deref().and_then(|p| p.parent()))
            .map(PathBuf::from)
            .collect();
        for dir in parents.difference(&self.parents) {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
            info!("watching {:?}", dir);
        }
        for dir in self.parents.difference(&parents) {
            watcher.unwatch(dir)?;
        }
        self.parents = parents;
        self.real_file_id = real_file
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| (m.dev(), m.ino()));
        self.real_file = real_file;
        Ok(())
    }

    fn is_relevant(&self, event: &Event) -> bool {
        event.paths.iter().any(|path| {
            path == &self.file
                || self.real_file.as_ref() == Some(path)
                || self.parents.contains(path)
                || self
                    .real_file_id
                    .is_some_and(|id| fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == id))
        })
    }

    fn reload_if_changed(&mut self) {
        let text = match fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(error) => {
                // Recreation with the same bytes still deserves a fresh attempt.
                self.last_text = None;
                warn!("Failed to read config file: {error}");
                return;
            }
        };
        if self.last_text.as_ref() == Some(&text) {
            return;
        }
        // Runtime commands can change hot_reload before the file is saved.
        let Ok(current) = self.query_config() else { return };
        let text = self.last_text.insert(text);
        if !current.settings.hot_reload {
            match config::Config::parse(text) {
                Ok(new_cfg) if new_cfg.keys != current.keys => {}
                Ok(_) => return,
                Err(error) => {
                    warn!("Failed to parse config file for diff check: {error}");
                    return;
                }
            }
        }
        if let Err(error) = self.request_reload() {
            self.last_text = None;
            warn!("Failed to reload config: {error}");
        } else {
            debug!("config reloaded successfully");
        }
    }

    fn request_reload(&self) -> Result<(), String> {
        info!("requesting config reload");
        let (response, result) = std::sync::mpsc::sync_channel(1);
        self.config_tx
            .try_send(ConfigEvent::ApplyConfig {
                cmd: ConfigCommand::ReloadConfig,
                response,
            })
            .map_err(|_| "Config actor unavailable".to_string())?;
        result.recv().map_err(|_| "Config actor stopped before replying".to_string())?
    }

    fn query_config(&self) -> Result<config::Config, ()> {
        let (response, result) = std::sync::mpsc::sync_channel(1);
        self.config_tx.try_send(ConfigEvent::QueryConfig(response)).map_err(|_| ())?;
        result.recv().map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use super::*;
    use crate::actor::{self, reactor};

    struct WatchingConfig {
        actor: ConfigWatcher,
        watcher: RecommendedWatcher,
        events: Receiver<Event>,
        updates: actor::Receiver<reactor::Event>,
    }

    impl WatchingConfig {
        fn new(path: &Path) -> Self {
            let (reactor_tx, updates) = actor::channel();
            let config_tx = config_actor::ConfigActor::spawn_with_path(
                config::Config::default(),
                reactor_tx,
                actor::channel().0,
                path.to_path_buf(),
            );
            let (actor, watcher, events) =
                ConfigWatcher::watch(config_tx, path.to_path_buf()).unwrap();
            actor.query_config().unwrap();
            Self { actor, watcher, events, updates }
        }

        fn await_animate(&mut self, expected: bool) {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                while let Ok((_, update)) = self.updates.try_recv() {
                    if let reactor::Event::ConfigUpdated(config) = update {
                        assert_eq!(config.settings.animate, expected);
                        return;
                    }
                }
                let event = self
                    .events
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("filesystem changes must publish ConfigUpdated");
                self.actor.handle_event(&mut self.watcher, &self.events, event);
            }
        }
    }

    fn write_config(path: &Path, animate: bool) {
        fs::write(path, format!("[settings]\nanimate = {animate}\n[keys]\n")).unwrap();
    }

    #[test]
    fn reloads_after_missing_parent_directories_are_created() {
        let directory = tempfile::Builder::new()
            .prefix("wm-config-watcher-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = directory.path().join("new/settings/config.toml");
        let mut config = WatchingConfig::new(&path);

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_config(&path, true);
        config.await_animate(true);

        write_config(&path, false);
        config.await_animate(false);
    }

    #[test]
    fn reloads_atomic_replacements_and_retargeted_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first/config.toml");
        let second = directory.path().join("second/config.toml");
        fs::create_dir(first.parent().unwrap()).unwrap();
        fs::create_dir(second.parent().unwrap()).unwrap();
        write_config(&first, false);
        write_config(&second, false);
        let path = directory.path().join("config.toml");
        std::os::unix::fs::symlink(&first, &path).unwrap();
        let mut config = WatchingConfig::new(&path);

        let replacement = first.with_file_name("replacement.toml");
        write_config(&replacement, true);
        fs::rename(replacement, &first).unwrap();
        config.await_animate(true);

        let replacement = path.with_file_name("replacement-link");
        std::os::unix::fs::symlink(&second, &replacement).unwrap();
        fs::rename(replacement, &path).unwrap();
        config.await_animate(false);

        let replacement = second.with_file_name("replacement.toml");
        write_config(&replacement, true);
        fs::rename(replacement, &second).unwrap();
        config.await_animate(true);
    }
}

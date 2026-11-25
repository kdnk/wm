// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use objc2::MainThreadMarker;
use serde::de::value::{Error as DeserializeError, StrDeserializer};
use serde::{Deserialize, Serialize};
use wm_protocol::{DEFAULT_SERVICE_NAME, EventKind, WindowId, WmRequest, WmResponse};

use self::execute::ExecuteArgs;
use self::query::QueryArgs;
use crate::actor::app::WindowId as InternalWindowId;
use crate::common::config::{Config, config_file, restore_file};
use crate::ipc::{WmMachClient, WmMachClientError};
use crate::layout_engine::LayoutEngine;
use crate::sys::service::{ServiceCommands, handle_service_command};

mod execute;
mod query;

#[derive(Parser)]
#[command(name = "wm", version = env!("WM_VERSION"), args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Only run the window manager on the current space.
    #[arg(long)]
    pub one: bool,

    /// Disable new spaces by default.
    ///
    /// Ignored if --one is used.
    #[arg(long)]
    pub default_disable: bool,

    /// Disable animations.
    #[arg(long)]
    pub no_animate: bool,

    /// Check whether the saved layout snapshot can be loaded without starting wm.
    #[arg(long)]
    pub validate: bool,

    /// Record reactor events to the specified file path. Overwrites the file if
    /// exists.
    #[arg(long)]
    pub record: Option<PathBuf>,

    /// Path to configuration file to use (overrides default).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Internal: recover offscreen windows after this daemon PID exits.
    #[arg(long, hide = true)]
    recover_windows: Option<i32>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Close the focused window, including all its tabs, using its native close button
    CloseWindow,
    /// Check a configuration file without starting wm
    Config {
        #[command(subcommand)]
        command: LocalConfigCommands,
    },
    /// Query information from wm
    Query(QueryArgs),
    /// Execute commands in wm
    Execute(ExecuteArgs),
    /// Event subscription commands
    Subscribe {
        #[command(subcommand)]
        subscribe: SubscribeCommands,
    },
    /// Manage the launchd service for wm
    Service {
        #[command(subcommand)]
        service: ServiceCommands,
    },
}

#[derive(Subcommand)]
enum LocalConfigCommands {
    /// Check TOML syntax and configuration settings
    Check {
        /// Configuration file to check. Defaults to ~/.config/wm/config.toml.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SubscribeCommands {
    /// Print Mach IPC events until this process exits. Press Ctrl-C to unsubscribe.
    Mach {
        /// Event to subscribe to (workspace_changed, windows_changed, window_title_changed, focused_window_changed, stacks_changed, layout_changed, selection_changed, binding_mode_changed, config_changed, *)
        event: String,
    },
    /// Subscribe to events via CLI command execution
    Cli {
        /// Event to subscribe to (workspace_changed, windows_changed, window_title_changed, focused_window_changed, stacks_changed, layout_changed, selection_changed, binding_mode_changed, config_changed, *)
        #[arg(long)]
        event: String,
        /// Command to execute when event occurs
        #[arg(long)]
        command: String,
        /// Arguments to pass to command (event data will be appended as JSON)
        #[arg(long, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Unsubscribe from CLI events
    UnsubCli {
        /// Event to unsubscribe from
        event: String,
    },
    /// List current CLI subscriptions
    ListCli,
}

impl SubscribeCommands {
    fn run(self) -> Result<(), String> {
        let request = match self {
            Self::Mach { event } => {
                run_mach_subscription(parse_event_kind(&event)?);
                return Ok(());
            }
            Self::Cli { event, command, args } => WmRequest::SubscribeCli {
                event: parse_event_kind(&event)?,
                command,
                args,
            },
            Self::UnsubCli { event } => WmRequest::UnsubscribeCli {
                event: parse_event_kind(&event)?,
            },
            Self::ListCli => WmRequest::ListCliSubscriptions,
        };
        send_request(request);
        Ok(())
    }
}

impl Cli {
    /// Run a local or IPC command before native daemon initialization.
    /// Returns false when the daemon should start.
    pub fn run_command(&mut self) -> bool {
        let command = self.command.take();
        if !matches!(command, Some(Commands::CloseWindow)) {
            if let Some(pid) = self.recover_windows {
                crate::sys::window_recovery::run(pid).expect("window recovery failed");
                return true;
            }
            sigpipe::reset();
        }

        match command {
            None if self.validate => {
                let path = restore_file();
                if let Err(error) = LayoutEngine::validate_file(path.clone()) {
                    eprintln!("Could not load master file at {}: {error}", path.display());
                    process::exit(1);
                }
                println!("Master file is valid: {}", path.display());
            }
            None => return false,
            Some(Commands::CloseWindow) => {
                if let Err(error) = crate::sys::close_window::close_focused_window() {
                    eprintln!("wm close-window: {error:#}");
                    process::exit(1);
                }
            }
            Some(Commands::Config {
                command: LocalConfigCommands::Check { path },
            }) => {
                if let Some(mtm) = MainThreadMarker::new() {
                    crate::sys::hotkey::refresh_virtual_keymap(mtm);
                }
                let path = path.unwrap_or_else(config_file);
                if let Err(error) = Config::read(&path) {
                    eprintln!("Could not load config file at {}: {error:#}", path.display());
                    process::exit(1);
                }
                println!("Config file is valid: {}", path.display());
            }
            Some(Commands::Service { service }) => match handle_service_command(&service) {
                Ok(message) => println!("{message}"),
                Err(error) => {
                    eprintln!("{error}");
                    process::exit(1);
                }
            },
            Some(Commands::Query(query)) => {
                let request = query.into_request().unwrap_or_else(|error| exit_with_error(error));
                send_request(request);
            }
            Some(Commands::Execute(execute)) => {
                let request = execute.into_request().unwrap_or_else(|error| exit_with_error(error));
                send_request(request);
            }
            Some(Commands::Subscribe { subscribe }) => {
                subscribe.run().unwrap_or_else(|error| exit_with_error(error));
            }
        }
        true
    }
}

fn exit_with_error(error: impl std::fmt::Display) -> ! {
    eprintln!("Error: {error}");
    process::exit(1);
}

fn exit_on_ipc_error(error: WmMachClientError) -> ! {
    eprintln!("Communication error: {error}");
    if matches!(error, WmMachClientError::ServiceUnavailable) {
        match std::env::var("WM_BS_NAME") {
            Ok(name) if name != DEFAULT_SERVICE_NAME => eprintln!(
                "Hint: start wm with WM_BS_NAME={name:?}, or unset WM_BS_NAME to use the default service."
            ),
            _ => eprintln!("Hint: start the wm service with `wm service start`."),
        }
    }
    process::exit(1);
}

fn send_request(request: WmRequest) {
    let client = WmMachClient::connect().unwrap_or_else(|error| exit_on_ipc_error(error));
    let response = client.send_request(&request).unwrap_or_else(|error| exit_on_ipc_error(error));
    match response {
        WmResponse::Success { data } => {
            if let Err(error) = write_json(
                &data,
                std::env::var("WM_CLI_PRETTY").map(|v| v != "0").unwrap_or(false),
            ) {
                eprintln!("Failed to handle response: {error}");
                process::exit(1);
            }
        }
        WmResponse::Error { error } => {
            match serde_json::to_string_pretty(&error) {
                Ok(pretty) => eprintln!("{pretty}"),
                Err(_) => eprintln!("Error: {error}"),
            }
            process::exit(1);
        }
        _ => {
            eprintln!("Received an unknown response shape from wm");
            process::exit(1);
        }
    }
}

fn parse_window_id(input: &str) -> Result<WindowId, String> {
    let input = input.trim();
    if let Ok(window_id) = serde_json::from_str::<InternalWindowId>(input) {
        return Ok(window_id.into());
    }
    if let Some(window_id) = InternalWindowId::from_debug_string(input) {
        return Ok(window_id.into());
    }

    Err(format!(
        "Invalid window id '{}'; expected `{{\"pid\":123,\"idx\":456}}` or `WindowId {{ pid: 123, idx: 456 }}`",
        input
    ))
}

fn parse_event_kind(input: &str) -> Result<EventKind, String> {
    let event = input.trim().to_ascii_lowercase();
    EventKind::deserialize(StrDeserializer::<DeserializeError>::new(&event)).map_err(|_| {
        format!(
            "Invalid event '{event}'; expected workspace_changed, windows_changed, window_title_changed, focused_window_changed, stacks_changed, layout_changed, selection_changed, binding_mode_changed, config_changed, or *"
        )
    })
}

fn write_json<T: Serialize>(value: &T, pretty: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let mut writer = io::BufWriter::new(&mut handle);

    if pretty {
        serde_json::to_writer_pretty(&mut writer, value)?;
    } else {
        serde_json::to_writer(&mut writer, value)?;
    }
    writer.write_all(b"\n")?;
    writer.flush()
}

fn run_mach_subscription(event: EventKind) {
    let pretty = std::env::var("WM_CLI_PRETTY").map(|v| v != "0").unwrap_or(false);
    let client = WmMachClient::connect().unwrap_or_else(|error| exit_on_ipc_error(error));
    let subscription = client.subscribe(event).unwrap_or_else(|error| exit_on_ipc_error(error));

    loop {
        let event_payload =
            subscription.recv_event().unwrap_or_else(|error| exit_on_ipc_error(error));
        if let Err(error) = write_json(&event_payload, pretty) {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return;
            }
            exit_with_error(format_args!("Failed to write event output: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_preserves_daemon_options() {
        let cli = Cli::try_parse_from([
            "wm",
            "--one",
            "--default-disable",
            "--no-animate",
            "--validate",
            "--record",
            "events.json",
            "--config",
            "config.toml",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert!(cli.one && cli.default_disable && cli.no_animate && cli.validate);
        assert_eq!(cli.record, Some(PathBuf::from("events.json")));
        assert_eq!(cli.config, Some(PathBuf::from("config.toml")));

        let recovery = Cli::try_parse_from(["wm", "--recover-windows", "123"]).unwrap();
        assert_eq!(recovery.recover_windows, Some(123));
        assert!(recovery.command.is_none());
    }

    #[test]
    fn local_commands_use_the_unified_parser() {
        let close = Cli::try_parse_from(["wm", "close-window"]).unwrap();
        assert!(matches!(close.command, Some(Commands::CloseWindow)));
        let service = Cli::try_parse_from(["wm", "service", "start"]).unwrap();
        assert!(matches!(
            service.command,
            Some(Commands::Service {
                service: ServiceCommands::Start,
            })
        ));
    }

    #[test]
    fn daemon_options_conflict_with_subcommands() {
        for command in [
            vec!["wm", "--one", "query", "windows"],
            vec!["wm", "--config", "config.toml", "execute", "debug"],
            vec!["wm", "--validate", "close-window"],
            vec!["wm", "--recover-windows", "123", "service", "start"],
        ] {
            assert!(Cli::try_parse_from(command).is_err());
        }
    }

    #[test]
    fn restore_options_are_not_needed_or_accepted() {
        assert!(Cli::try_parse_from(["wm"]).is_ok());
        for option in ["--restore", "--no-restore"] {
            let error = Cli::try_parse_from(["wm", option]).err().unwrap();
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn binding_mode_query_and_subscription_parse() {
        let cli = Cli::try_parse_from(["wm", "query", "binding-mode"]).unwrap();
        let Some(Commands::Query(query)) = cli.command else {
            panic!("expected a query command");
        };
        assert_eq!(query.into_request().unwrap(), WmRequest::GetBindingMode);
        assert_eq!(
            parse_event_kind(" BINDING_MODE_CHANGED ").unwrap(),
            EventKind::BindingModeChanged
        );
    }

    #[test]
    fn mach_subscription_has_no_cross_process_unsubscribe() {
        let subscribe =
            Cli::try_parse_from(["wm", "subscribe", "mach", "windows_changed"]).unwrap();
        assert!(matches!(
            subscribe.command,
            Some(Commands::Subscribe {
                subscribe: SubscribeCommands::Mach { event },
            }) if event == "windows_changed"
        ));
        let Err(error) = Cli::try_parse_from(["wm", "subscribe", "unsub-mach", "windows_changed"])
        else {
            panic!("a separate process cannot unsubscribe another Mach subscriber");
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}

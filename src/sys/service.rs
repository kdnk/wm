// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
// ported from https://github.com/koekeishiya/yabai/blob/master/src/misc/service.h

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, io};

use clap::Subcommand;
use nix::unistd::getuid;
use wm_protocol::{WmRequest, WmResponse};

use crate::ipc::{WmMachClient, WmMachClientError};

const LAUNCHCTL_PATH: &str = "/bin/launchctl";
const WM_PLIST: &str = wm_protocol::DEFAULT_SERVICE_NAME;
const LAUNCHCTL_SERVICE_NOT_FOUND: i32 = 113;
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(10);
const CHECKPOINT_RETRY_INTERVAL: Duration = Duration::from_millis(100);

trait ServiceControl {
    fn running_pid(&mut self, target: &str) -> io::Result<Option<u32>>;
    fn checkpoint(&mut self, pid: u32, timeout: Duration) -> io::Result<()>;
    fn start(&mut self) -> io::Result<()>;
    fn launchctl(&mut self, args: &[&str], suppress_output: bool) -> io::Result<i32>;
}

struct NativeServiceControl;

#[derive(Debug, PartialEq, Eq)]
enum ListedService {
    Missing,
    Stopped,
    Running(u32),
}

impl ServiceControl for NativeServiceControl {
    fn running_pid(&mut self, target: &str) -> io::Result<Option<u32>> {
        let output = Command::new(LAUNCHCTL_PATH).arg("list").output()?;
        if !output.status.success() {
            return Err(io::Error::other("could not query the running wm service"));
        }
        let jobs = std::str::from_utf8(&output.stdout)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        match listed_service(jobs)? {
            ListedService::Running(pid) => Ok(Some(pid)),
            ListedService::Stopped => Ok(None),
            ListedService::Missing => {
                if run_launchctl(&["print", target], true)? == 0 {
                    Err(io::Error::other(
                        "cannot determine whether the loaded wm service is running",
                    ))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn checkpoint(&mut self, pid: u32, timeout: Duration) -> io::Result<()> {
        let client = WmMachClient::connect().map_err(checkpoint_transport_error)?;
        let response = client
            .send_typed_request_with_timeout::<String>(
                &WmRequest::CheckpointService { pid },
                timeout,
            )
            .map_err(checkpoint_transport_error)?;
        checkpoint_response(response)
    }

    fn launchctl(&mut self, args: &[&str], suppress_output: bool) -> io::Result<i32> {
        run_launchctl(args, suppress_output)
    }

    fn start(&mut self) -> io::Result<()> { service_start() }
}

fn checkpoint_transport_error(error: WmMachClientError) -> io::Error {
    match error {
        WmMachClientError::ServiceUnavailable => io::Error::new(io::ErrorKind::WouldBlock, error),
        _ => io::Error::other(error),
    }
}

fn checkpoint_response(response: WmResponse<String>) -> io::Result<()> {
    match response {
        WmResponse::Success { data } if data == "Layout saved" => Ok(()),
        WmResponse::Error { error } => {
            let kind = match error.get("retryable").and_then(serde_json::Value::as_bool) {
                Some(true) => io::ErrorKind::WouldBlock,
                _ => io::ErrorKind::Other,
            };
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            Err(io::Error::new(kind, message))
        }
        _ => Err(io::Error::other(
            "daemon did not acknowledge a durable layout save",
        )),
    }
}

fn listed_service(jobs: &str) -> io::Result<ListedService> {
    for job in jobs.lines() {
        let mut fields = job.split_whitespace();
        let (Some(pid), Some(_status), Some(label)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if label != WM_PLIST {
            continue;
        }
        if pid == "-" {
            return Ok(ListedService::Stopped);
        }
        return pid
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .map(ListedService::Running)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid wm service PID"));
    }
    Ok(ListedService::Missing)
}

#[derive(Debug)]
enum CheckpointOutcome {
    NotRunning,
    Saved,
}

fn checkpoint_running_service(
    control: &mut impl ServiceControl,
    target: &str,
    timeout: Duration,
) -> io::Result<CheckpointOutcome> {
    let Some(mut pid) = control.running_pid(target)? else {
        return Ok(CheckpointOutcome::NotRunning);
    };
    let deadline = Instant::now() + timeout;
    let mut remaining = timeout;
    loop {
        let error = match control.checkpoint(pid, remaining) {
            Ok(()) => return Ok(CheckpointOutcome::Saved),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => error,
            Err(error) => {
                return Err(io::Error::other(format!(
                    "could not save layout; wm is still running: {error}"
                )));
            }
        };
        let delay =
            CHECKPOINT_RETRY_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
        std::thread::sleep(delay);
        let Some(current_pid) = control.running_pid(target)? else {
            return Ok(CheckpointOutcome::NotRunning);
        };
        remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting to save layout; wm is still running: {error}"),
            ));
        }
        pid = current_pid;
    }
}

#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Install the per-user launchd service
    Install,
    /// Uninstall the per-user launchd service
    Uninstall,
    /// Start (or bootstrap) the service
    Start,
    /// Stop (or bootout/kill) the service
    Stop,
    /// Restart the service (kickstart -k)
    Restart,
}

pub fn handle_service_command(cmd: &ServiceCommands) -> Result<&'static str, String> {
    match cmd {
        ServiceCommands::Install => service_install()
            .map(|_| "Service installed.")
            .map_err(|e| format!("Failed to install service: {}", e)),
        ServiceCommands::Uninstall => service_uninstall()
            .map(|_| "Service uninstalled.")
            .map_err(|e| format!("Failed to uninstall service: {}", e)),
        ServiceCommands::Start => service_start()
            .map(|_| "Service started.")
            .map_err(|e| format!("Failed to start service: {}", e)),
        ServiceCommands::Stop => service_stop()
            .map(|_| "Service stopped.")
            .map_err(|e| format!("Failed to stop service: {}", e)),
        ServiceCommands::Restart => service_restart()
            .map(|_| "Service restarted.")
            .map_err(|e| format!("Failed to restart service: {}", e)),
    }
}

fn plist_path() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "HOME not set"))?;
    Ok(home.join("Library").join("LaunchAgents").join(format!("{WM_PLIST}.plist")))
}

fn xml_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist_contents() -> io::Result<String> {
    let user =
        env::var("USER").map_err(|_| io::Error::new(io::ErrorKind::Other, "env USER not set"))?;
    let path_env =
        env::var("PATH").map_err(|_| io::Error::new(io::ErrorKind::Other, "env PATH not set"))?;

    let agent_exe = env::current_exe()?;
    let exe_str = agent_exe
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "non-UTF8 executable path"))?;

    let install_dir = agent_exe
        .parent()
        .and_then(Path::to_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "invalid Wm installation directory"))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{name}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path_env}</string>
        <key>WM_INSTALL_DIR</key>
        <string>{install_dir}</string>
        <key>RUST_LOG</key>
        <string>error,warn,info</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
        <key>Crashed</key>
        <true/>
    </dict>
    <key>StandardOutPath</key>
    <string>/tmp/wm_{user}.out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/wm_{user}.err.log</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>Nice</key>
    <integer>-20</integer>
</dict>
</plist>
"#,
        name = WM_PLIST,
        exe = xml_text(exe_str),
        path_env = xml_text(&path_env),
        install_dir = xml_text(install_dir),
        user = xml_text(&user)
    );

    Ok(plist)
}

fn write_plist(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

fn run_launchctl(args: &[&str], suppress_output: bool) -> io::Result<i32> {
    let mut cmd = Command::new(LAUNCHCTL_PATH);
    cmd.args(args);
    if suppress_output {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status()?;

    if let Some(code) = status.code() {
        Ok(code)
    } else {
        let sig = status.signal().unwrap_or_default();
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("launchctl terminated by signal {}", sig),
        ))
    }
}

pub fn service_install_internal(plist_path: &Path) -> io::Result<()> {
    write_plist(plist_path, &plist_contents()?)
}

fn ensure_plist_up_to_date(plist_path: &Path) -> io::Result<bool> {
    let desired = plist_contents()?;
    match fs::read_to_string(plist_path) {
        Ok(existing) if existing == desired => return Ok(false),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    write_plist(plist_path, &desired)?;
    Ok(true)
}

pub fn service_install() -> io::Result<()> {
    let plist_path = plist_path()?;
    if plist_path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("service file '{}' is already installed", plist_path.display()),
        ));
    }
    service_install_internal(&plist_path)
}

pub fn service_uninstall() -> io::Result<()> {
    let agent = LaunchAgent::current()?;
    agent.require_installed()?;
    if run_launchctl(&["print", &agent.target], true).is_ok_and(|code| code == 0) {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "service is still running; stop it first with `wm service stop` before uninstalling",
        ));
    }
    fs::remove_file(agent.plist_path)
}

pub fn service_start() -> io::Result<()> {
    let agent = LaunchAgent::current()?;
    let plist_path = &agent.plist_path;
    if !plist_path.is_file() {
        service_install_internal(plist_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "service file '{}' could not be installed: {}",
                    plist_path.display(),
                    e
                ),
            )
        })?;
    }

    let plist_changed = if env::var_os("USER").is_some() && env::var_os("PATH").is_some() {
        ensure_plist_up_to_date(plist_path)?
    } else {
        false
    };

    agent.start(&mut NativeServiceControl, plist_changed)
}

pub fn service_restart() -> io::Result<()> {
    let agent = LaunchAgent::current()?;
    agent.require_installed()?;
    agent.restart(&mut NativeServiceControl)
}

pub fn service_stop() -> io::Result<()> {
    let agent = LaunchAgent::current()?;
    agent.require_installed()?;
    agent.stop(&mut NativeServiceControl)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Registration {
    Present,
    Absent,
}

struct LaunchAgent {
    domain: String,
    target: String,
    plist_path: PathBuf,
}

impl LaunchAgent {
    fn current() -> io::Result<Self> { Ok(Self::new(plist_path()?, format!("gui/{}", getuid()))) }

    fn new(plist_path: PathBuf, domain: String) -> Self {
        let target = format!("{domain}/{WM_PLIST}");
        Self { domain, target, plist_path }
    }

    fn require_installed(&self) -> io::Result<()> {
        if self.plist_path.is_file() {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("service file '{}' is not installed", self.plist_path.display()),
        ))
    }

    fn start(&self, control: &mut impl ServiceControl, plist_changed: bool) -> io::Result<()> {
        let plist_path = self.plist_path.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "non-UTF8 service plist path")
        })?;
        let kickstart = ["kickstart", &self.target];
        if self.registration(control)? == Registration::Present {
            let code = control.launchctl(&kickstart, false)?;
            if code == 0 || !plist_changed {
                return check_exit("kickstart", code);
            }
            checkpoint_running_service(control, &self.target, CHECKPOINT_TIMEOUT)?;
            check_exit(
                "bootout",
                control.launchctl(&["bootout", &self.domain, plist_path], false)?,
            )?;
            self.wait_for_registration(control, Registration::Absent)?;
        } else {
            check_exit("enable", control.launchctl(&["enable", &self.target], false)?)?;
        }
        check_exit(
            "bootstrap",
            control.launchctl(&["bootstrap", &self.domain, plist_path], false)?,
        )?;
        self.wait_for_registration(control, Registration::Present)?;
        check_exit("kickstart", control.launchctl(&kickstart, false)?)
    }

    fn restart(&self, control: &mut impl ServiceControl) -> io::Result<()> {
        if let CheckpointOutcome::NotRunning =
            checkpoint_running_service(control, &self.target, CHECKPOINT_TIMEOUT)?
        {
            return control.start();
        }
        let code = control.launchctl(&["kickstart", "-k", &self.target], false)?;
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!("kickstart -k failed (exit {code})")))
        }
    }

    fn stop(&self, control: &mut impl ServiceControl) -> io::Result<()> {
        checkpoint_running_service(control, &self.target, CHECKPOINT_TIMEOUT)?;

        if control.launchctl(&["print", &self.target], true)? != 0 {
            let code = control.launchctl(&["kill", "SIGTERM", &self.target], false)?;
            return if code == 0 {
                Ok(())
            } else {
                Err(io::Error::other(format!("kill SIGTERM failed (exit {code})")))
            };
        }

        let plist_path = self.plist_path.to_str().unwrap();
        let bootout = control.launchctl(&["bootout", &self.domain, plist_path], false)?;
        let disable = control.launchctl(&["disable", &self.target], false)?;

        if bootout == 0 && disable == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "bootout exit {bootout}, disable exit {disable}"
            )))
        }
    }

    fn registration(&self, control: &mut impl ServiceControl) -> io::Result<Registration> {
        match control.launchctl(&["print", &self.target], true)? {
            0 => Ok(Registration::Present),
            LAUNCHCTL_SERVICE_NOT_FOUND => Ok(Registration::Absent),
            code => Err(io::Error::other(format!(
                "launchctl print {} failed (exit {code})",
                self.target
            ))),
        }
    }

    fn wait_for_registration(
        &self,
        control: &mut impl ServiceControl,
        expected: Registration,
    ) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.registration(control)? != expected {
            if Instant::now() >= deadline {
                let operation = match expected {
                    Registration::Present => "register",
                    Registration::Absent => "remove",
                };
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for launchd to {operation} {}", self.target),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
}

fn check_exit(command: &str, code: i32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "launchctl {command} failed (exit {code})"
        )))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

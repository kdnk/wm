// Modified by kdnk for wm on 2026-11-18. Original attribution is preserved in LICENSE.
use super::*;

struct FixtureControl {
    pid: Option<u32>,
    checkpoint_error: bool,
    checkpoint_data: &'static str,
    state_error: bool,
    bootstrapped: bool,
    calls: Vec<String>,
}

impl Default for FixtureControl {
    fn default() -> Self {
        Self {
            pid: Some(42),
            checkpoint_error: false,
            checkpoint_data: "Layout saved",
            state_error: false,
            bootstrapped: true,
            calls: Vec::new(),
        }
    }
}

impl ServiceControl for FixtureControl {
    fn running_pid(&mut self) -> io::Result<Option<u32>> {
        self.calls.push("state".into());
        if self.state_error {
            Err(io::Error::other("state query failed"))
        } else {
            Ok(self.pid)
        }
    }

    fn checkpoint(&mut self, pid: u32) -> io::Result<()> {
        assert_eq!(Some(pid), self.pid);
        self.calls.push("checkpoint".into());
        if self.checkpoint_error {
            Err(io::Error::other("snapshot failed"))
        } else {
            checkpoint_response(WmResponse::Success {
                data: self.checkpoint_data.into(),
            })
        }
    }

    fn launchctl(&mut self, args: &[&str], _suppress_output: bool) -> io::Result<i32> {
        self.calls.push(args.join(" "));
        Ok(i32::from(args[0] == "print" && !self.bootstrapped))
    }

    fn start(&mut self) -> io::Result<()> {
        self.calls.push("start".into());
        Ok(())
    }
}

#[test]
fn service_list_distinguishes_live_and_idle_jobs() {
    assert_eq!(
        listed_service("PID\tStatus\tLabel\n123\t0\tgit.kdnk.wm\n").unwrap(),
        ListedService::Running(123)
    );
    assert_eq!(
        listed_service("-\t0\tgit.kdnk.wm\n").unwrap(),
        ListedService::Stopped
    );
    assert_eq!(
        listed_service("456\t0\tother.wm\n").unwrap(),
        ListedService::Missing
    );
    assert!(listed_service("invalid\t0\tgit.kdnk.wm\n").is_err());
    assert!(listed_service("0\t0\tgit.kdnk.wm\n").is_err());
}

#[test]
fn only_a_durable_save_acknowledgement_allows_restart() {
    assert!(checkpoint_response(WmResponse::Success { data: "Layout saved".into() }).is_ok());
    assert!(
        checkpoint_response(WmResponse::Success {
            data: "Command executed successfully".into(),
        })
        .is_err()
    );
    assert!(
        checkpoint_response(WmResponse::Error {
            error: serde_json::json!({"message": "snapshot failed"}),
        })
        .is_err()
    );
}

#[test]
fn restart_saves_before_terminating_the_running_service() {
    let mut control = FixtureControl::default();
    restart_service(&mut control, "fixture").unwrap();
    assert_eq!(control.calls, ["state", "checkpoint", "kickstart -k fixture"]);
}

#[test]
fn stopped_service_can_restart_without_a_live_daemon() {
    let mut control = FixtureControl {
        pid: None,
        ..Default::default()
    };
    restart_service(&mut control, "fixture").unwrap();
    assert_eq!(control.calls, ["state", "start"]);
}

#[test]
fn stop_saves_before_bootout_and_disable() {
    let mut control = FixtureControl::default();
    stop_service(&mut control, "fixture", "domain", Path::new("fixture.plist")).unwrap();
    assert_eq!(control.calls, [
        "state",
        "checkpoint",
        "print fixture",
        "bootout domain fixture.plist",
        "disable fixture",
    ]);
}

#[test]
fn stop_saves_before_signaling_an_unloaded_running_service() {
    let mut control = FixtureControl {
        bootstrapped: false,
        ..Default::default()
    };
    stop_service(&mut control, "fixture", "domain", Path::new("fixture.plist")).unwrap();
    assert_eq!(control.calls, [
        "state",
        "checkpoint",
        "print fixture",
        "kill SIGTERM fixture"
    ]);
}

#[test]
fn save_or_state_failure_never_stops_or_restarts_the_daemon() {
    for restart in [false, true] {
        for state_error in [false, true] {
            let mut control = FixtureControl {
                checkpoint_error: !state_error,
                state_error,
                ..Default::default()
            };
            let result = if restart {
                restart_service(&mut control, "fixture")
            } else {
                stop_service(&mut control, "fixture", "domain", Path::new("fixture.plist"))
            };
            assert!(result.is_err());
            assert!(
                control.calls.iter().all(|call| matches!(call.as_str(), "state" | "checkpoint"))
            );
        }
    }
}

#[test]
fn legacy_command_success_never_stops_or_restarts_the_daemon() {
    for restart in [false, true] {
        let mut control = FixtureControl {
            checkpoint_data: "Command executed successfully",
            ..Default::default()
        };
        let result = if restart {
            restart_service(&mut control, "fixture")
        } else {
            stop_service(&mut control, "fixture", "domain", Path::new("fixture.plist"))
        };
        assert!(result.is_err());
        assert_eq!(control.calls, ["state", "checkpoint"]);
    }
}

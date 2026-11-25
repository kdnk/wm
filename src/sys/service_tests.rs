// Modified by kdnk for wm on 2026-11-18. Original attribution is preserved in LICENSE.
use std::collections::VecDeque;

use super::*;

fn fixture_agent() -> LaunchAgent { LaunchAgent::new("fixture.plist".into(), "gui/501".into()) }

struct FixtureControl {
    pid: Option<u32>,
    checkpoint_busy: usize,
    pid_after_busy: Option<u32>,
    checkpoint_error: bool,
    checkpoint_data: &'static str,
    state_error: bool,
    bootstrapped: bool,
    launchctl_exits: VecDeque<i32>,
    calls: Vec<String>,
}

impl Default for FixtureControl {
    fn default() -> Self {
        Self {
            pid: Some(42),
            checkpoint_busy: 0,
            pid_after_busy: None,
            checkpoint_error: false,
            checkpoint_data: "Layout saved",
            state_error: false,
            bootstrapped: true,
            launchctl_exits: VecDeque::new(),
            calls: Vec::new(),
        }
    }
}

impl ServiceControl for FixtureControl {
    fn running_pid(&mut self, target: &str) -> io::Result<Option<u32>> {
        assert_eq!(target, "gui/501/git.kdnk.wm");
        self.calls.push("state".into());
        if self.state_error {
            Err(io::Error::other("state query failed"))
        } else {
            Ok(self.pid)
        }
    }

    fn checkpoint(&mut self, pid: u32, _timeout: Duration) -> io::Result<()> {
        assert_eq!(Some(pid), self.pid);
        self.calls.push("checkpoint".into());
        if self.checkpoint_busy > 0 {
            self.checkpoint_busy -= 1;
            if let Some(pid) = self.pid_after_busy.take() {
                self.pid = Some(pid);
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "layout is still waiting for displays",
            ))
        } else if self.checkpoint_error {
            Err(io::Error::other("snapshot failed"))
        } else {
            checkpoint_response(WmResponse::Success {
                data: self.checkpoint_data.into(),
            })
        }
    }

    fn launchctl(&mut self, args: &[&str], _suppress_output: bool) -> io::Result<i32> {
        self.calls.push(args.join(" "));
        Ok(self.launchctl_exits.pop_front().unwrap_or_else(|| {
            if args[0] == "print" && !self.bootstrapped {
                LAUNCHCTL_SERVICE_NOT_FOUND
            } else {
                0
            }
        }))
    }

    fn start(&mut self) -> io::Result<()> {
        self.calls.push("start".into());
        Ok(())
    }
}

#[test]
fn start_waits_for_registration_before_kickstart() {
    let missing = LAUNCHCTL_SERVICE_NOT_FOUND;
    let mut control = FixtureControl {
        launchctl_exits: [missing, 0, 0, missing, 0, 0].into(),
        ..Default::default()
    };
    fixture_agent().start(&mut control, false).unwrap();
    assert_eq!(control.calls, [
        "print gui/501/git.kdnk.wm",
        "enable gui/501/git.kdnk.wm",
        "bootstrap gui/501 fixture.plist",
        "print gui/501/git.kdnk.wm",
        "print gui/501/git.kdnk.wm",
        "kickstart gui/501/git.kdnk.wm"
    ]);
}

#[test]
fn start_reports_bootstrap_failure_without_attempting_kickstart() {
    let mut control = FixtureControl {
        launchctl_exits: [LAUNCHCTL_SERVICE_NOT_FOUND, 0, 5].into(),
        ..Default::default()
    };
    let error = fixture_agent().start(&mut control, false).unwrap_err();
    assert!(error.to_string().contains("bootstrap failed (exit 5)"));
    assert_eq!(control.calls, [
        "print gui/501/git.kdnk.wm",
        "enable gui/501/git.kdnk.wm",
        "bootstrap gui/501 fixture.plist"
    ]);
}

#[test]
fn start_waits_for_old_job_to_disappear_before_replacing_it() {
    let missing = LAUNCHCTL_SERVICE_NOT_FOUND;
    let mut control = FixtureControl {
        launchctl_exits: [0, 5, 0, 0, missing, 0, missing, 0, 0].into(),
        ..Default::default()
    };
    fixture_agent().start(&mut control, true).unwrap();
    assert_eq!(control.calls, [
        "print gui/501/git.kdnk.wm",
        "kickstart gui/501/git.kdnk.wm",
        "state",
        "checkpoint",
        "bootout gui/501 fixture.plist",
        "print gui/501/git.kdnk.wm",
        "print gui/501/git.kdnk.wm",
        "bootstrap gui/501 fixture.plist",
        "print gui/501/git.kdnk.wm",
        "print gui/501/git.kdnk.wm",
        "kickstart gui/501/git.kdnk.wm"
    ]);
}

#[test]
fn start_keeps_old_job_registered_if_checkpoint_fails() {
    let mut control = FixtureControl {
        checkpoint_error: true,
        launchctl_exits: [0, 5].into(),
        ..Default::default()
    };
    let error = fixture_agent().start(&mut control, true).unwrap_err();
    assert_eq!(
        error.to_string(),
        "could not save layout; wm is still running: snapshot failed"
    );
    assert_eq!(control.calls, [
        "print gui/501/git.kdnk.wm",
        "kickstart gui/501/git.kdnk.wm",
        "state",
        "checkpoint"
    ]);
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
}

#[test]
fn checkpoint_failures_preserve_the_message_and_only_retry_explicit_temporary_errors() {
    for (value, expected_kind) in [
        (serde_json::Value::Null, io::ErrorKind::Other),
        (serde_json::json!(false), io::ErrorKind::Other),
        (serde_json::json!("true"), io::ErrorKind::Other),
        (serde_json::json!(true), io::ErrorKind::WouldBlock),
    ] {
        let error = checkpoint_response(WmResponse::Error {
            error: serde_json::json!({"message": "snapshot failed", "retryable": value}),
        })
        .unwrap_err();
        assert_eq!(error.kind(), expected_kind);
        assert_eq!(error.to_string(), "snapshot failed");
    }

    let legacy = checkpoint_response(WmResponse::Error {
        error: serde_json::json!({"message": "snapshot failed"}),
    })
    .unwrap_err();
    assert_eq!(legacy.kind(), io::ErrorKind::Other);

    assert_eq!(
        checkpoint_transport_error(WmMachClientError::ServiceUnavailable).kind(),
        io::ErrorKind::WouldBlock
    );
    assert_ne!(
        checkpoint_transport_error(WmMachClientError::RequestTimedOut).kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn restart_saves_before_terminating_the_running_service() {
    let mut control = FixtureControl::default();
    fixture_agent().restart(&mut control).unwrap();
    assert_eq!(control.calls, [
        "state",
        "checkpoint",
        "kickstart -k gui/501/git.kdnk.wm"
    ]);
}

#[test]
fn restart_retries_a_temporary_checkpoint_failure_before_terminating() {
    let mut control = FixtureControl {
        checkpoint_busy: 1,
        pid_after_busy: Some(43),
        ..Default::default()
    };
    fixture_agent().restart(&mut control).unwrap();
    assert_eq!(control.calls, [
        "state",
        "checkpoint",
        "state",
        "checkpoint",
        "kickstart -k gui/501/git.kdnk.wm"
    ]);
}

#[test]
fn checkpoint_deadline_expires_if_the_layout_stays_unready() {
    let mut control = FixtureControl {
        checkpoint_busy: usize::MAX,
        ..Default::default()
    };
    let error =
        checkpoint_running_service(&mut control, "gui/501/git.kdnk.wm", Duration::from_millis(10))
            .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("layout is still waiting for displays"));
    assert!(error.to_string().contains("wm is still running"));
}

#[test]
fn stopped_service_can_restart_without_a_live_daemon() {
    let mut control = FixtureControl {
        pid: None,
        ..Default::default()
    };
    fixture_agent().restart(&mut control).unwrap();
    assert_eq!(control.calls, ["state", "start"]);
}

#[test]
fn stop_saves_before_bootout_and_disable() {
    let mut control = FixtureControl::default();
    fixture_agent().stop(&mut control).unwrap();
    assert_eq!(control.calls, [
        "state",
        "checkpoint",
        "print gui/501/git.kdnk.wm",
        "bootout gui/501 fixture.plist",
        "disable gui/501/git.kdnk.wm",
    ]);
}

#[test]
fn stop_saves_before_signaling_an_unloaded_running_service() {
    let mut control = FixtureControl {
        bootstrapped: false,
        ..Default::default()
    };
    fixture_agent().stop(&mut control).unwrap();
    assert_eq!(control.calls, [
        "state",
        "checkpoint",
        "print gui/501/git.kdnk.wm",
        "kill SIGTERM gui/501/git.kdnk.wm"
    ]);
}

#[test]
fn stop_disables_the_job_even_if_bootout_exits_unsuccessfully() {
    let mut control = FixtureControl {
        launchctl_exits: [0, 5, 6].into(),
        ..Default::default()
    };
    let error = fixture_agent().stop(&mut control).unwrap_err();
    assert_eq!(error.to_string(), "bootout exit 5, disable exit 6");
    assert_eq!(control.calls, [
        "state",
        "checkpoint",
        "print gui/501/git.kdnk.wm",
        "bootout gui/501 fixture.plist",
        "disable gui/501/git.kdnk.wm",
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
                fixture_agent().restart(&mut control)
            } else {
                fixture_agent().stop(&mut control)
            };
            assert!(result.is_err());
            let expected: &[&str] = if state_error {
                &["state"]
            } else {
                &["state", "checkpoint"]
            };
            assert_eq!(control.calls, expected);
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
            fixture_agent().restart(&mut control)
        } else {
            fixture_agent().stop(&mut control)
        };
        assert!(result.is_err());
        assert_eq!(control.calls, ["state", "checkpoint"]);
    }
}

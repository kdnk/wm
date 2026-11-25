// Modified by kdnk for wm on 2026-11-26. Original attribution is preserved in LICENSE.
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError, bounded};
use serde_json::json;
use wm_protocol::ConfigCommand;

use super::{encode_error, encode_success, send_encoded_response, send_error_response};
use crate::actor::config as config_actor;
use crate::sys::mach::{OwnedMachReply, mach_msg_header_t};

const CONFIG_QUEUE_CAPACITY: usize = 8;

pub(super) enum ConfigRequest {
    Query,
    Apply(ConfigCommand),
}

struct Job {
    request: ConfigRequest,
    destination: OwnedMachReply,
}

pub(super) struct ConfigWorker {
    jobs: Sender<Job>,
}

impl ConfigWorker {
    pub(super) fn spawn(config_tx: config_actor::Sender) -> std::io::Result<Self> {
        let (jobs, pending) = bounded::<Job>(CONFIG_QUEUE_CAPACITY);
        // Config commands can wait for disk I/O, so keep them off the reactor run loop.
        std::thread::Builder::new().name("ipc-config".into()).spawn(move || {
            while let Ok(mut job) = pending.recv() {
                let response = job.request.respond(&config_tx);
                send_encoded_response(job.destination.header_mut(), &response);
            }
        })?;
        Ok(Self { jobs })
    }

    pub(super) fn dispatch(&self, request: ConfigRequest, header: &mut mach_msg_header_t) {
        let Some(destination) = (unsafe { OwnedMachReply::retain(header) }) else {
            send_error_response(header, "Failed to retain IPC reply port");
            return;
        };
        match self.jobs.try_send(Job { request, destination }) {
            Ok(()) => {}
            Err(TrySendError::Full(mut job)) => send_error_response(
                job.destination.header_mut(),
                "Too many config requests in flight; try again",
            ),
            Err(TrySendError::Disconnected(mut job)) => send_error_response(
                job.destination.header_mut(),
                "Config request service is unavailable",
            ),
        }
    }
}

impl ConfigRequest {
    fn respond(self, config_tx: &config_actor::Sender) -> Vec<u8> {
        match self {
            Self::Query => match perform(config_tx, config_actor::Event::QueryConfig) {
                Ok(config) => encode_success(config),
                Err(error) => encode_error(json!({
                    "message": "Failed to get config response",
                    "details": error,
                })),
            },
            Self::Apply(command) => match perform(config_tx, |response| {
                config_actor::Event::ApplyConfig { cmd: command, response }
            }) {
                Ok(Ok(())) => encode_success("Config applied successfully"),
                Ok(Err(message)) => encode_error(json!({ "message": message })),
                Err(error) => encode_error(json!({
                    "message": format!("Failed to apply config: {error}"),
                })),
            },
        }
    }
}

fn perform<T>(
    config_tx: &config_actor::Sender,
    make_event: impl FnOnce(SyncSender<T>) -> config_actor::Event,
) -> Result<T, String>
where
    T: Send + 'static,
{
    // Buffer one reply so the actor never waits for this worker to receive it.
    let (response, result) = sync_channel(1);
    config_tx
        .try_send(make_event(response))
        .map_err(|error| format!("Failed to send config query: {error}"))?;
    result.recv_timeout(Duration::from_secs(5)).map_err(|error| match error {
        RecvTimeoutError::Timeout => "Failed to get response: config actor timed out".into(),
        RecvTimeoutError::Disconnected => {
            "Failed to get response: config actor disconnected before replying".into()
        }
    })
}

# wm-client

`wm-client` is the Rust client for Wm's JSON-over-Mach IPC API. It is meant
for macOS plugins and companion applications that need to query Wm, execute a
command, or subscribe to Wm events without depending on the window manager.

## Query Wm

```rust
use wm_client::WmMachClient;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = WmMachClient::connect()?;
    let workspaces = client.get_workspaces(None)?;
    println!("{workspaces:#?}");
    Ok(())
}
```

Run the complete [query example](examples/query.rs) from the Wm repository:

```sh
cargo run -p wm-client --example query
```

## Listen for events

`subscribe` returns a subscription handle whose `recv_event` method blocks until
Wm publishes the next matching, typed `WmEvent`:

```rust
use wm_client::{EventKind, WmMachClient};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = WmMachClient::connect()?;
    let subscription = client.subscribe(EventKind::WorkspaceChanged)?;

    loop {
        let event = subscription.recv_event()?;
        println!("{}", serde_json::to_string_pretty(&event)?);
    }
}
```

Run the complete [event listener example](examples/listen.rs). It listens for
all events by default, or for the event name passed as its first argument:

```sh
cargo run -p wm-client --example listen
cargo run -p wm-client --example listen -- workspace_changed
```

Supported event names are `workspace_changed`, `windows_changed`,
`window_title_changed`, `focused_window_changed`, `stacks_changed`, and
`layout_changed`, `selection_changed`, `binding_mode_changed`, and `config_changed`. Use `*` to listen for all events.

For a more complete example, see the [dimmer example](examples/dimmer.rs),
which dims unfocused windows and updates them as Wm events arrive:

```sh
cargo run -p wm-client --example dimmer
```

## Claim windows

A `WindowManagementSession` lets a companion app take windows out of Wm's
management (for example, a scratchpad). Wm stops tiling, placing, animating,
and dragging a claimed window, and keeps its workspace, floating state, and
layout slot for when it is released:

```rust
use wm_client::{WmMachClient, WindowClaimFlags, WindowId};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = WmMachClient::connect()?;
    let mut session = client.window_management_session()?;
    let window = WindowId { pid: 1234, idx: 56 };
    session.claim_window_with_flags(window, WindowClaimFlags::SUPPRESS_FOCUS_FOLLOWS_MOUSE)?;
    // ... manage the window ...
    session.release_window(window)?;
    Ok(())
}
```

Claims end when they are released or when the session is dropped, including
when the process exits. Claiming is idempotent and updates the flags of a
claim the session already holds. `release_window` accepts the id that was
claimed even if macOS later gives the window a new identity. If macOS reveals
that two claimed ids are one window, the claim on its original identity
survives, and requests from the session whose claim ended fail with "Window
owned by another external manager". Claimed windows report
`externally_managed: true` in window queries.
`SUPPRESS_FOCUS_FOLLOWS_MOUSE` pauses focus follows mouse only while the
claimed window is focused.

The [owner example](examples/owner.rs) claims one window until it is stopped:

```sh
cargo run -p wm-client --example owner -- --window-id 1234:1
```

Set `WM_BS_NAME` to use a non-default Wm bootstrap service (for example,
when running multiple instances).

## Binding modes

Use `client.get_binding_mode()` or `wm-cli query binding-mode` to read the
active mode name (for example, `"default"`). Subscribe with
`wm-cli subscribe mach binding_mode_changed` or
`client.subscribe(EventKind::BindingModeChanged)` to receive changes:

```json
{"type":"binding_mode_changed","previous_mode":"default","mode":"resize"}
```

Changes include configuration reloads that reset the mode to `default`.
Re-selecting the active mode or selecting an unknown mode emits no event.
CLI command subscriptions receive `WM_BINDING_MODE`,
`WM_PREVIOUS_BINDING_MODE`, and the usual `WM_EVENT_JSON` environment variables.
Binding modes are global, so these events have no space or display context;
`WmEvent::space_id()` keeps its `u64` return type and returns `0` for global
events. Use `WmEvent::space_id_opt()` for explicit optional space context
(`None` for global events).

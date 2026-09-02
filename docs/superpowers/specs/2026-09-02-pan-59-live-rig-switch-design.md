# PAN-59: Live rig-config switch — design

**Ticket:** PAN-59 — "Operators should be able to switch rig config while pancetta is running,
without restarting." Reported by K5TR (George): audio has a live device picker (`d` key); rig has
no equivalent, so changing rig model/port/baud/PTT method requires re-running `pancetta setup` and
restarting.

## Decision: dedicated `SelectRig` command + modal, not the dead hot-reload path

Two candidate designs existed going in:

1. Wire up the existing (dead) hot-reload scaffolding: `pancetta_config::ConfigHotReload`
   (`pancetta-config/src/hot_reload.rs`, a file-watcher) + `classify_config_reload`
   (`pancetta/src/coordinator/health.rs:112`, which already classifies `rig` as `SafeLive`).
2. A dedicated `TuiCommand::SelectRig` + ratatui modal, mirroring the existing
   `TuiCommand::SelectDevice` audio-device picker end to end.

**Chose (2).** The hot-reload scaffolding is a dead end for this ticket, not a shortcut:

- `classify_config_reload` has zero production call sites (only direct unit tests in
  `pancetta/tests/coord_robustness.rs` and an unused re-export from `coordinator/mod.rs`).
- `ConfigHotReload`'s file watcher is never constructed outside its own `#[cfg(test)]` module
  (confirmed by repo-wide grep; matches the doc comment on `pancetta/src/coordinator/effort.rs:29`).
- It's shaped wrong for this ticket anyway: it's a *file-watch* reload (detects external edits to
  `pancetta.toml` on disk), not an in-TUI operator action. Wiring a file watcher to fire an
  in-process reconnect is a bigger, differently-shaped change than reusing the already-proven
  command-driven pattern audio uses.

No named rig-profile concept is needed. The ticket only asks for switching the *live* config, not
managing multiple saved profiles — `pancetta-config::Config` keeps its single flat `rig: RigConfig`
field (`pancetta-config/src/lib.rs:138`). Profiles would be a separate, larger ticket if wanted
later.

## What actually changes live

`start_hamlib_component` (`pancetta/src/coordinator/hamlib.rs:1069`) already reads rig config
**fresh from shared state** on every call: `let rig_config = { let config = self.config.read().await;
config.rig.clone() };`. `self.config` is the same `Arc<RwLock<Config>>` that `SelectDevice`'s
handler (`pancetta/src/coordinator/tui_relay.rs:1996-2040`) already mutates for audio. Nothing about
Hamlib startup is baked in at construction time.

`teardown_hamlib()` (`hamlib.rs:684`) + `start_hamlib_component()` (`hamlib.rs:1069`) is **already**
the crash-restart reconnect sequence (`restart_component(ComponentId::Hamlib)` in `health.rs`) — it
aborts orphan watchdog tasks, retries PTT-off up to 3× before declaring teardown done, bumps a
`hamlib_generation` epoch, reaps the old `rigctld_process`, and re-spawns rigctld from whatever
`self.config.rig` currently holds. **A live rig switch reuses this exact pair** — no new reconnect
logic, only new wiring to reach it from an operator action.

### The `&mut self` wrinkle

`SelectDevice`'s handler runs inside `start_tui_pipeline`'s spawned `'static` command-relay task
(`tui_relay.rs`, the `cmd_handle = tokio::spawn(async move { ... })` block starting around line
909), which only holds cloned `Arc`/channel handles — never `&mut ApplicationCoordinator`. Audio
tolerates this because the audio thread owns its own `cpal::Stream` independently. Hamlib does not:
`teardown_hamlib`/`start_hamlib_component` are `pub(crate) async fn` methods needing `&mut self`
(they touch `self.hamlib_orphans`, `self.rig_handle`, `self.rigctld_process`,
`self.hamlib_generation`, `self.hamlib_command_loop_ready`, `self.hamlib_command_in_flight`).

The only place in the running process that already holds `&mut self` in a loop is
`run_main_loop` (`pancetta/src/coordinator/health.rs`, `pub(crate) async fn run_main_loop(&mut
self)`). So `SelectRig`'s live-apply step is a **request routed through a new channel that
`run_main_loop`'s `tokio::select!` consumes**, not a direct method call from the command-relay task
— analogous to how `AudioReopenRequest` routes into the audio thread, except the receiving side here
is `run_main_loop` itself rather than a dedicated thread.

New type (`pancetta/src/coordinator/hamlib.rs`):

```rust
pub struct HamlibReconnectRequest {
    pub respond: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
}
```

New `ApplicationCoordinator` fields (`coordinator/mod.rs`), constructed once in `new()`:

```rust
hamlib_reconnect_tx: tokio::sync::mpsc::Sender<crate::coordinator::hamlib::HamlibReconnectRequest>,
hamlib_reconnect_rx: Option<tokio::sync::mpsc::Receiver<crate::coordinator::hamlib::HamlibReconnectRequest>>,
```

`run_main_loop` takes the receiver once before its loop and adds a `tokio::select!` arm:

```rust
Some(req) = hamlib_reconnect_rx.recv() => {
    self.handle_hamlib_reconnect_request(req).await;
}
```

`handle_hamlib_reconnect_request` (new, `hamlib.rs`, `#[cfg(feature = "pancetta-hamlib")]`):
refuses (responds with an `Err`, does **not** tear down) while `self.ptt_active.load(Ordering::Acquire)`
is true — tearing down Hamlib mid-key-down would yank CAT/PTT control out from under an active
transmission — otherwise runs `self.teardown_hamlib().await; let result =
self.start_hamlib_component().await; let _ = req.respond.send(result);`. A
`#[cfg(not(feature = "pancetta-hamlib"))]` stub immediately responds with an error ("rig control not
compiled in"). This mirrors the codebase's existing safety instinct around Hamlib/PTT (see
`TxInhibitGuard`, `hamlib_command_loop_ready`) rather than inventing a new pattern.

`SelectRig`'s handler in `tui_relay.rs` sends a `HamlibReconnectRequest`, awaits the oneshot with a
5s timeout (mirroring `SelectDevice`'s existing `tokio::time::timeout(Duration::from_secs(5),
resp_rx)` pattern), and reports the outcome to the TUI via `TuiMessage::StatusUpdate` — same shape as
audio's live-switch status reporting.

## Config surface actually exposed to the operator

`pancetta setup`'s `setup_rig()`/`setup_ptt()` (`pancetta/src/main.rs:1316-1430`) only ever prompt
for **4 fields**: `rig.interface.enabled` (yes/no), `rig.model` (free text), `rig.interface.port`
(enumerated via `serialport::available_ports()`, falls back to free text), `rig.interface.baud_rate`
(pick from `[4800, 9600, 19200, 38400, 57600, 115200]`), and `rig.ptt.method` (pick from `[None, Cat,
Serial, Vox]`). `RigConfig` itself has ~12 nested sub-configs (frequency limits, band switching,
antenna switching, power control, filters, calibration, quirks, …) that neither the CLI wizard nor
this ticket touch — the live-switch modal covers exactly the same 4 fields as the wizard, nothing
more. This mirrors the wizard's own already-validated scope rather than inventing a bigger surface.

There is **no reusable runtime rig-config form to repurpose**: `setup_rig`/`setup_ptt` are
blocking `println!`/stdin CLI prompts that run before the ratatui TUI starts and cannot be invoked
live from inside the running raw-mode terminal (they'd fight over stdin/raw-mode). The live-switch
modal is new ratatui UI, structurally modeled on `DeviceSelectionState`/
`render_device_selection_modal` (`pancetta-tui/src/tui_runner.rs:2013`) but as a 4-field *form*
(Tab cycles focus between Model/Port/Baud/PTT; Up/Down cycles the enumerated Port/Baud/PTT lists;
printable chars edit the Model text field; Enter confirms; Esc cancels) rather than a single
list-select, since rig fields aren't one enumerable list the way audio devices are.

## Data flow

1. **Startup** (`tui_relay.rs`, alongside the existing `DeviceListUpdate` push): coordinator calls
   `serialport::available_ports()` and reads `cfg.rig`, pushes a new
   `TuiMessage::RigConfigUpdate { available_ports: Vec<String>, current_model: String, current_port:
   String, current_baud_rate: u32, current_ptt_method: pancetta_config::rig::PttMethod }` once. The
   TUI seeds `App::rig_selection` from it (`App::apply_rig_config_update`).
2. **Operator presses `i`** (new binding — `d`/`r`/`R` are all taken; see key-choice note below):
   opens the modal, already pre-populated with current values.
3. **Operator edits fields, presses Enter**: TUI sends
   `TuiCommand::SelectRig { model: String, port: String, baud_rate: u32, ptt_method:
   pancetta_config::rig::PttMethod }` (always all 4 values — no partial-update `Option` semantics;
   the form always has a definite value for each field, unlike audio's independent input/output
   axes).
4. **Coordinator handler** (`tui_relay.rs`, new match arm): writes all 4 fields into the live
   `Arc<RwLock<Config>>`, persists via a new `Config::set_rig_in_file` (mirrors
   `set_audio_devices_in_file`, `pancetta-config/src/lib.rs:400`), then sends a
   `HamlibReconnectRequest` and awaits it (5s timeout), then reports the outcome via
   `TuiMessage::StatusUpdate`.
5. **`run_main_loop`** receives the request, checks `ptt_active`, runs
   `teardown_hamlib()` → `start_hamlib_component()` (which re-reads `self.config.rig`, now the new
   values), responds.

## Key choice

`r` is taken (`ResendQso`), `R` is taken (Recent-QSOs panel), `d` is taken (audio device picker).
Chose **`i`** — "interface", the codebase's own term for the CAT/rig link (`CatInterfaceConfig`,
`rig.interface.*`) — documented in `pancetta-tui/src/keymap.rs` under `Category::AudioDevices`
("Audio & devices" — a reasonable home for both device pickers), `essential: false` (doesn't disturb
the README's fixed 10-row essentials table).

## Non-goals

- Named/saved rig profiles (separate future ticket if wanted).
- Exposing the other ~8 `RigConfig` sub-sections (frequency limits, band/antenna switching, power
  control, filters, calibration, quirks) live — out of scope, matches the CLI wizard's own scope.
- Wiring `ConfigHotReload`'s file-watcher — actively rejected above.

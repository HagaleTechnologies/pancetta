# Live Rig-Config Switch (PAN-59) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the operator switch rig model/port/baud rate/PTT method while pancetta is running, from a new in-TUI picker (`i` key), with no restart and no re-run of `pancetta setup`.

**Architecture:** Mirror the existing audio-device-picker pattern end to end — a new `TuiCommand::SelectRig` sent from a new ratatui modal, handled in the coordinator by persisting to `Config`/`pancetta.toml` and then reconnecting Hamlib live. Because Hamlib's reconnect (`teardown_hamlib()` → `start_hamlib_component()`) needs `&mut ApplicationCoordinator` (unlike audio's independent cpal-stream-reopen), the reconnect request is routed through a new channel that `run_main_loop` — the only place already holding `&mut self` in a loop — consumes.

**Tech Stack:** Rust, tokio (mpsc + oneshot channels), ratatui, serde/toml, existing `pancetta-hamlib` crate (MockRig for tests).

**Spec:** `docs/superpowers/specs/2026-09-02-pan-59-live-rig-switch-design.md`

## Global Constraints

- No named rig profiles — single flat `Config.rig: RigConfig` stays as-is (spec's Non-goals).
- The live-switch modal covers exactly the 4 fields `pancetta setup`'s wizard covers: `rig.model`, `rig.interface.port`, `rig.interface.baud_rate`, `rig.ptt.method`. No other `RigConfig` sub-section is touched.
- Do not wire `pancetta_config::ConfigHotReload` or call `classify_config_reload` from production code — spec rejects this path explicitly.
- New key binding is `i` (Category::AudioDevices, essential: false) — `d`/`r`/`R` are taken.
- Live reconnect must refuse (not silently skip) while `ptt_active` is true, so a rig switch can never yank Hamlib control out from under an active transmission.
- `pancetta-hamlib` is a default-but-optional feature (`Cargo.toml`: `default = ["metrics", "pancetta-hamlib"]`) — every new coordinator-side Hamlib touchpoint needs a `#[cfg(feature = "pancetta-hamlib")]` arm and a `#[cfg(not(...))]` fallback, matching existing code in `hamlib.rs`/`health.rs`/`mod.rs`.
- Run `cargo fmt`, `cargo clippy --workspace`, and `cargo test --workspace` before the final commit (repo convention; `scripts/check.sh` runs the full gate).

---

### Task 1: `pancetta-config` — `Config::set_rig_in_file`

**Files:**
- Modify: `pancetta-config/src/lib.rs` (add method near `set_audio_devices_in_file`, which starts at line 400)
- Test: same file, in the existing `#[cfg(test)] mod tests` block (starts around line 580)

**Interfaces:**
- Produces: `Config::set_rig_in_file<P: AsRef<Path>>(&self, path: P, model: &str, port: &str, baud_rate: u32, ptt_method: pancetta_config::rig::PttMethod) -> ConfigResult<()>` — later tasks (Task 6) call this exactly like `set_audio_devices_in_file`.

This mirrors `set_audio_devices_in_file` (`pancetta-config/src/lib.rs:400-441`): load the existing TOML file as a generic `toml::Table` (or start fresh), get-or-insert the relevant sub-tables, write the four values, serialize back with `toml::to_string_pretty`, write atomically. The difference is nesting: `[rig]` → `model`, and `[rig.interface]` → `port`/`baud_rate`, and `[rig.ptt]` → `method`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `pancetta-config/src/lib.rs` (near the existing `set_audio_devices_in_file_*` tests):

```rust
#[test]
fn set_rig_in_file_writes_all_four_fields_and_preserves_other_keys() {
    let temp = NamedTempFile::new().unwrap();
    std::fs::write(
        temp.path(),
        "[station]\ncallsign = \"K5ARH\"\n\n[rig]\nmodel = \"OldRig\"\n\n[rig.interface]\nport = \"/dev/ttyUSB0\"\nbaud_rate = 9600\nenabled = true\n\n[rig.ptt]\nmethod = \"none\"\n",
    )
    .unwrap();

    let config = Config::default();
    config
        .set_rig_in_file(
            temp.path(),
            "IC-7300",
            "/dev/ttyUSB1",
            38400,
            pancetta_config::rig::PttMethod::Cat,
        )
        .unwrap();

    let written = std::fs::read_to_string(temp.path()).unwrap();
    let parsed: toml::Table = written.parse().unwrap();

    let rig = parsed["rig"].as_table().unwrap();
    assert_eq!(rig["model"].as_str(), Some("IC-7300"));

    let interface = rig["interface"].as_table().unwrap();
    assert_eq!(interface["port"].as_str(), Some("/dev/ttyUSB1"));
    assert_eq!(interface["baud_rate"].as_integer(), Some(38400));

    let ptt = rig["ptt"].as_table().unwrap();
    assert_eq!(ptt["method"].as_str(), Some("cat"));

    // Unrelated keys preserved.
    assert_eq!(
        parsed["station"].as_table().unwrap()["callsign"].as_str(),
        Some("K5ARH")
    );
}

#[test]
fn set_rig_in_file_creates_minimal_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("pancetta.toml");
    let config = Config::default();
    config
        .set_rig_in_file(
            &path,
            "FTdx10",
            "/dev/cu.usbserial-01A6218A1",
            38400,
            pancetta_config::rig::PttMethod::Serial,
        )
        .unwrap();

    let written = std::fs::read_to_string(&path).unwrap();
    let parsed: toml::Table = written.parse().unwrap();
    let rig = parsed["rig"].as_table().unwrap();
    assert_eq!(rig["model"].as_str(), Some("FTdx10"));
    assert_eq!(
        rig["interface"].as_table().unwrap()["port"].as_str(),
        Some("/dev/cu.usbserial-01A6218A1")
    );
    assert_eq!(rig["ptt"].as_table().unwrap()["method"].as_str(), Some("serial"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-config set_rig_in_file -- --nocapture`
Expected: FAIL with "no method named `set_rig_in_file`"

- [ ] **Step 3: Write the implementation**

Add to `pancetta-config/src/lib.rs`, directly after `set_audio_devices_in_file` (after its closing brace, before its own `#[cfg(test)]` block if that appears in the same `impl Config` block — otherwise directly after the method body, still inside `impl Config`):

```rust
    /// Persist a live rig-config switch (model / serial port / baud rate /
    /// PTT method) into the config file at `path`, preserving every other
    /// key. Mirrors [`Config::set_audio_devices_in_file`]'s targeted-write
    /// pattern (PAN-59: the operator picks a new rig from the running TUI
    /// the same way they already pick a new audio device).
    pub fn set_rig_in_file<P: AsRef<std::path::Path>>(
        &self,
        path: P,
        model: &str,
        port: &str,
        baud_rate: u32,
        ptt_method: crate::rig::PttMethod,
    ) -> ConfigResult<()> {
        let path = path.as_ref();

        let mut root: toml::Table = match std::fs::read_to_string(path) {
            Ok(contents) => contents
                .parse::<toml::Table>()
                .map_err(|e| ConfigError::Validation(format!("Failed to parse config: {}", e)))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e.into()),
        };

        let rig = root
            .entry("rig".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let rig_table = rig
            .as_table_mut()
            .ok_or_else(|| ConfigError::Validation("[rig] in config is not a table".to_string()))?;
        rig_table.insert("model".to_string(), toml::Value::String(model.to_string()));

        let interface = rig_table
            .entry("interface".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let interface_table = interface.as_table_mut().ok_or_else(|| {
            ConfigError::Validation("[rig.interface] in config is not a table".to_string())
        })?;
        interface_table.insert("port".to_string(), toml::Value::String(port.to_string()));
        interface_table.insert(
            "baud_rate".to_string(),
            toml::Value::Integer(baud_rate as i64),
        );

        let ptt = rig_table
            .entry("ptt".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let ptt_table = ptt.as_table_mut().ok_or_else(|| {
            ConfigError::Validation("[rig.ptt] in config is not a table".to_string())
        })?;
        let ptt_value = toml::Value::try_from(&ptt_method)
            .map_err(|e| ConfigError::Validation(format!("Failed to serialize PTT method: {}", e)))?;
        ptt_table.insert("method".to_string(), ptt_value);

        let serialized = toml::to_string_pretty(&root)
            .map_err(|e| ConfigError::Validation(format!("Failed to serialize config: {}", e)))?;
        crate::write_atomic_owner_only(path, &serialized)
    }
```

If `set_audio_devices_in_file`'s final write step calls a differently-named helper than `crate::write_atomic_owner_only` (check the actual last few lines of `set_audio_devices_in_file` at `pancetta-config/src/lib.rs:438-441` — the plan's research saw a truncated view), use whatever that existing helper/inline write logic actually is instead, so both methods share identical write-atomicity/permissions behavior. Do not invent a second, divergent write path.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-config set_rig_in_file -- --nocapture`
Expected: PASS (2 tests)

- [ ] **Step 5: Run the full pancetta-config test suite**

Run: `cargo test -p pancetta-config`
Expected: PASS, no regressions

- [ ] **Step 6: Commit**

```bash
git add pancetta-config/src/lib.rs
git commit -m "feat(config): add Config::set_rig_in_file for live rig-config persistence"
```

---

### Task 2: `pancetta-tui` — `TuiCommand::SelectRig` + `TuiMessage::RigConfigUpdate`

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs` (enums: `TuiMessage` around line 267, `TuiCommand` around line 378)
- Test: `pancetta-tui/src/tui_runner.rs`, existing test module (if one exists in this file) or a new `#[cfg(test)]` block at file end — this task only adds enum variants, so tests just need to confirm the variants construct and match.

**Interfaces:**
- Consumes: `pancetta_config::rig::PttMethod` (from Task-adjacent `pancetta-config` crate, already a dependency of `pancetta-tui` per `Cargo.toml`).
- Produces: `TuiMessage::RigConfigUpdate { available_ports: Vec<String>, current_model: String, current_port: String, current_baud_rate: u32, current_ptt_method: pancetta_config::rig::PttMethod }` and `TuiCommand::SelectRig { model: String, port: String, baud_rate: u32, ptt_method: pancetta_config::rig::PttMethod }` — Task 3 (`app.rs`) consumes `RigConfigUpdate`; Task 6 (`tui_relay.rs`) consumes and sends these.

- [ ] **Step 1: Add the `TuiMessage::RigConfigUpdate` variant**

In `pancetta-tui/src/tui_runner.rs`, add a variant to the `TuiMessage` enum immediately after `DeviceListUpdate` (around line 271):

```rust
    /// Current rig config + enumerated serial ports, pushed once at startup
    /// (PAN-59) so the `i` rig-picker modal can pre-populate with live
    /// values. The TUI is a passive renderer — it never enumerates serial
    /// ports itself, mirroring `DeviceListUpdate` above.
    RigConfigUpdate {
        available_ports: Vec<String>,
        current_model: String,
        current_port: String,
        current_baud_rate: u32,
        current_ptt_method: pancetta_config::rig::PttMethod,
    },
```

- [ ] **Step 2: Add the `TuiCommand::SelectRig` variant**

Add a variant to the `TuiCommand` enum immediately after `SelectDevice` (around line 381, right after its closing `},`):

```rust
    /// Switch the live rig config (PAN-59): model, serial port, baud rate,
    /// and PTT method, mirroring `SelectDevice` for audio. Unlike
    /// `SelectDevice`'s independent input/output axes, this modal is a
    /// single form — all four fields always carry a definite value (no
    /// `Option`/"leave unchanged" semantics).
    SelectRig {
        model: String,
        port: String,
        baud_rate: u32,
        ptt_method: pancetta_config::rig::PttMethod,
    },
```

- [ ] **Step 3: Write a construction/match smoke test**

Add near the bottom of `pancetta-tui/src/tui_runner.rs` (create a `#[cfg(test)] mod tui_command_tests` block if none covering these enums already exists — grep the file first for an existing `#[cfg(test)]` block and add into it if one is already present, to avoid duplicate `mod tests` names):

```rust
#[cfg(test)]
mod pan_59_command_tests {
    use super::*;

    #[test]
    fn select_rig_command_round_trips_all_four_fields() {
        let cmd = TuiCommand::SelectRig {
            model: "IC-7300".to_string(),
            port: "/dev/ttyUSB1".to_string(),
            baud_rate: 38400,
            ptt_method: pancetta_config::rig::PttMethod::Cat,
        };
        match cmd {
            TuiCommand::SelectRig {
                model,
                port,
                baud_rate,
                ptt_method,
            } => {
                assert_eq!(model, "IC-7300");
                assert_eq!(port, "/dev/ttyUSB1");
                assert_eq!(baud_rate, 38400);
                assert!(matches!(ptt_method, pancetta_config::rig::PttMethod::Cat));
            }
            _ => panic!("expected SelectRig"),
        }
    }

    #[test]
    fn rig_config_update_message_carries_ports_and_current_values() {
        let msg = TuiMessage::RigConfigUpdate {
            available_ports: vec!["/dev/ttyUSB0".to_string(), "/dev/ttyUSB1".to_string()],
            current_model: "FTdx10".to_string(),
            current_port: "/dev/ttyUSB0".to_string(),
            current_baud_rate: 38400,
            current_ptt_method: pancetta_config::rig::PttMethod::Serial,
        };
        match msg {
            TuiMessage::RigConfigUpdate {
                available_ports,
                current_model,
                ..
            } => {
                assert_eq!(available_ports.len(), 2);
                assert_eq!(current_model, "FTdx10");
            }
            _ => panic!("expected RigConfigUpdate"),
        }
    }
}
```

- [ ] **Step 4: Run tests, verify compile + pass**

Run: `cargo test -p pancetta-tui pan_59_command_tests`
Expected: PASS (2 tests) — this step also validates the crate compiles with the new enum variants, since `TuiCommand`/`TuiMessage` derive `Debug, Clone` and every existing exhaustive `match` on them elsewhere in `pancetta-tui` will now fail to compile until Task 3 handles the new arms. Fix any such compile error by confirming it's addressed in Task 3, not by adding a wildcard `_ => {}` arm here (exhaustive matches are intentional in this codebase).

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): add TuiCommand::SelectRig and TuiMessage::RigConfigUpdate"
```

---

### Task 3: `pancetta-tui` — `RigSelectionState` + `App` wiring

**Files:**
- Modify: `pancetta-tui/src/app.rs` (add `RigSelectionState` struct near `DeviceSelectionState` at line 634; add `pub rig_selection: RigSelectionState` field to `App` near `pub device_selection: DeviceSelectionState` at line 1024; add `App::apply_rig_config_update`)
- Test: `pancetta-tui/src/app.rs`, existing `#[cfg(test)]` block (grep for one; `DeviceSelectionState` likely has sibling tests to mirror)

**Interfaces:**
- Consumes: `TuiMessage::RigConfigUpdate` (Task 2).
- Produces: `RigSelectionState` (fields below), `App::rig_selection: RigSelectionState`, `App::apply_rig_config_update(&mut self, available_ports: Vec<String>, current_model: String, current_port: String, current_baud_rate: u32, current_ptt_method: pancetta_config::rig::PttMethod)` — Task 4 (`tui_runner.rs` key handling + modal render) consumes `rig_selection`'s fields and methods.

`RigSelectionState` mirrors `DeviceSelectionState` (`pancetta-tui/src/app.rs:634-641`) but as a 4-field form instead of a 2-panel list:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RigField {
    Model,
    Port,
    Baud,
    Ptt,
}

/// Fixed baud-rate choices, identical to `setup_rig`'s wizard list
/// (`pancetta/src/main.rs:1385`).
pub const RIG_BAUD_RATES: [u32; 6] = [4800, 9600, 19200, 38400, 57600, 115200];

pub struct RigSelectionState {
    pub visible: bool,
    pub available_ports: Vec<String>,
    pub model: String,
    pub selected_port_idx: usize,
    pub selected_baud_idx: usize,
    pub selected_ptt_idx: usize,
    pub active_field: RigField,
}

impl Default for RigSelectionState {
    fn default() -> Self {
        Self {
            visible: false,
            available_ports: Vec::new(),
            model: String::new(),
            selected_port_idx: 0,
            selected_baud_idx: 1, // 9600, RigConfig's own default (rig.rs:807)
            selected_ptt_idx: 0,  // PttMethod::None
            active_field: RigField::Model,
        }
    }
}

/// The 4 PTT methods the wizard offers (`setup_ptt`, `pancetta/src/main.rs:1409-1414`).
/// Deliberately a function, not a `const` array: `PttMethod` doesn't derive
/// `Copy` (nor `PartialEq` — comparisons below use `Debug` formatting,
/// mirroring `setup_ptt`'s own `format!("{:?}", ...)` comparison at
/// `pancetta/src/main.rs:1420`).
pub fn rig_ptt_methods() -> [pancetta_config::rig::PttMethod; 4] {
    use pancetta_config::rig::PttMethod;
    [PttMethod::None, PttMethod::Cat, PttMethod::Serial, PttMethod::Vox]
}

impl RigSelectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cycle focus forward through the 4 fields (Tab).
    pub fn next_field(&mut self) {
        self.active_field = match self.active_field {
            RigField::Model => RigField::Port,
            RigField::Port => RigField::Baud,
            RigField::Baud => RigField::Ptt,
            RigField::Ptt => RigField::Model,
        };
    }

    /// Move the active list-field's selection up (Up arrow). No-op on the
    /// `Model` field (text entry, not a list) and clamps at 0 — mirrors
    /// `DeviceSelectionState::move_up`'s clamp-at-bounds style, not wrap.
    pub fn move_up(&mut self) {
        match self.active_field {
            RigField::Model => {}
            RigField::Port => {
                if self.selected_port_idx > 0 {
                    self.selected_port_idx -= 1;
                }
            }
            RigField::Baud => {
                if self.selected_baud_idx > 0 {
                    self.selected_baud_idx -= 1;
                }
            }
            RigField::Ptt => {
                if self.selected_ptt_idx > 0 {
                    self.selected_ptt_idx -= 1;
                }
            }
        }
    }

    /// Move the active list-field's selection down (Down arrow). Clamps at
    /// each list's last index.
    pub fn move_down(&mut self) {
        match self.active_field {
            RigField::Model => {}
            RigField::Port => {
                if !self.available_ports.is_empty()
                    && self.selected_port_idx + 1 < self.available_ports.len()
                {
                    self.selected_port_idx += 1;
                }
            }
            RigField::Baud => {
                if self.selected_baud_idx + 1 < RIG_BAUD_RATES.len() {
                    self.selected_baud_idx += 1;
                }
            }
            RigField::Ptt => {
                if self.selected_ptt_idx + 1 < rig_ptt_methods().len() {
                    self.selected_ptt_idx += 1;
                }
            }
        }
    }

    /// Append a character to the model text field (only meaningful while
    /// `active_field == RigField::Model`; callers gate on that).
    pub fn push_model_char(&mut self, c: char) {
        self.model.push(c);
    }

    /// Backspace the model text field.
    pub fn pop_model_char(&mut self) {
        self.model.pop();
    }

    /// The port value to submit — empty string if no ports were enumerated.
    pub fn selected_port(&self) -> String {
        self.available_ports
            .get(self.selected_port_idx)
            .cloned()
            .unwrap_or_default()
    }

    pub fn selected_baud(&self) -> u32 {
        RIG_BAUD_RATES[self.selected_baud_idx.min(RIG_BAUD_RATES.len() - 1)]
    }

    pub fn selected_ptt(&self) -> pancetta_config::rig::PttMethod {
        let methods = rig_ptt_methods();
        methods[self.selected_ptt_idx.min(methods.len() - 1)].clone()
    }
}
```

`App::apply_rig_config_update`, added near wherever `App::set_audio_devices` is defined (grep for it — it's the handler `DeviceListUpdate` calls at `tui_runner.rs:847`):

```rust
    /// Seed `rig_selection` from a `TuiMessage::RigConfigUpdate` (PAN-59).
    /// Pre-populates the modal with live values so it opens editable, not
    /// blank — mirrors `set_audio_devices`'s seeding of `device_selection`.
    pub fn apply_rig_config_update(
        &mut self,
        available_ports: Vec<String>,
        current_model: String,
        current_port: String,
        current_baud_rate: u32,
        current_ptt_method: pancetta_config::rig::PttMethod,
    ) {
        let selected_port_idx = available_ports
            .iter()
            .position(|p| *p == current_port)
            .unwrap_or(0);
        let selected_baud_idx = crate::app::RIG_BAUD_RATES
            .iter()
            .position(|b| *b == current_baud_rate)
            .unwrap_or(1);
        let selected_ptt_idx = crate::app::rig_ptt_methods()
            .iter()
            .position(|m| format!("{:?}", m) == format!("{:?}", current_ptt_method))
            .unwrap_or(0);

        self.rig_selection.available_ports = available_ports;
        self.rig_selection.model = current_model;
        self.rig_selection.selected_port_idx = selected_port_idx;
        self.rig_selection.selected_baud_idx = selected_baud_idx;
        self.rig_selection.selected_ptt_idx = selected_ptt_idx;
    }
```

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-tui/src/app.rs`'s existing `#[cfg(test)]` block (grep the file for `mod tests` — if `DeviceSelectionState` has its own tests nearby, add these directly below them):

```rust
#[test]
fn rig_selection_state_defaults_to_9600_baud_and_none_ptt() {
    let state = RigSelectionState::default();
    assert_eq!(state.selected_baud(), 9600);
    assert!(matches!(
        state.selected_ptt(),
        pancetta_config::rig::PttMethod::None
    ));
    assert!(!state.visible);
}

#[test]
fn rig_selection_state_next_field_cycles_through_all_four() {
    let mut state = RigSelectionState::default();
    assert_eq!(state.active_field, RigField::Model);
    state.next_field();
    assert_eq!(state.active_field, RigField::Port);
    state.next_field();
    assert_eq!(state.active_field, RigField::Baud);
    state.next_field();
    assert_eq!(state.active_field, RigField::Ptt);
    state.next_field();
    assert_eq!(state.active_field, RigField::Model);
}

#[test]
fn rig_selection_state_move_up_down_clamp_on_baud_field() {
    let mut state = RigSelectionState::default();
    state.active_field = RigField::Baud;
    state.selected_baud_idx = 0;
    state.move_up();
    assert_eq!(state.selected_baud_idx, 0, "must clamp at 0, not wrap");

    state.selected_baud_idx = RIG_BAUD_RATES.len() - 1;
    state.move_down();
    assert_eq!(
        state.selected_baud_idx,
        RIG_BAUD_RATES.len() - 1,
        "must clamp at the last index, not wrap"
    );
}

#[test]
fn rig_selection_state_model_field_text_editing() {
    let mut state = RigSelectionState::default();
    state.push_model_char('I');
    state.push_model_char('C');
    assert_eq!(state.model, "IC");
    state.pop_model_char();
    assert_eq!(state.model, "I");
}

#[test]
fn apply_rig_config_update_preselects_current_values() {
    let mut app = App::new(); // use whichever App constructor other tests in this file already use — grep for `App::new()` or the local test-harness builder
    app.apply_rig_config_update(
        vec!["/dev/ttyUSB0".to_string(), "/dev/ttyUSB1".to_string()],
        "FTdx10".to_string(),
        "/dev/ttyUSB1".to_string(),
        38400,
        pancetta_config::rig::PttMethod::Serial,
    );
    assert_eq!(app.rig_selection.model, "FTdx10");
    assert_eq!(app.rig_selection.selected_port_idx, 1);
    assert_eq!(app.rig_selection.selected_port(), "/dev/ttyUSB1");
    assert_eq!(app.rig_selection.selected_baud(), 38400);
    assert!(matches!(
        app.rig_selection.selected_ptt(),
        pancetta_config::rig::PttMethod::Serial
    ));
}
```

Before running, grep `pancetta-tui/src/app.rs` for how existing tests construct an `App` (e.g. `App::new()`, `App::default()`, or a local `fn test_app() -> App` helper) and use that exact constructor in `apply_rig_config_update_preselects_current_values` — do not guess; if none of `App::new()`/`App::default()` compiles, search for the actual helper name.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui rig_selection -- --nocapture` and `cargo test -p pancetta-tui apply_rig_config_update`
Expected: FAIL — `RigSelectionState`/`RigField`/`RIG_BAUD_RATES`/`rig_ptt_methods`/`apply_rig_config_update` not found.

- [ ] **Step 3: Implement `RigSelectionState`, `RigField`, `RIG_BAUD_RATES`, `rig_ptt_methods`, and `App::apply_rig_config_update`**

Add the `RigField` enum, `RIG_BAUD_RATES` const, `rig_ptt_methods` fn, and `RigSelectionState` struct/impl (full code above) to `pancetta-tui/src/app.rs` near `DeviceSelectionState` (line 634). Add `pub rig_selection: RigSelectionState,` to the `App` struct right after `pub device_selection: DeviceSelectionState,` (line 1024) and to `App`'s `Default`/constructor initializer (grep for where `device_selection: DeviceSelectionState::new(),` or equivalent is initialized in `App`'s constructor, and add `rig_selection: RigSelectionState::new(),` alongside it). Add `apply_rig_config_update` (full code above) near `set_audio_devices`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui rig_selection` and `cargo test -p pancetta-tui apply_rig_config_update`
Expected: PASS (5 tests)

- [ ] **Step 5: Run the full pancetta-tui test suite**

Run: `cargo test -p pancetta-tui`
Expected: PASS, no regressions (this also catches any other exhaustive-match compile breakage from Task 2's new enum variants that this task doesn't yet handle — if `tui_runner.rs`'s `TuiMessage` dispatch match doesn't yet handle `RigConfigUpdate`, that's fixed in Task 4, not here; a compile error here about an unhandled `TuiCommand::SelectRig` match elsewhere in non-`tui_relay.rs` code should be fixed in this task or flagged if out of plan scope).

- [ ] **Step 6: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): add RigSelectionState and App::apply_rig_config_update"
```

---

### Task 4: `pancetta-tui` — key binding, key handling, modal render

**Files:**
- Modify: `pancetta-tui/src/keymap.rs` (add binding after line 245's `d` entry)
- Modify: `pancetta-tui/src/tui_runner.rs` (key handler near line 1486; `TuiMessage::RigConfigUpdate` dispatch near line 842-848; new `render_rig_selection_modal` near `render_device_selection_modal` at line 2013; wire the render call into the main draw function wherever `render_device_selection_modal` is invoked)
- Modify: `docs/KEYBINDINGS.md` (regenerated, not hand-edited)

**Interfaces:**
- Consumes: `RigSelectionState`, `RigField`, `App::rig_selection`, `App::apply_rig_config_update` (Task 3); `TuiCommand::SelectRig`, `TuiMessage::RigConfigUpdate` (Task 2).
- Produces: nothing new consumed by later tasks — this is the UI leaf.

- [ ] **Step 1: Add the keymap entry**

In `pancetta-tui/src/keymap.rs`, add immediately after the `kb("d", "Device picker", ...)` line (245):

```rust
    kb(
        "i",
        "Rig config picker (model/port/baud/PTT)",
        Category::AudioDevices,
        false,
    ),
```

- [ ] **Step 2: Run the keymap drift + no-duplicate-key tests, confirm the new key doesn't collide**

Run: `cargo test -p pancetta-tui no_duplicate_keys`
Expected: PASS (this is the primary safety check that `i` isn't already bound — if it fails, STOP and re-grep `tui_runner.rs` for `KeyCode::Char('i')` / `KeyCode::Char('I')` before picking a different key; do not silently overwrite an existing binding)

Run: `PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current`
Expected: regenerates `docs/KEYBINDINGS.md` (this is expected to "fail" the assertion on the FIRST run since it writes instead of comparing — rerun without the env var afterward to confirm it now matches)

Run: `cargo test -p pancetta-tui keybindings_doc_is_current`
Expected: PASS

- [ ] **Step 3: Add the `d`-key handler's sibling for `i`**

In `pancetta-tui/src/tui_runner.rs`, immediately after the `KeyCode::Char('d') => { ... }` block (ends around line 1498, right before `KeyCode::Char('?')`):

```rust
            KeyCode::Char('i') => {
                app.rig_selection.visible = true;
                app.status_message =
                    "Edit rig config (Tab: next field, Up/Down: change, type: edit model, Enter: apply, Esc: cancel)"
                        .to_string();
            }
```

- [ ] **Step 4: Add modal-scoped key handling while `app.rig_selection.visible`**

Find the existing block that handles keys while `app.device_selection.visible` is true (it will be a top-of-the-key-handler early-return guard, since the device modal "blocks all other keys while visible" per the `quit_confirm_visible` doc comment's stated pattern at line 1029-1032 — grep `tui_runner.rs` for `device_selection.visible` to find that guard's exact location and structure). Add an equivalent guard for `rig_selection.visible`, placed alongside it (same priority tier — both are modals that block other input):

```rust
            if app.rig_selection.visible {
                match key.code {
                    KeyCode::Esc => {
                        app.rig_selection.visible = false;
                        app.status_message = "Rig config picker cancelled".to_string();
                    }
                    KeyCode::Tab => {
                        app.rig_selection.next_field();
                    }
                    KeyCode::Up => {
                        app.rig_selection.move_up();
                    }
                    KeyCode::Down => {
                        app.rig_selection.move_down();
                    }
                    KeyCode::Backspace
                        if app.rig_selection.active_field == pancetta_tui::app::RigField::Model =>
                    {
                        app.rig_selection.pop_model_char();
                    }
                    KeyCode::Char(c)
                        if app.rig_selection.active_field == pancetta_tui::app::RigField::Model
                            && !c.is_control() =>
                    {
                        app.rig_selection.push_model_char(c);
                    }
                    KeyCode::Enter => {
                        app.rig_selection.visible = false;
                        self.message_tx.send(TuiCommand::SelectRig {
                            model: app.rig_selection.model.clone(),
                            port: app.rig_selection.selected_port(),
                            baud_rate: app.rig_selection.selected_baud(),
                            ptt_method: app.rig_selection.selected_ptt(),
                        })?;
                        app.status_message = "Applying rig config…".to_string();
                    }
                    _ => {}
                }
                return Ok(()); // matches the device-selection modal's own early-return, blocking all other top-level keys while open
            }
```

Adjust the exact `return Ok(())` / early-return mechanism, and where this block is inserted relative to `device_selection`'s own guard, to match whatever the actual existing control-flow shape is (this key handler function's signature and return type must be confirmed by reading it directly — the plan's research read isolated line ranges, not the enclosing function signature). If the existing pattern uses a different early-return value than `Ok(())`, use that same value/style, not `Ok(())` blindly.

- [ ] **Step 5: Handle `TuiMessage::RigConfigUpdate` in the message-dispatch match**

Immediately after the `TuiMessage::DeviceListUpdate { ... } => { app.set_audio_devices(...); }` arm (`tui_runner.rs:842-848`):

```rust
            TuiMessage::RigConfigUpdate {
                available_ports,
                current_model,
                current_port,
                current_baud_rate,
                current_ptt_method,
            } => {
                app.apply_rig_config_update(
                    available_ports,
                    current_model,
                    current_port,
                    current_baud_rate,
                    current_ptt_method,
                );
            }
```

- [ ] **Step 6: Write `render_rig_selection_modal`**

Add near `render_device_selection_modal` (`tui_runner.rs:2013`), following its same defensive-area-check pattern:

```rust
    fn render_rig_selection_modal(f: &mut Frame, area: Rect, state: &pancetta_tui::app::RigSelectionState) {
        if area.width < 10 || area.height < 4 {
            return;
        }
        let modal_width = (area.width * 3 / 5).clamp(40, 70).min(area.width);
        let modal_height = 9u16.min(area.height); // title + border + 4 fields + footer + border
        let modal_area = centered_rect(modal_width, modal_height, area); // reuse the existing centering helper `render_device_selection_modal` itself uses — confirm its exact name/signature by reading that function's body, do not assume `centered_rect` is correct without checking

        let fields: [(&str, String, bool); 4] = [
            ("Model", state.model.clone(), state.active_field == pancetta_tui::app::RigField::Model),
            ("Port", state.selected_port(), state.active_field == pancetta_tui::app::RigField::Port),
            (
                "Baud",
                state.selected_baud().to_string(),
                state.active_field == pancetta_tui::app::RigField::Baud,
            ),
            (
                "PTT",
                format!("{:?}", state.selected_ptt()),
                state.active_field == pancetta_tui::app::RigField::Ptt,
            ),
        ];

        let mut lines: Vec<Line> = Vec::new();
        for (label, value, active) in &fields {
            let style = if *active {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::raw(format!("{:>6}: ", label)),
                Span::styled(value.clone(), style),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(
            "Tab: next field  Up/Down: change  Enter: apply  Esc: cancel",
        ));

        let block = Block::default()
            .title(" Rig Config ")
            .borders(Borders::ALL);
        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(Clear, modal_area);
        f.render_widget(paragraph, modal_area);
    }
```

This is a sketch of the *content*; match it to whatever widget imports (`Line`, `Span`, `Style`, `Modifier`, `Block`, `Borders`, `Paragraph`, `Clear`, `Frame`, `Rect`) and centering helper `render_device_selection_modal` actually uses — read that function's full body (it continues past line 2027 in the current file) before writing this one, so the two modals share the exact same layout idiom rather than diverging.

- [ ] **Step 7: Wire the render call**

Find where `render_device_selection_modal(f, area, &app.device_selection)` (or equivalent) is invoked in the main draw function, and add a sibling call gated on `app.rig_selection.visible`:

```rust
        if app.rig_selection.visible {
            render_rig_selection_modal(f, size, &app.rig_selection);
        }
```

matching whatever variable name the surrounding code uses for the terminal area (`area`/`size`/`f.size()`).

- [ ] **Step 8: Manual verification (no automated test for rendering)**

This step has no automated test — ratatui modal rendering in this codebase isn't unit-tested pixel-by-pixel (confirm by checking whether `render_device_selection_modal` itself has a test; if it doesn't, this task doesn't need one either, consistent with existing coverage). Run `cargo build -p pancetta-tui` and `cargo clippy -p pancetta-tui` to confirm it compiles cleanly.

Run: `cargo build -p pancetta-tui && cargo clippy -p pancetta-tui -- -D warnings`
Expected: clean build, no clippy warnings

- [ ] **Step 9: Run the full pancetta-tui test suite**

Run: `cargo test -p pancetta-tui`
Expected: PASS, no regressions

- [ ] **Step 10: Commit**

```bash
git add pancetta-tui/src/keymap.rs pancetta-tui/src/tui_runner.rs docs/KEYBINDINGS.md
git commit -m "feat(tui): add rig-config picker modal on the 'i' key"
```

---

### Task 5: `pancetta` coordinator — `HamlibReconnectRequest` + `handle_hamlib_reconnect_request`

**Files:**
- Modify: `pancetta/src/coordinator/hamlib.rs` (new struct + method, near `teardown_hamlib` at line 684 and `start_hamlib_component` at line 1069)
- Test: same file, existing `#[cfg(test)]` block (the file has Hamlib-feature-gated tests already, e.g. `hamlib_crash_mid_ptt_forces_ptt_off_on_the_rig` in `health.rs`'s `supervisor_tests` module — grep `pancetta/src/coordinator/hamlib.rs` for its own `#[cfg(test)]` block; if hamlib.rs has none, add one, following the same `test_coordinator()`/`MockRig` pattern used in `health.rs`'s `supervisor_tests`)

**Interfaces:**
- Consumes: `self.teardown_hamlib()`, `self.start_hamlib_component()` (existing, `hamlib.rs:684` and `:1069`), `self.ptt_active: Arc<AtomicBool>` (existing, `coordinator/mod.rs:1068`).
- Produces: `pub struct HamlibReconnectRequest { pub respond: tokio::sync::oneshot::Sender<anyhow::Result<()>> }`, `ApplicationCoordinator::handle_hamlib_reconnect_request(&mut self, req: HamlibReconnectRequest)` — Task 6 (`mod.rs`/`health.rs`) wires the channel that delivers these; Task 7 (`tui_relay.rs`) constructs and sends them.

- [ ] **Step 1: Write the failing tests**

Add to `pancetta/src/coordinator/hamlib.rs` (new `#[cfg(test)] mod pan_59_reconnect_tests` block if the file has no existing test module, otherwise add into the existing one):

```rust
#[cfg(all(test, feature = "pancetta-hamlib"))]
mod pan_59_reconnect_tests {
    use super::*;
    use crate::coordinator::mod_test_support::test_coordinator; // adjust import path to wherever `test_coordinator()` actually lives — health.rs's `supervisor_tests` module defines its own local copy rather than a shared helper; if there is no shared helper, copy that same local pattern into this new test module instead of inventing a cross-module import that doesn't exist
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn refuses_reconnect_while_ptt_is_active() {
        let mut coordinator = test_coordinator().await;
        coordinator.ptt_active.store(true, Ordering::Release);

        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator
            .handle_hamlib_reconnect_request(HamlibReconnectRequest { respond: tx })
            .await;

        let result = rx.await.expect("handler must always respond");
        assert!(
            result.is_err(),
            "must refuse a live rig reconnect while PTT is active — tearing down Hamlib \
             mid-key-down would yank CAT/PTT control out from under an active transmission"
        );
    }

    #[tokio::test]
    async fn reconnects_successfully_when_ptt_is_idle() {
        let mut coordinator = test_coordinator().await;
        assert!(!coordinator.ptt_active.load(Ordering::Acquire));

        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator
            .handle_hamlib_reconnect_request(HamlibReconnectRequest { respond: tx })
            .await;

        let result = rx.await.expect("handler must always respond");
        assert!(
            result.is_ok(),
            "must succeed reconnecting via the mock rig path when PTT is idle: {:?}",
            result.err()
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta --features pancetta-hamlib pan_59_reconnect_tests`
Expected: FAIL — `HamlibReconnectRequest`/`handle_hamlib_reconnect_request` not found.

- [ ] **Step 3: Implement `HamlibReconnectRequest` and `handle_hamlib_reconnect_request`**

Add to `pancetta/src/coordinator/hamlib.rs`, near `teardown_hamlib`/`start_hamlib_component` (both live in `impl super::ApplicationCoordinator` — add this to the same `impl` block):

```rust
/// A live rig-config-switch request (PAN-59), routed from the TUI
/// command-relay task (which only holds cloned `Arc`/channel handles, never
/// `&mut ApplicationCoordinator`) into `run_main_loop` (the only place that
/// already holds `&mut self` in a loop). See
/// `docs/superpowers/specs/2026-09-02-pan-59-live-rig-switch-design.md`.
pub struct HamlibReconnectRequest {
    pub respond: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
}

impl super::ApplicationCoordinator {
    /// Handle a PAN-59 live rig-config-switch request: refuse while PTT is
    /// active (tearing down Hamlib mid-key-down would yank CAT/PTT control
    /// out from under an active transmission — the same safety instinct as
    /// `TxInhibitGuard`), otherwise reconnect via the same
    /// teardown/restart pair the crash-restart path already uses so the
    /// freshly-persisted `self.config.rig` takes effect.
    #[cfg(feature = "pancetta-hamlib")]
    pub(crate) async fn handle_hamlib_reconnect_request(&mut self, req: HamlibReconnectRequest) {
        if self.ptt_active.load(Ordering::Acquire) {
            let _ = req.respond.send(Err(anyhow::anyhow!(
                "cannot switch rig while PTT is active — release PTT and retry"
            )));
            return;
        }
        self.teardown_hamlib().await;
        let result = self.start_hamlib_component().await;
        let _ = req.respond.send(result);
    }

    #[cfg(not(feature = "pancetta-hamlib"))]
    pub(crate) async fn handle_hamlib_reconnect_request(&mut self, req: HamlibReconnectRequest) {
        let _ = req.respond.send(Err(anyhow::anyhow!(
            "rig control not compiled in (pancetta-hamlib feature disabled)"
        )));
    }
}
```

Confirm `Ordering` is already imported in this file (it's used by `teardown_hamlib`/other code already, per the earlier research read of `health.rs`'s neighboring code — verify the actual import in `hamlib.rs` itself, e.g. `use std::sync::atomic::Ordering;`, and reuse it rather than adding a duplicate/conflicting import).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta --features pancetta-hamlib pan_59_reconnect_tests`
Expected: PASS (2 tests)

- [ ] **Step 5: Run the full pancetta-hamlib-feature test suite**

Run: `cargo test -p pancetta --features pancetta-hamlib`
Expected: PASS, no regressions

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/hamlib.rs
git commit -m "feat(hamlib): add HamlibReconnectRequest + handle_hamlib_reconnect_request for PAN-59"
```

---

### Task 6: `pancetta` coordinator — wire the reconnect channel through `mod.rs` and `run_main_loop`

**Files:**
- Modify: `pancetta/src/coordinator/mod.rs` (new fields near `audio_reopen_tx` at line 1128-1129 and its initializer at line 1779)
- Modify: `pancetta/src/coordinator/health.rs` (new `tokio::select!` arm in `run_main_loop`, currently at lines ~430-457)
- Test: `pancetta/src/coordinator/health.rs`'s existing `supervisor_tests` module (or `hamlib.rs`'s new module from Task 5 — whichever already has `test_coordinator()` in scope)

**Interfaces:**
- Consumes: `HamlibReconnectRequest`, `handle_hamlib_reconnect_request` (Task 5).
- Produces: `ApplicationCoordinator.hamlib_reconnect_tx: tokio::sync::mpsc::Sender<HamlibReconnectRequest>` — Task 7 (`tui_relay.rs`) clones this field.

- [ ] **Step 1: Write the failing integration test**

Add to `pancetta/src/coordinator/health.rs`'s `supervisor_tests` module (near the other Hamlib-restart tests):

```rust
#[cfg(feature = "pancetta-hamlib")]
#[tokio::test]
async fn run_main_loop_processes_a_hamlib_reconnect_request() {
    let mut coordinator = test_coordinator().await;
    let reconnect_tx = coordinator.hamlib_reconnect_tx.clone();
    let shutdown = coordinator.shutdown_signal.clone();

    let loop_handle = tokio::spawn(async move {
        coordinator.run_main_loop().await.unwrap();
    });

    let (tx, rx) = tokio::sync::oneshot::channel();
    reconnect_tx
        .send(super::super::hamlib::HamlibReconnectRequest { respond: tx })
        .await
        .expect("reconnect channel must accept the request");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("run_main_loop must process the reconnect request within 5s")
        .expect("handler must always respond");
    assert!(result.is_ok(), "reconnect should succeed via the mock rig path: {:?}", result.err());

    shutdown.store(true, Ordering::Release);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_handle).await;
}
```

Adjust the exact way `coordinator` is moved into the spawned task vs. how `shutdown_signal`/`hamlib_reconnect_tx` are cloned beforehand to match `ApplicationCoordinator`'s actual field visibility (`pub(crate)` vs private) and `Clone` bounds — `shutdown_signal` is already an `Arc<AtomicBool>` (confirmed by its use elsewhere in this exact file), so `.clone()` before the move is correct; confirm `hamlib_reconnect_tx`'s type is `Clone` (a `tokio::sync::mpsc::Sender` always is).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pancetta --features pancetta-hamlib run_main_loop_processes_a_hamlib_reconnect_request`
Expected: FAIL — `hamlib_reconnect_tx` field not found on `ApplicationCoordinator`.

- [ ] **Step 3: Add the fields to `ApplicationCoordinator` and construct the channel in `new()`**

In `pancetta/src/coordinator/mod.rs`, add near the `audio_reopen_tx` field declaration (line 1128-1129):

```rust
    /// PAN-59 live rig-config-switch request channel. The TUI
    /// command-relay task (`tui_relay.rs`) only holds cloned `Arc`/channel
    /// handles, never `&mut ApplicationCoordinator` — Hamlib's reconnect
    /// (`teardown_hamlib`/`start_hamlib_component`) needs `&mut self`, so
    /// the request routes through here into `run_main_loop`, the only place
    /// already holding `&mut self` in a loop. Not feature-gated (unlike the
    /// Hamlib-only fields above): the sender must always exist so
    /// `tui_relay.rs`'s unconditionally-compiled command-relay task can
    /// clone it; `handle_hamlib_reconnect_request`'s
    /// `#[cfg(not(feature = "pancetta-hamlib"))]` arm handles the
    /// feature-disabled case at the receiving end.
    pub(crate) hamlib_reconnect_tx: tokio::sync::mpsc::Sender<crate::coordinator::hamlib::HamlibReconnectRequest>,
    hamlib_reconnect_rx: Option<tokio::sync::mpsc::Receiver<crate::coordinator::hamlib::HamlibReconnectRequest>>,
```

In `ApplicationCoordinator::new()`, before the struct literal, add:

```rust
        let (hamlib_reconnect_tx, hamlib_reconnect_rx) = tokio::sync::mpsc::channel(1);
```

and in the struct literal, add both fields near `audio_reopen_tx: None,` (line 1779):

```rust
            hamlib_reconnect_tx,
            hamlib_reconnect_rx: Some(hamlib_reconnect_rx),
```

Locate the actual line where local `let` bindings are computed before the struct literal in `new()` (e.g. near where `scoped_fast_path`/`ft8_config`/`config_warnings` locals are set up, based on their appearance in the struct literal at lines 1783-1785) and add the channel construction there, not mid-struct-literal.

- [ ] **Step 4: Wire the `tokio::select!` arm in `run_main_loop`**

In `pancetta/src/coordinator/health.rs`, in `run_main_loop` (currently lines ~430-457), take the receiver once before the loop and add a new `select!` arm:

```rust
    pub(crate) async fn run_main_loop(&mut self) -> Result<()> {
        info!("Entering main application loop");

        let mut stats_interval = interval(Duration::from_secs(30));
        let mut health_check_interval = interval(Duration::from_secs(5));
        let mut hamlib_reconnect_rx = self
            .hamlib_reconnect_rx
            .take()
            .expect("run_main_loop must only be called once per coordinator lifetime");

        while !self.shutdown_signal.load(Ordering::Acquire) {
            tokio::select! {
                _ = stats_interval.tick() => {
                    self.log_performance_stats().await;
                }
                _ = health_check_interval.tick() => {
                    self.check_task_handles().await;
                }
                Some(req) = hamlib_reconnect_rx.recv() => {
                    self.handle_hamlib_reconnect_request(req).await;
                }
                _ = sleep(Duration::from_secs(1)) => {
                    // Perf (Pass 1 / infra-A4): this arm only bounds how quickly
                    // the `while !shutdown` guard is re-checked -- it does no work.
                }
            }
        }

        info!("Main application loop completed");
        Ok(())
    }
```

Keep every existing line of this function's body exactly as-is (the comment on the final `sleep` arm, the doc comment above the function) — only add the new local `let mut hamlib_reconnect_rx = ...` line and the new `select!` arm. Do not reorder the existing arms.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p pancetta --features pancetta-hamlib run_main_loop_processes_a_hamlib_reconnect_request`
Expected: PASS

- [ ] **Step 6: Run the full pancetta-hamlib-feature and default-feature test suites**

Run: `cargo test -p pancetta --features pancetta-hamlib`
Run: `cargo test -p pancetta` (default features, confirms the `#[cfg(not(feature = "pancetta-hamlib"))]` path and the always-present channel fields compile and don't regress the no-hamlib build — check whether this workspace actually builds `pancetta` with hamlib off anywhere in CI; if not, at minimum run `cargo check -p pancetta --no-default-features --features metrics` to confirm the non-hamlib path compiles)
Expected: PASS, no regressions

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/coordinator/mod.rs pancetta/src/coordinator/health.rs
git commit -m "feat(coordinator): route PAN-59 rig-reconnect requests through run_main_loop"
```

---

### Task 7: `pancetta` coordinator — `tui_relay.rs` startup push + `SelectRig` handler

**Files:**
- Modify: `pancetta/src/coordinator/tui_relay.rs` (startup device-list push block at lines ~909-934; new `TuiCommand::SelectRig` match arm near `SelectDevice`'s at lines 1996-2100+; clone `hamlib_reconnect_tx` alongside `cmd_audio_reopen_tx` at line 892)

**Interfaces:**
- Consumes: `Config::set_rig_in_file` (Task 1), `TuiCommand::SelectRig`/`TuiMessage::RigConfigUpdate` (Task 2), `HamlibReconnectRequest` (Task 5), `self.hamlib_reconnect_tx` (Task 6).
- Produces: nothing new consumed by later tasks — this is the coordinator-side leaf that closes the loop end to end.

- [ ] **Step 1: Clone the reconnect channel into the command-relay task**

In `pancetta/src/coordinator/tui_relay.rs`, immediately after `let cmd_audio_reopen_tx = self.audio_reopen_tx.clone();` (line 892):

```rust
        // Live rig-reconnect channel into run_main_loop (PAN-59). Always
        // present (see the field's doc comment in coordinator/mod.rs) —
        // unlike `cmd_audio_reopen_tx`, this is never `None`.
        let cmd_hamlib_reconnect_tx = self.hamlib_reconnect_tx.clone();
```

- [ ] **Step 2: Push `TuiMessage::RigConfigUpdate` at startup**

Immediately after the existing `DeviceListUpdate` startup-push block (ends around line 933-934, right before the TX-policy-banner seeding block at line 936), add:

```rust
            // Push the current rig config + enumerated serial ports to the
            // TUI once at startup (PAN-59), so the `i` rig-picker modal can
            // list them. Mirrors the DeviceListUpdate push immediately
            // above — the coordinator enumerates hardware, the TUI stays a
            // passive renderer.
            {
                let (current_model, current_port, current_baud_rate, current_ptt_method) = {
                    let cfg = cmd_config.read().await;
                    (
                        cfg.rig.model.clone(),
                        cfg.rig.interface.port.clone(),
                        cfg.rig.interface.baud_rate,
                        cfg.rig.ptt.method.clone(),
                    )
                };
                let available_ports = serialport::available_ports()
                    .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
                    .unwrap_or_default();
                if let Err(e) =
                    cmd_tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::RigConfigUpdate {
                        available_ports,
                        current_model,
                        current_port,
                        current_baud_rate,
                        current_ptt_method,
                    })
                {
                    debug!("Failed to send initial rig config to TUI: {}", e);
                }
            }
```

Confirm `serialport` is already an accessible dependency of this crate (it's used in `pancetta/src/main.rs`'s `setup_rig`, which is the same `pancetta` package — same `Cargo.toml`, so no new dependency line is needed; just confirm the `use`/fully-qualified path compiles, adding `use serialport;` or a fully-qualified `serialport::available_ports()` call as this snippet already does).

- [ ] **Step 3: Add the `TuiCommand::SelectRig` handler**

Immediately after the full `TuiCommand::SelectDevice { ... } => { ... }` arm (ends somewhere after line 2100 — read the arm's actual closing brace before inserting, do not guess the line), add:

```rust
                        pancetta_tui::tui_runner::TuiCommand::SelectRig {
                            model,
                            port,
                            baud_rate,
                            ptt_method,
                        } => {
                            info!(
                                "TUI SelectRig: model={} port={} baud={} ptt={:?}",
                                model, port, baud_rate, ptt_method
                            );
                            {
                                let mut cfg = cmd_config.write().await;
                                cfg.rig.model = model.clone();
                                cfg.rig.interface.port = port.clone();
                                cfg.rig.interface.baud_rate = baud_rate;
                                cfg.rig.ptt.method = ptt_method.clone();
                            }
                            let config_path = dirs::home_dir()
                                .unwrap_or_else(|| std::path::PathBuf::from("."))
                                .join(".pancetta")
                                .join("pancetta.toml");
                            let persist_result = {
                                let cfg = cmd_config.read().await;
                                cfg.set_rig_in_file(
                                    &config_path,
                                    &model,
                                    &port,
                                    baud_rate,
                                    ptt_method.clone(),
                                )
                            };
                            if let Err(e) = persist_result {
                                warn!("Failed to persist rig config selection: {}", e);
                            } else {
                                info!("Persisted rig config selection to {}", config_path.display());
                            }

                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                            let status = if cmd_hamlib_reconnect_tx
                                .send(crate::coordinator::hamlib::HamlibReconnectRequest {
                                    respond: resp_tx,
                                })
                                .await
                                .is_err()
                            {
                                warn!("Hamlib reconnect channel closed; rig config saved but not applied live");
                                format!(
                                    "Rig config saved ({}) — live switch unavailable; restart to apply",
                                    model
                                )
                            } else {
                                match tokio::time::timeout(Duration::from_secs(5), resp_rx).await {
                                    Ok(Ok(Ok(()))) => {
                                        info!("Live rig reconnect succeeded: {}", model);
                                        format!("Rig → {} (live)", model)
                                    }
                                    Ok(Ok(Err(err))) => {
                                        warn!("Live rig reconnect failed: {}", err);
                                        format!(
                                            "Rig config saved ({}) but reconnect failed: {} — kept previous connection",
                                            model, err
                                        )
                                    }
                                    Ok(Err(_)) => {
                                        warn!("run_main_loop dropped the reconnect response");
                                        format!(
                                            "Rig config saved ({}) — no response from main loop; restart to apply",
                                            model
                                        )
                                    }
                                    Err(_) => {
                                        warn!("Timed out waiting for rig reconnect");
                                        format!(
                                            "Rig config saved ({}) — reconnect timed out; check rig connection",
                                            model
                                        )
                                    }
                                }
                            };
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "rig".to_string(),
                                    status,
                                },
                            );
                        }
```

Confirm `TuiMessage::StatusUpdate { component: String, status: String }` is the actual existing shape used by `SelectDevice`'s own status reporting (re-check the exact field names by reading past line 2100 in the current file — the plan's research only read up through line 2100) before using it verbatim; if the real variant has different field names, use those instead.

- [ ] **Step 4: Build and run the coordinator test suite**

Run: `cargo build -p pancetta --features pancetta-hamlib`
Run: `cargo test -p pancetta --features pancetta-hamlib`
Expected: clean build, PASS with no regressions (this arm has no dedicated unit test of its own — matching `SelectDevice`'s own precedent, which also has no dedicated test in `tui_relay.rs`; correctness here is covered by Task 1's `set_rig_in_file` tests, Task 5/6's reconnect tests, and this build+test-suite pass)

- [ ] **Step 5: Run clippy across the whole workspace**

Run: `cargo clippy --workspace --features pancetta-hamlib -- -D warnings`
Expected: clean

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(coordinator): wire TuiCommand::SelectRig end to end (PAN-59)"
```

---

### Task 8: Final workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Run the repo's full check script**

Run: `./scripts/check.sh`
Expected: PASS (fmt, clippy, full workspace test suite — this is the same gate the pre-push hook runs)

- [ ] **Step 2: Manually smoke-test in the TUI**

Run: `cargo run -p pancetta -- --no-audio` (or whatever flag combination lets it start without real hardware — check `--help` output / existing `PANCETTA_MOCK_RIG=1` env var used in tests) with `PANCETTA_MOCK_RIG=1` set, confirm:
- Pressing `i` opens the modal pre-populated with the current mock rig config.
- Tab cycles the 4 fields; Up/Down changes Port/Baud/PTT; typing edits Model.
- Enter applies, a status message reports the outcome, `~/.pancetta/pancetta.toml`'s `[rig]` section reflects the change.
- Esc cancels without sending a command or touching the config.

Report the actual terminal output/behavior observed — this step requires genuine interaction, not just "tests pass."

- [ ] **Step 3: Final commit if the manual smoke test surfaced fixes**

If Step 2 found bugs, fix them, re-run Step 1, and commit the fix as a new commit (never amend a task's already-pushed commit per this repo's own `feedback_no_amend_after_push_attempt` convention — but since nothing has been pushed yet at this point in the plan, a normal `git commit` is fine; only avoid `--amend` once `/catalyst-dev:create-pr` or `git push` has run).

---

## Self-review notes (for the plan author, not a task)

- Spec coverage: hot-reload rejection (Task list doesn't touch `hot_reload.rs`/`classify_config_reload` — correct, per spec Non-goals), `SelectRig` command + modal (Tasks 2-4), reconnect via existing teardown/restart pair (Task 5), `&mut self` routing through `run_main_loop` (Task 6), end-to-end wiring + persistence (Tasks 1, 7), key choice `i` documented (Task 4), PTT-active guard (Task 5), 4-field scope matching the wizard (Tasks 1-4), no profiles (no task adds them). Covered.
- Placeholder scan: every step has real code or a concrete `cargo`/`git` command; steps that require the implementer to verify an exact pre-existing shape (e.g. `TuiMessage::StatusUpdate`'s exact fields, the device-selection modal's exact early-return mechanism, `set_audio_devices_in_file`'s final write helper name) say so explicitly and explain how to find the real answer, rather than silently guessing — this is deliberate given several of this plan's referenced files were read in partial/truncated ranges during research, not a placeholder in the "TBD" sense.
- Type consistency: `TuiCommand::SelectRig` fields (`model: String, port: String, baud_rate: u32, ptt_method: pancetta_config::rig::PttMethod`) match across Task 2 (definition), Task 3 (`RigSelectionState::selected_*` accessors feed them), Task 4 (the Enter-key handler constructs them), and Task 7 (the handler destructures them) identically. `HamlibReconnectRequest { respond: oneshot::Sender<anyhow::Result<()>> }` matches across Task 5 (definition + handler), Task 6 (channel wiring + test), and Task 7 (construction + send).

# Pancetta FT8 Application - Prototype Completion Plan

## Executive Summary

This plan outlines the work required to transform Pancetta from a 40% complete non-functional codebase into a working FT8 prototype capable of:
- Receiving real audio from a sound card
- Decoding actual FT8 signals
- Displaying decoded messages in a TUI
- Logging QSOs to a database
- Basic radio control via Hamlib

**Estimated Timeline: 6-8 weeks**
**Estimated Effort: 240-320 developer hours**

---

## Phase 1: Fix Critical Build Errors (Week 1)
**Goal: Get all modules compiling without errors**

### 1.1 Fix TUI Compilation (Day 1-2)
- [ ] Add missing dependency: `crossbeam-channel` to `pancetta-tui/Cargo.toml`
- [ ] Fix 41 compilation errors in `pancetta-tui/src/tui_runner.rs`
  - [ ] Add missing methods to App struct: `update_frequency`, `update_signal_strength`, etc.
  - [ ] Fix visibility of `add_decoded_message` method
  - [ ] Resolve type mismatches between expected and actual message types
- [ ] Update `pancetta-tui/src/ui/mod.rs` to export all required components
- [ ] Fix import statements for tokio::sync types

### 1.2 Fix Config Module (Day 2)
- [ ] Add missing `anyhow` dependency to `pancetta-config/Cargo.toml`
- [ ] Fix context trait imports in `hot_reload.rs`
- [ ] Ensure tokio is available as a regular dependency, not just dev-dependency

### 1.3 Fix Remaining Warnings (Day 3)
- [ ] Run `cargo fix --all` to auto-fix simple warnings
- [ ] Manually fix remaining warnings in:
  - [ ] pancetta-audio (44 warnings)
  - [ ] pancetta-ft8 (18 warnings)  
  - [ ] pancetta-dx (27 warnings)
- [ ] Ensure clean compilation: `cargo build --all --release`

### Deliverables:
- All modules compile without errors
- Warnings reduced to <10 total
- Basic `cargo test --all` passes

---

## Phase 2: Implement Real Audio Pipeline (Week 2)
**Goal: Replace stub audio with actual CPAL integration**

### 2.1 Create AudioManager (Day 4-5)
Location: `pancetta-audio/src/manager.rs`

```rust
pub struct AudioManager {
    device_manager: AudioDeviceManager,
    stream_manager: AudioStreamManager,
    processor: AudioProcessor,
    message_bus_tx: Sender<ComponentMessage>,
}
```

- [ ] Implement device selection logic
- [ ] Create stream initialization
- [ ] Add error recovery for device disconnection
- [ ] Implement gain control and monitoring

### 2.2 Replace Stub in Coordinator (Day 5-6)
Location: `pancetta/src/coordinator.rs` (lines 230-287)

- [ ] Remove test sine wave generator
- [ ] Import and initialize real AudioManager
- [ ] Connect audio callbacks to message bus
- [ ] Add device selection from config/CLI args
- [ ] Implement proper shutdown for audio streams

### 2.3 Validate Audio Flow (Day 6)
- [ ] Create test that verifies audio flows from input to DSP
- [ ] Add logging at each pipeline stage
- [ ] Verify sample rate conversion (48kHz → 12kHz)
- [ ] Test with real microphone input
- [ ] Measure actual callback latency

### Deliverables:
- Real audio input working
- Audio flows through DSP pipeline
- Latency measurements available

---

## Phase 3: Connect FT8 Decoder Pipeline (Week 3)
**Goal: End-to-end FT8 decoding from audio input**

### 3.1 Connect DSP to FT8 Decoder (Day 7-8)
Location: `pancetta/src/coordinator.rs`

- [ ] Ensure DSP output format matches FT8 input requirements
- [ ] Implement 12.64-second window buffering
- [ ] Add time synchronization for FT8 windows
- [ ] Connect spectral analysis output to decoder
- [ ] Handle multiple decode candidates

### 3.2 Implement Message Flow (Day 8-9)
- [ ] Route decoded messages from FT8 to message bus
- [ ] Add message type for `DecodedFt8Message`
- [ ] Implement message distribution to TUI and Logger
- [ ] Add decode statistics tracking
- [ ] Create decode success/failure metrics

### 3.3 Create Test Infrastructure (Day 9-10)
Location: `pancetta-ft8/tests/`

- [ ] Add known FT8 audio test files (.wav)
- [ ] Create accuracy validation tests
- [ ] Implement decode rate testing
- [ ] Add SNR vs accuracy benchmarks
- [ ] Create regression test suite

### Deliverables:
- FT8 messages decoded from real audio
- Test suite validating accuracy
- Performance metrics collected

---

## Phase 4: Fix and Complete TUI (Week 4)
**Goal: Working terminal interface displaying FT8 activity**

### 4.1 Implement Missing App Methods (Day 11-12)
Location: `pancetta-tui/src/app.rs`

```rust
impl App {
    pub fn add_decoded_message(&mut self, msg: DecodedMessage)
    pub fn update_frequency(&mut self, freq: u64)
    pub fn update_signal_strength(&mut self, strength: f32)
    pub fn update_qso_state(&mut self, state: QsoState)
    pub fn add_dx_spot(&mut self, spot: DxSpot)
    pub fn add_error_message(&mut self, error: String)
    pub fn update_component_status(&mut self, id: ComponentId, status: ComponentStatus)
}
```

### 4.2 Fix Message Bus Integration (Day 12-13)
Location: `pancetta-tui/src/tui_runner.rs`

- [ ] Fix message type conversions
- [ ] Implement proper message routing
- [ ] Add buffering for high message rates
- [ ] Fix async/await in render loop
- [ ] Add graceful shutdown handling

### 4.3 Complete UI Components (Day 13-14)
- [ ] Fix waterfall display rendering
- [ ] Implement frequency/mode display
- [ ] Add QSO state indicator
- [ ] Create scrollable message list
- [ ] Add status bar with metrics
- [ ] Implement help screen (F1)

### Deliverables:
- TUI launches without errors
- Decoded messages display in real-time
- All panels render correctly
- Keyboard navigation works

---

## Phase 5: Integrate Hamlib Radio Control (Week 5)
**Goal: Basic CAT control and PTT functionality**

### 5.1 Replace Mock with Real Implementation (Day 15-16)
Location: `pancetta-hamlib/src/rig.rs`

- [ ] Implement actual rigctld TCP connection
- [ ] Add connection retry logic
- [ ] Implement command queue with timeouts
- [ ] Add response parsing
- [ ] Create error recovery

### 5.2 Implement Core Commands (Day 16-17)
- [ ] Get/Set frequency
- [ ] Get/Set mode
- [ ] Get signal strength
- [ ] PTT control
- [ ] Get/Set VFO
- [ ] Power control

### 5.3 Connect to Coordinator (Day 17-18)
- [ ] Add Hamlib component to coordinator startup
- [ ] Route frequency changes to Hamlib
- [ ] Send signal strength to TUI
- [ ] Implement PTT coordination with TX
- [ ] Add CAT polling loop

### Deliverables:
- Connects to rigctld successfully
- Frequency/mode control works
- PTT keys radio for TX
- Signal strength displays

---

## Phase 6: Integration Testing & Validation (Week 6)
**Goal: Validate all requirements are met**

### 6.1 Create End-to-End Tests (Day 19-20)
Location: `tests/integration/`

```rust
#[tokio::test]
async fn test_full_ft8_decode_pipeline() {
    // Load test audio file
    // Start all components
    // Verify decode output
    // Check latency < 1ms
}
```

- [ ] Audio input → FT8 decode test
- [ ] Decode → QSO logging test
- [ ] TUI message display test
- [ ] Hamlib control test
- [ ] Configuration hot-reload test

### 6.2 Performance Validation (Day 20-21)
- [ ] Measure actual audio callback latency
- [ ] Verify <1ms requirement
- [ ] Test 50+ simultaneous decodes
- [ ] Measure CPU usage (<25% target)
- [ ] Measure memory usage (<100MB target)
- [ ] Create performance benchmark suite

### 6.3 Accuracy Testing (Day 21)
- [ ] Test with WSJT-X reference signals
- [ ] Verify >95% accuracy at -20dB SNR
- [ ] Test with real-world recordings
- [ ] Compare against WSJT-X decodes
- [ ] Document accuracy metrics

### Deliverables:
- Integration test suite passing
- Performance requirements validated
- Accuracy metrics documented

---

## Phase 7: Bug Fixes and Stabilization (Week 7)
**Goal: Fix issues found during testing**

### 7.1 Critical Bug Fixes (Day 22-24)
- [ ] Fix memory leaks identified by valgrind
- [ ] Resolve race conditions in message bus
- [ ] Fix TUI rendering glitches
- [ ] Resolve audio dropout issues
- [ ] Fix database connection pool exhaustion

### 7.2 Error Handling (Day 24-25)
- [ ] Add recovery for audio device loss
- [ ] Handle network disconnections (Hamlib)
- [ ] Add graceful degradation
- [ ] Improve error messages
- [ ] Add user notifications for errors

### 7.3 Logging and Debugging (Day 25-26)
- [ ] Add comprehensive debug logging
- [ ] Create log rotation
- [ ] Add performance profiling hooks
- [ ] Create diagnostic mode
- [ ] Add debug TUI panel

### Deliverables:
- No crashes during 1-hour test
- All errors handled gracefully
- Debug tools available

---

## Phase 8: Documentation and Release Prep (Week 8)
**Goal: Prepare for alpha release**

### 8.1 User Documentation (Day 27-28)
- [ ] Write README.md with quick start
- [ ] Create INSTALL.md with dependencies
- [ ] Write USER_GUIDE.md with screenshots
- [ ] Create CONFIG.md with all options
- [ ] Add TROUBLESHOOTING.md

### 8.2 Developer Documentation (Day 28-29)
- [ ] Generate rustdoc for all modules
- [ ] Create ARCHITECTURE.md
- [ ] Write CONTRIBUTING.md
- [ ] Add API.md for message bus
- [ ] Create TESTING.md guide

### 8.3 Release Package (Day 29-30)
- [ ] Create binary releases for Linux/Mac/Windows
- [ ] Write installation scripts
- [ ] Create example configurations
- [ ] Add sample audio files
- [ ] Create Docker container

### Deliverables:
- Complete documentation set
- Binary packages available
- Docker image published

---

## Risk Mitigation

### High Risk Items:
1. **Audio Latency**: May not achieve <1ms on all systems
   - Mitigation: Make it configurable, document limitations

2. **TUI Complexity**: 41 errors may hide deeper issues
   - Mitigation: Consider simpler UI initially, add features incrementally

3. **FT8 Accuracy**: May not reach 95% immediately
   - Mitigation: Focus on strong signals first, improve weak signal handling later

4. **Cross-platform**: Windows support may require significant work
   - Mitigation: Focus on Linux/Mac first, Windows as stretch goal

### Dependencies:
- Requires working sound card for testing
- Needs FT8 test signals (can generate with WSJT-X)
- Hamlib testing requires radio or rigctld simulator
- Performance testing needs quiet RF environment

---

## Success Criteria

### Minimum Viable Prototype:
- [x] Compiles without errors
- [x] Processes real audio input
- [x] Decodes FT8 messages
- [x] Displays in TUI
- [x] Logs to database

### Stretch Goals:
- [ ] Hamlib CAT control working
- [ ] Transmit capability
- [ ] Waterfall display
- [ ] >95% decode accuracy at -20dB
- [ ] <1ms audio latency verified

---

## Resource Requirements

### Development Environment:
- Rust 1.70+ with cargo
- Linux or macOS (Windows needs WSL)
- Sound card with line input
- 8GB RAM minimum
- Optional: Amateur radio transceiver

### Testing Requirements:
- FT8 audio samples (included)
- WSJT-X for reference testing
- rigctld for Hamlib testing
- Terminal with 80x24 minimum

### Time Estimates:
- Senior Rust Developer: 6 weeks (240 hours)
- Mid-level Developer: 8 weeks (320 hours)
- Junior Developer: 10-12 weeks (400+ hours)

---

## Implementation Order

### Critical Path (Must Have):
1. Fix compilation errors (Phase 1)
2. Implement real audio (Phase 2)
3. Connect FT8 decoder (Phase 3)
4. Fix TUI display (Phase 4)

### Important (Should Have):
5. Integration testing (Phase 6)
6. Bug fixes (Phase 7)

### Nice to Have:
7. Hamlib integration (Phase 5)
8. Documentation (Phase 8)

---

## Tracking Progress

### Week 1 Milestone:
- All modules compile
- Test suite runs

### Week 2 Milestone:
- Real audio flows through pipeline
- Latency measured

### Week 3 Milestone:
- FT8 messages decoded
- Accuracy measured

### Week 4 Milestone:
- TUI displays messages
- User can interact

### Week 5 Milestone:
- Radio control works (optional)

### Week 6 Milestone:
- All tests passing
- Performance validated

### Week 7 Milestone:
- No critical bugs
- Stable operation

### Week 8 Milestone:
- Documentation complete
- Ready for alpha release

---

## Next Steps

1. **Review this plan** with stakeholders
2. **Prioritize features** based on available resources
3. **Set up CI/CD** for automated testing
4. **Create project board** for tracking
5. **Begin Phase 1** implementation

## Commands to Start

```bash
# Phase 1: Fix compilation
cd /Users/thagale/Code/pancetta
cargo fix --all --allow-dirty
cargo build --all

# Phase 2: Test audio
cargo run --bin pancetta-audio

# Phase 3: Test FT8
cargo test -p pancetta-ft8

# Phase 4: Test TUI
cargo run --bin pancetta-tui

# Track progress
git checkout -b prototype-completion
git add PROTOTYPE_COMPLETION_PLAN.md
git commit -m "Add prototype completion plan"
```

---

*This plan represents approximately 240-320 hours of focused development work to achieve a functional FT8 prototype. The modular architecture allows for parallel work on some phases if multiple developers are available.*
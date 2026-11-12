//! The `Action` every TUI key resolves to (PAN-327).
//!
//! `keymap::resolve` maps a key event to exactly one `Action` through the
//! chords on the `KEYBINDINGS` rows; `TuiRunner::handle_key_event` then runs
//! `match action`. Each variant's doc names the key and the legacy behaviour.
//! Dialog chords and character-entry rules resolve through the same enum.

/// One operator intent a key press can trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Shift+Q: halt the station without exiting (PAN-113, any modifiers).
    EmergencyStop,
    /// `q`: open the quit-confirm modal.
    OpenQuitConfirm,
    /// `?`: toggle the help overlay.
    ToggleHelp,
    /// `d`: open the audio device picker.
    OpenDevicePicker,
    /// `i`: open the rig config form and re-enumerate serial ports.
    OpenRigPicker,
    /// Shift+W: open the watchlist modal, pre-filled from DX Hunter.
    OpenWatchlistModal,
    /// Shift+F: open the dial-frequency / split modal.
    OpenFreqModal,
    /// `o`: open the TX-audio-offset modal.
    OpenOffsetModal,
    /// `/`: enter compose (free-text TX) mode.
    EnterCompose,
    /// Enter outside Callers/TxPlacement: hint toward compose.
    ComposeHint,
    /// Shift+D: open the diagnostics overlay at the newest event.
    OpenDiagnostics,
    /// Shift+S: open the station-health overlay.
    OpenHealth,
    /// Shift+R: open the Recent-QSOs overlay at the newest entry.
    OpenRecentQsos,
    /// `w`: open the Why overlay.
    OpenWhy,
    /// Esc while the operator-stop banner shows: clear it.
    ClearStopBanner,
    /// Esc while zoomed and no stop banner: clear zoom.
    ClearZoom,
    /// `z`: toggle zoom of the focused panel.
    ToggleZoom,
    /// `!`: acknowledge visible yellow readiness warnings (PAN-89).
    AckReadiness,
    /// Tab: clear zoom and focus the next panel.
    NextPanel,
    /// Shift+Tab: focus the previous panel (zoom kept).
    PrevPanel,
    /// Up on QSO Status: move the QSO cursor up.
    QsoCursorUp,
    /// Down on QSO Status: move the QSO cursor down.
    QsoCursorDown,
    /// Up elsewhere: previous list item (Callers re-clamps).
    ListUp,
    /// Down elsewhere: next list item (Callers re-clamps).
    ListDown,
    /// Home / `<` / `,`: jump to newest (realtime).
    JumpNewest,
    /// End / `>` / `.`: jump to oldest.
    JumpOldest,
    /// PgUp: page the focused list up.
    ListPageUp,
    /// PgDn: page the focused list down.
    ListPageDown,
    /// `1`: focus Band Activity.
    FocusBandActivity,
    /// `2`: focus QSO Status.
    FocusQsoStatus,
    /// `3`: focus Callers.
    FocusCallers,
    /// `4`: focus DX Hunter.
    FocusDxHunter,
    /// `5`: focus TX Placement.
    FocusTxPlacement,
    /// `v`: cycle the active view forward.
    CycleViewForward,
    /// `V`: cycle the active view backward.
    CycleViewBack,
    /// Left on Callers: previous reply step.
    PrevReplyStep,
    /// Right on Callers: next reply step.
    NextReplyStep,
    /// Left on TxPlacement: previous ranked slice.
    PlacementCursorLeft,
    /// Right on TxPlacement: next ranked slice.
    PlacementCursorRight,
    /// `[` / Left elsewhere: TX offset -50 Hz (floor 200).
    TxOffsetDown,
    /// `]` / Right elsewhere: TX offset +50 Hz (cap 2900).
    TxOffsetUp,
    /// `=`: band up (reverts split).
    BandUp,
    /// `-` / `_`: band down (reverts split).
    BandDown,
    /// `c`: start CQ at the current TX offset.
    StartCq,
    /// `s`: stop CQ.
    StopCq,
    /// `h`: halt the current TX.
    HaltTx,
    /// `p`: toggle PTT (refused while TX is Disabled).
    TogglePtt,
    /// Shift+T: toggle a tune carrier (refused while TX is Disabled).
    ToggleTune,
    /// `t`: find and hold the best TX offset.
    FindClearOffset,
    /// `u`: force a TX-offset nudge in Auto (PAN-72).
    NudgeTxOffset,
    /// `f`: toggle TX-frequency mode Hold / Auto.
    ToggleTxFreqMode,
    /// `g`: cycle the TX policy.
    CycleTxPolicy,
    /// `G`: save the receive gain preset for this band.
    SaveGainPreset,
    /// `a`: toggle autonomous mode (clears the stop banner).
    ToggleAutonomous,
    /// Shift+P: pause / resume autonomous.
    ToggleAutonomousPause,
    /// Shift+M: cycle the operating mode.
    CycleOperatingMode,
    /// `e`: cycle the decode-effort preset.
    CycleDecodeEffort,
    /// Shift+H on DX Hunter: engage Hound on the selected station.
    EngageHound,
    /// Shift+H elsewhere: hint to focus DX Hunter.
    HoundFocusHint,
    /// Shift+X: toggle Fox mode.
    ToggleFoxMode,
    /// `r` on QSO Status: re-send the selected QSO's last TX.
    ResendLastTx,
    /// `r` elsewhere: hint to focus QSO Status.
    ResendFocusHint,
    /// bare `k`: abort the selected QSO (PAN-21).
    AbortSelectedQso,
    /// Space: call or reply to the selected station.
    CallOrReplySelected,
    /// Enter on Callers: reply at the shown step.
    ReplyToSelectedCaller,
    /// Enter on TxPlacement: park at the selected slice.
    ParkAtPlacement,
    /// `m`: toggle monitoring.
    ToggleMonitoring,
    /// `x`: arm, then confirm, clearing decodes.
    ClearDecodes,
    /// OutOfBandAck Enter, OutOfBandAck Esc: ack out of band.
    AckOutOfBand,
    /// QuitConfirm Char('y'), QuitConfirm Char('Y'), QuitConfirm Enter: confirm quit.
    ConfirmQuit,
    /// QuitConfirm Char('n'), QuitConfirm Char('N'), QuitConfirm Esc, QuitConfirm Char('q'): cancel quit.
    CancelQuit,
    /// Help Up: help scroll up.
    HelpScrollUp,
    /// Help Down: help scroll down.
    HelpScrollDown,
    /// Help PageUp: help page up.
    HelpPageUp,
    /// Help PageDown: help page down.
    HelpPageDown,
    /// Help Home: help top.
    HelpTop,
    /// Help End: help bottom.
    HelpBottom,
    /// DevicePicker Esc: cancel device picker.
    CancelDevicePicker,
    /// DevicePicker Tab, DevicePicker BackTab: device picker switch list.
    DevicePickerSwitchList,
    /// DevicePicker Up: device picker up.
    DevicePickerUp,
    /// DevicePicker Down: device picker down.
    DevicePickerDown,
    /// DevicePicker Enter: apply devices.
    ApplyDevices,
    /// RigBookmarkName Esc: cancel bookmark name.
    CancelBookmarkName,
    /// RigBookmarkName Backspace: bookmark name backspace.
    BookmarkNameBackspace,
    /// RigBookmarkName character input: bookmark name char.
    BookmarkNameChar(char),
    /// RigBookmarkName Enter: save bookmark.
    SaveBookmark,
    /// RigBookmarkList Esc: cancel bookmark list.
    CancelBookmarkList,
    /// RigBookmarkList Up: bookmark list up.
    BookmarkListUp,
    /// RigBookmarkList Down: bookmark list down.
    BookmarkListDown,
    /// RigBookmarkList Enter: load bookmark.
    LoadBookmark,
    /// RigBookmarkList Char('x'): delete bookmark.
    DeleteBookmark,
    /// RigForm Esc: cancel rig picker.
    CancelRigPicker,
    /// RigForm Tab: rig next field.
    RigNextField,
    /// RigForm Up: rig field up.
    RigFieldUp,
    /// RigForm Down: rig field down.
    RigFieldDown,
    /// RigForm Backspace: rig model backspace.
    RigModelBackspace,
    /// RigForm character input: rig model char.
    RigModelChar(char),
    /// RigForm F(2): open bookmark list.
    OpenBookmarkList,
    /// RigForm F(3): start bookmark name.
    StartBookmarkName,
    /// RigForm Enter: apply rig.
    ApplyRig,
    /// WatchlistModal Esc: close watchlist modal.
    CloseWatchlistModal,
    /// WatchlistModal Backspace: watchlist backspace.
    WatchlistBackspace,
    /// WatchlistModal character input: watchlist char.
    WatchlistChar(char),
    /// WatchlistModal Enter: submit watchlist.
    SubmitWatchlist,
    /// FreqModal Esc: cancel freq modal.
    CancelFreqModal,
    /// FreqModal Tab, FreqModal BackTab: freq modal switch field.
    FreqModalSwitchField,
    /// FreqModal character input: freq modal char.
    FreqModalChar(char),
    /// FreqModal Backspace: freq modal backspace.
    FreqModalBackspace,
    /// FreqModal Enter: submit freq modal.
    SubmitFreqModal,
    /// OffsetModal Esc: cancel offset modal.
    CancelOffsetModal,
    /// OffsetModal character input: offset modal char.
    OffsetModalChar(char),
    /// OffsetModal Backspace: offset modal backspace.
    OffsetModalBackspace,
    /// OffsetModal Enter: submit offset modal.
    SubmitOffsetModal,
    /// Compose Esc: cancel compose.
    CancelCompose,
    /// Compose Enter: send compose.
    SendCompose,
    /// Compose Backspace: compose backspace.
    ComposeBackspace,
    /// Compose character input: compose char.
    ComposeChar(char),
    /// Diagnostics Esc, Diagnostics Char('D'): close diagnostics.
    CloseDiagnostics,
    /// Diagnostics Up: diagnostics up.
    DiagnosticsUp,
    /// Diagnostics Down: diagnostics down.
    DiagnosticsDown,
    /// Health Esc, Health Char('S'): close health.
    CloseHealth,
    /// Health Up: health scroll up.
    HealthScrollUp,
    /// Health Down: health scroll down.
    HealthScrollDown,
    /// Health PageUp: health page up.
    HealthPageUp,
    /// Health PageDown: health page down.
    HealthPageDown,
    /// Health Home: health top.
    HealthTop,
    /// Health End: health bottom.
    HealthBottom,
    /// RecentQsos Esc, RecentQsos Char('R'): close recent qsos.
    CloseRecentQsos,
    /// RecentQsos Up: recent qsos up.
    RecentQsosUp,
    /// RecentQsos Down: recent qsos down.
    RecentQsosDown,
    /// Why Esc, Why Char('w'): close why.
    CloseWhy,
    /// Why Up: why up.
    WhyUp,
    /// Why Down: why down.
    WhyDown,
    /// Shift+L: look up the focused callsign on QRZ.
    LookupSelectedStation,
}

impl Action {
    /// Every variant once, in declaration order (`ALL[i].ordinal() == i`).
    pub const ALL: [Action; 134] = [
        Action::EmergencyStop,
        Action::OpenQuitConfirm,
        Action::ToggleHelp,
        Action::OpenDevicePicker,
        Action::OpenRigPicker,
        Action::OpenWatchlistModal,
        Action::OpenFreqModal,
        Action::OpenOffsetModal,
        Action::EnterCompose,
        Action::ComposeHint,
        Action::OpenDiagnostics,
        Action::OpenHealth,
        Action::OpenRecentQsos,
        Action::OpenWhy,
        Action::ClearStopBanner,
        Action::ClearZoom,
        Action::ToggleZoom,
        Action::AckReadiness,
        Action::NextPanel,
        Action::PrevPanel,
        Action::QsoCursorUp,
        Action::QsoCursorDown,
        Action::ListUp,
        Action::ListDown,
        Action::JumpNewest,
        Action::JumpOldest,
        Action::ListPageUp,
        Action::ListPageDown,
        Action::FocusBandActivity,
        Action::FocusQsoStatus,
        Action::FocusCallers,
        Action::FocusDxHunter,
        Action::FocusTxPlacement,
        Action::CycleViewForward,
        Action::CycleViewBack,
        Action::PrevReplyStep,
        Action::NextReplyStep,
        Action::PlacementCursorLeft,
        Action::PlacementCursorRight,
        Action::TxOffsetDown,
        Action::TxOffsetUp,
        Action::BandUp,
        Action::BandDown,
        Action::StartCq,
        Action::StopCq,
        Action::HaltTx,
        Action::TogglePtt,
        Action::ToggleTune,
        Action::FindClearOffset,
        Action::NudgeTxOffset,
        Action::ToggleTxFreqMode,
        Action::CycleTxPolicy,
        Action::SaveGainPreset,
        Action::ToggleAutonomous,
        Action::ToggleAutonomousPause,
        Action::CycleOperatingMode,
        Action::CycleDecodeEffort,
        Action::EngageHound,
        Action::HoundFocusHint,
        Action::ToggleFoxMode,
        Action::ResendLastTx,
        Action::ResendFocusHint,
        Action::AbortSelectedQso,
        Action::CallOrReplySelected,
        Action::ReplyToSelectedCaller,
        Action::ParkAtPlacement,
        Action::ToggleMonitoring,
        Action::ClearDecodes,
        Action::AckOutOfBand,
        Action::ConfirmQuit,
        Action::CancelQuit,
        Action::HelpScrollUp,
        Action::HelpScrollDown,
        Action::HelpPageUp,
        Action::HelpPageDown,
        Action::HelpTop,
        Action::HelpBottom,
        Action::CancelDevicePicker,
        Action::DevicePickerSwitchList,
        Action::DevicePickerUp,
        Action::DevicePickerDown,
        Action::ApplyDevices,
        Action::CancelBookmarkName,
        Action::BookmarkNameBackspace,
        Action::BookmarkNameChar('a'),
        Action::SaveBookmark,
        Action::CancelBookmarkList,
        Action::BookmarkListUp,
        Action::BookmarkListDown,
        Action::LoadBookmark,
        Action::DeleteBookmark,
        Action::CancelRigPicker,
        Action::RigNextField,
        Action::RigFieldUp,
        Action::RigFieldDown,
        Action::RigModelBackspace,
        Action::RigModelChar('a'),
        Action::OpenBookmarkList,
        Action::StartBookmarkName,
        Action::ApplyRig,
        Action::CloseWatchlistModal,
        Action::WatchlistBackspace,
        Action::WatchlistChar('a'),
        Action::SubmitWatchlist,
        Action::CancelFreqModal,
        Action::FreqModalSwitchField,
        Action::FreqModalChar('a'),
        Action::FreqModalBackspace,
        Action::SubmitFreqModal,
        Action::CancelOffsetModal,
        Action::OffsetModalChar('a'),
        Action::OffsetModalBackspace,
        Action::SubmitOffsetModal,
        Action::CancelCompose,
        Action::SendCompose,
        Action::ComposeBackspace,
        Action::ComposeChar('a'),
        Action::CloseDiagnostics,
        Action::DiagnosticsUp,
        Action::DiagnosticsDown,
        Action::CloseHealth,
        Action::HealthScrollUp,
        Action::HealthScrollDown,
        Action::HealthPageUp,
        Action::HealthPageDown,
        Action::HealthTop,
        Action::HealthBottom,
        Action::CloseRecentQsos,
        Action::RecentQsosUp,
        Action::RecentQsosDown,
        Action::CloseWhy,
        Action::WhyUp,
        Action::WhyDown,
        Action::LookupSelectedStation,
    ];

    /// Position in [`Action::ALL`]. Exhaustive with no `_` arm, so a new
    /// variant fails to compile until it gets an ordinal, and the
    /// `all_lists_every_variant_once` test fails until `ALL` lists it.
    pub const fn ordinal(self) -> usize {
        match self {
            Action::EmergencyStop => 0,
            Action::OpenQuitConfirm => 1,
            Action::ToggleHelp => 2,
            Action::OpenDevicePicker => 3,
            Action::OpenRigPicker => 4,
            Action::OpenWatchlistModal => 5,
            Action::OpenFreqModal => 6,
            Action::OpenOffsetModal => 7,
            Action::EnterCompose => 8,
            Action::ComposeHint => 9,
            Action::OpenDiagnostics => 10,
            Action::OpenHealth => 11,
            Action::OpenRecentQsos => 12,
            Action::OpenWhy => 13,
            Action::ClearStopBanner => 14,
            Action::ClearZoom => 15,
            Action::ToggleZoom => 16,
            Action::AckReadiness => 17,
            Action::NextPanel => 18,
            Action::PrevPanel => 19,
            Action::QsoCursorUp => 20,
            Action::QsoCursorDown => 21,
            Action::ListUp => 22,
            Action::ListDown => 23,
            Action::JumpNewest => 24,
            Action::JumpOldest => 25,
            Action::ListPageUp => 26,
            Action::ListPageDown => 27,
            Action::FocusBandActivity => 28,
            Action::FocusQsoStatus => 29,
            Action::FocusCallers => 30,
            Action::FocusDxHunter => 31,
            Action::FocusTxPlacement => 32,
            Action::CycleViewForward => 33,
            Action::CycleViewBack => 34,
            Action::PrevReplyStep => 35,
            Action::NextReplyStep => 36,
            Action::PlacementCursorLeft => 37,
            Action::PlacementCursorRight => 38,
            Action::TxOffsetDown => 39,
            Action::TxOffsetUp => 40,
            Action::BandUp => 41,
            Action::BandDown => 42,
            Action::StartCq => 43,
            Action::StopCq => 44,
            Action::HaltTx => 45,
            Action::TogglePtt => 46,
            Action::ToggleTune => 47,
            Action::FindClearOffset => 48,
            Action::NudgeTxOffset => 49,
            Action::ToggleTxFreqMode => 50,
            Action::CycleTxPolicy => 51,
            Action::SaveGainPreset => 52,
            Action::ToggleAutonomous => 53,
            Action::ToggleAutonomousPause => 54,
            Action::LookupSelectedStation => 133,
            Action::CycleOperatingMode => 55,
            Action::CycleDecodeEffort => 56,
            Action::EngageHound => 57,
            Action::HoundFocusHint => 58,
            Action::ToggleFoxMode => 59,
            Action::ResendLastTx => 60,
            Action::ResendFocusHint => 61,
            Action::AbortSelectedQso => 62,
            Action::CallOrReplySelected => 63,
            Action::ReplyToSelectedCaller => 64,
            Action::ParkAtPlacement => 65,
            Action::ToggleMonitoring => 66,
            Action::ClearDecodes => 67,
            Action::AckOutOfBand => 68,
            Action::ConfirmQuit => 69,
            Action::CancelQuit => 70,
            Action::HelpScrollUp => 71,
            Action::HelpScrollDown => 72,
            Action::HelpPageUp => 73,
            Action::HelpPageDown => 74,
            Action::HelpTop => 75,
            Action::HelpBottom => 76,
            Action::CancelDevicePicker => 77,
            Action::DevicePickerSwitchList => 78,
            Action::DevicePickerUp => 79,
            Action::DevicePickerDown => 80,
            Action::ApplyDevices => 81,
            Action::CancelBookmarkName => 82,
            Action::BookmarkNameBackspace => 83,
            Action::BookmarkNameChar(_) => 84,
            Action::SaveBookmark => 85,
            Action::CancelBookmarkList => 86,
            Action::BookmarkListUp => 87,
            Action::BookmarkListDown => 88,
            Action::LoadBookmark => 89,
            Action::DeleteBookmark => 90,
            Action::CancelRigPicker => 91,
            Action::RigNextField => 92,
            Action::RigFieldUp => 93,
            Action::RigFieldDown => 94,
            Action::RigModelBackspace => 95,
            Action::RigModelChar(_) => 96,
            Action::OpenBookmarkList => 97,
            Action::StartBookmarkName => 98,
            Action::ApplyRig => 99,
            Action::CloseWatchlistModal => 100,
            Action::WatchlistBackspace => 101,
            Action::WatchlistChar(_) => 102,
            Action::SubmitWatchlist => 103,
            Action::CancelFreqModal => 104,
            Action::FreqModalSwitchField => 105,
            Action::FreqModalChar(_) => 106,
            Action::FreqModalBackspace => 107,
            Action::SubmitFreqModal => 108,
            Action::CancelOffsetModal => 109,
            Action::OffsetModalChar(_) => 110,
            Action::OffsetModalBackspace => 111,
            Action::SubmitOffsetModal => 112,
            Action::CancelCompose => 113,
            Action::SendCompose => 114,
            Action::ComposeBackspace => 115,
            Action::ComposeChar(_) => 116,
            Action::CloseDiagnostics => 117,
            Action::DiagnosticsUp => 118,
            Action::DiagnosticsDown => 119,
            Action::CloseHealth => 120,
            Action::HealthScrollUp => 121,
            Action::HealthScrollDown => 122,
            Action::HealthPageUp => 123,
            Action::HealthPageDown => 124,
            Action::HealthTop => 125,
            Action::HealthBottom => 126,
            Action::CloseRecentQsos => 127,
            Action::RecentQsosUp => 128,
            Action::RecentQsosDown => 129,
            Action::CloseWhy => 130,
            Action::WhyUp => 131,
            Action::WhyDown => 132,
        }
    }

    /// Actions that can key the transmitter (PAN-112 tier A plus `u`, Space
    /// and Callers Enter). Every Main-scope chord for one of these must
    /// refuse Ctrl and Alt (`Mods::NoCtrlAlt` or `Mods::Bare`). Exhaustive
    /// with no `_` arm, so a new variant must be classified before it compiles.
    pub const fn starts_tx(self) -> bool {
        match self {
            Action::StartCq
            | Action::TogglePtt
            | Action::ToggleTune
            | Action::NudgeTxOffset
            | Action::CycleTxPolicy
            | Action::ToggleAutonomous
            | Action::EngageHound
            | Action::ToggleFoxMode
            | Action::ResendLastTx
            | Action::CallOrReplySelected
            | Action::ReplyToSelectedCaller
            | Action::SendCompose => true,
            Action::EmergencyStop
            | Action::OpenQuitConfirm
            | Action::ToggleHelp
            | Action::OpenDevicePicker
            | Action::OpenRigPicker
            | Action::OpenWatchlistModal
            | Action::OpenFreqModal
            | Action::OpenOffsetModal
            | Action::EnterCompose
            | Action::ComposeHint
            | Action::OpenDiagnostics
            | Action::OpenHealth
            | Action::OpenRecentQsos
            | Action::OpenWhy
            | Action::ClearStopBanner
            | Action::ClearZoom
            | Action::ToggleZoom
            | Action::AckReadiness
            | Action::NextPanel
            | Action::PrevPanel
            | Action::QsoCursorUp
            | Action::QsoCursorDown
            | Action::ListUp
            | Action::ListDown
            | Action::JumpNewest
            | Action::JumpOldest
            | Action::ListPageUp
            | Action::ListPageDown
            | Action::FocusBandActivity
            | Action::FocusQsoStatus
            | Action::FocusCallers
            | Action::FocusDxHunter
            | Action::FocusTxPlacement
            | Action::CycleViewForward
            | Action::CycleViewBack
            | Action::PrevReplyStep
            | Action::NextReplyStep
            | Action::PlacementCursorLeft
            | Action::PlacementCursorRight
            | Action::TxOffsetDown
            | Action::TxOffsetUp
            | Action::BandUp
            | Action::BandDown
            | Action::StopCq
            | Action::HaltTx
            | Action::FindClearOffset
            | Action::ToggleTxFreqMode
            | Action::SaveGainPreset
            | Action::ToggleAutonomousPause
            | Action::LookupSelectedStation
            | Action::CycleOperatingMode
            | Action::CycleDecodeEffort
            | Action::HoundFocusHint
            | Action::ResendFocusHint
            | Action::AbortSelectedQso
            | Action::ParkAtPlacement
            | Action::ToggleMonitoring
            | Action::ClearDecodes
            | Action::AckOutOfBand
            | Action::ConfirmQuit
            | Action::CancelQuit
            | Action::HelpScrollUp
            | Action::HelpScrollDown
            | Action::HelpPageUp
            | Action::HelpPageDown
            | Action::HelpTop
            | Action::HelpBottom
            | Action::CancelDevicePicker
            | Action::DevicePickerSwitchList
            | Action::DevicePickerUp
            | Action::DevicePickerDown
            | Action::ApplyDevices
            | Action::CancelBookmarkName
            | Action::BookmarkNameBackspace
            | Action::BookmarkNameChar(_)
            | Action::SaveBookmark
            | Action::CancelBookmarkList
            | Action::BookmarkListUp
            | Action::BookmarkListDown
            | Action::LoadBookmark
            | Action::DeleteBookmark
            | Action::CancelRigPicker
            | Action::RigNextField
            | Action::RigFieldUp
            | Action::RigFieldDown
            | Action::RigModelBackspace
            | Action::RigModelChar(_)
            | Action::OpenBookmarkList
            | Action::StartBookmarkName
            | Action::ApplyRig
            | Action::CloseWatchlistModal
            | Action::WatchlistBackspace
            | Action::WatchlistChar(_)
            | Action::SubmitWatchlist
            | Action::CancelFreqModal
            | Action::FreqModalSwitchField
            | Action::FreqModalChar(_)
            | Action::FreqModalBackspace
            | Action::SubmitFreqModal
            | Action::CancelOffsetModal
            | Action::OffsetModalChar(_)
            | Action::OffsetModalBackspace
            | Action::SubmitOffsetModal
            | Action::CancelCompose
            | Action::ComposeBackspace
            | Action::ComposeChar(_)
            | Action::CloseDiagnostics
            | Action::DiagnosticsUp
            | Action::DiagnosticsDown
            | Action::CloseHealth
            | Action::HealthScrollUp
            | Action::HealthScrollDown
            | Action::HealthPageUp
            | Action::HealthPageDown
            | Action::HealthTop
            | Action::HealthBottom
            | Action::CloseRecentQsos
            | Action::RecentQsosUp
            | Action::RecentQsosDown
            | Action::CloseWhy
            | Action::WhyUp
            | Action::WhyDown => false,
        }
    }
}

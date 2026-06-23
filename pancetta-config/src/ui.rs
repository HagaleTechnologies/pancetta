//! UI preferences configuration module
//!
//! This module handles user interface preferences including themes, layouts,
//! window management, accessibility settings, and visual customization options.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// User interface configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
// Container-level serde default: omitted fields fall back to defaults rather
// than failing to deserialize a partial config.
#[serde(default)]
pub struct UiConfig {
    /// Color theme settings
    pub theme: String,

    /// UI layout configuration
    pub layout: String,

    /// Window management settings
    pub window: WindowConfig,

    /// Font and typography settings
    pub typography: TypographyConfig,

    /// Color scheme customization
    pub colors: ColorSchemeConfig,

    /// Widget and panel settings
    pub panels: PanelConfig,

    /// Accessibility settings
    pub accessibility: AccessibilityConfig,

    /// Animation and transition settings
    pub animations: AnimationConfig,

    /// Toolbar and menu customization
    pub toolbars: ToolbarConfig,

    /// Status bar configuration
    pub status_bar: StatusBarConfig,

    /// Keyboard shortcuts
    pub keyboard: KeyboardConfig,

    /// Logging and display preferences
    pub logging: LoggingDisplayConfig,

    /// Waterfall and spectrum display settings
    pub spectrum: SpectrumDisplayConfig,

    /// Custom UI components
    #[serde(default)]
    pub custom_widgets: HashMap<String, WidgetConfig>,
}

/// Window management configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    /// Default window width
    pub width: u32,

    /// Default window height
    pub height: u32,

    /// Default window position X
    pub position_x: Option<i32>,

    /// Default window position Y
    pub position_y: Option<i32>,

    /// Window state (normal, maximized, minimized)
    pub state: WindowState,

    /// Enable window decorations
    pub decorations: bool,

    /// Enable window resizing
    pub resizable: bool,

    /// Window transparency (0.0 to 1.0)
    pub transparency: f32,

    /// Always on top
    pub always_on_top: bool,

    /// Multi-monitor settings
    pub multi_monitor: MultiMonitorConfig,

    /// Window session management
    pub session: SessionConfig,
}

/// Window state options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    Normal,
    Maximized,
    Minimized,
    Fullscreen,
}

/// Multi-monitor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiMonitorConfig {
    /// Preferred monitor index
    pub preferred_monitor: u8,

    /// Allow spanning across monitors
    pub span_monitors: bool,

    /// Remember monitor placement
    pub remember_placement: bool,

    /// DPI scaling per monitor
    pub monitor_scaling: HashMap<u8, f32>,
}

/// Session management configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Restore window positions on startup
    pub restore_positions: bool,

    /// Restore panel layouts
    pub restore_layouts: bool,

    /// Session file path
    pub session_file: Option<String>,

    /// Auto-save session interval (minutes)
    pub auto_save_interval_minutes: u32,
}

/// Typography and font configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypographyConfig {
    /// Default font family
    pub font_family: String,

    /// Default font size in points
    pub font_size: f32,

    /// Monospace font for logs and data
    pub monospace_font: String,

    /// Monospace font size
    pub monospace_size: f32,

    /// Font weights
    pub font_weights: FontWeightConfig,

    /// Text rendering settings
    pub rendering: TextRenderingConfig,

    /// Font scaling for different UI elements
    pub scaling: FontScalingConfig,
}

/// Font weight configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontWeightConfig {
    /// Normal text weight
    pub normal: u16,

    /// Bold text weight
    pub bold: u16,

    /// Light text weight
    pub light: u16,

    /// Heavy text weight
    pub heavy: u16,
}

/// Text rendering configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextRenderingConfig {
    /// Antialiasing mode
    pub antialiasing: AntialiasingMode,

    /// Subpixel rendering
    pub subpixel_rendering: bool,

    /// Hinting mode
    pub hinting: HintingMode,

    /// Line height multiplier
    pub line_height: f32,

    /// Letter spacing adjustment
    pub letter_spacing: f32,
}

/// Antialiasing modes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AntialiasingMode {
    None,
    Grayscale,
    Subpixel,
}

/// Font hinting modes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HintingMode {
    None,
    Slight,
    Medium,
    Full,
}

/// Font scaling configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontScalingConfig {
    /// UI element font scaling
    pub ui_scale: f32,

    /// Log text scaling
    pub log_scale: f32,

    /// Button text scaling
    pub button_scale: f32,

    /// Menu text scaling
    pub menu_scale: f32,

    /// Tooltip text scaling
    pub tooltip_scale: f32,
}

/// Color scheme configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorSchemeConfig {
    /// Primary colors
    pub primary: ColorPalette,

    /// Secondary colors
    pub secondary: ColorPalette,

    /// Background colors
    pub background: BackgroundColors,

    /// Text colors
    pub text: TextColors,

    /// Border and separator colors
    pub borders: BorderColors,

    /// Status and alert colors
    pub status: StatusColors,

    /// Spectrum and waterfall colors
    pub spectrum: SpectrumColors,

    /// Custom color definitions
    #[serde(default)]
    pub custom_colors: HashMap<String, String>,
}

/// Color palette definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorPalette {
    /// Base color
    pub base: String,

    /// Light variant
    pub light: String,

    /// Dark variant
    pub dark: String,

    /// Contrast color
    pub contrast: String,
}

/// Background color configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundColors {
    /// Main background
    pub primary: String,

    /// Secondary background
    pub secondary: String,

    /// Panel background
    pub panel: String,

    /// Input field background
    pub input: String,

    /// Selected item background
    pub selected: String,

    /// Hover state background
    pub hover: String,
}

/// Text color configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextColors {
    /// Primary text color
    pub primary: String,

    /// Secondary text color
    pub secondary: String,

    /// Disabled text color
    pub disabled: String,

    /// Link text color
    pub link: String,

    /// Selected text color
    pub selected: String,

    /// Placeholder text color
    pub placeholder: String,
}

/// Border color configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BorderColors {
    /// Normal border color
    pub normal: String,

    /// Focus border color
    pub focus: String,

    /// Error border color
    pub error: String,

    /// Success border color
    pub success: String,

    /// Warning border color
    pub warning: String,
}

/// Status and alert color configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusColors {
    /// Success/OK status
    pub success: String,

    /// Warning status
    pub warning: String,

    /// Error status
    pub error: String,

    /// Information status
    pub info: String,

    /// Connected status
    pub connected: String,

    /// Disconnected status
    pub disconnected: String,

    /// Transmitting status
    pub transmitting: String,

    /// Receiving status
    pub receiving: String,
}

/// Spectrum display colors
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectrumColors {
    /// Spectrum line color
    pub spectrum_line: String,

    /// Waterfall gradient colors
    pub waterfall_gradient: Vec<String>,

    /// Grid line color
    pub grid_lines: String,

    /// Frequency marker color
    pub frequency_markers: String,

    /// Signal detection color
    pub signal_detection: String,

    /// Background noise color
    pub background_noise: String,
}

/// Panel layout and management configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelConfig {
    /// Available panels
    pub available_panels: Vec<PanelDefinition>,

    /// Default panel layout
    pub default_layout: String,

    /// Panel docking settings
    pub docking: DockingConfig,

    /// Panel sizing and spacing
    pub sizing: PanelSizingConfig,

    /// Panel visibility settings
    pub visibility: PanelVisibilityConfig,
}

/// Panel definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelDefinition {
    /// Panel identifier
    pub id: String,

    /// Display name
    pub name: String,

    /// Panel type
    pub panel_type: PanelType,

    /// Default position
    pub default_position: PanelPosition,

    /// Default size
    pub default_size: PanelSize,

    /// Can be closed by user
    pub closeable: bool,

    /// Can be moved/docked
    pub moveable: bool,

    /// Can be resized
    pub resizable: bool,
}

/// Panel types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelType {
    /// Spectrum/waterfall display
    Spectrum,

    /// Logging panel
    Logging,

    /// Rig control panel
    RigControl,

    /// Audio controls
    AudioControl,

    /// Band/frequency selection
    BandControl,

    /// Memory channels
    Memory,

    /// QSO details
    QsoDetails,

    /// Macro/function keys
    Macros,

    /// Status information
    Status,

    /// Custom panel
    Custom,
}

/// Panel position
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelPosition {
    Left,
    Right,
    Top,
    Bottom,
    Center,
    Floating,
}

/// Panel size definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelSize {
    /// Width in pixels or percentage
    pub width: SizeSpec,

    /// Height in pixels or percentage
    pub height: SizeSpec,

    /// Minimum width
    pub min_width: Option<u32>,

    /// Minimum height
    pub min_height: Option<u32>,

    /// Maximum width
    pub max_width: Option<u32>,

    /// Maximum height
    pub max_height: Option<u32>,
}

/// Size specification (pixels or percentage)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SizeSpec {
    /// Absolute size in pixels
    Pixels(u32),

    /// Percentage of available space
    Percentage(f32),
}

/// Panel docking configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockingConfig {
    /// Enable panel docking
    pub enabled: bool,

    /// Docking sensitivity in pixels
    pub sensitivity: u32,

    /// Show docking guides
    pub show_guides: bool,

    /// Snap to edges
    pub snap_to_edges: bool,

    /// Docking zones
    pub zones: Vec<DockingZone>,
}

/// Docking zone definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockingZone {
    /// Zone name
    pub name: String,

    /// Zone area
    pub area: ZoneArea,

    /// Maximum panels in zone
    pub max_panels: u8,

    /// Panel arrangement in zone
    pub arrangement: ZoneArrangement,
}

/// Zone area specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneArea {
    /// X position (pixels or percentage)
    pub x: SizeSpec,

    /// Y position (pixels or percentage)
    pub y: SizeSpec,

    /// Width (pixels or percentage)
    pub width: SizeSpec,

    /// Height (pixels or percentage)
    pub height: SizeSpec,
}

/// Panel arrangement in zones
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZoneArrangement {
    /// Horizontal tabs
    Tabbed,

    /// Vertical stack
    Stacked,

    /// Side by side
    Horizontal,

    /// Grid layout
    Grid,
}

/// Panel sizing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelSizingConfig {
    /// Default panel spacing
    pub spacing: u32,

    /// Panel border width
    pub border_width: u32,

    /// Panel title bar height
    pub title_bar_height: u32,

    /// Splitter width
    pub splitter_width: u32,

    /// Tab height
    pub tab_height: u32,
}

/// Panel visibility configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelVisibilityConfig {
    /// Initially visible panels
    pub initial_panels: Vec<String>,

    /// Hide panels when inactive
    pub auto_hide: bool,

    /// Auto-hide delay in milliseconds
    pub auto_hide_delay_ms: u64,

    /// Show panel titles
    pub show_titles: bool,

    /// Show close buttons
    pub show_close_buttons: bool,
}

/// Accessibility configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AccessibilityConfig {
    /// High contrast mode
    pub high_contrast: bool,

    /// Large fonts mode
    pub large_fonts: bool,

    /// Screen reader support
    pub screen_reader: bool,

    /// Keyboard navigation
    pub keyboard_navigation: KeyboardNavigationConfig,

    /// Visual indicators
    pub visual_indicators: VisualIndicatorConfig,

    /// Sound feedback
    pub sound_feedback: SoundFeedbackConfig,

    /// Motion sensitivity
    pub motion_sensitivity: MotionSensitivityConfig,
}

/// Keyboard navigation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardNavigationConfig {
    /// Enable keyboard navigation
    pub enabled: bool,

    /// Show focus indicators
    pub show_focus: bool,

    /// Focus ring width
    pub focus_ring_width: u32,

    /// Focus ring color
    pub focus_ring_color: String,

    /// Tab navigation order
    pub tab_order: TabOrderConfig,
}

/// Tab order configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabOrderConfig {
    /// Left to right, top to bottom
    Standard,

    /// Logical grouping
    Logical,

    /// Custom order
    Custom,
}

/// Visual indicator configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualIndicatorConfig {
    /// Show status indicators
    pub show_status: bool,

    /// Use color coding
    pub color_coding: bool,

    /// Show progress indicators
    pub show_progress: bool,

    /// Animation for changes
    pub change_animation: bool,

    /// Highlight active elements
    pub highlight_active: bool,
}

/// Sound feedback configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundFeedbackConfig {
    /// Enable sound feedback
    pub enabled: bool,

    /// Button click sounds
    pub button_clicks: bool,

    /// Alert sounds
    pub alerts: bool,

    /// Status change sounds
    pub status_changes: bool,

    /// Sound volume (0.0 to 1.0)
    pub volume: f32,
}

/// Motion sensitivity configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MotionSensitivityConfig {
    /// Reduce animations
    pub reduce_animations: bool,

    /// Disable auto-scroll
    pub disable_auto_scroll: bool,

    /// Parallax effects
    pub disable_parallax: bool,

    /// Static backgrounds
    pub static_backgrounds: bool,
}

/// Animation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationConfig {
    /// Enable animations
    pub enabled: bool,

    /// Animation duration scale (0.0 to 2.0)
    pub duration_scale: f32,

    /// Easing functions
    pub easing: EasingConfig,

    /// Specific animation settings
    pub transitions: TransitionConfig,

    /// Performance settings
    pub performance: AnimationPerformanceConfig,
}

/// Easing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EasingConfig {
    /// Default easing function
    pub default: EasingFunction,

    /// Button hover easing
    pub button_hover: EasingFunction,

    /// Panel transition easing
    pub panel_transition: EasingFunction,

    /// Fade in/out easing
    pub fade: EasingFunction,
}

/// Easing function types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EasingFunction {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    EaseInBack,
    EaseOutBack,
    EaseInOutBack,
    EaseInElastic,
    EaseOutElastic,
    EaseInOutElastic,
}

/// Transition configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionConfig {
    /// Fade transitions
    pub fade_duration_ms: u64,

    /// Slide transitions
    pub slide_duration_ms: u64,

    /// Scale transitions
    pub scale_duration_ms: u64,

    /// Rotation transitions
    pub rotation_duration_ms: u64,

    /// Color transitions
    pub color_duration_ms: u64,
}

/// Animation performance configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationPerformanceConfig {
    /// Use hardware acceleration
    pub hardware_acceleration: bool,

    /// Maximum FPS for animations
    pub max_fps: u32,

    /// Reduce animations on low performance
    pub adaptive_quality: bool,

    /// Performance monitoring
    pub performance_monitoring: bool,
}

/// Toolbar configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ToolbarConfig {
    /// Available toolbars
    pub toolbars: Vec<ToolbarDefinition>,

    /// Toolbar visibility
    pub visibility: ToolbarVisibilityConfig,

    /// Toolbar customization
    pub customization: ToolbarCustomizationConfig,
}

/// Toolbar definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolbarDefinition {
    /// Toolbar identifier
    pub id: String,

    /// Display name
    pub name: String,

    /// Toolbar position
    pub position: ToolbarPosition,

    /// Toolbar items
    pub items: Vec<ToolbarItem>,

    /// Show text labels
    pub show_labels: bool,

    /// Icon size
    pub icon_size: IconSize,
}

/// Toolbar position
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolbarPosition {
    Top,
    Bottom,
    Left,
    Right,
    Floating,
}

/// Toolbar item definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolbarItem {
    /// Item identifier
    pub id: String,

    /// Item type
    pub item_type: ToolbarItemType,

    /// Display text
    pub text: Option<String>,

    /// Icon name
    pub icon: Option<String>,

    /// Tooltip text
    pub tooltip: Option<String>,

    /// Action command
    pub action: Option<String>,

    /// Item enabled state
    pub enabled: bool,

    /// Item visibility
    pub visible: bool,
}

/// Toolbar item types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolbarItemType {
    Button,
    Toggle,
    Dropdown,
    Separator,
    Spacer,
    Text,
    Progress,
    Custom,
}

/// Icon size options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconSize {
    Small,  // 16x16
    Medium, // 24x24
    Large,  // 32x32
    XLarge, // 48x48
}

/// Toolbar visibility configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolbarVisibilityConfig {
    /// Show main toolbar
    pub main_toolbar: bool,

    /// Show status toolbar
    pub status_toolbar: bool,

    /// Show formatting toolbar
    pub formatting_toolbar: bool,

    /// Auto-hide toolbars
    pub auto_hide: bool,

    /// Lock toolbar positions
    pub lock_positions: bool,
}

/// Toolbar customization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolbarCustomizationConfig {
    /// Allow toolbar customization
    pub enabled: bool,

    /// Allow adding/removing items
    pub allow_add_remove: bool,

    /// Allow reordering items
    pub allow_reorder: bool,

    /// Allow creating new toolbars
    pub allow_new_toolbars: bool,

    /// Save customizations
    pub save_customizations: bool,
}

/// Status bar configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarConfig {
    /// Show status bar
    pub visible: bool,

    /// Status bar position
    pub position: StatusBarPosition,

    /// Status bar items
    pub items: Vec<StatusBarItem>,

    /// Update interval in milliseconds
    pub update_interval_ms: u64,

    /// Show progress indicators
    pub show_progress: bool,
}

/// Status bar position
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusBarPosition {
    Top,
    Bottom,
}

/// Status bar item definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarItem {
    /// Item identifier
    pub id: String,

    /// Item type
    pub item_type: StatusBarItemType,

    /// Display format
    pub format: Option<String>,

    /// Item width
    pub width: Option<u32>,

    /// Item alignment
    pub alignment: ItemAlignment,

    /// Item priority (for overflow)
    pub priority: u8,

    /// Item visibility
    pub visible: bool,
}

/// Status bar item types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusBarItemType {
    Text,
    Clock,
    Frequency,
    Mode,
    Power,
    SWR,
    SignalLevel,
    Progress,
    Connection,
    Custom,
}

/// Item alignment options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemAlignment {
    Left,
    Center,
    Right,
}

/// Keyboard shortcuts configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardConfig {
    /// Keyboard shortcuts map
    pub shortcuts: HashMap<String, KeyboardShortcut>,

    /// Enable global shortcuts
    pub global_shortcuts: bool,

    /// Shortcut conflict resolution
    pub conflict_resolution: ConflictResolution,

    /// Show shortcut hints
    pub show_hints: bool,
}

/// Keyboard shortcut definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardShortcut {
    /// Key combination
    pub keys: String,

    /// Action command
    pub action: String,

    /// Shortcut description
    pub description: String,

    /// Context where shortcut is active
    pub context: ShortcutContext,

    /// Shortcut enabled state
    pub enabled: bool,
}

/// Shortcut context
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutContext {
    /// Global (always active)
    Global,

    /// Main window focus
    MainWindow,

    /// Specific panel focus
    Panel(String),

    /// Dialog windows
    Dialog,

    /// Input fields
    Input,
}

/// Conflict resolution strategies
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    /// First registered wins
    FirstWins,

    /// Last registered wins
    LastWins,

    /// Warn user about conflicts
    WarnUser,

    /// Disable conflicting shortcuts
    DisableConflicting,
}

/// Logging display configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingDisplayConfig {
    /// Maximum log entries to display
    pub max_entries: u32,

    /// Log entry format
    pub entry_format: String,

    /// Show timestamps
    pub show_timestamps: bool,

    /// Timestamp format
    pub timestamp_format: String,

    /// Color coding by log level
    pub color_by_level: bool,

    /// Auto-scroll to new entries
    pub auto_scroll: bool,

    /// Log filtering options
    pub filtering: LogFilterConfig,
}

/// Log filtering configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFilterConfig {
    /// Minimum log level to display
    pub min_level: LogLevel,

    /// Category filters
    pub category_filters: Vec<String>,

    /// Text search filter
    pub text_filter: Option<String>,

    /// Show only recent entries
    pub recent_only: bool,

    /// Recent time window in minutes
    pub recent_window_minutes: u32,
}

/// Log levels
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Spectrum display configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectrumDisplayConfig {
    /// Spectrum display mode
    pub mode: SpectrumMode,

    /// FFT size
    pub fft_size: u32,

    /// Averaging factor
    pub averaging: f32,

    /// Update rate in Hz
    pub update_rate: f32,

    /// Waterfall settings
    pub waterfall: WaterfallConfig,

    /// Frequency axis settings
    pub frequency_axis: FrequencyAxisConfig,

    /// Amplitude axis settings
    pub amplitude_axis: AmplitudeAxisConfig,

    /// Grid settings
    pub grid: GridConfig,

    /// Marker settings
    pub markers: MarkerConfig,
}

/// Spectrum display modes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpectrumMode {
    /// Spectrum only
    Spectrum,

    /// Waterfall only
    Waterfall,

    /// Both spectrum and waterfall
    Combined,

    /// Oscilloscope mode
    Oscilloscope,
}

/// Waterfall configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaterfallConfig {
    /// Waterfall height in pixels
    pub height: u32,

    /// Scroll speed in pixels per second
    pub scroll_speed: f32,

    /// Color mapping
    pub color_mapping: ColorMapping,

    /// Contrast adjustment
    pub contrast: f32,

    /// Brightness adjustment
    pub brightness: f32,
}

/// Color mapping for waterfall
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMapping {
    Grayscale,
    Rainbow,
    Heat,
    Cool,
    Plasma,
    Custom,
}

/// Frequency units
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FrequencyUnits {
    Hz,
    Khz,
    Mhz,
    Ghz,
}

/// Frequency axis configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyAxisConfig {
    /// Show frequency axis
    pub visible: bool,

    /// Frequency units
    pub units: FrequencyUnits,

    /// Major tick interval
    pub major_ticks: f32,

    /// Minor tick interval
    pub minor_ticks: f32,

    /// Axis label format
    pub label_format: String,
}

/// Amplitude units
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AmplitudeUnits {
    /// Decibels
    Db,

    /// Decibels relative to milliwatt
    Dbm,

    /// Decibels relative to microvolt
    Dbuv,

    /// Linear scale
    Linear,
}

/// Amplitude scale types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AmplitudeScale {
    Linear,
    Logarithmic,
}

/// Amplitude axis configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmplitudeAxisConfig {
    /// Show amplitude axis
    pub visible: bool,

    /// Amplitude units
    pub units: AmplitudeUnits,

    /// Scale type
    pub scale: AmplitudeScale,

    /// Reference level
    pub reference_level: f32,

    /// Dynamic range
    pub dynamic_range: f32,
}

/// Grid configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridConfig {
    /// Show grid lines
    pub visible: bool,

    /// Major grid color
    pub major_color: String,

    /// Minor grid color
    pub minor_color: String,

    /// Grid line width
    pub line_width: f32,

    /// Grid line style
    pub line_style: LineStyle,
}

/// Line style options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineStyle {
    Solid,
    Dashed,
    Dotted,
    DashDot,
}

/// Marker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkerConfig {
    /// Show frequency markers
    pub visible: bool,

    /// Marker color
    pub color: String,

    /// Marker width
    pub width: f32,

    /// Marker style
    pub style: MarkerStyle,

    /// Show marker labels
    pub show_labels: bool,
}

/// Marker style options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerStyle {
    Line,
    Arrow,
    Triangle,
    Circle,
    Square,
}

/// Custom widget configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetConfig {
    /// Widget type
    pub widget_type: String,

    /// Widget properties
    pub properties: HashMap<String, serde_json::Value>,

    /// Widget position
    pub position: Option<WidgetPosition>,

    /// Widget size
    pub size: Option<WidgetSize>,

    /// Widget visibility
    pub visible: bool,
}

/// Widget position
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetPosition {
    pub x: i32,
    pub y: i32,
}

/// Widget size
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetSize {
    pub width: u32,
    pub height: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            layout: "standard".to_string(),
            window: WindowConfig::default(),
            typography: TypographyConfig::default(),
            colors: ColorSchemeConfig::default(),
            panels: PanelConfig::default(),
            accessibility: AccessibilityConfig::default(),
            animations: AnimationConfig::default(),
            toolbars: ToolbarConfig::default(),
            status_bar: StatusBarConfig::default(),
            keyboard: KeyboardConfig::default(),
            logging: LoggingDisplayConfig::default(),
            spectrum: SpectrumDisplayConfig::default(),
            custom_widgets: HashMap::new(),
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 800,
            position_x: None,
            position_y: None,
            state: WindowState::Normal,
            decorations: true,
            resizable: true,
            transparency: 1.0,
            always_on_top: false,
            multi_monitor: MultiMonitorConfig::default(),
            session: SessionConfig::default(),
        }
    }
}

impl Default for MultiMonitorConfig {
    fn default() -> Self {
        Self {
            preferred_monitor: 0,
            span_monitors: false,
            remember_placement: true,
            monitor_scaling: HashMap::new(),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            restore_positions: true,
            restore_layouts: true,
            session_file: None,
            auto_save_interval_minutes: 5,
        }
    }
}

impl Default for TypographyConfig {
    fn default() -> Self {
        Self {
            font_family: "Inter".to_string(),
            font_size: 12.0,
            monospace_font: "Fira Code".to_string(),
            monospace_size: 11.0,
            font_weights: FontWeightConfig::default(),
            rendering: TextRenderingConfig::default(),
            scaling: FontScalingConfig::default(),
        }
    }
}

impl Default for FontWeightConfig {
    fn default() -> Self {
        Self {
            normal: 400,
            bold: 600,
            light: 300,
            heavy: 700,
        }
    }
}

impl Default for TextRenderingConfig {
    fn default() -> Self {
        Self {
            antialiasing: AntialiasingMode::Subpixel,
            subpixel_rendering: true,
            hinting: HintingMode::Medium,
            line_height: 1.4,
            letter_spacing: 0.0,
        }
    }
}

impl Default for FontScalingConfig {
    fn default() -> Self {
        Self {
            ui_scale: 1.0,
            log_scale: 0.9,
            button_scale: 1.0,
            menu_scale: 1.0,
            tooltip_scale: 0.9,
        }
    }
}

impl Default for ColorSchemeConfig {
    fn default() -> Self {
        Self {
            primary: ColorPalette {
                base: "#3b82f6".to_string(),
                light: "#60a5fa".to_string(),
                dark: "#1d4ed8".to_string(),
                contrast: "#ffffff".to_string(),
            },
            secondary: ColorPalette {
                base: "#6b7280".to_string(),
                light: "#9ca3af".to_string(),
                dark: "#374151".to_string(),
                contrast: "#ffffff".to_string(),
            },
            background: BackgroundColors {
                primary: "#ffffff".to_string(),
                secondary: "#f9fafb".to_string(),
                panel: "#f3f4f6".to_string(),
                input: "#ffffff".to_string(),
                selected: "#dbeafe".to_string(),
                hover: "#f0f9ff".to_string(),
            },
            text: TextColors {
                primary: "#111827".to_string(),
                secondary: "#6b7280".to_string(),
                disabled: "#d1d5db".to_string(),
                link: "#3b82f6".to_string(),
                selected: "#1e40af".to_string(),
                placeholder: "#9ca3af".to_string(),
            },
            borders: BorderColors {
                normal: "#d1d5db".to_string(),
                focus: "#3b82f6".to_string(),
                error: "#ef4444".to_string(),
                success: "#10b981".to_string(),
                warning: "#f59e0b".to_string(),
            },
            status: StatusColors {
                success: "#10b981".to_string(),
                warning: "#f59e0b".to_string(),
                error: "#ef4444".to_string(),
                info: "#3b82f6".to_string(),
                connected: "#10b981".to_string(),
                disconnected: "#6b7280".to_string(),
                transmitting: "#ef4444".to_string(),
                receiving: "#10b981".to_string(),
            },
            spectrum: SpectrumColors {
                spectrum_line: "#3b82f6".to_string(),
                waterfall_gradient: vec![
                    "#000080".to_string(),
                    "#0000ff".to_string(),
                    "#00ffff".to_string(),
                    "#ffff00".to_string(),
                    "#ff0000".to_string(),
                ],
                grid_lines: "#e5e7eb".to_string(),
                frequency_markers: "#6b7280".to_string(),
                signal_detection: "#ef4444".to_string(),
                background_noise: "#374151".to_string(),
            },
            custom_colors: HashMap::new(),
        }
    }
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            available_panels: vec![
                PanelDefinition {
                    id: "spectrum".to_string(),
                    name: "Spectrum".to_string(),
                    panel_type: PanelType::Spectrum,
                    default_position: PanelPosition::Center,
                    default_size: PanelSize {
                        width: SizeSpec::Percentage(70.0),
                        height: SizeSpec::Percentage(60.0),
                        min_width: Some(400),
                        min_height: Some(300),
                        max_width: None,
                        max_height: None,
                    },
                    closeable: false,
                    moveable: true,
                    resizable: true,
                },
                PanelDefinition {
                    id: "logging".to_string(),
                    name: "Logging".to_string(),
                    panel_type: PanelType::Logging,
                    default_position: PanelPosition::Bottom,
                    default_size: PanelSize {
                        width: SizeSpec::Percentage(100.0),
                        height: SizeSpec::Percentage(30.0),
                        min_width: Some(200),
                        min_height: Some(100),
                        max_width: None,
                        max_height: None,
                    },
                    closeable: true,
                    moveable: true,
                    resizable: true,
                },
            ],
            default_layout: "standard".to_string(),
            docking: DockingConfig::default(),
            sizing: PanelSizingConfig::default(),
            visibility: PanelVisibilityConfig::default(),
        }
    }
}

impl Default for DockingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sensitivity: 20,
            show_guides: true,
            snap_to_edges: true,
            zones: vec![],
        }
    }
}

impl Default for PanelSizingConfig {
    fn default() -> Self {
        Self {
            spacing: 4,
            border_width: 1,
            title_bar_height: 24,
            splitter_width: 4,
            tab_height: 28,
        }
    }
}

impl Default for PanelVisibilityConfig {
    fn default() -> Self {
        Self {
            initial_panels: vec!["spectrum".to_string(), "logging".to_string()],
            auto_hide: false,
            auto_hide_delay_ms: 2000,
            show_titles: true,
            show_close_buttons: true,
        }
    }
}

impl Default for KeyboardNavigationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_focus: true,
            focus_ring_width: 2,
            focus_ring_color: "#3b82f6".to_string(),
            tab_order: TabOrderConfig::Standard,
        }
    }
}

impl Default for VisualIndicatorConfig {
    fn default() -> Self {
        Self {
            show_status: true,
            color_coding: true,
            show_progress: true,
            change_animation: true,
            highlight_active: true,
        }
    }
}

impl Default for SoundFeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            button_clicks: false,
            alerts: true,
            status_changes: true,
            volume: 0.5,
        }
    }
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_scale: 1.0,
            easing: EasingConfig::default(),
            transitions: TransitionConfig::default(),
            performance: AnimationPerformanceConfig::default(),
        }
    }
}

impl Default for EasingConfig {
    fn default() -> Self {
        Self {
            default: EasingFunction::EaseInOut,
            button_hover: EasingFunction::EaseOut,
            panel_transition: EasingFunction::EaseInOut,
            fade: EasingFunction::Linear,
        }
    }
}

impl Default for TransitionConfig {
    fn default() -> Self {
        Self {
            fade_duration_ms: 200,
            slide_duration_ms: 300,
            scale_duration_ms: 200,
            rotation_duration_ms: 300,
            color_duration_ms: 150,
        }
    }
}

impl Default for AnimationPerformanceConfig {
    fn default() -> Self {
        Self {
            hardware_acceleration: true,
            max_fps: 60,
            adaptive_quality: true,
            performance_monitoring: false,
        }
    }
}

impl Default for ToolbarVisibilityConfig {
    fn default() -> Self {
        Self {
            main_toolbar: true,
            status_toolbar: true,
            formatting_toolbar: false,
            auto_hide: false,
            lock_positions: false,
        }
    }
}

impl Default for ToolbarCustomizationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_add_remove: true,
            allow_reorder: true,
            allow_new_toolbars: false,
            save_customizations: true,
        }
    }
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            visible: true,
            position: StatusBarPosition::Bottom,
            items: vec![
                StatusBarItem {
                    id: "frequency".to_string(),
                    item_type: StatusBarItemType::Frequency,
                    format: Some("{:.3} MHz".to_string()),
                    width: Some(100),
                    alignment: ItemAlignment::Left,
                    priority: 10,
                    visible: true,
                },
                StatusBarItem {
                    id: "mode".to_string(),
                    item_type: StatusBarItemType::Mode,
                    format: None,
                    width: Some(60),
                    alignment: ItemAlignment::Left,
                    priority: 9,
                    visible: true,
                },
                StatusBarItem {
                    id: "connection".to_string(),
                    item_type: StatusBarItemType::Connection,
                    format: None,
                    width: Some(80),
                    alignment: ItemAlignment::Right,
                    priority: 8,
                    visible: true,
                },
            ],
            update_interval_ms: 500,
            show_progress: true,
        }
    }
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        let mut shortcuts = HashMap::new();

        // Add default shortcuts
        shortcuts.insert(
            "quit".to_string(),
            KeyboardShortcut {
                keys: "Ctrl+Q".to_string(),
                action: "application.quit".to_string(),
                description: "Quit application".to_string(),
                context: ShortcutContext::Global,
                enabled: true,
            },
        );

        shortcuts.insert(
            "connect".to_string(),
            KeyboardShortcut {
                keys: "Ctrl+Shift+C".to_string(),
                action: "rig.connect".to_string(),
                description: "Connect to rig".to_string(),
                context: ShortcutContext::MainWindow,
                enabled: true,
            },
        );

        Self {
            shortcuts,
            global_shortcuts: false,
            conflict_resolution: ConflictResolution::WarnUser,
            show_hints: true,
        }
    }
}

impl Default for LoggingDisplayConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            entry_format: "[{timestamp}] {level}: {message}".to_string(),
            show_timestamps: true,
            timestamp_format: "%H:%M:%S".to_string(),
            color_by_level: true,
            auto_scroll: true,
            filtering: LogFilterConfig::default(),
        }
    }
}

impl Default for LogFilterConfig {
    fn default() -> Self {
        Self {
            min_level: LogLevel::Info,
            category_filters: vec![],
            text_filter: None,
            recent_only: false,
            recent_window_minutes: 60,
        }
    }
}

impl Default for SpectrumDisplayConfig {
    fn default() -> Self {
        Self {
            mode: SpectrumMode::Combined,
            fft_size: 2048,
            averaging: 0.8,
            update_rate: 30.0,
            waterfall: WaterfallConfig::default(),
            frequency_axis: FrequencyAxisConfig::default(),
            amplitude_axis: AmplitudeAxisConfig::default(),
            grid: GridConfig::default(),
            markers: MarkerConfig::default(),
        }
    }
}

impl Default for WaterfallConfig {
    fn default() -> Self {
        Self {
            height: 200,
            scroll_speed: 50.0,
            color_mapping: ColorMapping::Heat,
            contrast: 1.0,
            brightness: 0.0,
        }
    }
}

impl Default for FrequencyAxisConfig {
    fn default() -> Self {
        Self {
            visible: true,
            units: FrequencyUnits::Mhz,
            major_ticks: 1.0,
            minor_ticks: 0.1,
            label_format: "{:.1}".to_string(),
        }
    }
}

impl Default for AmplitudeAxisConfig {
    fn default() -> Self {
        Self {
            visible: true,
            units: AmplitudeUnits::Dbm,
            scale: AmplitudeScale::Logarithmic,
            reference_level: -30.0,
            dynamic_range: 80.0,
        }
    }
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            visible: true,
            major_color: "#e5e7eb".to_string(),
            minor_color: "#f3f4f6".to_string(),
            line_width: 1.0,
            line_style: LineStyle::Solid,
        }
    }
}

impl Default for MarkerConfig {
    fn default() -> Self {
        Self {
            visible: true,
            color: "#ef4444".to_string(),
            width: 2.0,
            style: MarkerStyle::Line,
            show_labels: true,
        }
    }
}

impl ConfigSection for UiConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // Validate window dimensions
        if self.window.width == 0 || self.window.height == 0 {
            return Err(ConfigError::InvalidValue {
                field: "window.dimensions".to_string(),
                value: format!("{}x{}", self.window.width, self.window.height),
            });
        }

        // Validate transparency
        if self.window.transparency < 0.0 || self.window.transparency > 1.0 {
            return Err(ConfigError::InvalidValue {
                field: "window.transparency".to_string(),
                value: self.window.transparency.to_string(),
            });
        }

        // Validate font sizes
        if self.typography.font_size <= 0.0 || self.typography.monospace_size <= 0.0 {
            return Err(ConfigError::InvalidValue {
                field: "typography.font_size".to_string(),
                value: format!(
                    "{}/{}",
                    self.typography.font_size, self.typography.monospace_size
                ),
            });
        }

        // Validate spectrum settings
        if !self.spectrum.fft_size.is_power_of_two() || self.spectrum.fft_size < 128 {
            return Err(ConfigError::InvalidValue {
                field: "spectrum.fft_size".to_string(),
                value: self.spectrum.fft_size.to_string(),
            });
        }

        if self.spectrum.averaging < 0.0 || self.spectrum.averaging > 1.0 {
            return Err(ConfigError::InvalidValue {
                field: "spectrum.averaging".to_string(),
                value: self.spectrum.averaging.to_string(),
            });
        }

        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        // Always take the other value — only skip empty/zero
        if !other.theme.is_empty() {
            self.theme = other.theme;
        }

        if !other.layout.is_empty() {
            self.layout = other.layout;
        }

        if other.window.width != 0 {
            self.window.width = other.window.width;
        }
        if other.window.height != 0 {
            self.window.height = other.window.height;
        }

        // Merge custom widgets
        self.custom_widgets.extend(other.custom_widgets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_ui_config() {
        let config = UiConfig::default();
        assert_eq!(config.theme, "default");
        assert_eq!(config.layout, "standard");
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_window_validation() {
        let mut config = UiConfig::default();

        // Valid window dimensions
        assert!(config.validate_section().is_ok());

        // Invalid window dimensions
        config.window.width = 0;
        assert!(config.validate_section().is_err());

        // Invalid transparency
        config.window.width = 1200; // Reset to valid
        config.window.transparency = 1.5;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_spectrum_validation() {
        let mut config = UiConfig::default();

        // Valid FFT size
        config.spectrum.fft_size = 2048;
        assert!(config.validate_section().is_ok());

        // Invalid FFT size (not power of 2)
        config.spectrum.fft_size = 2000;
        assert!(config.validate_section().is_err());

        // Invalid averaging
        config.spectrum.fft_size = 2048; // Reset to valid
        config.spectrum.averaging = 1.5;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_keyboard_shortcuts() {
        let config = UiConfig::default();
        assert!(config.keyboard.shortcuts.contains_key("quit"));
        assert!(config.keyboard.shortcuts.contains_key("connect"));
    }

    #[test]
    fn test_color_scheme() {
        let config = UiConfig::default();
        assert_eq!(config.colors.primary.base, "#3b82f6");
        assert_eq!(config.colors.status.connected, "#10b981");
        assert_eq!(config.colors.status.transmitting, "#ef4444");
    }
}

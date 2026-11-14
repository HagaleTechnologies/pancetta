//! A lexical floor for config consumers, not proof of runtime consumption:
//! an unrelated word with the same leaf name also passes. Definitions in the
//! config crate, integration tests, and source after the first cfg(test) are
//! excluded. Default serialization cannot enumerate absent optional values or
//! user-defined map entries. Existing debt is explicit and cannot grow.
use pancetta_config::Config;
use std::collections::BTreeSet;
use std::path::Path;

const WAIVERS: &[(&str, &str)] = &[
    ("audio.bit_depth", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.meter_ballistics.hold_time_ms", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.meter_ballistics.peak_attack_ms", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.meter_ballistics.peak_decay_ms", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.meter_ballistics.rms_averaging_ms", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.output_gain_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.peak_clip_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.peak_warning_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.levels.rms_target_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.noise_reduction.noise_gate_threshold_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.noise_reduction.wiener_filter.estimation_window_ms", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.noise_reduction.wiener_filter.min_gain", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.noise_reduction.wiener_filter.smoothing_factor", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.bandwidth.bandwidth_hz", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.bandwidth.center_frequency", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.compression.attack_ms", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.compression.knee_width_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.compression.makeup_gain_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.compression.release_ms", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.emphasis.de_emphasis_enabled", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.emphasis.pre_emphasis_enabled", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.emphasis.time_constant_us", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.equalizer.global_gain_db", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.processing.highpass_cutoff", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.recording.bit_depth", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.recording.compression_level", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.recording.filename_pattern", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.recording.max_length_minutes", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.routing.input_routing", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("audio.routing.output_routing", "Pre-existing audio schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("autonomous.unattended_policy", "Pre-existing autonomous schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("fox.max_streams", "Pre-existing fox schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.cqdx.authorizations_poll_interval_secs", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.dx_cluster.connection.auto_connect", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.dx_cluster.connection.keepalive_interval_seconds", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.dx_cluster.connection.reconnect_attempts", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.lotw.station_location", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.lotw.tqsl_path", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.batch_size", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.enabled_bands", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.geographic.distance.great_circle", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.geographic.exclude_cq_zones", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.geographic.exclude_dxcc", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.geographic.exclude_itu_zones", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.geographic.include_cq_zones", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.geographic.include_dxcc", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.filters.geographic.include_itu_zones", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.frequency_accuracy_hz", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.include_receives", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.include_transmits", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.max_age_hours", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.min_snr_db", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.psk_reporter.server_url", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.reliability.circuit_breaker.failure_threshold", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.reliability.circuit_breaker.success_threshold", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.reliability.connection_timeout_seconds", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.reliability.health_check.interval_seconds", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.reliability.health_check.unhealthy_threshold", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.reliability.request_timeout_seconds", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("network.remote_gateway.allow_network_exposure", "Pre-existing network schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.antenna_switching.antennas", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.antenna_switching.auto_rules", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.antenna_switching.switching_delay_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.band_switching.auto_switching", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.band_switching.switching_delay_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.frequency.memory_channels.auto_save", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.frequency.memory_channels.quick_memories", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.frequency.polling_interval_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.interface.data_bits", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.interface.flow_control", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.interface.response_timeout_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.interface.stop_bits", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.modes.filters.available_widths", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.modes.filters.custom_filters", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.modes.supported_modes", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.power_control.default_level", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.power_control.protection.max_temperature_c", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.power_control.protection.temperature_protection", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.power_control.ramping.ramp_down_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.power_control.ramping.ramp_up_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.ptt.polarity", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.ptt.tx_delay_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.ptt.tx_tail_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.ptt.vox.anti_vox", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.ptt.vox.delay_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.rig_parameters.calibration.s_meter_calibration", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.rig_parameters.calibration.swr_calibration", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.rig_parameters.extended_features", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.rig_parameters.quirks.workarounds", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.timing.command_interval_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.timing.keepalive_interval_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.timing.response_timeout_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.timing.retry_interval_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("rig.timing.status_polling_ms", "Pre-existing rig schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("station.antennas", "Pre-existing station schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("tx_placement.auto_repark", "Pre-existing tx_placement schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("tx_placement.repark_min_score_gain", "Pre-existing tx_placement schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.high_contrast", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.keyboard_navigation.focus_ring_color", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.keyboard_navigation.focus_ring_width", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.keyboard_navigation.show_focus", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.keyboard_navigation.tab_order", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.large_fonts", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.motion_sensitivity.disable_auto_scroll", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.motion_sensitivity.disable_parallax", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.motion_sensitivity.reduce_animations", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.motion_sensitivity.static_backgrounds", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.screen_reader", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.visual_indicators.change_animation", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.visual_indicators.color_coding", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.visual_indicators.highlight_active", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.visual_indicators.show_progress", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.accessibility.visual_indicators.show_status", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.duration_scale", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.easing.button_hover", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.easing.panel_transition", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.performance.adaptive_quality", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.performance.hardware_acceleration", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.performance.max_fps", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.performance.performance_monitoring", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.transitions.color_duration_ms", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.transitions.fade_duration_ms", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.transitions.rotation_duration_ms", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.transitions.scale_duration_ms", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.animations.transitions.slide_duration_ms", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.colors.background.hover", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.colors.spectrum.background_noise", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.colors.spectrum.frequency_markers", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.colors.spectrum.grid_lines", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.colors.spectrum.signal_detection", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.colors.spectrum.spectrum_line", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.colors.spectrum.waterfall_gradient", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.auto_scroll", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.color_by_level", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.entry_format", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.filtering.category_filters", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.filtering.min_level", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.filtering.recent_only", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.filtering.recent_window_minutes", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.max_entries", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.show_timestamps", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.logging.timestamp_format", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.available_panels", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.default_layout", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.docking.show_guides", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.docking.snap_to_edges", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.sizing.border_width", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.sizing.splitter_width", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.sizing.tab_height", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.sizing.title_bar_height", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.visibility.auto_hide", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.visibility.auto_hide_delay_ms", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.visibility.initial_panels", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.visibility.show_close_buttons", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.panels.visibility.show_titles", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.amplitude_axis.dynamic_range", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.amplitude_axis.reference_level", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.frequency_axis.label_format", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.frequency_axis.major_ticks", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.frequency_axis.minor_ticks", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.grid.line_style", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.grid.line_width", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.grid.major_color", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.grid.minor_color", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.markers.show_labels", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.update_rate", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.waterfall.color_mapping", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.spectrum.waterfall.scroll_speed", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.status_bar.show_progress", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.status_bar.update_interval_ms", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.customization.allow_add_remove", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.customization.allow_new_toolbars", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.customization.allow_reorder", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.customization.save_customizations", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.toolbars", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.visibility.auto_hide", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.visibility.formatting_toolbar", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.visibility.lock_positions", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.visibility.main_toolbar", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.toolbars.visibility.status_toolbar", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.font_family", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.font_size", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.monospace_font", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.monospace_size", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.rendering.antialiasing", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.rendering.hinting", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.rendering.letter_spacing", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.rendering.line_height", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.rendering.subpixel_rendering", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.scaling.button_scale", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.scaling.log_scale", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.scaling.menu_scale", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.scaling.tooltip_scale", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.typography.scaling.ui_scale", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.always_on_top", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.decorations", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.multi_monitor.preferred_monitor", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.multi_monitor.remember_placement", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.multi_monitor.span_monitors", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.resizable", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.session.auto_save_interval_minutes", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.session.restore_layouts", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("ui.window.session.restore_positions", "Pre-existing ui schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
    ("watchlist.ttl_secs", "Pre-existing watchlist schema debt or reference omitted by the conservative cfg(test) cutoff; outside PAN-303 removals"),
];
// PAN-303 baseline. This ceiling may only decrease.
const MAX_WAIVERS: usize = 204;

fn collect_words(path: &Path, words: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if !matches!(
                path.file_name().unwrap().to_str().unwrap(),
                "tests" | "target" | "thoughts"
            ) {
                collect_words(&path, words);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            let production = text.split("#[cfg(test)]").next().unwrap();
            words.extend(
                production
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|word| !word.is_empty())
                    .map(str::to_owned),
            );
        }
    }
}

fn leaf_paths(table: &toml::Table, prefix: &str, paths: &mut Vec<String>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if let toml::Value::Table(table) = value {
            leaf_paths(table, &path, paths);
        } else {
            paths.push(path);
        }
    }
}

#[test]
fn every_serialized_key_has_a_reference_or_waiver() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut words = BTreeSet::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() != "pancetta-config"
            && path.join("Cargo.toml").is_file()
            && path.join("src").is_dir()
        {
            collect_words(&path.join("src"), &mut words);
        }
    }
    assert!(!words.is_empty(), "runtime corpus must not be empty");
    let table: toml::Table = Config::defaults_toml().parse().unwrap();
    let mut paths = Vec::new();
    leaf_paths(&table, "", &mut paths);
    assert!(
        paths.len() > 400,
        "schema walker must cover the generated config"
    );
    let missing: Vec<_> = paths
        .iter()
        .filter(|path| {
            !words.contains(path.rsplit('.').next().unwrap())
                && !WAIVERS.iter().any(|(waiver, _)| waiver == path)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "unconsumed keys need a runtime reference or explicit debt waiver: {missing:#?}"
    );
    for (path, reason) in WAIVERS {
        assert!(!reason.trim().is_empty(), "{path}: missing reason");
        assert!(paths.iter().any(|p| p == path), "stale waiver: {path}");
    }
}

#[test]
fn waiver_list_never_grows() {
    assert!(
        WAIVERS.len() <= MAX_WAIVERS,
        "remove debt instead of expanding the waiver list"
    );
    let unique: BTreeSet<_> = WAIVERS.iter().map(|(path, _)| *path).collect();
    assert_eq!(unique.len(), WAIVERS.len(), "duplicate waivers");
}

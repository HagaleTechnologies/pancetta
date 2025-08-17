//! Enhanced logging and diagnostics for Pancetta

use anyhow::Result;
use std::path::PathBuf;
use tracing::{Level, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter, Layer, Registry,
};

/// Logging configuration
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Log level filter (e.g., "info", "debug", "trace")
    pub level: String,
    
    /// Enable file logging
    pub file_logging: bool,
    
    /// Log file directory
    pub log_dir: PathBuf,
    
    /// Enable JSON formatting
    pub json_format: bool,
    
    /// Enable color output
    pub use_color: bool,
    
    /// Enable thread IDs in logs
    pub show_thread_ids: bool,
    
    /// Enable span events
    pub span_events: bool,
    
    /// Maximum log file size in MB
    pub max_file_size_mb: u64,
    
    /// Number of log files to keep
    pub max_files: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            file_logging: false,
            log_dir: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("pancetta")
                .join("logs"),
            json_format: false,
            use_color: true,
            show_thread_ids: true,
            span_events: false,
            max_file_size_mb: 10,
            max_files: 5,
        }
    }
}

/// Initialize logging system
pub fn init_logging(config: LogConfig) -> Result<Option<WorkerGuard>> {
    // Create environment filter
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.level));
    
    // Console layer for terminal output
    let console_layer = if config.json_format {
        fmt::layer()
            .json()
            .with_thread_ids(config.show_thread_ids)
            .with_thread_names(true)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .boxed()
    } else {
        let layer = fmt::layer()
            .with_thread_ids(config.show_thread_ids)
            .with_thread_names(true)
            .with_target(false)
            .with_file(false)
            .with_line_number(false)
            .with_ansi(config.use_color);
        
        if config.span_events {
            layer.with_span_events(FmtSpan::FULL).boxed()
        } else {
            layer.boxed()
        }
    };
    
    // File layer for persistent logging
    let (file_layer, guard) = if config.file_logging {
        // Create log directory
        std::fs::create_dir_all(&config.log_dir)?;
        
        // Create rotating file appender
        let file_appender = tracing_appender::rolling::daily(
            config.log_dir,
            "pancetta.log"
        );
        
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        
        let file_layer = fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_current_span(true)
            .with_span_list(true);
        
        (Some(file_layer.boxed()), Some(guard))
    } else {
        (None, None)
    };
    
    // Build subscriber
    let subscriber = Registry::default()
        .with(env_filter)
        .with(console_layer);
    
    if let Some(file_layer) = file_layer {
        subscriber.with(file_layer).init();
    } else {
        subscriber.init();
    }
    
    Ok(guard)
}

/// Log a performance metric
#[macro_export]
macro_rules! log_metric {
    ($name:expr, $value:expr) => {
        tracing::debug!(
            metric = $name,
            value = $value,
            "Performance metric"
        );
    };
    ($name:expr, $value:expr, $unit:expr) => {
        tracing::debug!(
            metric = $name,
            value = $value,
            unit = $unit,
            "Performance metric"
        );
    };
}

/// Log a component state change
#[macro_export]
macro_rules! log_state_change {
    ($component:expr, $old_state:expr, $new_state:expr) => {
        tracing::info!(
            component = $component,
            old_state = ?$old_state,
            new_state = ?$new_state,
            "Component state changed"
        );
    };
}

/// Log an error with context
#[macro_export]
macro_rules! log_error_context {
    ($error:expr, $context:expr) => {
        tracing::error!(
            error = %$error,
            context = $context,
            "Error occurred"
        );
    };
}

/// Diagnostic information collector
pub struct DiagnosticInfo {
    pub uptime: std::time::Duration,
    pub memory_usage_mb: u64,
    pub cpu_usage_percent: f32,
    pub active_threads: usize,
    pub message_queue_depth: usize,
    pub audio_buffer_usage: f32,
    pub decode_success_rate: f32,
    pub last_error: Option<String>,
}

impl DiagnosticInfo {
    /// Log diagnostic information
    pub fn log(&self) {
        tracing::info!(
            uptime_secs = self.uptime.as_secs(),
            memory_mb = self.memory_usage_mb,
            cpu_percent = self.cpu_usage_percent,
            threads = self.active_threads,
            queue_depth = self.message_queue_depth,
            audio_buffer = self.audio_buffer_usage,
            decode_rate = self.decode_success_rate,
            last_error = ?self.last_error,
            "System diagnostics"
        );
    }
    
    /// Generate diagnostic report
    pub fn report(&self) -> String {
        format!(
            "=== Pancetta Diagnostic Report ===\n\
             Uptime: {:?}\n\
             Memory Usage: {} MB\n\
             CPU Usage: {:.1}%\n\
             Active Threads: {}\n\
             Message Queue: {} messages\n\
             Audio Buffer: {:.1}%\n\
             Decode Success: {:.1}%\n\
             Last Error: {}\n",
            self.uptime,
            self.memory_usage_mb,
            self.cpu_usage_percent,
            self.active_threads,
            self.message_queue_depth,
            self.audio_buffer_usage * 100.0,
            self.decode_success_rate * 100.0,
            self.last_error.as_deref().unwrap_or("None")
        )
    }
}

/// Create a span for timing operations
#[macro_export]
macro_rules! timed_operation {
    ($name:expr, $code:block) => {{
        let _span = tracing::debug_span!("timed", operation = $name).entered();
        let _start = std::time::Instant::now();
        let result = $code;
        let _elapsed = _start.elapsed();
        tracing::debug!(
            operation = $name,
            elapsed_ms = _elapsed.as_millis() as u64,
            "Operation completed"
        );
        result
    }};
}
//! Message bus integration for audio processing
//!
//! Provides integration between the audio processor and the Pancetta message bus
//! for coordinated operation with other components.

use crate::{
    error::{AudioError, AudioResult},
    processor::{AudioProcessingStats, AudioProcessor},
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, error, info};

// Simplified message bus types for audio integration

/// Simplified message bus types for audio integration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentId {
    Audio,
    Dsp,
    Ft8Decoder,
    Tui,
    Coordinator,
}

#[derive(Debug, Clone)]
pub enum MessageType {
    AudioData(Vec<f32>),
    AudioStats(AudioProcessingStats),
    ControlMessage(ControlMessage),
    HealthCheck,
}

#[derive(Debug, Clone)]
pub enum ControlMessage {
    Start,
    Stop,
    Pause,
    Resume,
    StatusRequest,
    ConfigUpdate,
}

#[derive(Debug, Clone)]
pub struct ComponentMessage {
    pub source: ComponentId,
    pub destination: ComponentId,
    pub message_type: MessageType,
    pub timestamp: Instant,
    pub priority: u8,
}

/// Audio component integration with message bus
pub struct AudioMessageBusIntegration {
    audio_processor: Arc<RwLock<AudioProcessor>>,
    message_sender: Option<tokio::sync::mpsc::UnboundedSender<ComponentMessage>>,
    message_receiver: tokio::sync::mpsc::UnboundedReceiver<ComponentMessage>,
    component_id: ComponentId,
    is_running: Arc<RwLock<bool>>,
    health_check_interval: Duration,
    stats_broadcast_interval: Duration,
}

impl AudioMessageBusIntegration {
    /// Create new audio message bus integration
    pub fn new(
        audio_processor: AudioProcessor,
        health_check_interval: Duration,
        stats_broadcast_interval: Duration,
    ) -> (Self, tokio::sync::mpsc::UnboundedSender<ComponentMessage>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (outbound_tx, _) = tokio::sync::mpsc::unbounded_channel();

        let integration = Self {
            audio_processor: Arc::new(RwLock::new(audio_processor)),
            message_sender: Some(outbound_tx),
            message_receiver: rx,
            component_id: ComponentId::Audio,
            is_running: Arc::new(RwLock::new(false)),
            health_check_interval,
            stats_broadcast_interval,
        };

        (integration, tx)
    }

    /// Start the message bus integration
    pub async fn start(&mut self) -> AudioResult<()> {
        let mut is_running = self.is_running.write().await;
        if *is_running {
            return Err(AudioError::system("Audio integration already running"));
        }

        info!("Starting audio message bus integration");

        // Start the audio processor
        {
            let mut processor = self.audio_processor.write().await;
            processor.start().await?;
        }

        *is_running = true;
        drop(is_running);

        // Start message processing tasks
        self.start_message_handler().await;
        self.start_audio_data_publisher().await;
        self.start_health_monitor().await;
        self.start_stats_broadcaster().await;

        info!("Audio message bus integration started");
        Ok(())
    }

    /// Stop the message bus integration
    pub async fn stop(&mut self) -> AudioResult<()> {
        let mut is_running = self.is_running.write().await;
        if !*is_running {
            return Ok(());
        }

        info!("Stopping audio message bus integration");

        // Stop the audio processor
        {
            let mut processor = self.audio_processor.write().await;
            processor.stop().await?;
        }

        *is_running = false;
        info!("Audio message bus integration stopped");
        Ok(())
    }

    /// Check if the integration is running
    pub async fn is_running(&self) -> bool {
        *self.is_running.read().await
    }

    /// Send a message to the message bus
    async fn send_message(&self, message: ComponentMessage) {
        if let Some(ref sender) = self.message_sender {
            if let Err(e) = sender.send(message) {
                error!("Failed to send message to bus: {}", e);
            }
        }
    }

    /// Start the message handler task
    async fn start_message_handler(&mut self) {
        let _processor = self.audio_processor.clone();
        let is_running = self.is_running.clone();
        let _component_id = self.component_id;

        // Simplified version to avoid Send/Sync issues for now
        tokio::spawn(async move {
            info!("Audio message handler started");

            let mut interval_timer = interval(Duration::from_millis(100));

            while *is_running.read().await {
                interval_timer.tick().await;
                // Handle any incoming control messages
                // In a real implementation, this would process messages from the receiver
            }

            info!("Audio message handler stopped");
        });
    }

    /// Start the audio data publisher task
    async fn start_audio_data_publisher(&self) {
        let _processor = self.audio_processor.clone();
        let is_running = self.is_running.clone();
        let _sender = self.message_sender.clone();

        // Simplified version to avoid Send/Sync issues for now
        tokio::spawn(async move {
            info!("Audio data publisher started");

            let mut publish_interval = interval(Duration::from_millis(10));

            while *is_running.read().await {
                publish_interval.tick().await;
                // In a real implementation, this would get and publish audio samples
            }

            info!("Audio data publisher stopped");
        });
    }

    /// Start the health monitor task
    async fn start_health_monitor(&self) {
        let _processor = self.audio_processor.clone();
        let is_running = self.is_running.clone();
        let _sender = self.message_sender.clone();
        let health_interval = self.health_check_interval;

        // Simplified version to avoid Send/Sync issues for now
        tokio::spawn(async move {
            info!("Audio health monitor started");

            let mut health_timer = interval(health_interval);

            while *is_running.read().await {
                health_timer.tick().await;
                // In a real implementation, this would check processor health
                debug!("Audio processor health check (simplified)");
            }

            info!("Audio health monitor stopped");
        });
    }

    /// Start the statistics broadcaster task
    async fn start_stats_broadcaster(&self) {
        let _processor = self.audio_processor.clone();
        let is_running = self.is_running.clone();
        let _sender = self.message_sender.clone();
        let stats_interval = self.stats_broadcast_interval;

        // Simplified version to avoid Send/Sync issues for now
        tokio::spawn(async move {
            info!("Audio stats broadcaster started");

            let mut stats_timer = interval(stats_interval);

            while *is_running.read().await {
                stats_timer.tick().await;
                // In a real implementation, this would broadcast statistics
                debug!("Audio stats broadcast (simplified)");
            }

            info!("Audio stats broadcaster stopped");
        });
    }

    /// Handle incoming control messages
    async fn handle_control_message(&self, message: ControlMessage) -> AudioResult<()> {
        match message {
            ControlMessage::Start => {
                let mut processor = self.audio_processor.write().await;
                processor.start().await?;
                info!("Audio processor started via control message");
            }
            ControlMessage::Stop => {
                let mut processor = self.audio_processor.write().await;
                processor.stop().await?;
                info!("Audio processor stopped via control message");
            }
            ControlMessage::Pause => {
                // For now, pause means stop
                let mut processor = self.audio_processor.write().await;
                processor.stop().await?;
                info!("Audio processor paused via control message");
            }
            ControlMessage::Resume => {
                // For now, resume means start
                let mut processor = self.audio_processor.write().await;
                processor.start().await?;
                info!("Audio processor resumed via control message");
            }
            ControlMessage::StatusRequest => {
                // Send current status
                let stats = {
                    let processor = self.audio_processor.read().await;
                    processor.get_statistics().await
                };

                let status_message = ComponentMessage {
                    source: ComponentId::Audio,
                    destination: ComponentId::Coordinator,
                    message_type: MessageType::AudioStats(stats),
                    timestamp: Instant::now(),
                    priority: 64, // Medium priority for status
                };

                self.send_message(status_message).await;
            }
            ControlMessage::ConfigUpdate => {
                info!("Configuration update requested - would reload config");
                // In a real implementation, this would reload configuration
            }
        }

        Ok(())
    }

    /// Get integration statistics
    pub async fn get_integration_stats(&self) -> IntegrationStats {
        let processor_stats = {
            let processor = self.audio_processor.read().await;
            processor.get_statistics().await
        };

        IntegrationStats {
            is_running: *self.is_running.read().await,
            processor_stats,
            health_check_interval: self.health_check_interval,
            stats_broadcast_interval: self.stats_broadcast_interval,
            component_id: self.component_id,
        }
    }
}

/// Integration statistics
#[derive(Debug, Clone)]
pub struct IntegrationStats {
    pub is_running: bool,
    pub processor_stats: AudioProcessingStats,
    pub health_check_interval: Duration,
    pub stats_broadcast_interval: Duration,
    pub component_id: ComponentId,
}

impl IntegrationStats {
    /// Check if the integration is healthy
    pub fn is_healthy(&self) -> bool {
        self.is_running && self.processor_stats.is_healthy
    }

    /// Get a status description
    pub fn status_description(&self) -> String {
        if !self.is_running {
            "Stopped".to_string()
        } else if self.is_healthy() {
            "Healthy".to_string()
        } else {
            "Degraded".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioProcessor, AudioProcessorConfig};

    #[tokio::test]
    async fn test_integration_creation() {
        let config = AudioProcessorConfig::for_ft8();
        let processor = AudioProcessor::new(config).await.unwrap();

        let (integration, _sender) = AudioMessageBusIntegration::new(
            processor,
            Duration::from_secs(5),
            Duration::from_secs(1),
        );

        assert!(!integration.is_running().await);
    }

    #[tokio::test]
    async fn test_integration_stats() {
        let config = AudioProcessorConfig::for_ft8();
        let processor = AudioProcessor::new(config).await.unwrap();

        let (integration, _sender) = AudioMessageBusIntegration::new(
            processor,
            Duration::from_secs(5),
            Duration::from_secs(1),
        );

        let stats = integration.get_integration_stats().await;
        assert!(!stats.is_running);
        assert_eq!(stats.component_id, ComponentId::Audio);
    }

    #[test]
    fn test_message_types() {
        let message = ComponentMessage {
            source: ComponentId::Audio,
            destination: ComponentId::Dsp,
            message_type: MessageType::AudioData(vec![0.1, 0.2, 0.3]),
            timestamp: Instant::now(),
            priority: 0,
        };

        assert_eq!(message.source, ComponentId::Audio);
        assert_eq!(message.destination, ComponentId::Dsp);
        assert_eq!(message.priority, 0);
    }
}

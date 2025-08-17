use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEvent};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, error};

use crate::app::DecodedMessage;

#[derive(Debug, Clone)]
pub enum Event {
    Tick,
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    AudioData(Vec<f32>),
    DecodedMessage(DecodedMessage),
}

pub struct EventHandler {
    tick_rate: Duration,
    last_tick: Instant,
    event_rx: mpsc::UnboundedReceiver<Event>,
    _event_tx: mpsc::UnboundedSender<Event>,
}

impl EventHandler {
    pub fn new(tick_rate: u64) -> Self {
        let tick_rate = Duration::from_millis(tick_rate);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        
        // Clone sender for the event loop
        let tx = event_tx.clone();
        
        // Spawn crossterm event handler
        tokio::spawn(async move {
            loop {
                // Check for crossterm events with timeout
                if event::poll(Duration::from_millis(10)).unwrap_or(false) {
                    match event::read() {
                        Ok(CrosstermEvent::Key(key)) => {
                            if let Err(e) = tx.send(Event::Key(key)) {
                                error!("Failed to send key event: {}", e);
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Mouse(mouse)) => {
                            if let Err(e) = tx.send(Event::Mouse(mouse)) {
                                error!("Failed to send mouse event: {}", e);
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Resize(width, height)) => {
                            if let Err(e) = tx.send(Event::Resize(width, height)) {
                                error!("Failed to send resize event: {}", e);
                                break;
                            }
                        }
                        Ok(CrosstermEvent::FocusGained) => {
                            debug!("Terminal focus gained");
                        }
                        Ok(CrosstermEvent::FocusLost) => {
                            debug!("Terminal focus lost");
                        }
                        Ok(CrosstermEvent::Paste(data)) => {
                            debug!("Paste event: {}", data);
                        }
                        Err(e) => {
                            error!("Failed to read crossterm event: {}", e);
                        }
                    }
                }
                
                // Small delay to prevent busy waiting
                sleep(Duration::from_millis(10)).await;
            }
        });

        Self {
            tick_rate,
            last_tick: Instant::now(),
            event_rx,
            _event_tx: event_tx,
        }
    }

    pub async fn next(&mut self) -> Event {
        // Calculate time until next tick
        let timeout = self.tick_rate
            .checked_sub(self.last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        // Try to receive an event within the timeout
        match tokio::time::timeout(timeout, self.event_rx.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                // Channel closed, return tick
                self.last_tick = Instant::now();
                Event::Tick
            }
            Err(_) => {
                // Timeout, send tick
                self.last_tick = Instant::now();
                Event::Tick
            }
        }
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<Event> {
        self._event_tx.clone()
    }
}

// Audio event handler for processing audio input
pub struct AudioEventHandler {
    event_tx: mpsc::UnboundedSender<Event>,
    is_running: bool,
}

impl AudioEventHandler {
    pub fn new(event_tx: mpsc::UnboundedSender<Event>) -> Self {
        Self {
            event_tx,
            is_running: false,
        }
    }

    pub async fn start_audio_processing(&mut self, device_name: Option<&str>) -> Result<()> {
        if self.is_running {
            return Ok(());
        }

        self.is_running = true;
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            // TODO: Initialize CPAL audio input stream
            // For now, simulate audio data
            let mut counter = 0u32;
            
            loop {
                // Simulate audio data generation
                let audio_data: Vec<f32> = (0..1024)
                    .map(|i| {
                        let t = (counter * 1024 + i) as f32 / 48000.0;
                        (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.1
                    })
                    .collect();

                if let Err(e) = tx.send(Event::AudioData(audio_data)) {
                    error!("Failed to send audio data: {}", e);
                    break;
                }

                counter += 1;
                sleep(Duration::from_millis(20)).await; // ~50 FPS audio updates
            }
        });

        debug!("Started audio processing for device: {:?}", device_name);
        Ok(())
    }

    pub fn stop_audio_processing(&mut self) {
        self.is_running = false;
        debug!("Stopped audio processing");
    }
}

// Decoder event handler for processing decoded messages
pub struct DecoderEventHandler {
    event_tx: mpsc::UnboundedSender<Event>,
    decoder_rx: Option<mpsc::UnboundedReceiver<DecodedMessage>>,
}

impl DecoderEventHandler {
    pub fn new(event_tx: mpsc::UnboundedSender<Event>) -> Self {
        Self {
            event_tx,
            decoder_rx: None,
        }
    }

    pub fn set_decoder_receiver(&mut self, rx: mpsc::UnboundedReceiver<DecodedMessage>) {
        self.decoder_rx = Some(rx);
    }

    pub async fn start_message_processing(&mut self) -> Result<()> {
        if let Some(mut rx) = self.decoder_rx.take() {
            let tx = self.event_tx.clone();

            tokio::spawn(async move {
                while let Some(message) = rx.recv().await {
                    if let Err(e) = tx.send(Event::DecodedMessage(message)) {
                        error!("Failed to send decoded message: {}", e);
                        break;
                    }
                }
            });

            debug!("Started message processing");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_event_handler_tick() {
        let mut handler = EventHandler::new(100); // 100ms tick rate
        
        let start = Instant::now();
        let event = handler.next().await;
        let elapsed = start.elapsed();

        match event {
            Event::Tick => {
                assert!(elapsed >= Duration::from_millis(90)); // Allow some variance
                assert!(elapsed <= Duration::from_millis(150));
            }
            _ => panic!("Expected tick event"),
        }
    }

    #[tokio::test]
    async fn test_audio_event_handler() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut audio_handler = AudioEventHandler::new(tx);
        
        audio_handler.start_audio_processing(None).await.unwrap();
        
        // Wait for audio data
        let event = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();

        match event {
            Event::AudioData(data) => {
                assert_eq!(data.len(), 1024);
            }
            _ => panic!("Expected audio data event"),
        }
    }
}
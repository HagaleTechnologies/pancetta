//! # pancetta-audio
//!
//! Real-time audio I/O via cpal with sub-millisecond latency.
//!
//! This crate provides the critical real-time audio infrastructure that must
//! achieve sub-millisecond latency for the Pancetta project to be viable.
//! It captures audio from the system sound card and pushes raw samples to
//! the DSP pipeline via a lock-free ring buffer.
//!
//! ## Data Flow
//! hardware audio device -> **pancetta-audio** -> `pancetta-dsp` (raw f32 samples via crossbeam channel)
//!
//! ## Key Types
//! - [`AudioManager`] -- top-level audio I/O lifecycle manager
//! - [`AudioManagerConfig`] -- configuration for sample rate, buffer size, device selection
//! - [`AudioCommand`] -- messages sent to the audio manager (start, stop, device change)
//! - [`AudioMessage`] -- messages emitted by the audio manager (samples, stats, errors)
//!
//! ## Crate Relationships
//! - Receives from: system audio hardware (via `cpal`)
//! - Sends to: `pancetta-dsp`

#![warn(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(dead_code, unused_imports)]

pub mod converter;
pub mod device;
pub mod error;
pub mod latency;
pub mod manager;
pub mod message_bus_integration;
pub mod processor;
pub mod realtime;
pub mod ringbuffer_comm;
pub mod stream;

pub use converter::*;
pub use device::*;
pub use error::*;
pub use latency::*;
pub use manager::{
    AudioCommand, AudioManager, AudioManagerConfig, AudioManagerStats, AudioMessage,
};
pub use message_bus_integration::*;
pub use processor::*;
pub use realtime::*;
pub use ringbuffer_comm::*;
pub use stream::*;

//! Pancetta Real-Time Audio Processing Core
//!
//! Week 0 Technical POC: Prove <1ms latency real-time audio processing
//!
//! This crate provides the critical real-time audio infrastructure that must
//! achieve sub-millisecond latency for the Pancetta project to be viable.

// #![warn(missing_docs)] // TODO: Re-enable once documentation is complete
#![deny(unsafe_op_in_unsafe_fn)]

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

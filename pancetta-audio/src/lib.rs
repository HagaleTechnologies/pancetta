//! Pancetta Real-Time Audio Processing Core
//! 
//! Week 0 Technical POC: Prove <1ms latency real-time audio processing
//! 
//! This crate provides the critical real-time audio infrastructure that must
//! achieve sub-millisecond latency for the Pancetta project to be viable.

#![warn(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod realtime;
pub mod latency;
pub mod ringbuffer_comm;
pub mod device;
pub mod stream;
pub mod error;
pub mod processor;
pub mod converter;
pub mod message_bus_integration;

pub use realtime::*;
pub use latency::*;
pub use ringbuffer_comm::*;
pub use device::*;
pub use stream::*;
pub use error::*;
pub use processor::*;
pub use converter::*;
pub use message_bus_integration::*;
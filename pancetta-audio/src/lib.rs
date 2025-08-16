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

pub use realtime::*;
pub use latency::*;
pub use ringbuffer_comm::*;
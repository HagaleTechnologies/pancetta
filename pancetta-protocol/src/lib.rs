//! Versioned, serde-serializable wire protocol for the pancetta remote API
//! (consumed by the Panino client). Fresh DTOs — decoupled from TUI/bus
//! internals; conversions live in the coordinator's remote gateway.
#![forbid(unsafe_code)]

/// Wire protocol version. Bump on any incompatible change; clients negotiate
/// it in the `Hello`/`Welcome` handshake.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod command;
pub mod dto;
pub mod event;
pub mod session;

pub use command::ClientCommand;
pub use dto::{DecodedView, DxRow, PendingCall, QsoProgress};
pub use event::ServerEvent;
pub use session::{ClientFrame, Hello, ServerFrame, StateSnapshot, Welcome};

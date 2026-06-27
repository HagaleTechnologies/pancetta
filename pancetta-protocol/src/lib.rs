//! Versioned, serde-serializable wire protocol for the pancetta remote API
//! (consumed by the Panino client). Fresh DTOs — decoupled from TUI/bus
//! internals; conversions live in the coordinator's remote gateway.
//!
//! All wire JSON uses **camelCase** field names and enum tag values per the
//! dispensa contracts/rig/rig-api.v1 schema (ADR-0003). The pancetta-core
//! enum types (`SlotParity`, `ResponseStep`, `TxPolicy`) use their own
//! internal PascalCase serialization; `wire_serde` helpers re-map them to the
//! schema's camelCase values without modifying core.
#![forbid(unsafe_code)]

/// Wire protocol version. Bump on any incompatible change; clients negotiate
/// it in the `Hello`/`Welcome` handshake.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod command;
pub mod dto;
pub mod event;
pub mod session;
pub(crate) mod wire_serde;

pub use command::ClientCommand;
pub use dto::{DecodedView, DxRow, PendingCall, QsoProgress};
pub use event::ServerEvent;
pub use session::{ClientFrame, Hello, ServerFrame, StateSnapshot, Welcome};

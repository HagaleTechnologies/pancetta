//! Read-only remote-view gateway (Panino client). Default-OFF, localhost-bound.
//! Serves the v1 read-only view (decodes + QSO progress + scalar status) to
//! WebSocket clients using `pancetta_protocol` wire types. The axum server is
//! added in a later task; this module currently exposes only the pure
//! bus→protocol translation layer.
pub(crate) mod translate;

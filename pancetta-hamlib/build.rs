// build.rs is intentionally a no-op. This file used to invoke `bindgen`
// against `wrapper.h` to generate FFI bindings to libhamlib; that path
// has been removed in favour of the `rigctld` TCP client (`src/rigctld.rs`).
// Cargo expects the file to exist if it was ever there in the package
// metadata, so we keep it as an empty stub.
fn main() {}

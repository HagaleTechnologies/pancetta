//! Severity of an operator-facing diagnostic event.
//!
//! Shared between `pancetta` (which constructs diagnostic events on the
//! `MessageType::DiagnosticEvent` bus variant) and `pancetta-tui` (which
//! renders them in the retained diagnostics history) — see
//! `docs/observability-diagnostics-plan.md`.

/// Severity of a retained, operator-facing diagnostic event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    /// Informational — a normal outcome worth recording (e.g. a QSO
    /// completed).
    Info,
    /// A drop, rejection, or degraded condition worth the operator's
    /// attention but not necessarily action.
    Warn,
    /// A failure the operator should investigate.
    Error,
}

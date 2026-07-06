//! Local append-only audit log — re-pointed onto the canonical copy in
//! `signer-core` (`hl::audit`, itself the verbatim port of this file).
//! One JSONL line per signed+submitted instruction; the user's own
//! record of what their key signed, independent of the server.

pub use degenbox_signer_core::hl::audit::{AuditEntry, AuditLog};

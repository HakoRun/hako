//! One convention for user-facing stderr diagnostics (issue #21).
//!
//! Every note, warning, or error a command prints to stderr goes through
//! [`diag!`], which prefixes it with `hako: ` — so a no-op `commit`
//! ("nothing to commit") and a workspace-not-found error read the same way,
//! instead of the old mix of bare `eprintln!`, `hako: `, and `hako serve: `.
//!
//! Structured *report* output — an `fsck` problem list, a merge-conflict
//! listing, a `diff` — is not a diagnostic; it is written directly, without the
//! prefix, so the report reads as a block.

/// Print a user-facing diagnostic to stderr as `hako: <message>`.
///
/// Takes the same format arguments as [`eprintln!`]; the `hako: ` prefix is
/// prepended. Use for single-line notes, warnings, and errors — not for
/// structured report bodies.
#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {
        eprintln!("hako: {}", format_args!($($arg)*))
    };
}

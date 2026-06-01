// Advisory rationale warnings for `#[field(outbox = "ignore")]`.
//
// Two models in the same file exercise the two cases:
//
// 1. `OutboxIgnoreWithRationale` — carries `rationale = "..."` alongside
//    `outbox = "ignore"`. No advisory warning is emitted; compilation succeeds
//    silently.
//
// 2. `OutboxIgnoreNoRationale` — carries `outbox = "ignore"` with NO rationale.
//    The macro emits a `#[deprecated]` advisory that fires as a compiler warning.
//    Compilation still succeeds (warn, not error), which is the point of this
//    compile_pass fixture — lihaaf `pass()` asserts the file compiles, not that
//    it is warning-free.
use djogi::prelude::*;

/// Model whose PII field carries a rationale alongside `outbox = "ignore"`.
/// No advisory warning is emitted for this model.
///
/// `events` is omitted intentionally: the advisory fires on the field
/// attribute alone, independent of whether the model has outbox enabled.
/// Omitting `events` avoids the `serde::Serialize` bound that `emit_event`
/// requires — keeping this fixture self-contained.
#[model(table = "audit_events_with_rationale")]
#[derive(Debug, Clone)]
pub struct OutboxIgnoreWithRationale {
    pub action: String,
    /// PII — never published to consumers outside this service.
    #[field(outbox = "ignore", rationale = "PII — never published")]
    pub user_email: String,
}

/// Model whose sensitive field omits the rationale.
/// The macro emits an advisory deprecation warning, but compilation succeeds.
///
/// Same note: `events` omitted to keep the fixture free of the
/// `serde::Serialize` bound.
#[model(table = "audit_events_no_rationale")]
#[derive(Debug, Clone)]
pub struct OutboxIgnoreNoRationale {
    pub action: String,
    /// This field lacks a rationale annotation — the macro warns.
    #[field(outbox = "ignore")]
    pub internal_token: String,
}

fn main() {}

const PHASE_ZERO_BANNER_MARKER: &str =
    "Djogi Phase 0 bootstrap — HeeRanjID + extensions + node seed";
const BASE_SCHEMA_MARKER: &str = "-- HeeRanjID base schema + functions (idempotent).";
const NODE_SEED_MARKER: &str = "-- HeeRanjID node-id GUC seed (database-level + session-level).";
const SESSION_NODE_SET_MARKER: &str = "SET heer.node_id = '";
const SESSION_RANJ_SET_MARKER: &str = "SET heer.ranj_node_id = '";
const DYNAMIC_NODE_DEFAULT_MARKER: &str = "ALTER DATABASE %I SET heer.node_id = %L";
const DYNAMIC_RANJ_DEFAULT_MARKER: &str = "ALTER DATABASE %I SET heer.ranj_node_id = %L";

/// Classification of a persisted or in-memory Phase 0 SQL artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhaseZeroArtifactState {
    Missing,
    Incomplete,
    Current,
    GeneratedStale,
    Ambiguous,
}

/// Typed refusal reasons for callers that must fail closed on non-current
/// Phase 0 artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhaseZeroRefusal {
    Missing,
    Incomplete,
    GeneratedStale,
    Ambiguous,
}

impl std::fmt::Display for PhaseZeroRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "phase 0 artifact is missing"),
            Self::Incomplete => write!(f, "phase 0 artifact is incomplete"),
            Self::GeneratedStale => write!(f, "phase 0 artifact is a stale generated variant"),
            Self::Ambiguous => write!(f, "phase 0 artifact is hand-edited or ambiguous"),
        }
    }
}

impl std::error::Error for PhaseZeroRefusal {}

#[derive(Debug, Clone, Copy)]
struct PhaseZeroShape {
    has_banner: bool,
    has_base_schema_marker: bool,
    has_node_seed_marker: bool,
    has_session_node_set: bool,
    has_session_ranj_set: bool,
    has_dynamic_node_default: bool,
    has_dynamic_ranj_default: bool,
    has_literal_node_default: bool,
    has_literal_ranj_default: bool,
    has_current_database_call: bool,
}

impl PhaseZeroShape {
    fn from_sql(sql: &str) -> Self {
        Self {
            has_banner: sql.contains(PHASE_ZERO_BANNER_MARKER),
            has_base_schema_marker: sql.contains(BASE_SCHEMA_MARKER),
            has_node_seed_marker: sql.contains(NODE_SEED_MARKER),
            has_session_node_set: sql.contains(SESSION_NODE_SET_MARKER),
            has_session_ranj_set: sql.contains(SESSION_RANJ_SET_MARKER),
            has_dynamic_node_default: sql.contains(DYNAMIC_NODE_DEFAULT_MARKER),
            has_dynamic_ranj_default: sql.contains(DYNAMIC_RANJ_DEFAULT_MARKER),
            has_literal_node_default: contains_literal_database_default(
                sql,
                "\" SET heer.node_id = '",
            ),
            has_literal_ranj_default: contains_literal_database_default(
                sql,
                "\" SET heer.ranj_node_id = '",
            ),
            has_current_database_call: sql.contains("current_database()"),
        }
    }

    fn has_any_generated_marker(self) -> bool {
        // Node seed marker is a generated artifact signal, but production
        // Phase 0 may not have it. Banner and base schema markers are the
        // primary signals.
        self.has_banner || self.has_base_schema_marker || self.has_node_seed_marker
    }

    fn has_required_generated_markers(self) -> bool {
        // Production Phase 0 only needs banner + base schema.
        // Node seed marker is optional — production omits it, dev includes it.
        self.has_banner && self.has_base_schema_marker
    }

    fn has_both_session_sets(self) -> bool {
        self.has_session_node_set && self.has_session_ranj_set
    }

    fn has_both_dynamic_defaults(self) -> bool {
        self.has_current_database_call
            && self.has_dynamic_node_default
            && self.has_dynamic_ranj_default
    }

    fn has_both_literal_defaults(self) -> bool {
        self.has_literal_node_default && self.has_literal_ranj_default
    }

    fn has_defaults_for_both_gucs(self) -> bool {
        (self.has_dynamic_node_default || self.has_literal_node_default)
            && (self.has_dynamic_ranj_default || self.has_literal_ranj_default)
    }

    fn has_any_seed_fragment(self) -> bool {
        self.has_session_node_set
            || self.has_session_ranj_set
            || self.has_dynamic_node_default
            || self.has_dynamic_ranj_default
            || self.has_literal_node_default
            || self.has_literal_ranj_default
            || self.has_current_database_call
    }
}

/// Classify a Phase 0 artifact from raw bytes. Invalid UTF-8 is treated as an
/// ambiguous/manual artifact so later stages fail closed.
pub(crate) fn classify_phase_zero_artifact(bytes: &[u8]) -> PhaseZeroArtifactState {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return PhaseZeroArtifactState::Missing;
    }
    match std::str::from_utf8(bytes) {
        Ok(sql) => classify_phase_zero_sql(sql),
        Err(_) => PhaseZeroArtifactState::Ambiguous,
    }
}

/// Classify a Phase 0 SQL payload using the generated banner/section markers
/// and the node-seed shape. Descriptor-extension drift is intentionally ignored.
///
/// Three current shapes are recognized:
/// 1. **Production-current** — has all required generated markers, no node-seed
///    fragment, no session SET, no database-level default. This is the form
///    emitted by `ensure_phase_zero_emitted` for production/cluster use.
/// 2. **Single-node-dev current** — has dynamic `current_database()` defaults
///    plus session SETs. No literal database names.
/// 3. **Generated-stale** — recognizable generated Phase 0 with literal
///    `ALTER DATABASE "<label>" SET heer.node_id` / `heer.ranj_node_id`.
///    This is the known-bad shape from pre-fix generated files.
pub(crate) fn classify_phase_zero_sql(sql: &str) -> PhaseZeroArtifactState {
    if sql.trim().is_empty() {
        return PhaseZeroArtifactState::Missing;
    }

    let shape = PhaseZeroShape::from_sql(sql);

    // Not a generated artifact at all → hand-edited or unknown.
    if !shape.has_any_generated_marker() {
        return PhaseZeroArtifactState::Ambiguous;
    }
    // Missing required sections → truncated generation.
    if !shape.has_required_generated_markers() {
        return PhaseZeroArtifactState::Incomplete;
    }

    // ── Production-current: all required markers, no seed fragments ──
    if !shape.has_any_seed_fragment() {
        return PhaseZeroArtifactState::Current;
    }

    // ── Single-node-dev current: dynamic defaults + session SETs ──
    if shape.has_both_dynamic_defaults() && shape.has_both_session_sets() {
        // Reject if also contains literal defaults (mixed/hand-edited).
        if shape.has_literal_node_default || shape.has_literal_ranj_default {
            return PhaseZeroArtifactState::Ambiguous;
        }
        return PhaseZeroArtifactState::Current;
    }

    // ── Generated-stale: literal ALTER DATABASE defaults ──
    if shape.has_both_literal_defaults() && shape.has_both_session_sets() {
        // Reject if also contains dynamic fragments (mixed/hand-edited).
        if shape.has_dynamic_node_default
            || shape.has_dynamic_ranj_default
            || shape.has_current_database_call
        {
            return PhaseZeroArtifactState::Ambiguous;
        }
        return PhaseZeroArtifactState::GeneratedStale;
    }

    // ── Partial seed fragments → incomplete or ambiguous ──
    if shape.has_both_session_sets() && shape.has_defaults_for_both_gucs() {
        return PhaseZeroArtifactState::Ambiguous;
    }
    PhaseZeroArtifactState::Incomplete
}

/// Require a current/non-stale Phase 0 artifact from raw bytes.
pub(crate) fn require_current_phase_zero_artifact(bytes: &[u8]) -> Result<(), PhaseZeroRefusal> {
    map_phase_zero_refusal(classify_phase_zero_artifact(bytes))
}

/// Require a current/non-stale Phase 0 SQL payload.
pub(crate) fn require_current_phase_zero_sql(sql: &str) -> Result<(), PhaseZeroRefusal> {
    map_phase_zero_refusal(classify_phase_zero_sql(sql))
}

fn map_phase_zero_refusal(state: PhaseZeroArtifactState) -> Result<(), PhaseZeroRefusal> {
    match state {
        PhaseZeroArtifactState::Current => Ok(()),
        PhaseZeroArtifactState::Missing => Err(PhaseZeroRefusal::Missing),
        PhaseZeroArtifactState::Incomplete => Err(PhaseZeroRefusal::Incomplete),
        PhaseZeroArtifactState::GeneratedStale => Err(PhaseZeroRefusal::GeneratedStale),
        PhaseZeroArtifactState::Ambiguous => Err(PhaseZeroRefusal::Ambiguous),
    }
}

fn contains_literal_database_default(sql: &str, statement_suffix: &str) -> bool {
    sql.lines().map(str::trim).any(|line| {
        line.starts_with("ALTER DATABASE \"")
            && line.contains(statement_suffix)
            && line.ends_with("';")
    })
}

#[cfg(test)]
mod tests {
    use super::{
        BASE_SCHEMA_MARKER, NODE_SEED_MARKER, PHASE_ZERO_BANNER_MARKER, PhaseZeroArtifactState,
        PhaseZeroRefusal, classify_phase_zero_sql, require_current_phase_zero_sql,
    };

    fn old_generated_phase_zero(database: &str) -> String {
        format!(
            "-- ╭───────────────────────────────────────────────────────────────╮\n\
             -- │ {PHASE_ZERO_BANNER_MARKER} │\n\
             -- │ Auto-emitted by `djogi migrations compose`. Idempotent.        │\n\
             -- ╰───────────────────────────────────────────────────────────────╯\n\n\
             {BASE_SCHEMA_MARKER}\n\
             SELECT 1;\n\n\
             {NODE_SEED_MARKER}\n\
             -- `heer.node_id` powers heerid_next(); `heer.ranj_node_id` powers ranjid_next().\n\
             ALTER DATABASE \"{database}\" SET heer.node_id = '7';\n\
             ALTER DATABASE \"{database}\" SET heer.ranj_node_id = '7';\n\
             SET heer.node_id = '7';\n\
             SET heer.ranj_node_id = '7';\n"
        )
    }

    fn dev_generated_phase_zero(extension_lines: &str) -> String {
        format!(
            "-- ╭───────────────────────────────────────────────────────────────╮\n\
             -- │ {PHASE_ZERO_BANNER_MARKER} │\n\
             -- │ Auto-emitted by `djogi migrations compose`. Idempotent.        │\n\
             -- ╰───────────────────────────────────────────────────────────────╯\n\n\
             {BASE_SCHEMA_MARKER}\n\
             SELECT 1;\n\n\
             {extension_lines}\n\
             {NODE_SEED_MARKER}\n\
             -- `heer.node_id` powers heerid_next(); `heer.ranj_node_id` powers ranjid_next().\n\
             DO $djogi$\n\
             BEGIN\n\
                 EXECUTE format('ALTER DATABASE %I SET heer.node_id = %L', current_database(), '7');\n\
                 EXECUTE format('ALTER DATABASE %I SET heer.ranj_node_id = %L', current_database(), '7');\n\
             END\n\
             $djogi$;\n\
             SET heer.node_id = '7';\n\
             SET heer.ranj_node_id = '7';\n"
        )
    }

    /// Generate production Phase 0 SQL — no node-seed section, no database defaults.
    fn production_generated_phase_zero(extension_lines: &str) -> String {
        format!(
            "-- ╭───────────────────────────────────────────────────────────────╮\n\
             -- │ {PHASE_ZERO_BANNER_MARKER} │\n\
             -- │ Auto-emitted by `djogi migrations compose`. Idempotent.        │\n\
             -- ╰───────────────────────────────────────────────────────────────╯\n\n\
             {BASE_SCHEMA_MARKER}\n\
             SELECT 1;\n\n\
             {extension_lines}"
        )
    }

    #[test]
    fn classify_blank_phase_zero_sql_as_missing() {
        assert_eq!(
            classify_phase_zero_sql(" \n\t"),
            PhaseZeroArtifactState::Missing
        );
    }

    #[test]
    fn classify_old_generated_literal_database_target_as_generated_stale() {
        assert_eq!(
            classify_phase_zero_sql(&old_generated_phase_zero("main")),
            PhaseZeroArtifactState::GeneratedStale
        );
    }

    #[test]
    fn classify_single_node_dev_dynamic_phase_zero_as_current() {
        assert_eq!(
            classify_phase_zero_sql(&dev_generated_phase_zero("")),
            PhaseZeroArtifactState::Current
        );
    }

    #[test]
    fn classify_production_current_phase_zero_without_seed_as_current() {
        // Production Phase 0: HeeRanjID schema + extensions, no node seed.
        assert_eq!(
            classify_phase_zero_sql(&production_generated_phase_zero("")),
            PhaseZeroArtifactState::Current
        );
    }

    #[test]
    fn classify_descriptor_extension_drift_as_current_not_stale() {
        // Descriptor-extension drift on production Phase 0 is still current.
        let sql = production_generated_phase_zero(
            "-- Postgres extensions required by descriptor inventory (idempotent).\n\
             CREATE EXTENSION IF NOT EXISTS \"postgis\";\n",
        );
        assert_eq!(
            classify_phase_zero_sql(&sql),
            PhaseZeroArtifactState::Current
        );
    }

    #[test]
    fn classify_truncated_generated_phase_zero_as_incomplete() {
        let sql = format!(
            "-- ╭───────────────────────────────────────────────────────────────╮\n\
             -- │ {PHASE_ZERO_BANNER_MARKER} │\n\
             -- │ Auto-emitted by `djogi migrations compose`. Idempotent.        │\n\
             -- ╰───────────────────────────────────────────────────────────────╯\n\n\
             {BASE_SCHEMA_MARKER}\n\
             SELECT 1;\n\n\
             {NODE_SEED_MARKER}\n\
             -- `heer.node_id` powers heerid_next(); `heer.ranj_node_id` powers ranjid_next().\n\
             DO $djogi$\n\
             BEGIN\n\
                 EXECUTE format('ALTER DATABASE %I SET heer.node_id = %L', current_database(), '7');\n\
             END\n\
             $djogi$;\n\
             SET heer.node_id = '7';\n"
        );
        assert_eq!(
            classify_phase_zero_sql(&sql),
            PhaseZeroArtifactState::Incomplete
        );
        assert_eq!(
            require_current_phase_zero_sql(&sql),
            Err(PhaseZeroRefusal::Incomplete)
        );
    }

    #[test]
    fn classify_hand_edited_phase_zero_as_ambiguous() {
        let sql = format!(
            "-- ╭───────────────────────────────────────────────────────────────╮\n\
             -- │ {PHASE_ZERO_BANNER_MARKER} │\n\
             -- │ Auto-emitted by `djogi migrations compose`. Idempotent.        │\n\
             -- ╰───────────────────────────────────────────────────────────────╯\n\n\
             {BASE_SCHEMA_MARKER}\n\
             SELECT 1;\n\n\
             {NODE_SEED_MARKER}\n\
             -- `heer.node_id` powers heerid_next(); `heer.ranj_node_id` powers ranjid_next().\n\
             ALTER DATABASE \"main\" SET heer.node_id = '7';\n\
             DO $djogi$\n\
             BEGIN\n\
                 EXECUTE format('ALTER DATABASE %I SET heer.ranj_node_id = %L', current_database(), '7');\n\
             END\n\
             $djogi$;\n\
             SET heer.node_id = '7';\n\
             SET heer.ranj_node_id = '7';\n"
        );
        assert_eq!(
            classify_phase_zero_sql(&sql),
            PhaseZeroArtifactState::Ambiguous
        );
        assert_eq!(
            require_current_phase_zero_sql(&sql),
            Err(PhaseZeroRefusal::Ambiguous)
        );
    }
}

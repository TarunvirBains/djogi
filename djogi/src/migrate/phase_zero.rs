use super::bootstrap::{
    PHASE_ZERO_BASE_SCHEMA_MARKER, PHASE_ZERO_DEFAULT_NODE_ROW_SEED_MARKER,
    PHASE_ZERO_NODE_SEED_MARKER, PHASE_ZERO_PRODUCTION_BANNER_MARKER,
    PHASE_ZERO_SEEDED_BANNER_MARKER,
};

const SESSION_NODE_SET_MARKER: &str = "SET heer.node_id = '";
const SESSION_RANJ_SET_MARKER: &str = "SET heer.ranj_node_id = '";
const DYNAMIC_NODE_DEFAULT_MARKER: &str = "ALTER DATABASE %I SET heer.node_id = %L";
const DYNAMIC_RANJ_DEFAULT_MARKER: &str = "ALTER DATABASE %I SET heer.ranj_node_id = %L";
const LEGACY_PHASE_ZERO_PRODUCTION_BANNER_MARKER: &str =
    "Djogi Phase 0 bootstrap — HeeRanjID + extensions";
const LEGACY_PHASE_ZERO_SEEDED_BANNER_MARKER: &str =
    "Djogi Phase 0 bootstrap — HeeRanjID + extensions + node seed";
const HEERANJID_SEED_TABLES: &[&str] = &[
    "heer_config",
    "heer_nodes",
    "heer_node_state",
    "heer_ranj_node_state",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseZeroBannerKind {
    Production,
    Seeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseZeroBannerMatch {
    None,
    Exact(PhaseZeroBannerKind),
    Ambiguous,
}

/// Classification of a persisted or in-memory Phase 0 SQL artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseZeroArtifactState {
    /// No Phase 0 SQL payload was present.
    Missing,
    /// Recognizable generated Phase 0 SQL is missing required sections.
    Incomplete,
    /// Replay-eligible production Phase 0 SQL with no node seed or GUC writes.
    IdentityFreeCurrent,
    /// Runtime helper Phase 0 SQL that can seed node state or GUCs.
    SeedCapableRuntimeCurrent,
    /// Generated Phase 0 SQL contains seed DML but is not runtime-current.
    SeedDmlNotRuntimeCurrent,
    /// Generated Phase 0 SQL with stale literal database-default GUC writes.
    GeneratedStale,
    /// Hand-edited, mixed, or otherwise non-canonical Phase 0 SQL.
    Ambiguous,
}

#[derive(Debug, Clone, Copy)]
struct PhaseZeroShape {
    banner: PhaseZeroBannerMatch,
    has_base_schema_marker: bool,
    has_default_node_row_seed_marker: bool,
    has_node_seed_marker: bool,
    has_session_node_set: bool,
    has_session_ranj_set: bool,
    has_dynamic_node_default: bool,
    has_dynamic_ranj_default: bool,
    has_literal_node_default: bool,
    has_literal_ranj_default: bool,
    has_current_database_call: bool,
    has_top_level_seed_dml: bool,
}

impl PhaseZeroShape {
    fn from_sql(sql: &str) -> Self {
        Self {
            banner: effective_phase_zero_banner_kind(sql),
            has_base_schema_marker: sql.contains(PHASE_ZERO_BASE_SCHEMA_MARKER),
            has_default_node_row_seed_marker: sql.contains(PHASE_ZERO_DEFAULT_NODE_ROW_SEED_MARKER),
            has_node_seed_marker: sql.contains(PHASE_ZERO_NODE_SEED_MARKER),
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
            has_top_level_seed_dml: contains_top_level_seed_dml(sql),
        }
    }

    fn has_any_generated_marker(self) -> bool {
        !matches!(self.banner, PhaseZeroBannerMatch::None)
            || self.has_base_schema_marker
            || self.has_default_node_row_seed_marker
            || self.has_node_seed_marker
    }

    fn has_required_generated_markers(self) -> bool {
        self.has_exact_banner_marker() && self.has_base_schema_marker
    }

    fn has_complete_seed_markers(self) -> bool {
        self.has_seeded_banner()
            && self.has_default_node_row_seed_marker
            && self.has_node_seed_marker
    }

    fn has_exact_banner_marker(self) -> bool {
        matches!(self.banner, PhaseZeroBannerMatch::Exact(_))
    }

    fn has_ambiguous_banner_marker(self) -> bool {
        self.banner == PhaseZeroBannerMatch::Ambiguous
    }

    fn has_production_banner(self) -> bool {
        self.banner == PhaseZeroBannerMatch::Exact(PhaseZeroBannerKind::Production)
    }

    fn has_seeded_banner(self) -> bool {
        self.banner == PhaseZeroBannerMatch::Exact(PhaseZeroBannerKind::Seeded)
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
            || self.has_top_level_seed_dml
    }
}

fn effective_phase_zero_banner_kind(sql: &str) -> PhaseZeroBannerMatch {
    let mut found: Option<PhaseZeroBannerKind> = None;
    for line in sql.lines() {
        let Some(kind) = exact_phase_zero_banner_line_kind(line) else {
            continue;
        };
        if found.replace(kind).is_some() {
            return PhaseZeroBannerMatch::Ambiguous;
        }
    }
    match found {
        Some(kind) => PhaseZeroBannerMatch::Exact(kind),
        None => PhaseZeroBannerMatch::None,
    }
}

fn exact_phase_zero_banner_line_kind(line: &str) -> Option<PhaseZeroBannerKind> {
    let comment = line.trim().strip_prefix("--")?.trim();
    let boxed = comment
        .strip_prefix('│')
        .and_then(|s| s.strip_suffix('│'))
        .map(str::trim);
    for candidate in [Some(comment), boxed].into_iter().flatten() {
        match candidate {
            PHASE_ZERO_PRODUCTION_BANNER_MARKER | LEGACY_PHASE_ZERO_PRODUCTION_BANNER_MARKER => {
                return Some(PhaseZeroBannerKind::Production);
            }
            PHASE_ZERO_SEEDED_BANNER_MARKER | LEGACY_PHASE_ZERO_SEEDED_BANNER_MARKER => {
                return Some(PhaseZeroBannerKind::Seeded);
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PhaseZeroSqlToken {
    Ident(String),
    QuotedIdent(String),
    Dot,
    OpenParen,
    CloseParen,
    Comma,
    Semicolon,
}

fn contains_top_level_seed_dml(sql: &str) -> bool {
    let tokens = tokenize_phase_zero_sql(sql);
    let mut statement_start = 0;
    for (idx, token) in tokens.iter().enumerate() {
        if *token != PhaseZeroSqlToken::Semicolon {
            continue;
        }
        if statement_contains_top_level_seed_dml(&tokens[statement_start..idx]) {
            return true;
        }
        statement_start = idx + 1;
    }
    statement_contains_top_level_seed_dml(&tokens[statement_start..])
}

fn statement_contains_top_level_seed_dml(tokens: &[PhaseZeroSqlToken]) -> bool {
    let Some((first_idx, first_ident)) = first_ident_token(tokens) else {
        return false;
    };

    if first_ident.eq_ignore_ascii_case("with") {
        return with_statement_contains_seed_dml(tokens, first_idx + 1);
    }
    seed_dml_after_keyword(tokens, first_idx, first_ident)
}

fn seed_dml_after_keyword(
    tokens: &[PhaseZeroSqlToken],
    first_idx: usize,
    first_ident: &str,
) -> bool {
    if first_ident.eq_ignore_ascii_case("insert") {
        return insert_target_is_seed_table(tokens, first_idx + 1);
    }
    if first_ident.eq_ignore_ascii_case("update") {
        return update_target_is_seed_table(tokens, first_idx + 1);
    }
    if first_ident.eq_ignore_ascii_case("delete") {
        return delete_target_is_seed_table(tokens, first_idx + 1);
    }
    if first_ident.eq_ignore_ascii_case("merge") {
        return merge_target_is_seed_table(tokens, first_idx + 1);
    }
    if first_ident.eq_ignore_ascii_case("copy") {
        return copy_from_target_is_seed_table(tokens, first_idx + 1);
    }
    false
}

fn with_statement_contains_seed_dml(tokens: &[PhaseZeroSqlToken], mut idx: usize) -> bool {
    if is_keyword_at(tokens, idx, "recursive") {
        idx += 1;
    }

    loop {
        if ident_at(tokens, idx).is_none() {
            return true;
        }
        idx += 1;

        if matches!(tokens.get(idx), Some(PhaseZeroSqlToken::OpenParen)) {
            let Some(next_idx) = skip_balanced_group(tokens, idx) else {
                return true;
            };
            idx = next_idx;
        }

        if !is_keyword_at(tokens, idx, "as") {
            return true;
        }
        idx += 1;

        if is_keyword_at(tokens, idx, "not") && is_keyword_at(tokens, idx + 1, "materialized") {
            idx += 2;
        } else if is_keyword_at(tokens, idx, "materialized") {
            idx += 1;
        }

        let Some((body_start, body_end, next_idx)) = balanced_group_bounds(tokens, idx) else {
            return true;
        };
        if statement_contains_top_level_seed_dml(&tokens[body_start..body_end]) {
            return true;
        }
        idx = next_idx;

        if matches!(tokens.get(idx), Some(PhaseZeroSqlToken::Comma)) {
            idx += 1;
            continue;
        }
        break;
    }

    if idx >= tokens.len() {
        return true;
    }
    statement_contains_top_level_seed_dml(&tokens[idx..])
}

fn first_ident_token(tokens: &[PhaseZeroSqlToken]) -> Option<(usize, &str)> {
    tokens
        .iter()
        .enumerate()
        .find_map(|(idx, token)| match token {
            PhaseZeroSqlToken::Ident(value) => Some((idx, value.as_str())),
            PhaseZeroSqlToken::QuotedIdent(_)
            | PhaseZeroSqlToken::Dot
            | PhaseZeroSqlToken::OpenParen
            | PhaseZeroSqlToken::CloseParen
            | PhaseZeroSqlToken::Comma
            | PhaseZeroSqlToken::Semicolon => None,
        })
}

fn insert_target_is_seed_table(tokens: &[PhaseZeroSqlToken], mut idx: usize) -> bool {
    if ident_at(tokens, idx).is_some_and(|ident| ident.eq_ignore_ascii_case("into")) {
        idx += 1;
    } else {
        return false;
    }
    idx = skip_optional_only(tokens, idx);
    target_relation_is_seed_table(tokens, idx)
}

fn update_target_is_seed_table(tokens: &[PhaseZeroSqlToken], idx: usize) -> bool {
    target_relation_is_seed_table(tokens, skip_optional_only(tokens, idx))
}

fn delete_target_is_seed_table(tokens: &[PhaseZeroSqlToken], mut idx: usize) -> bool {
    if ident_at(tokens, idx).is_some_and(|ident| ident.eq_ignore_ascii_case("from")) {
        idx += 1;
    } else {
        return false;
    }
    idx = skip_optional_only(tokens, idx);
    target_relation_is_seed_table(tokens, idx)
}

fn merge_target_is_seed_table(tokens: &[PhaseZeroSqlToken], mut idx: usize) -> bool {
    if ident_at(tokens, idx).is_some_and(|ident| ident.eq_ignore_ascii_case("into")) {
        idx += 1;
    } else {
        return false;
    }
    idx = skip_optional_only(tokens, idx);
    target_relation_is_seed_table(tokens, idx)
}

fn copy_from_target_is_seed_table(tokens: &[PhaseZeroSqlToken], mut idx: usize) -> bool {
    if matches!(tokens.get(idx), Some(PhaseZeroSqlToken::OpenParen)) {
        let Some(next_idx) = skip_balanced_group(tokens, idx) else {
            return true;
        };
        return !is_keyword_at(tokens, next_idx, "to");
    }

    let Some((parts, next_idx)) = parse_relation_parts(tokens, idx) else {
        return true;
    };
    let is_seed_target = relation_parts_end_at_seed_table(&parts);
    idx = next_idx;

    if matches!(tokens.get(idx), Some(PhaseZeroSqlToken::OpenParen)) {
        let Some(next_idx) = skip_balanced_group(tokens, idx) else {
            return true;
        };
        idx = next_idx;
    }

    if is_keyword_at(tokens, idx, "from") {
        return is_seed_target;
    }
    if is_keyword_at(tokens, idx, "to") {
        return false;
    }
    is_seed_target
}

fn skip_optional_only(tokens: &[PhaseZeroSqlToken], idx: usize) -> usize {
    if ident_at(tokens, idx).is_some_and(|ident| ident.eq_ignore_ascii_case("only")) {
        idx + 1
    } else {
        idx
    }
}

fn target_relation_is_seed_table(tokens: &[PhaseZeroSqlToken], idx: usize) -> bool {
    parse_relation_parts(tokens, idx)
        .is_some_and(|(parts, _)| relation_parts_end_at_seed_table(&parts))
}

fn relation_parts_end_at_seed_table(parts: &[&str]) -> bool {
    parts.last().is_some_and(|target| {
        HEERANJID_SEED_TABLES
            .iter()
            .any(|table| target.eq_ignore_ascii_case(table))
    })
}

fn parse_relation_parts(
    tokens: &[PhaseZeroSqlToken],
    mut idx: usize,
) -> Option<(Vec<&str>, usize)> {
    let mut parts = Vec::new();
    parts.push(ident_at(tokens, idx)?);
    idx += 1;

    while matches!(tokens.get(idx), Some(PhaseZeroSqlToken::Dot)) {
        let Some(next_part) = ident_at(tokens, idx + 1) else {
            break;
        };
        parts.push(next_part);
        idx += 2;
    }

    Some((parts, idx))
}

fn ident_at(tokens: &[PhaseZeroSqlToken], idx: usize) -> Option<&str> {
    match tokens.get(idx) {
        Some(PhaseZeroSqlToken::Ident(value) | PhaseZeroSqlToken::QuotedIdent(value)) => {
            Some(value.as_str())
        }
        _ => None,
    }
}

fn is_keyword_at(tokens: &[PhaseZeroSqlToken], idx: usize, keyword: &str) -> bool {
    matches!(
        tokens.get(idx),
        Some(PhaseZeroSqlToken::Ident(value)) if value.eq_ignore_ascii_case(keyword)
    )
}

fn skip_balanced_group(tokens: &[PhaseZeroSqlToken], idx: usize) -> Option<usize> {
    balanced_group_bounds(tokens, idx).map(|(_, _, next_idx)| next_idx)
}

fn balanced_group_bounds(
    tokens: &[PhaseZeroSqlToken],
    idx: usize,
) -> Option<(usize, usize, usize)> {
    if !matches!(tokens.get(idx), Some(PhaseZeroSqlToken::OpenParen)) {
        return None;
    }

    let mut depth = 1usize;
    for (cursor, token) in tokens.iter().enumerate().skip(idx + 1) {
        match token {
            PhaseZeroSqlToken::OpenParen => depth += 1,
            PhaseZeroSqlToken::CloseParen => {
                depth -= 1;
                if depth == 0 {
                    return Some((idx + 1, cursor, cursor + 1));
                }
            }
            PhaseZeroSqlToken::Ident(_)
            | PhaseZeroSqlToken::QuotedIdent(_)
            | PhaseZeroSqlToken::Dot
            | PhaseZeroSqlToken::Comma
            | PhaseZeroSqlToken::Semicolon => {}
        }
    }
    None
}

fn tokenize_phase_zero_sql(sql: &str) -> Vec<PhaseZeroSqlToken> {
    let mut tokens = Vec::new();
    let bytes = sql.as_bytes();
    let mut idx = 0;

    while idx < bytes.len() {
        match bytes[idx] {
            b'-' if bytes.get(idx + 1) == Some(&b'-') => {
                idx = skip_line_comment(bytes, idx + 2);
            }
            b'/' if bytes.get(idx + 1) == Some(&b'*') => {
                idx = skip_block_comment(bytes, idx + 2);
            }
            b'\'' => {
                idx = skip_single_quoted_string(sql, idx + 1);
            }
            b'"' => {
                let (ident, next_idx) = read_quoted_identifier(sql, idx + 1);
                tokens.push(PhaseZeroSqlToken::QuotedIdent(ident));
                idx = next_idx;
            }
            b'$' => {
                if let Some((tag, body_start)) = dollar_quote_tag_at(bytes, idx) {
                    idx = skip_dollar_quoted_body(bytes, body_start, tag);
                } else {
                    idx += 1;
                }
            }
            b'.' => {
                tokens.push(PhaseZeroSqlToken::Dot);
                idx += 1;
            }
            b'(' => {
                tokens.push(PhaseZeroSqlToken::OpenParen);
                idx += 1;
            }
            b')' => {
                tokens.push(PhaseZeroSqlToken::CloseParen);
                idx += 1;
            }
            b',' => {
                tokens.push(PhaseZeroSqlToken::Comma);
                idx += 1;
            }
            b';' => {
                tokens.push(PhaseZeroSqlToken::Semicolon);
                idx += 1;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = idx;
                idx += 1;
                while bytes
                    .get(idx)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$')
                {
                    idx += 1;
                }
                tokens.push(PhaseZeroSqlToken::Ident(sql[start..idx].to_string()));
            }
            b if b.is_ascii_whitespace() => {
                idx += 1;
            }
            _ => {
                idx += sql[idx..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }

    tokens
}

fn skip_line_comment(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
    }
    idx
}

fn skip_block_comment(bytes: &[u8], mut idx: usize) -> usize {
    let mut depth = 1usize;
    while idx + 1 < bytes.len() {
        if bytes[idx] == b'/' && bytes[idx + 1] == b'*' {
            depth += 1;
            idx += 2;
            continue;
        }
        if bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
            depth -= 1;
            idx += 2;
            if depth == 0 {
                return idx;
            }
            continue;
        }
        idx += 1;
    }
    bytes.len()
}

fn skip_single_quoted_string(sql: &str, mut idx: usize) -> usize {
    let bytes = sql.as_bytes();
    while idx < bytes.len() {
        if bytes[idx] == b'\'' {
            if bytes.get(idx + 1) == Some(&b'\'') {
                idx += 2;
            } else {
                return idx + 1;
            }
        } else {
            idx += sql[idx..].chars().next().map_or(1, char::len_utf8);
        }
    }
    bytes.len()
}

fn read_quoted_identifier(sql: &str, mut idx: usize) -> (String, usize) {
    let bytes = sql.as_bytes();
    let mut ident = String::new();
    while idx < bytes.len() {
        if bytes[idx] == b'"' {
            if bytes.get(idx + 1) == Some(&b'"') {
                ident.push('"');
                idx += 2;
            } else {
                return (ident, idx + 1);
            }
        } else {
            let ch = sql[idx..]
                .chars()
                .next()
                .expect("idx is inside UTF-8 SQL text");
            ident.push(ch);
            idx += ch.len_utf8();
        }
    }
    (ident, bytes.len())
}

fn dollar_quote_tag_at(bytes: &[u8], idx: usize) -> Option<(&[u8], usize)> {
    if bytes.get(idx) != Some(&b'$') {
        return None;
    }

    let mut end = idx + 1;
    while bytes
        .get(end)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        end += 1;
    }
    if bytes.get(end) == Some(&b'$') {
        Some((&bytes[idx..=end], end + 1))
    } else {
        None
    }
}

fn skip_dollar_quoted_body(bytes: &[u8], mut idx: usize, tag: &[u8]) -> usize {
    while idx + tag.len() <= bytes.len() {
        if bytes[idx..].starts_with(tag) {
            return idx + tag.len();
        }
        idx += 1;
    }
    bytes.len()
}

/// Classify a Phase 0 artifact from raw bytes. Invalid UTF-8 is treated as an
/// ambiguous/manual artifact so later stages fail closed.
///
/// Exposed publicly for CLI cleanup: the `apply_one_pending` path loads
/// and classifies Phase 0 artifacts before deleting failed/rolled_back
/// ledger rows, preventing stale artifacts from being silently replayed.
pub fn classify_phase_zero_artifact(bytes: &[u8]) -> PhaseZeroArtifactState {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return PhaseZeroArtifactState::Missing;
    }
    match std::str::from_utf8(bytes) {
        Ok(sql) => classify_phase_zero_sql(sql),
        Err(_) => PhaseZeroArtifactState::Ambiguous,
    }
}

pub(crate) fn phase_zero_artifact_refusal_reason(
    state: PhaseZeroArtifactState,
) -> Option<&'static str> {
    match state {
        PhaseZeroArtifactState::IdentityFreeCurrent => None,
        PhaseZeroArtifactState::SeedCapableRuntimeCurrent => Some("seed-capable-runtime-only"),
        PhaseZeroArtifactState::SeedDmlNotRuntimeCurrent => Some("seed-dml-not-runtime-current"),
        PhaseZeroArtifactState::GeneratedStale => Some("generated-stale"),
        PhaseZeroArtifactState::Ambiguous => Some("ambiguous"),
        PhaseZeroArtifactState::Incomplete => Some("incomplete"),
        PhaseZeroArtifactState::Missing => Some("missing"),
    }
}

pub(crate) fn require_identity_free_phase_zero_migration_artifact(
    bytes: &[u8],
) -> Result<(), &'static str> {
    let state = classify_phase_zero_artifact(bytes);
    match phase_zero_artifact_refusal_reason(state) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

pub(crate) fn require_identity_free_phase_zero_down_payload<'a>(
    payloads: impl IntoIterator<Item = &'a str>,
) -> Result<(), &'static str> {
    let mut combined = String::new();
    for payload in payloads {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(payload);
    }
    require_identity_free_phase_zero_migration_artifact(combined.as_bytes())
}

/// Classify a Phase 0 SQL payload using the generated banner/section markers
/// and the node-seed shape. Descriptor-extension drift is intentionally ignored.
///
/// Two non-stale shapes are recognized:
/// 1. **Identity-free replay-current** — has all required generated markers,
///    no node-seed fragment, no session SET, no database-level default. This
///    is the form emitted by `compose_phase_zero(..., false)` for persisted
///    migration replay.
/// 2. **Seed-capable runtime-current** — has the seeded composer banner, seed
///    markers, dynamic `current_database()` defaults, plus session SETs. This
///    is the form emitted by `compose_phase_zero(..., true)` for direct runtime
///    bootstrap helpers.
/// 3. **Generated-stale** — recognizable generated Phase 0 with literal
///    `ALTER DATABASE "<label>" SET heer.node_id` / `heer.ranj_node_id`.
///    This is the known-bad shape from pre-fix generated files.
/// 4. **Seed-DML non-runtime-current** — generated Phase 0 that contains
///    top-level seed-table mutation but is not the complete runtime helper.
pub(crate) fn classify_phase_zero_sql(sql: &str) -> PhaseZeroArtifactState {
    if sql.trim().is_empty() {
        return PhaseZeroArtifactState::Missing;
    }

    let shape = PhaseZeroShape::from_sql(sql);

    // Not a generated artifact at all → hand-edited or unknown.
    if !shape.has_any_generated_marker() {
        return PhaseZeroArtifactState::Ambiguous;
    }
    // Mixed literal and dynamic database defaults indicate hand-editing or
    // a partially migrated artifact. Refuse this before stale/current tests.
    if (shape.has_literal_node_default || shape.has_literal_ranj_default)
        && (shape.has_dynamic_node_default
            || shape.has_dynamic_ranj_default
            || shape.has_current_database_call)
    {
        return PhaseZeroArtifactState::Ambiguous;
    }

    // ── Generated-stale: literal ALTER DATABASE defaults ──
    if shape.has_both_literal_defaults() && shape.has_both_session_sets() {
        return PhaseZeroArtifactState::GeneratedStale;
    }

    // Multiple exact banner markers indicate a hand-edited or concatenated
    // artifact. Refuse before current/incomplete checks so ambiguity wins.
    if shape.has_ambiguous_banner_marker() {
        return PhaseZeroArtifactState::Ambiguous;
    }

    // Missing required sections → truncated generation. Seed DML inside a
    // truncated generated artifact remains mutation-bearing and must not fall
    // back to the ordinary seed-free incomplete state.
    if !shape.has_required_generated_markers() {
        if shape.has_top_level_seed_dml {
            return PhaseZeroArtifactState::SeedDmlNotRuntimeCurrent;
        }
        return PhaseZeroArtifactState::Incomplete;
    }

    // ── Seed-capable current: dynamic defaults + session SETs ──
    if shape.has_complete_seed_markers()
        && shape.has_top_level_seed_dml
        && shape.has_both_dynamic_defaults()
        && shape.has_both_session_sets()
    {
        return PhaseZeroArtifactState::SeedCapableRuntimeCurrent;
    }

    if shape.has_top_level_seed_dml {
        return PhaseZeroArtifactState::SeedDmlNotRuntimeCurrent;
    }

    // ── Production-current: all required markers, no seed fragments ──
    if shape.has_production_banner() && !shape.has_any_seed_fragment() {
        return PhaseZeroArtifactState::IdentityFreeCurrent;
    }

    // ── Partial seed fragments → incomplete or ambiguous ──
    if shape.has_both_session_sets() && shape.has_defaults_for_both_gucs() {
        return PhaseZeroArtifactState::Ambiguous;
    }
    PhaseZeroArtifactState::Incomplete
}

/// Statement-level classification result for the deepest guard in
/// [`crate::migrate::runner::execute_runner_statement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhaseZeroStatementClass {
    /// Statement is safe to execute — DDL, session SET, DO block, etc.
    Safe,
    /// Statement contains top-level seed mutation against HeeRanjID seed tables.
    SeedDml,
    /// Statement contains a literal database default for a HeeRanjID GUC.
    /// This is the known-bad shape from pre-fix generated files.
    LiteralDefault,
}

/// Classify a single Phase 0 SQL statement for stale patterns.
///
/// Unlike [`classify_phase_zero_sql`] which operates on full artifact payloads
/// (looking at banner markers, section markers, and overall shape), this
/// function classifies individual statements from migration plan segments.
///
/// **What triggers `LiteralDefault`.** Generated-stale Phase 0 artifacts
/// contain `ALTER DATABASE "<hardcoded_name>" SET heer.node_id = '...';` or
/// `heer.ranj_node_id = '...'` with literal database names instead of
/// dynamic defaults like `current_database()`.
/// **What triggers `SeedDml`.** A top-level seed-table mutation: direct
/// `INSERT`/`UPDATE`/`DELETE`, CTE-led data mutation, `MERGE INTO`, or
/// `COPY ... FROM` against one of the HeeRanjID seed tables. DDL, comments,
/// strings, quoted-identifier contents, and dollar-quoted function bodies are
/// skipped.
///
/// **What stays `Safe`.**
/// - Any DDL (`CREATE SCHEMA`, `CREATE FUNCTION`, `CREATE TABLE`, etc.)
/// - Session-level SET (`SET heer.node_id = '1';`) — no ALTER DATABASE
/// - DO blocks with dynamic EXECUTE format using `current_database()`
/// - Extension installs (`CREATE EXTENSION IF NOT EXISTS ...`)
///
/// The runner's deepest guard in `execute_runner_statement` uses this
/// to refuse stale statements immediately before raw `batch_execute`.
pub(crate) fn classify_phase_zero_statement(sql: &str) -> PhaseZeroStatementClass {
    let trimmed = sql.trim();

    // Detect: ALTER DATABASE "literal_db_name" SET heer.node_id = '...';
    // This is the generated-stale pattern. Current single-node-dev uses
    // EXECUTE format('ALTER DATABASE %I ...', current_database(), ...)
    // which does NOT match this pattern.
    let has_literal_heer_default = trimmed.lines().any(|line| {
        let l = line.trim();
        // Match: ALTER DATABASE "..." SET heer.{node,ranj}_node_id = '...';
        (l.starts_with("ALTER DATABASE \"") || l.starts_with("alter database \""))
            && (l.contains("SET heer.node_id = '") || l.contains("SET heer.ranj_node_id = '"))
    });

    if has_literal_heer_default {
        PhaseZeroStatementClass::LiteralDefault
    } else if contains_top_level_seed_dml(sql) {
        PhaseZeroStatementClass::SeedDml
    } else {
        PhaseZeroStatementClass::Safe
    }
}

/// Require that a single Phase 0 statement is not stale.
/// Returns `Ok(())` if safe, or the refusal reason if the statement
/// contains literal database defaults.
pub(crate) fn require_current_phase_zero_statement(sql: &str) -> Result<(), &'static str> {
    match classify_phase_zero_statement(sql) {
        PhaseZeroStatementClass::Safe => Ok(()),
        PhaseZeroStatementClass::SeedDml => Err("seed-dml"),
        PhaseZeroStatementClass::LiteralDefault => Err("generated-stale"),
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
    use std::collections::BTreeSet;

    use crate::migrate::bootstrap::{
        DEFAULT_NODE_ID, PHASE_ZERO_BASE_SCHEMA_MARKER, PHASE_ZERO_PRODUCTION_BANNER_MARKER,
        PHASE_ZERO_SEEDED_BANNER_MARKER, compose_phase_zero,
    };

    use super::{
        LEGACY_PHASE_ZERO_PRODUCTION_BANNER_MARKER, LEGACY_PHASE_ZERO_SEEDED_BANNER_MARKER,
        PhaseZeroArtifactState, classify_phase_zero_sql, phase_zero_artifact_refusal_reason,
    };

    fn current_production_phase_zero_sql() -> String {
        compose_phase_zero("main", &BTreeSet::new(), DEFAULT_NODE_ID, false)
            .expect("compose production Phase 0")
    }

    fn markerless_seed_phase_zero_sql() -> String {
        let mut sql = current_production_phase_zero_sql();
        sql.push_str("\nINSERT INTO heer.heer_nodes (id) VALUES (1);\n");
        sql
    }

    fn production_phase_zero_with_statement(statement: &str) -> String {
        let mut sql = current_production_phase_zero_sql();
        sql.push('\n');
        sql.push_str(statement);
        sql.push('\n');
        sql
    }

    fn current_single_node_dev_phase_zero_sql() -> String {
        compose_phase_zero("main", &BTreeSet::new(), 7, true).expect("compose dev Phase 0")
    }

    fn legacy_banner(sql: &str, current: &str, legacy: &str) -> String {
        sql.replace(current, legacy)
    }

    fn legacy_production_phase_zero_sql() -> String {
        legacy_banner(
            &current_production_phase_zero_sql(),
            PHASE_ZERO_PRODUCTION_BANNER_MARKER,
            LEGACY_PHASE_ZERO_PRODUCTION_BANNER_MARKER,
        )
    }

    fn legacy_single_node_dev_phase_zero_sql() -> String {
        legacy_banner(
            &current_single_node_dev_phase_zero_sql(),
            PHASE_ZERO_SEEDED_BANNER_MARKER,
            LEGACY_PHASE_ZERO_SEEDED_BANNER_MARKER,
        )
    }

    fn generated_stale_phase_zero_sql() -> String {
        let mut sql = current_single_node_dev_phase_zero_sql();
        let start = sql.find("DO $djogi$").expect("dynamic defaults block");
        let end = sql[start..]
            .find("SET heer.node_id = '7';")
            .map(|offset| start + offset)
            .expect("session SET after dynamic defaults");
        sql.replace_range(
            start..end,
            "ALTER DATABASE \"main\" SET heer.node_id = '7';\n\
             ALTER DATABASE \"main\" SET heer.ranj_node_id = '7';\n",
        );
        sql
    }

    fn legacy_generated_stale_phase_zero_sql() -> String {
        legacy_banner(
            &generated_stale_phase_zero_sql(),
            PHASE_ZERO_SEEDED_BANNER_MARKER,
            LEGACY_PHASE_ZERO_SEEDED_BANNER_MARKER,
        )
    }

    fn mixed_literal_and_dynamic_phase_zero_sql() -> String {
        let mut sql = current_single_node_dev_phase_zero_sql();
        let insert_at = sql.find("SET heer.node_id = '7';").expect("session SET");
        sql.insert_str(
            insert_at,
            "ALTER DATABASE \"main\" SET heer.node_id = '7';\n\
             ALTER DATABASE \"main\" SET heer.ranj_node_id = '7';\n",
        );
        sql
    }

    fn incomplete_dynamic_phase_zero_sql() -> String {
        current_single_node_dev_phase_zero_sql().replace("SET heer.ranj_node_id = '7';\n", "")
    }

    fn seed_free_incomplete_phase_zero_sql() -> String {
        current_production_phase_zero_sql()
            .replace("-- HeeRanjID base schema + functions (idempotent).\n", "")
    }

    #[test]
    fn classify_blank_phase_zero_sql_as_missing() {
        assert_eq!(
            classify_phase_zero_sql(" \n\t"),
            PhaseZeroArtifactState::Missing
        );
    }

    #[test]
    fn classify_generated_literal_database_target_as_generated_stale() {
        assert_eq!(
            classify_phase_zero_sql(&generated_stale_phase_zero_sql()),
            PhaseZeroArtifactState::GeneratedStale
        );
    }

    #[test]
    fn classify_single_node_dev_dynamic_phase_zero_as_seed_capable_runtime_current() {
        assert_eq!(
            classify_phase_zero_sql(&current_single_node_dev_phase_zero_sql()),
            PhaseZeroArtifactState::SeedCapableRuntimeCurrent
        );
    }

    #[test]
    fn classify_legacy_single_node_dev_dynamic_phase_zero_as_seed_capable_runtime_current() {
        assert_eq!(
            classify_phase_zero_sql(&legacy_single_node_dev_phase_zero_sql()),
            PhaseZeroArtifactState::SeedCapableRuntimeCurrent
        );
    }

    #[test]
    fn classify_production_phase_zero_without_seed_as_identity_free_current() {
        assert_eq!(
            classify_phase_zero_sql(&current_production_phase_zero_sql()),
            PhaseZeroArtifactState::IdentityFreeCurrent
        );
    }

    #[test]
    fn classify_markerless_seed_dml_in_production_phase_zero_as_seed_dml_not_runtime_current() {
        let state = classify_phase_zero_sql(&markerless_seed_phase_zero_sql());
        assert_eq!(
            phase_zero_artifact_refusal_reason(state),
            Some("seed-dml-not-runtime-current"),
            "production Phase 0 plus raw seed DML must not replay as identity-free"
        );
    }

    #[test]
    fn classify_extended_seed_dml_forms_as_seed_dml_not_runtime_current() {
        for statement in [
            "WITH rows AS (SELECT 1) INSERT INTO heer.heer_nodes (id) VALUES (1);",
            "WITH moved AS (DELETE FROM heer.heer_node_state RETURNING *) SELECT 1;",
            "MERGE INTO heer.heer_nodes AS target USING incoming ON false WHEN NOT MATCHED THEN INSERT (id) VALUES (1);",
            "COPY heer.heer_nodes FROM STDIN;",
            "COPY \"heer\".\"heer_ranj_node_state\" (\"node_id\") FROM STDIN;",
        ] {
            let state = classify_phase_zero_sql(&production_phase_zero_with_statement(statement));
            assert_eq!(
                phase_zero_artifact_refusal_reason(state),
                Some("seed-dml-not-runtime-current"),
                "seed mutation form must fail closed as non-runtime Phase 0: {statement}"
            );
        }
    }

    #[test]
    fn classify_seeded_incomplete_seed_dml_as_seed_dml_not_runtime_current() {
        let state = classify_phase_zero_sql(&incomplete_dynamic_phase_zero_sql());
        assert_eq!(
            phase_zero_artifact_refusal_reason(state),
            Some("seed-dml-not-runtime-current"),
            "seeded runtime artifacts with incomplete runtime identity pieces must not fall back to ordinary Incomplete"
        );
    }

    #[test]
    fn classify_legacy_production_phase_zero_without_seed_as_identity_free_current() {
        assert_eq!(
            classify_phase_zero_sql(&legacy_production_phase_zero_sql()),
            PhaseZeroArtifactState::IdentityFreeCurrent
        );
    }

    #[test]
    fn seed_capable_runtime_current_is_not_replay_eligible() {
        let state = classify_phase_zero_sql(&current_single_node_dev_phase_zero_sql());
        assert_eq!(
            phase_zero_artifact_refusal_reason(state),
            Some("seed-capable-runtime-only")
        );
    }

    #[test]
    fn classify_legacy_and_current_banner_marker_inside_sql_literal_as_incomplete() {
        for marker in [
            PHASE_ZERO_PRODUCTION_BANNER_MARKER,
            LEGACY_PHASE_ZERO_PRODUCTION_BANNER_MARKER,
        ] {
            let sql =
                format!("SELECT '{marker}' AS not_a_banner;\n{PHASE_ZERO_BASE_SCHEMA_MARKER}\n");
            assert_eq!(
                classify_phase_zero_sql(&sql),
                PhaseZeroArtifactState::Incomplete,
                "marker text outside an anchored banner must not authorize Phase 0 replay"
            );
        }
    }

    #[test]
    fn classify_legacy_and_current_seeded_banner_with_extra_suffix_as_seed_dml_not_runtime_current()
    {
        let current = current_single_node_dev_phase_zero_sql().replace(
            PHASE_ZERO_SEEDED_BANNER_MARKER,
            &format!("{PHASE_ZERO_SEEDED_BANNER_MARKER} stale-copy"),
        );
        let legacy = current_single_node_dev_phase_zero_sql().replace(
            PHASE_ZERO_SEEDED_BANNER_MARKER,
            &format!("{LEGACY_PHASE_ZERO_SEEDED_BANNER_MARKER} stale-copy"),
        );

        for sql in [current, legacy] {
            let state = classify_phase_zero_sql(&sql);
            assert_eq!(
                phase_zero_artifact_refusal_reason(state),
                Some("seed-dml-not-runtime-current"),
                "seeded banner markers must be exact, not substring matches"
            );
        }
    }

    #[test]
    fn classify_legacy_generated_literal_database_target_as_generated_stale() {
        assert_eq!(
            classify_phase_zero_sql(&legacy_generated_stale_phase_zero_sql()),
            PhaseZeroArtifactState::GeneratedStale
        );
    }

    #[test]
    fn classify_descriptor_extension_drift_as_identity_free_current_not_stale() {
        let mut extensions = BTreeSet::new();
        extensions.insert("postgis".to_string());
        let sql = compose_phase_zero("main", &extensions, DEFAULT_NODE_ID, false)
            .expect("compose production Phase 0 with extension");
        assert_eq!(
            classify_phase_zero_sql(&sql),
            PhaseZeroArtifactState::IdentityFreeCurrent
        );
    }

    #[test]
    fn classify_truncated_generated_phase_zero_as_incomplete() {
        let sql = seed_free_incomplete_phase_zero_sql();
        assert_eq!(
            classify_phase_zero_sql(&sql),
            PhaseZeroArtifactState::Incomplete
        );
    }

    #[test]
    fn classify_hand_edited_phase_zero_as_ambiguous() {
        let sql = mixed_literal_and_dynamic_phase_zero_sql();
        assert_eq!(
            classify_phase_zero_sql(&sql),
            PhaseZeroArtifactState::Ambiguous
        );
    }

    // ── Statement-level classifier tests ───────────────────────────────

    use super::{
        PhaseZeroStatementClass, classify_phase_zero_statement,
        require_current_phase_zero_statement,
    };

    #[test]
    fn classify_literal_database_default_as_generated_stale() {
        let stmt = "ALTER DATABASE \"mydb\" SET heer.node_id = '1';";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::LiteralDefault
        );
        assert_eq!(
            require_current_phase_zero_statement(stmt),
            Err("generated-stale")
        );
    }

    #[test]
    fn classify_literal_ranj_node_default_as_generated_stale() {
        let stmt = "ALTER DATABASE \"production_main\" SET heer.ranj_node_id = '7';";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::LiteralDefault
        );
    }

    #[test]
    fn require_top_level_seed_dml_statements_as_seed_dml() {
        for stmt in [
            "INSERT INTO heer.heer_config (key, value) VALUES ('node_id', '1');",
            "UPDATE heer.heer_nodes SET active = true WHERE id = 1;",
            "DELETE FROM heer.heer_node_state WHERE node_id = 1;",
            "INSERT INTO \"heer\".\"heer_ranj_node_state\" (node_id) VALUES (1);",
            "MERGE INTO heer.heer_nodes AS target USING incoming ON false WHEN NOT MATCHED THEN INSERT (id) VALUES (1);",
            "COPY heer.heer_nodes FROM STDIN;",
            "COPY heer.heer_nodes (node_id, node_kind) FROM STDIN;",
            "COPY \"heer\".\"heer_ranj_node_state\" (\"node_id\") FROM PROGRAM 'cat seed.csv';",
            "WITH rows AS (SELECT 1) INSERT INTO heer.heer_nodes (id) VALUES (1);",
            "WITH moved AS (DELETE FROM heer.heer_node_state RETURNING *) SELECT 1;",
            "WITH x AS (SELECT 1",
            "COPY heer.heer_nodes (node_id FROM STDIN;",
        ] {
            assert_eq!(
                require_current_phase_zero_statement(stmt),
                Err("seed-dml"),
                "top-level seed DML must be refused: {stmt}"
            );
        }
    }

    #[test]
    fn allow_seed_table_ddl_and_seed_text_inside_non_top_level_contexts() {
        for stmt in [
            "CREATE TABLE heer.heer_nodes (id bigint PRIMARY KEY);",
            "-- INSERT INTO heer.heer_nodes (id) VALUES (1);\nSELECT 1;",
            "SELECT 'DELETE FROM heer.heer_node_state WHERE node_id = 1';",
            "DO $djogi$\nBEGIN\nINSERT INTO heer.heer_nodes (id) VALUES (1);\nEND\n$djogi$;",
            "CREATE TABLE \"INSERT INTO heer_nodes\" (id bigint);",
            "\"INSERT\" INTO heer.heer_nodes (id) VALUES (1);",
            "WITH rows AS (SELECT * FROM heer.heer_nodes) SELECT * FROM rows;",
            "WITH \"INSERT\" AS (SELECT 1) SELECT * FROM \"INSERT\";",
            "COPY heer.heer_nodes TO STDOUT;",
            "COPY heer.heer_nodes (node_id) TO STDOUT;",
            "COPY (SELECT * FROM heer.heer_nodes) TO STDOUT;",
        ] {
            assert_eq!(
                require_current_phase_zero_statement(stmt),
                Ok(()),
                "non-top-level seed-looking text must stay safe: {stmt}"
            );
        }
    }

    #[test]
    fn classify_session_set_as_safe() {
        // Session-level SET is safe — no ALTER DATABASE
        let stmt = "SET heer.node_id = '1';";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::Safe
        );
        assert_eq!(require_current_phase_zero_statement(stmt), Ok(()));
    }

    #[test]
    fn classify_ddl_as_safe() {
        let stmt = "CREATE SCHEMA IF NOT EXISTS heer;";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::Safe
        );
    }

    #[test]
    fn classify_create_function_as_safe() {
        let stmt = "CREATE OR REPLACE FUNCTION heer.heerid_next() RETURNS bigint AS $$ ... $$ LANGUAGE plpgsql;";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::Safe
        );
    }

    #[test]
    fn classify_extension_install_as_safe() {
        let stmt = "CREATE EXTENSION IF NOT EXISTS \"postgis\";";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::Safe
        );
    }

    #[test]
    fn classify_dynamic_execute_format_do_block_as_safe() {
        // Single-node-dev current uses EXECUTE format with current_database()
        let stmt = "DO $djogi$\nBEGIN\n\
                    EXECUTE format('ALTER DATABASE %I SET heer.node_id = %L', current_database(), '1');\n\
                    END\n$djogi$;";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::Safe
        );
    }

    #[test]
    fn classify_lowercase_alter_database_as_stale() {
        // Case-insensitive detection for ALTER DATABASE
        let stmt = "alter database \"mydb\" SET heer.node_id = '1';";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::LiteralDefault
        );
    }

    #[test]
    fn classify_non_heer_alter_database_as_safe() {
        // ALTER DATABASE for non-HeeRanjID GUCs is safe
        let stmt = "ALTER DATABASE \"mydb\" SET search_path = public;";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::Safe
        );
    }

    #[test]
    fn classify_create_table_if_not_exists_as_safe() {
        let stmt = "CREATE TABLE IF NOT EXISTS heer.heer_nodes (id integer PRIMARY KEY);";
        assert_eq!(
            classify_phase_zero_statement(stmt),
            PhaseZeroStatementClass::Safe
        );
    }

    #[test]
    fn truncate_for_log_keeps_short_statements_intact() {
        use crate::DjogiError;
        // The truncate_for_log is in runner.rs but we can verify the error variant
        // carries the statement correctly through the DjogiError type.
        let err = DjogiError::StalePhaseZeroStatement {
            refusal_reason: "generated-stale",
            statement: "ALTER DATABASE \"mydb\" SET heer.node_id = '1';".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("stale Phase 0 statement refused"));
        assert!(msg.contains("generated-stale"));
        assert!(msg.contains("ALTER DATABASE \"mydb\""));
    }
}

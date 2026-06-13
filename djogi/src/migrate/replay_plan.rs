use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::diff::Classification;
use super::projection::BucketKey;
use super::segment::{MigrationPlan, Segment, SegmentKind};
use super::sql::OperationSql;
use super::target::bucket_dir;

pub(crate) const COMMITTED_REPLAY_PLAN_FORMAT_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplayPlanLoadStatus {
    Missing,
    Loaded(MigrationPlan),
    Invalid(ReplayPlanInvalidReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplayPlanInvalidReason {
    ChecksumMismatch,
    Io(String),
    Parse(String),
    FormatVersion { found: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredReplayPlan {
    format_version: String,
    checksum_up: String,
    checksum_down: Option<String>,
    classification: StoredClassification,
    segments: Vec<StoredReplaySegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredClassification {
    NoOp,
    Additive,
    Reversible,
    Destructive,
    Lossy,
    Unsupported {
        reason: String,
    },
    PkTypeFlip {
        co_destructive: bool,
        co_lossy: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredReplaySegment {
    kind: StoredSegmentKind,
    statements: Vec<StoredReplayStatement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredSegmentKind {
    Transactional,
    NonTransactional,
    MetadataOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredReplayStatement {
    label: String,
    up: String,
}

pub(crate) fn committed_replay_plan_path(
    workspace_root: &Path,
    bucket: &BucketKey,
    version: &str,
) -> PathBuf {
    bucket_dir(workspace_root, bucket).join(committed_replay_plan_filename(version))
}

pub(crate) fn committed_replay_plan_filename(version: &str) -> String {
    format!("{version}.plan.json")
}

pub(crate) fn serialize_committed_replay_plan(
    plan: &MigrationPlan,
    checksum_up: &str,
    checksum_down: Option<&str>,
) -> Result<Vec<u8>, serde_json::Error> {
    let stored = StoredReplayPlan {
        format_version: COMMITTED_REPLAY_PLAN_FORMAT_VERSION.to_string(),
        checksum_up: checksum_up.to_string(),
        checksum_down: checksum_down.map(str::to_string),
        classification: StoredClassification::from(&plan.classification),
        segments: plan
            .segments
            .iter()
            .map(StoredReplaySegment::from)
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&stored)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(crate) fn load_committed_replay_plan(
    workspace_root: &Path,
    bucket: &BucketKey,
    version: &str,
    checksum_up: &str,
    checksum_down: Option<&str>,
) -> ReplayPlanLoadStatus {
    let path = committed_replay_plan_path(workspace_root, bucket, version);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReplayPlanLoadStatus::Missing,
        Err(e) => {
            return ReplayPlanLoadStatus::Invalid(ReplayPlanInvalidReason::Io(e.to_string()));
        }
    };

    let stored: StoredReplayPlan = match serde_json::from_slice(&bytes) {
        Ok(stored) => stored,
        Err(e) => {
            return ReplayPlanLoadStatus::Invalid(ReplayPlanInvalidReason::Parse(e.to_string()));
        }
    };

    if stored.format_version != COMMITTED_REPLAY_PLAN_FORMAT_VERSION {
        return ReplayPlanLoadStatus::Invalid(ReplayPlanInvalidReason::FormatVersion {
            found: stored.format_version,
        });
    }
    if stored.checksum_up != checksum_up || stored.checksum_down.as_deref() != checksum_down {
        return ReplayPlanLoadStatus::Invalid(ReplayPlanInvalidReason::ChecksumMismatch);
    }

    let plan = MigrationPlan {
        bucket: bucket.clone(),
        classification: stored.classification.into(),
        segments: stored
            .segments
            .into_iter()
            .map(|segment| Segment {
                kind: segment.kind.into(),
                statements: segment
                    .statements
                    .into_iter()
                    .map(|stmt| OperationSql {
                        label: stmt.label,
                        up: stmt.up,
                        down: String::new(),
                        lossy: None,
                    })
                    .collect(),
            })
            .collect(),
    };
    ReplayPlanLoadStatus::Loaded(plan)
}

/// Return the first non-transactional statement shape found in a SQL stream.
///
/// This scans statement-by-statement, skipping SQL comments, quoted strings,
/// quoted identifiers, and dollar-quoted bodies, and returns the classified
/// shape label for the first statement that cannot safely run inside a single
/// transaction.
///
/// This is used by rollback/reset callers that reconstruct execution from
/// committed SQL files and must fail closed before opening a transaction when
/// they encounter shapes such as `CREATE INDEX CONCURRENTLY`.
pub fn find_non_transactional_statement_shape(sql: &str) -> Option<&'static str> {
    find_non_transactional_statement_shape_in_sql(sql)
        .or_else(|| find_non_transactional_placeholder_shape(sql))
}

fn find_non_transactional_statement_shape_in_sql(sql: &str) -> Option<&'static str> {
    let bytes = sql.as_bytes();
    let mut stmt_start = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx].is_ascii_whitespace() {
            idx += 1;
            continue;
        }
        if bytes[idx] == b'-' && idx + 1 < bytes.len() && bytes[idx + 1] == b'-' {
            idx = skip_line_comment(bytes, idx + 2);
            continue;
        }
        if bytes[idx] == b'/' && idx + 1 < bytes.len() && bytes[idx + 1] == b'*' {
            idx = skip_block_comment(bytes, idx + 2);
            continue;
        }
        if bytes[idx] == b'\'' {
            idx = skip_single_quoted(bytes, idx + 1);
            continue;
        }
        if (bytes[idx] == b'E' || bytes[idx] == b'e')
            && idx + 1 < bytes.len()
            && bytes[idx + 1] == b'\''
        {
            idx = skip_e_string(bytes, idx + 2);
            continue;
        }
        if bytes[idx] == b'"' {
            idx = skip_double_quoted(bytes, idx + 1);
            continue;
        }
        if bytes[idx] == b'$'
            && let Some(end) = skip_dollar_quoted(bytes, idx)
        {
            idx = end;
            continue;
        }
        if bytes[idx] == b';' {
            if let Some(shape) = classify_non_transactional_statement_shape(&sql[stmt_start..idx]) {
                return Some(shape);
            }
            idx += 1;
            stmt_start = idx;
            continue;
        }
        idx += 1;
    }

    classify_non_transactional_statement_shape(&sql[stmt_start..])
}

fn classify_non_transactional_statement_shape(sql: &str) -> Option<&'static str> {
    let bytes = sql.as_bytes();
    let mut idx = 0usize;
    let first = next_leading_keyword(bytes, &mut idx)?;
    let second = next_leading_keyword(bytes, &mut idx);
    let third = next_leading_keyword(bytes, &mut idx);
    let fourth = next_leading_keyword(bytes, &mut idx);

    if token_eq(first, "CREATE")
        && second.is_some_and(|tok| token_eq(tok, "INDEX"))
        && third.is_some_and(|tok| token_eq(tok, "CONCURRENTLY"))
    {
        return Some("CREATE INDEX CONCURRENTLY");
    }
    if token_eq(first, "CREATE")
        && second.is_some_and(|tok| token_eq(tok, "UNIQUE"))
        && third.is_some_and(|tok| token_eq(tok, "INDEX"))
        && fourth.is_some_and(|tok| token_eq(tok, "CONCURRENTLY"))
    {
        return Some("CREATE UNIQUE INDEX CONCURRENTLY");
    }
    if token_eq(first, "DROP")
        && second.is_some_and(|tok| token_eq(tok, "INDEX"))
        && third.is_some_and(|tok| token_eq(tok, "CONCURRENTLY"))
    {
        return Some("DROP INDEX CONCURRENTLY");
    }
    if token_eq(first, "CALL") && second.is_some_and(|tok| token_eq(tok, "HEERANJID_BULK_BACKFILL"))
    {
        return Some("CALL heeranjid_bulk_backfill");
    }
    if token_eq(first, "DO") && contains_ascii_case_insensitive(sql, "COMMIT") {
        return Some("DO block with COMMIT");
    }
    None
}

fn find_non_transactional_placeholder_shape(sql: &str) -> Option<&'static str> {
    if contains_ascii_case_insensitive(sql, "PER LEAF: CREATE UNIQUE INDEX CONCURRENTLY")
        && contains_ascii_case_insensitive(sql, "ATTACH PARTITION")
    {
        return Some("PARTITIONED CONCURRENTLY placeholder");
    }
    if contains_ascii_case_insensitive(sql, "PER LEAF: CREATE INDEX CONCURRENTLY")
        && contains_ascii_case_insensitive(sql, "ATTACH PARTITION")
    {
        return Some("PARTITIONED CONCURRENTLY placeholder");
    }
    None
}

fn next_leading_keyword<'a>(bytes: &'a [u8], idx: &mut usize) -> Option<&'a [u8]> {
    skip_leading_ws_and_comments(bytes, idx);
    if *idx >= bytes.len() || !is_ident_start(bytes[*idx]) {
        return None;
    }
    let start = *idx;
    *idx += 1;
    while *idx < bytes.len() && is_ident_continue(bytes[*idx]) {
        *idx += 1;
    }
    Some(&bytes[start..*idx])
}

fn skip_leading_ws_and_comments(bytes: &[u8], idx: &mut usize) {
    loop {
        while *idx < bytes.len() && bytes[*idx].is_ascii_whitespace() {
            *idx += 1;
        }
        if *idx + 1 < bytes.len() && bytes[*idx] == b'-' && bytes[*idx + 1] == b'-' {
            *idx = skip_line_comment(bytes, *idx + 2);
            continue;
        }
        if *idx + 1 < bytes.len() && bytes[*idx] == b'/' && bytes[*idx + 1] == b'*' {
            *idx = skip_block_comment(bytes, *idx + 2);
            continue;
        }
        return;
    }
}

fn skip_line_comment(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
    }
    idx
}

fn skip_block_comment(bytes: &[u8], mut idx: usize) -> usize {
    let mut depth = 1usize;
    while idx < bytes.len() && depth > 0 {
        if idx + 1 < bytes.len() && bytes[idx] == b'/' && bytes[idx + 1] == b'*' {
            depth += 1;
            idx += 2;
            continue;
        }
        if idx + 1 < bytes.len() && bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
            depth -= 1;
            idx += 2;
            continue;
        }
        idx += 1;
    }
    idx
}

fn skip_single_quoted(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() {
        if bytes[idx] == b'\'' {
            if idx + 1 < bytes.len() && bytes[idx + 1] == b'\'' {
                idx += 2;
                continue;
            }
            return idx + 1;
        }
        idx += 1;
    }
    idx
}

fn skip_e_string(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() {
        if bytes[idx] == b'\\' {
            idx = idx.saturating_add(2);
            continue;
        }
        if bytes[idx] == b'\'' {
            if idx + 1 < bytes.len() && bytes[idx + 1] == b'\'' {
                idx += 2;
                continue;
            }
            return idx + 1;
        }
        idx += 1;
    }
    idx
}

fn skip_double_quoted(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() {
        if bytes[idx] == b'"' {
            if idx + 1 < bytes.len() && bytes[idx + 1] == b'"' {
                idx += 2;
                continue;
            }
            return idx + 1;
        }
        idx += 1;
    }
    idx
}

fn skip_dollar_quoted(bytes: &[u8], idx: usize) -> Option<usize> {
    let mut tag_end = idx + 1;
    while tag_end < bytes.len() {
        let byte = bytes[tag_end];
        if byte == b'$' {
            break;
        }
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return None;
        }
        tag_end += 1;
    }
    if tag_end >= bytes.len() || bytes[tag_end] != b'$' {
        return None;
    }
    let tag = &bytes[idx..=tag_end];
    let mut search = tag_end + 1;
    while search + tag.len() <= bytes.len() {
        if &bytes[search..search + tag.len()] == tag {
            return Some(search + tag.len());
        }
        search += 1;
    }
    Some(bytes.len())
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn token_eq(token: &[u8], expected: &str) -> bool {
    token.eq_ignore_ascii_case(expected.as_bytes())
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_uppercase()
        .contains(&needle.to_ascii_uppercase())
}

impl From<&Classification> for StoredClassification {
    fn from(value: &Classification) -> Self {
        match value {
            Classification::NoOp => StoredClassification::NoOp,
            Classification::Additive => StoredClassification::Additive,
            Classification::Reversible => StoredClassification::Reversible,
            Classification::Destructive => StoredClassification::Destructive,
            Classification::Lossy => StoredClassification::Lossy,
            Classification::Unsupported { reason } => StoredClassification::Unsupported {
                reason: reason.clone(),
            },
            Classification::PkTypeFlip {
                co_destructive,
                co_lossy,
            } => StoredClassification::PkTypeFlip {
                co_destructive: *co_destructive,
                co_lossy: *co_lossy,
            },
        }
    }
}

impl From<StoredClassification> for Classification {
    fn from(value: StoredClassification) -> Self {
        match value {
            StoredClassification::NoOp => Classification::NoOp,
            StoredClassification::Additive => Classification::Additive,
            StoredClassification::Reversible => Classification::Reversible,
            StoredClassification::Destructive => Classification::Destructive,
            StoredClassification::Lossy => Classification::Lossy,
            StoredClassification::Unsupported { reason } => Classification::Unsupported { reason },
            StoredClassification::PkTypeFlip {
                co_destructive,
                co_lossy,
            } => Classification::PkTypeFlip {
                co_destructive,
                co_lossy,
            },
        }
    }
}

impl From<&Segment> for StoredReplaySegment {
    fn from(value: &Segment) -> Self {
        Self {
            kind: value.kind.into(),
            statements: value
                .statements
                .iter()
                .map(|stmt| StoredReplayStatement {
                    label: stmt.label.clone(),
                    up: stmt.up.clone(),
                })
                .collect(),
        }
    }
}

impl From<SegmentKind> for StoredSegmentKind {
    fn from(value: SegmentKind) -> Self {
        match value {
            SegmentKind::Transactional => StoredSegmentKind::Transactional,
            SegmentKind::NonTransactional => StoredSegmentKind::NonTransactional,
            SegmentKind::MetadataOnly => StoredSegmentKind::MetadataOnly,
        }
    }
}

impl From<StoredSegmentKind> for SegmentKind {
    fn from(value: StoredSegmentKind) -> Self {
        match value {
            StoredSegmentKind::Transactional => SegmentKind::Transactional,
            StoredSegmentKind::NonTransactional => SegmentKind::NonTransactional,
            StoredSegmentKind::MetadataOnly => SegmentKind::MetadataOnly,
        }
    }
}

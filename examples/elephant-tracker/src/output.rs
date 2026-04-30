//! Output formatting for the demo subcommands.
//!
//! Each demo produces some structured output. The shape of that output
//! is the demo's call — this module supplies the three render targets
//! (JSON, Mermaid, Markdown) that every demo can opt into.
//!
//! ## Why these three
//!
//! - **JSON** is the universal contract. Every demo emits valid JSON
//!   that parses cleanly through `jq`; the shape is documented in
//!   each demo's docstring. Adopters writing scripts against this
//!   example pin their parsing to JSON.
//! - **Mermaid** is the human-readable graph format. Two demos
//!   (`cross-border-herds` and `lineage`) describe relationships that
//!   are easier to read as graphs than as tables; Mermaid's
//!   `graph LR` / `graph TD` source pastes directly into Markdown
//!   files and is rendered inline by GitHub, GitLab, and most
//!   documentation tooling.
//! - **Markdown** combines Mermaid (where applicable) with one or
//!   more tables and section headings — the right format for embedding
//!   demo output directly into runbooks, post-mortems, or handover
//!   docs.
//!
//! No format depends on a third-party crate beyond `serde_json`. The
//! Mermaid and Markdown renderers are pure string builders; their
//! output is byte-level escape-safe under the no-regex rule (see the
//! `escape_label` helper below).

use anyhow::Result;
use clap::ValueEnum;
use serde::Serialize;
use std::io::Write;

/// Render target for a demo's output.
///
/// `Json` is universally supported. `Mermaid` and `Markdown` are
/// available on demos that document them — see each demo's `--help`.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Format {
    Json,
    Mermaid,
    Markdown,
}

/// Where the formatted output should land.
///
/// Returned by [`open_writer`] — `--out <path>` opens the file,
/// otherwise stdout is locked for the duration of the call.
pub enum OutputTarget {
    /// Stdout, locked for exclusive write.
    Stdout(std::io::Stdout),
    /// Buffered file handle.
    File(std::fs::File),
}

impl OutputTarget {
    /// Acquire a `dyn Write` view appropriate for the variant.
    pub fn writer(&mut self) -> Box<dyn Write + '_> {
        match self {
            OutputTarget::Stdout(s) => Box::new(s.lock()),
            OutputTarget::File(f) => Box::new(f),
        }
    }
}

/// Open the output target named by `out`. `None` means "stdout".
pub fn open_writer(out: Option<&std::path::Path>) -> Result<OutputTarget> {
    match out {
        None => Ok(OutputTarget::Stdout(std::io::stdout())),
        Some(p) => Ok(OutputTarget::File(std::fs::File::create(p)?)),
    }
}

/// Pretty-print `value` as JSON to `target`. The caller passes the
/// already-opened target; this function handles the `serde_json` round
/// trip and the trailing newline.
pub fn write_json<T: Serialize>(target: &mut OutputTarget, value: &T) -> Result<()> {
    let mut w = target.writer();
    serde_json::to_writer_pretty(&mut w, value)?;
    writeln!(w)?;
    Ok(())
}

/// Write a string slice to the target, followed by a newline.
pub fn write_line(target: &mut OutputTarget, line: &str) -> Result<()> {
    let mut w = target.writer();
    writeln!(w, "{line}")?;
    Ok(())
}

/// Escape a label string for safe interpolation inside a Mermaid
/// `"…"` quote pair.
///
/// Mermaid label syntax accepts double-quoted strings; characters that
/// need escaping inside such a string are `"` (the quote terminator)
/// and `\` (the escape character). The replacement is byte-level —
/// the no-regex rule applies even at the example layer.
///
/// Newlines are mapped to literal `<br/>` per Mermaid's recommendation
/// for embedded line breaks.
pub fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("<br/>"),
            // ASCII control chars other than newline collapse to a
            // visible placeholder so the Mermaid parser does not
            // choke on them.
            b if *b < 0x20 => out.push(' '),
            _ => out.push(*byte as char),
        }
    }
    out
}

/// Build a Mermaid node identifier from a numeric id. Mermaid node
/// identifiers must start with an ASCII letter or underscore; we
/// prefix with `n` and stringify the i64.
pub fn mermaid_node_id(id: i64) -> String {
    format!("n{id}")
}

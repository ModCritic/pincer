//! Response shaping for `get_window_state`, shared by every platform backend.
//!
//! A backend walks its accessibility API and builds the complete legacy
//! payload (structured `elements`, `tree_markdown`, a text block with the same
//! markdown, plus metadata). [`apply`] then reduces that payload to what a
//! model needs:
//!
//! * ONE tree representation (`tree_format`, default compact markdown),
//! * trimmed metadata (`_note`, `background_input`) unless `verbose`,
//! * a stated truncation line when the walk hit the `max_elements` budget,
//! * with `since: <snapshot_id>`, only the rows that changed against that
//!   snapshot, falling back to a full read when the snapshot is unknown.
//!
//! `full_output: true` restores the previous payload byte for byte (both
//! representations, all metadata, the platform walk caps).
//!
//! The diff works on the rendered markdown rows, which every backend emits in
//! the same `- [N] Role "label" ...` shape, so it also sees display-only rows
//! (static text) that the structured `elements` array omits.

use crate::protocol::{Content, ToolResult};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default accessibility-node budget for a model-facing read. The platform
/// walkers default to 2 000 (macOS) or 5 000 (Windows, Linux) nodes, which
/// renders 40-100K characters for an ordinary window.
pub const DEFAULT_MAX_ELEMENTS: usize = 250;

/// Snapshots remembered per (pid, window) for `since`.
const STORE_PER_WINDOW: usize = 8;
/// Snapshots remembered across all windows.
const STORE_TOTAL: usize = 32;
/// A remembered snapshot stops being a valid `since` baseline after this long.
const STORE_TTL: Duration = Duration::from_secs(15 * 60);
/// Largest old-rows x new-rows product the line diff will compute.
const DIFF_CELL_LIMIT: usize = 6_000_000;
/// A diff larger than this share (percent) of the full markdown is not worth
/// it: return the full read instead.
const DIFF_MAX_PERCENT_OF_FULL: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeFormat {
    /// Compact markdown only (default).
    Markdown,
    /// Structured `elements` only.
    Elements,
    /// Both, as before this change.
    Both,
}

impl TreeFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            TreeFormat::Markdown => "markdown",
            TreeFormat::Elements => "elements",
            TreeFormat::Both => "both",
        }
    }
}

/// Caller-controlled shape of the response.
#[derive(Clone, Debug)]
pub struct ViewOptions {
    pub tree_format: TreeFormat,
    pub verbose: bool,
    pub full_output: bool,
    pub since: Option<String>,
}

/// JSON schema fragments for the shared arguments, so every backend declares
/// the same properties.
pub fn schema_properties() -> Value {
    json!({
        "tree_format": {
            "type": "string",
            "enum": ["markdown", "elements", "both"],
            "description": "\"markdown\" (default): compact `tree_markdown`; row `[N]` is element_token `<snapshot_id>:N`. \"elements\": structured `elements` array (element_token, frame, parent_index, value). \"both\": both, about twice the size."
        },
        "since": {
            "type": "string",
            "description": "`snapshot_id` of an earlier read of this window, or \"latest\" (its last read with the same query/max_elements/max_depth). Use it on every re-read after acting. Returns only changes: `+` added, `~` changed, `-` removed rows (old ids), or `no change`; unlisted rows keep their [N] unless a `reindexed:` line says otherwise. Falls back to a full read if the snapshot is unknown, expired, from another window, or read with a different query/max_elements/max_depth (`since_status` says which). Use the new snapshot_id in element_tokens."
        },
        "verbose": {
            "type": "boolean",
            "description": "Default false. True adds the `_note` string and, on macOS, the full `background_input` report."
        },
        "full_output": {
            "type": "boolean",
            "description": "Default false. True returns everything: `elements` and `tree_markdown`, whole values (Linux: up to 250,000 characters), all metadata, and the platform's own walk limits unless max_elements/max_depth are set. Not with `since`."
        },
        "display_only": {
            "type": "boolean",
            "description": DISPLAY_ONLY_DESCRIPTION
        }
    })
}

// `display_only` is a public, side-effect-free capture for live previews
// (picture-in-picture) that poll a window several times a second. It maps
// onto each backend's internal observation-only read: no snapshot is
// published, refreshed or removed, and no action capture is registered, so
// the agent's element tokens and pixel frame for the window stay valid.
//
// Hosts detect it by the property in `get_window_state`'s input schema
// (T3 Code checks `"display_only" in input_schema.properties`), so the
// description, refusal and response fields match T3 Code's Linux patch
// word for word.
const DISPLAY_ONLY_DESCRIPTION: &str = "Default false. With include_accessibility_tree:false, returns pixels for display without registering an action capture or replacing the agent snapshot. The image cannot ground input actions.";

/// Refusal when `display_only` is asked for together with a tree walk.
pub const DISPLAY_ONLY_NEEDS_NO_TREE: &str =
    "display_only requires include_accessibility_tree:false";

/// `frame_note` on a display-only response.
pub const DISPLAY_ONLY_FRAME_NOTE: &str = "Display-only preview; not registered for input actions. Obtain a normal get_window_state screenshot before acting.";

/// Read `display_only`. A display-only read must skip the tree walk, because
/// a walk is what produces element rows and tokens.
pub fn display_only(args: &Value) -> Result<bool, ToolResult> {
    if args.get("display_only").and_then(Value::as_bool) != Some(true) {
        return Ok(false);
    }
    if args
        .get("include_accessibility_tree")
        .and_then(Value::as_bool)
        != Some(false)
    {
        return Err(ToolResult::error(DISPLAY_ONLY_NEEDS_NO_TREE));
    }
    Ok(true)
}

/// Mark a display-only response.
pub fn mark_display_only(structured: &mut Value) {
    structured["display_only"] = json!(true);
    structured["frame_note"] = json!(DISPLAY_ONLY_FRAME_NOTE);
}

/// Add the shared `tree_format` / `since` / `verbose` / `full_output` / `display_only`
/// properties to a backend's `get_window_state` input schema.
pub fn extend_input_schema(mut schema: Value) -> Value {
    if let (Some(props), Some(extra)) = (
        schema.get_mut("properties").and_then(Value::as_object_mut),
        schema_properties().as_object().cloned(),
    ) {
        props.extend(extra);
    }
    schema
}

impl ViewOptions {
    pub fn from_args(args: &Value) -> Result<Self, ToolResult> {
        let full_output = args.get("full_output").and_then(Value::as_bool) == Some(true);
        let tree_format = match args.get("tree_format") {
            None | Some(Value::Null) => {
                if full_output {
                    TreeFormat::Both
                } else {
                    TreeFormat::Markdown
                }
            }
            Some(Value::String(s)) => match s.as_str() {
                "markdown" => TreeFormat::Markdown,
                "elements" => TreeFormat::Elements,
                "both" => TreeFormat::Both,
                other => {
                    return Err(ToolResult::error(format!(
                        "tree_format must be one of \"markdown\", \"elements\", \"both\"; got \"{other}\"."
                    )))
                }
            },
            Some(_) => {
                return Err(ToolResult::error(
                    "tree_format must be one of \"markdown\", \"elements\", \"both\".",
                ))
            }
        };
        let since = match args.get("since") {
            None | Some(Value::Null) => None,
            // Models sometimes spell "no baseline" as the string "null".
            Some(Value::String(s))
                if matches!(s.trim().to_ascii_lowercase().as_str(), "" | "null" | "none") =>
            {
                None
            }
            Some(Value::String(s)) => Some(s.trim().to_owned()),
            Some(_) => {
                return Err(ToolResult::error(
                    "since must be a snapshot_id string from an earlier get_window_state.",
                ))
            }
        };
        Ok(Self {
            tree_format,
            verbose: args.get("verbose").and_then(Value::as_bool) == Some(true),
            full_output,
            since: if full_output { None } else { since },
        })
    }

    /// Node budget for the walk. An explicit `max_elements` always wins;
    /// otherwise a model-facing read gets [`DEFAULT_MAX_ELEMENTS`], while
    /// `full_output` and internal observation reads keep the platform default.
    ///
    /// A `query` read filters the walked rows afterwards, so a small node
    /// budget would hide matches that sit past it (an Electron form field
    /// after a long list read back as zero matches). The filter already keeps
    /// the response small, so a query searches with at least the platform
    /// default budget.
    pub fn max_elements(
        &self,
        args: &Value,
        platform_default: usize,
        observation_only: bool,
    ) -> usize {
        let has_query = args
            .get("query")
            .and_then(Value::as_str)
            .is_some_and(|q| !q.trim().is_empty());
        if let Some(n) = args.get("max_elements").and_then(Value::as_u64) {
            let n = n.max(1) as usize;
            return if has_query {
                n.max(platform_default)
            } else {
                n
            };
        }
        if has_query {
            return platform_default;
        }
        if self.full_output || observation_only {
            platform_default
        } else {
            DEFAULT_MAX_ELEMENTS.min(platform_default)
        }
    }
}

/// What a `since` read resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SinceOutcome {
    Diff(Box<DiffReport>),
    Fallback(&'static str),
}

/// Inputs that identify the view a snapshot was read with. A diff is only
/// meaningful between two reads of the same view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewKey {
    pub query: Option<String>,
    pub max_elements: usize,
    pub max_depth: Option<usize>,
}

pub struct ViewContext<'a> {
    pub pid: i64,
    pub window_id: u64,
    pub key: ViewKey,
    /// Describes the focused element, called only for a `no change` answer.
    pub focus_probe: Option<&'a dyn Fn() -> Option<String>>,
}

/// Shape a finished `get_window_state` payload in place.
pub fn apply(
    opts: &ViewOptions,
    ctx: &ViewContext<'_>,
    content: &mut [Content],
    structured: &mut Value,
) {
    if opts.full_output {
        return;
    }
    trim_metadata(opts, structured);
    let Some(md) = structured
        .get("tree_markdown")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        drop_walk_diagnostics(opts, structured);
        return;
    };
    let snapshot_id = structured
        .get("snapshot_id")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // `since:"latest"` names this window's most recent read of the same view.
    let since_resolved = opts.since.as_deref().map(|since| {
        if since.eq_ignore_ascii_case(SINCE_LATEST) {
            latest_snapshot(ctx, snapshot_id.as_deref()).unwrap_or_default()
        } else {
            since.to_owned()
        }
    });
    let truncated = structured.get("truncated").and_then(Value::as_bool) == Some(true);
    let outcome = since_resolved.as_deref().map(|since| {
        if since.is_empty() {
            SinceOutcome::Fallback("no_baseline")
        } else {
            resolve_since(since, snapshot_id.as_deref(), &md, truncated, ctx)
        }
    });
    if let Some(sid) = &snapshot_id {
        remember(sid, ctx, &md, truncated);
    }

    // Header text added to the first line of the tree block.
    let mut header_extra = String::new();
    if let Some(sid) = &snapshot_id {
        header_extra.push_str(&format!(" snapshot_id={sid}"));
    }
    let truncation = truncation_line(structured, &ctx.key);
    if let Some(line) = &truncation {
        structured["truncation_hint"] = json!(line);
    }

    let body;
    match outcome {
        Some(SinceOutcome::Diff(report)) => {
            let since = since_resolved.as_deref().unwrap_or_default();
            let focus = if report.is_empty() {
                ctx.focus_probe.and_then(|probe| probe())
            } else {
                None
            };
            let text = report.render(since, snapshot_id.as_deref(), focus.as_deref());
            structured["since"] = json!(since);
            structured["since_status"] =
                json!(if report.is_empty() && report.first_read.is_empty() {
                    "no_change"
                } else {
                    "diff"
                });
            structured["tree_diff"] = json!(text);
            structured["diff_counts"] = json!({
                "added": report.added.len(),
                "changed": report.changed.len(),
                "removed": report.removed.len(),
            });
            if report.unreached > 0 {
                structured["diff_counts"]["not_reached"] = json!(report.unreached);
            }
            if !report.first_read.is_empty() {
                structured["diff_counts"]["first_read"] = json!(report.first_read.len());
            }
            if let Some(focus) = focus {
                structured["focused_element"] = json!(focus);
            }
            let keep: std::collections::HashSet<u64> = report
                .added
                .iter()
                .chain(report.changed.iter())
                .chain(report.first_read.iter())
                .filter_map(|row| row.index)
                .collect();
            retain_elements(structured, opts.tree_format, Some(&keep));
            if let Some(obj) = structured.as_object_mut() {
                obj.remove("tree_markdown");
            }
            if truncated {
                if let (Some(sid), Some(last)) = (
                    &snapshot_id,
                    parse_rows(&md).iter().filter_map(|row| row.index).max(),
                ) {
                    header_extra.push_str(&format!(
                        "\nthis read stops at row [{last}]: rows past it from a larger read are not in {sid}"
                    ));
                }
            }
            body = text;
        }
        other => {
            if let Some(SinceOutcome::Fallback(reason)) = &other {
                let since = opts.since.as_deref().unwrap_or_default();
                let since = match since_resolved.as_deref() {
                    Some(resolved) if !resolved.is_empty() => resolved,
                    _ => since,
                };
                structured["since"] = json!(since);
                structured["since_status"] = json!(reason);
                header_extra.push_str(&format!(
                    "\nsince={since}: {}; full read follows.",
                    fallback_text(reason)
                ));
            }
            match opts.tree_format {
                TreeFormat::Markdown => {
                    body = md.clone();
                    retain_elements(structured, TreeFormat::Markdown, None);
                }
                TreeFormat::Elements => {
                    body = "(tree is in structuredContent.elements; pass tree_format \"markdown\" for the compact text rendering)".to_owned();
                    if let Some(obj) = structured.as_object_mut() {
                        obj.remove("tree_markdown");
                    }
                }
                TreeFormat::Both => {
                    body = md.clone();
                }
            }
            if let (TreeFormat::Markdown, Some(sid)) = (opts.tree_format, &snapshot_id) {
                header_extra.push_str(&token_hint(sid, &md, truncated));
            }
        }
    }
    structured["tree_format"] = json!(opts.tree_format.as_str());

    if let Some(line) = truncation {
        // Backends that already print the long PARTIAL TREE note keep it.
        let already_noted = content.iter().any(|c| match c {
            Content::Text { text, .. } => text.contains("PARTIAL TREE"),
            _ => false,
        });
        if !already_noted {
            header_extra.push('\n');
            header_extra.push_str(&line);
        }
    }

    // Long values (an editor's whole document) are cut in what the caller
    // reads; the remembered `since` baseline keeps them whole, so an edit
    // past the cut still shows up as a changed row.
    let mut cut = false;
    for key in ["tree_markdown", "tree_diff"] {
        if let Some(text) = structured.get(key).and_then(Value::as_str) {
            if let std::borrow::Cow::Owned(short) = shorten_long_values(text) {
                structured[key] = json!(short);
                cut = true;
            }
        }
    }
    let short_body = match shorten_long_values(&body) {
        std::borrow::Cow::Owned(short) => Some(short),
        std::borrow::Cow::Borrowed(_) => None,
    };
    let body = match short_body {
        Some(short) => {
            cut = true;
            short
        }
        None => body,
    };
    if cut {
        header_extra.push_str(&format!(
            "\nvalues longer than {VALUE_PREVIEW_CHARS} characters show only their first \
             {VALUE_PREVIEW_CHARS} (`...\" (N chars)` gives the whole length); full_output:true \
             returns them whole"
        ));
    }
    rewrite_tree_block(content, &md, &header_extra, &body);
    drop_walk_diagnostics(opts, structured);
}

/// Characters of a row's `value="..."` shown before it is cut.
pub const VALUE_PREVIEW_CHARS: usize = 200;

/// Cut every `value="..."` longer than [`VALUE_PREVIEW_CHARS`] to its start
/// plus `..." (N chars)`. A text area's value is its whole document: a
/// 300-line file cost about 2,500 tokens on every read (desc-test round 3).
/// The value is the first ` value="` outside every quoted field, so a name
/// that contains ` value="` cannot hide the real one. The cut never ends
/// inside an escape (see [`escape_safe_prefix`]). A count the platform wrote
/// is kept as it is: it is the field's real length, where counting the
/// shown text would count each escape (`\n`, `\"`) twice and stop at what
/// the platform read.
pub fn shorten_long_values(md: &str) -> std::borrow::Cow<'_, str> {
    const OPEN: &str = " value=\"";
    if !md.lines().any(|line| {
        find_unquoted(line, OPEN).is_some_and(|start| {
            value_end(line, start + OPEN.len()).is_some_and(|end| {
                line[start + OPEN.len()..end].chars().count() > VALUE_PREVIEW_CHARS
            })
        })
    }) {
        return std::borrow::Cow::Borrowed(md);
    }
    let mut out = String::with_capacity(md.len().min(64 * 1024));
    for (i, line) in md.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let cut = find_unquoted(line, OPEN).and_then(|start| {
            let from = start + OPEN.len();
            let end = value_end(line, from)?;
            let value = &line[from..end];
            let chars = value.chars().count();
            (chars > VALUE_PREVIEW_CHARS).then(|| {
                let keep = escape_safe_prefix(value, VALUE_PREVIEW_CHARS);
                let rest = &line[end + 1..];
                if platform_count_len(rest).is_some() {
                    format!("{}{keep}...\"{rest}", &line[..from])
                } else {
                    format!("{}{keep}...\" ({chars} chars){rest}", &line[..from])
                }
            })
        });
        match cut {
            Some(short) => out.push_str(&short),
            None => out.push_str(line),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Byte offset of the quote that closes a value starting at `from`.
fn value_end(line: &str, from: usize) -> Option<usize> {
    let tail = &line[from..];
    // A Linux row escapes its value, so its first unescaped quote ends it,
    // and the row's end, its actions block, or a count followed by one of
    // those comes right after that quote.
    if let Some(end) = escaped_closing_quote(tail) {
        let after = &tail[end + 1..];
        let ends_row = |rest: &str| rest.is_empty() || rest.starts_with(" [actions=[");
        if ends_row(after) || platform_count_len(after).is_some_and(|len| ends_row(&after[len..]))
        {
            return Some(from + end);
        }
    }
    // A value with raw quotes in it, from a row that does not escape: a
    // count that ends the row or comes right before its actions, so text
    // inside the value that only looks like one is not taken for it.
    tail.rmatch_indices("\" (")
        .map(|(at, _)| at)
        .find(|at| {
            platform_count_len(&tail[at + 1..]).is_some_and(|len| {
                let after = &tail[at + 1 + len..];
                after.is_empty() || after.starts_with(" [actions=[")
            })
        })
        .or_else(|| tail.rfind("\" [actions=["))
        .or_else(|| tail.rfind('"'))
        .map(|end| from + end)
}

/// Byte offset of the `"` that closes a quoted row field whose text starts
/// at the start of `text`. Linux rows escape every field
/// (`untrusted_text::inline`), so `\"` and `\\` never close one. macOS and
/// Windows rows do not escape: when no unescaped quote follows (a title
/// that ends in a backslash, `"C:\"`), the first quote closes the field, as
/// before. Such a title followed by another quoted field is still misread;
/// only an escaping producer avoids that.
pub fn closing_quote(text: &str) -> Option<usize> {
    escaped_closing_quote(text).or_else(|| text.find('"'))
}

/// The first `"` in `text` that no backslash escapes. A backslash takes the
/// byte after it, which in escaped text is the escape's ASCII letter, so the
/// scan never stops inside a multi-byte character.
fn escaped_closing_quote(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return Some(at),
            _ => at += 1,
        }
    }
    None
}

/// Byte offset of the first `pattern` in `line` outside every quoted field
/// (escape-aware, as [`closing_quote`] reads fields). `pattern` starts with
/// an ASCII byte, so the offset is a character boundary.
fn find_unquoted(line: &str, pattern: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut at = 0;
    let mut quoted = false;
    while at < bytes.len() {
        if !quoted && bytes[at..].starts_with(pattern.as_bytes()) {
            return Some(at);
        }
        match bytes[at] {
            b'\\' => {
                at += 2;
                continue;
            }
            b'"' => quoted = !quoted,
            _ => {}
        }
        at += 1;
    }
    None
}

/// The longest start of escaped `value` of at most `max_chars` characters
/// that does not end inside an escape: `\"`, `\\`, `\n` and `\u{202e}` stay
/// whole or go whole.
fn escape_safe_prefix(value: &str, max_chars: usize) -> &str {
    let mut at = 0;
    let mut shown = 0;
    while at < value.len() {
        let (bytes, chars) = escape_unit(&value[at..]);
        if shown + chars > max_chars {
            break;
        }
        shown += chars;
        at += bytes;
    }
    &value[..at]
}

/// Bytes and characters of the first unit of non-empty `text`: one escape
/// (`\u{hex}` or a backslash and the character after it), or one character.
fn escape_unit(text: &str) -> (usize, usize) {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return (0, 0);
    };
    if first == '\\' {
        if let Some(hex) = text.strip_prefix("\\u{") {
            if let Some(close) = hex.find('}') {
                if (1..=6).contains(&close) && hex[..close].bytes().all(|b| b.is_ascii_hexdigit()) {
                    return (close + 4, close + 4);
                }
            }
        }
        if let Some(next) = chars.next() {
            return (1 + next.len_utf8(), 2);
        }
    }
    (first.len_utf8(), 1)
}

/// Length of a character count the platform wrote right after a value
/// (` (24492 chars)` or ` (24492 chars, the first 4096 read)`) at the start
/// of `rest`; `None` when `rest` does not start with one.
fn platform_count_len(rest: &str) -> Option<usize> {
    let inner = rest.strip_prefix(" (")?;
    let digits = inner.find(|c: char| !c.is_ascii_digit())?;
    if digits == 0 || !inner[digits..].starts_with(" chars") {
        return None;
    }
    let close = inner.find(')')?;
    Some(2 + close + 1)
}

/// Walk diagnostics cost every read context and no caller acts on them:
/// `truncated` and the truncation hint (built from them above) already say
/// when a walk stopped early. `verbose:true` keeps them.
fn drop_walk_diagnostics(opts: &ViewOptions, structured: &mut Value) {
    if opts.verbose {
        return;
    }
    if let Some(obj) = structured.as_object_mut() {
        for key in [
            "nodes_pending",
            "nodes_visited",
            "walk_elapsed_ms",
            "screenshot_refreshed_snapshot",
        ] {
            obj.remove(key);
        }
    }
}

/// `since` value that names the window's most recent read of the same view.
pub const SINCE_LATEST: &str = "latest";

/// The header line telling the caller how to address rows, with this read's
/// real snapshot id and, for a truncated read, where its rows end.
fn token_hint(snapshot_id: &str, md: &str, truncated: bool) -> String {
    let rows = parse_rows(md);
    let example = rows
        .iter()
        .filter_map(|row| row.index)
        .find(|index| *index > 0)
        .unwrap_or(0);
    let mut hint = format!(
        "\nelement_token for row [N] = {snapshot_id}:N (row [{example}] is \"{snapshot_id}:{example}\")"
    );
    if truncated {
        if let Some(last) = rows.iter().filter_map(|row| row.index).max() {
            hint.push_str(&format!(
                "; this read stops at row [{last}], so rows past it from a larger read are not in {snapshot_id}"
            ));
        }
    }
    hint
}

/// The most recent remembered read of this window with the same view key,
/// other than the read being shaped now.
/// Whether a plain read of this window (no query, no depth limit) with
/// `max_elements` is still remembered, so a `since:"latest"` read of that
/// view has a baseline to diff against.
pub(crate) fn remembers_plain_read(pid: i64, window_id: u64, max_elements: usize) -> bool {
    let ctx = ViewContext {
        pid,
        window_id,
        key: ViewKey {
            query: None,
            max_elements,
            max_depth: None,
        },
        focus_probe: None,
    };
    latest_snapshot(&ctx, None).is_some()
}

fn latest_snapshot(ctx: &ViewContext<'_>, current_id: Option<&str>) -> Option<String> {
    let guard = store().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .iter()
        .rev()
        .find(|s| {
            s.pid == ctx.pid
                && s.window_id == ctx.window_id
                && s.key == ctx.key
                && Some(s.snapshot_id.as_str()) != current_id
                && s.at.elapsed() <= STORE_TTL
        })
        .map(|s| s.snapshot_id.clone())
}

fn fallback_text(reason: &str) -> &'static str {
    match reason {
        "unknown_snapshot" => "unknown or expired snapshot_id",
        "other_window" => "that snapshot belongs to a different window",
        "view_changed" => "query, max_elements or max_depth differ from that snapshot's read",
        "too_much_changed" => "most of the tree changed",
        "diff_too_large" => "the trees are too large to compare",
        "no_snapshot" => "this read produced no snapshot to compare against",
        "no_baseline" => {
            "no earlier read of this window with the same query, max_elements and max_depth"
        }
        _ => "cannot diff",
    }
}

/// Replace the markdown inside the platform's text block with `body`, and add
/// `header_extra` to the end of that block's first line.
fn rewrite_tree_block(content: &mut [Content], md: &str, header_extra: &str, body: &str) {
    if md.is_empty() {
        return;
    }
    for part in content.iter_mut() {
        let Content::Text { text, .. } = part else {
            continue;
        };
        let Some(pos) = text.find(md) else { continue };
        let prefix = &text[..pos];
        let suffix = &text[pos + md.len()..];
        let mut out = String::with_capacity(text.len());
        match prefix.find('\n') {
            Some(nl) => {
                out.push_str(&prefix[..nl]);
                out.push_str(header_extra);
                out.push_str(&prefix[nl..]);
            }
            None => {
                out.push_str(prefix);
                out.push_str(header_extra);
                out.push('\n');
            }
        }
        out.push_str(body);
        out.push_str(suffix);
        *text = out;
        return;
    }
}

fn trim_metadata(opts: &ViewOptions, structured: &mut Value) {
    if opts.verbose {
        return;
    }
    let degraded = structured.get("degraded").and_then(Value::as_bool) == Some(true);
    if let Some(obj) = structured.as_object_mut() {
        obj.remove("_note");
        // Linux restates the pixel-coordinate convention on every read; the
        // tool description already carries it.
        obj.remove("frame_note");

        if !degraded {
            obj.remove("background_input");
        }
    }
}

/// Keep `elements` per the requested format. `keep` limits them to the given
/// element indices (diff reads).
fn retain_elements(
    structured: &mut Value,
    format: TreeFormat,
    keep: Option<&std::collections::HashSet<u64>>,
) {
    let Some(obj) = structured.as_object_mut() else {
        return;
    };
    if format == TreeFormat::Markdown {
        obj.remove("elements");
        return;
    }
    if let (Some(keep), Some(Value::Array(elements))) = (keep, obj.get_mut("elements")) {
        elements.retain(|e| {
            e.get("element_index")
                .and_then(Value::as_u64)
                .is_some_and(|i| keep.contains(&i))
        });
    }
}

fn truncation_line(structured: &Value, key: &ViewKey) -> Option<String> {
    if structured.get("truncated").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let reason = structured
        .get("truncation_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    if reason != "node_budget" {
        return None;
    }
    let pending = structured
        .get("nodes_pending")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(format!(
        "Tree truncated at max_elements={} ({pending} more node(s) not shown). Pass a larger max_elements, or a smaller max_depth, to reach the rest.",
        key.max_elements
    ))
}

// ── snapshot memory for `since` ──────────────────────────────────────────

#[derive(Clone)]
struct StoredSnapshot {
    snapshot_id: String,
    pid: i64,
    window_id: u64,
    key: ViewKey,
    markdown: String,
    /// The read was cut short (walk timeout or node budget): it holds only
    /// the leading rows of the window's tree.
    truncated: bool,
    at: Instant,
}

fn store() -> &'static Mutex<VecDeque<StoredSnapshot>> {
    static STORE: OnceLock<Mutex<VecDeque<StoredSnapshot>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn remember(snapshot_id: &str, ctx: &ViewContext<'_>, md: &str, truncated: bool) {
    let mut guard = store().lock().unwrap_or_else(|e| e.into_inner());
    // A screenshot-only read keeps its snapshot id and returns no tree. It
    // must not erase the rows that snapshot's tokens were read from.
    if md.trim().is_empty() && guard.iter().any(|s| s.snapshot_id == snapshot_id) {
        return;
    }
    guard.retain(|s| s.snapshot_id != snapshot_id);
    guard.push_back(StoredSnapshot {
        snapshot_id: snapshot_id.to_owned(),
        pid: ctx.pid,
        window_id: ctx.window_id,
        key: ctx.key.clone(),
        markdown: md.to_owned(),
        truncated,
        at: Instant::now(),
    });
    let same_window = |s: &StoredSnapshot| s.pid == ctx.pid && s.window_id == ctx.window_id;
    while guard.iter().filter(|s| same_window(s)).count() > STORE_PER_WINDOW {
        if let Some(pos) = guard.iter().position(same_window) {
            guard.remove(pos);
        }
    }
    while guard.len() > STORE_TOTAL {
        guard.pop_front();
    }
}

/// One row of an earlier read, as that read rendered it.
pub(crate) struct StoredRow {
    pub pid: i64,
    pub window_id: u64,
    /// The markdown row, e.g. `- [14] AXButton "Save" [actions=[press]]`.
    pub line: String,
}

/// Row `[index]` of the read that produced `snapshot_id`, while that read is
/// still remembered. Lets a caller holding a stale element token find the
/// same element again by what it was.
/// The pid and window of a read this process rendered, by its snapshot id.
/// It outlives the platform snapshot (a newer read replaces that), so it
/// also names the window of a token that has gone stale.
pub(crate) fn snapshot_window(snapshot_id: &str) -> Option<(i64, u64)> {
    let mut guard = store().lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|s| s.at.elapsed() <= STORE_TTL);
    guard
        .iter()
        .find(|s| s.snapshot_id == snapshot_id)
        .map(|s| (s.pid, s.window_id))
}

pub(crate) fn snapshot_row(snapshot_id: &str, index: u64) -> Option<StoredRow> {
    let snapshot = {
        let mut guard = store().lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|s| s.at.elapsed() <= STORE_TTL);
        guard.iter().find(|s| s.snapshot_id == snapshot_id).cloned()
    }?;
    let marker = format!("[{index}] ");
    let line = snapshot
        .markdown
        .lines()
        .map(str::trim)
        .find(|line| {
            line.strip_prefix("- ")
                .is_some_and(|rest| rest.starts_with(&marker))
        })?
        .to_owned();
    Some(StoredRow {
        pid: snapshot.pid,
        window_id: snapshot.window_id,
        line,
    })
}

/// The menu path of row `[index]` of the read that produced `snapshot_id`,
/// when that row is an item in the app's menu bar: the labels of its
/// AXMenuBarItem and AXMenuItem ancestors, then its own. `None` for any
/// other row.
pub(crate) fn snapshot_menu_path(snapshot_id: &str, index: u64) -> Option<Vec<String>> {
    let markdown = {
        let guard = store().lock().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .find(|s| s.snapshot_id == snapshot_id && s.at.elapsed() <= STORE_TTL)
            .map(|s| s.markdown.clone())
    }?;
    menu_path_in(&markdown, index)
}

fn menu_path_in(markdown: &str, index: u64) -> Option<Vec<String>> {
    let lines: Vec<&str> = markdown.lines().collect();
    let marker = format!("[{index}] ");
    let at = lines.iter().position(|line| {
        line.trim_start()
            .strip_prefix("- ")
            .is_some_and(|rest| rest.starts_with(&marker))
    })?;
    let indent = |line: &str| line.len() - line.trim_start().len();
    let item = |line: &str| -> Option<(String, String)> {
        let rest = line.trim_start().strip_prefix("- ")?;
        let rest = rest
            .strip_prefix('[')
            .and_then(|r| r.split_once("] ").map(|(_, r)| r))
            .unwrap_or(rest);
        let (role, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let label = rest
            .trim_start()
            .strip_prefix('"')
            .and_then(|r| closing_quote(r).map(|end| r[..end].to_owned()))
            .unwrap_or_default();
        Some((role.to_owned(), label))
    };
    let (role, label) = item(lines[at])?;
    if !matches!(role.as_str(), "AXMenuItem" | "AXMenuBarItem") || label.is_empty() {
        return None;
    }
    let mut path = vec![label];
    let mut level = indent(lines[at]);
    for line in lines[..at].iter().rev() {
        if indent(line) >= level {
            continue;
        }
        level = indent(line);
        let (role, label) = item(line)?;
        match role.as_str() {
            "AXMenuBar" => {
                path.reverse();
                return Some(path);
            }
            "AXMenuItem" | "AXMenuBarItem" if !label.is_empty() => path.push(label),
            "AXMenu" => {}
            _ => return None,
        }
    }
    None
}

/// Remember a read's markdown as `apply` does, for tests elsewhere in the
/// crate that drive a fake get_window_state.
#[cfg(test)]
#[test]
fn menu_paths_come_from_the_menu_bar_ancestors() {
    let md = "- [0] AXWindow \"Doc\"\n  - [3] AXButton \"OK\"\n- [27] AXMenuBar\n  - [700] AXMenuBarItem \"Sheet\" [actions=[press]]\n    - [701] AXMenu\n      - [760] AXMenuItem \"Navigate\" [actions=[press]]\n        - [761] AXMenu\n          - [771] AXMenuItem \"To Next Sheet\" [actions=[press]]\n      - [780] AXMenuItem \"Delete Sheet...\"\n";
    assert_eq!(
        menu_path_in(md, 771),
        Some(vec![
            "Sheet".to_owned(),
            "Navigate".to_owned(),
            "To Next Sheet".to_owned()
        ])
    );
    assert_eq!(
        menu_path_in(md, 780),
        Some(vec!["Sheet".to_owned(), "Delete Sheet...".to_owned()])
    );
    assert_eq!(menu_path_in(md, 3), None, "not a menu item");
    assert_eq!(menu_path_in(md, 999), None);
}

#[cfg(test)]
#[test]
fn a_screenshot_only_read_keeps_the_remembered_rows() {
    remember_for_test("s7e570001", 4, 5, "- [9] AXButton \"Send\"\n");
    remember_for_test("s7e570001", 4, 5, "");
    assert!(snapshot_row("s7e570001", 9).is_some_and(|row| row.line.contains("Send")));
}

#[cfg(test)]
pub(crate) fn remember_for_test(snapshot_id: &str, pid: i64, window_id: u64, markdown: &str) {
    let ctx = ViewContext {
        pid,
        window_id,
        key: ViewKey {
            query: None,
            max_elements: DEFAULT_MAX_ELEMENTS,
            max_depth: None,
        },
        focus_probe: None,
    };
    remember(snapshot_id, &ctx, markdown, false);
}

fn resolve_since(
    since: &str,
    current_id: Option<&str>,
    md: &str,
    truncated: bool,
    ctx: &ViewContext<'_>,
) -> SinceOutcome {
    if current_id.is_none() {
        return SinceOutcome::Fallback("no_snapshot");
    }
    let found = {
        let mut guard = store().lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|s| s.at.elapsed() <= STORE_TTL);
        guard.iter().find(|s| s.snapshot_id == since).cloned()
    };
    let Some(old) = found else {
        return SinceOutcome::Fallback("unknown_snapshot");
    };
    if old.pid != ctx.pid || old.window_id != ctx.window_id {
        return SinceOutcome::Fallback("other_window");
    }
    if old.key != ctx.key {
        return SinceOutcome::Fallback("view_changed");
    }
    match diff_markdown(&old.markdown, md, old.truncated, truncated) {
        Err(DiffError::TooLarge) => SinceOutcome::Fallback("diff_too_large"),
        Ok(report) => {
            // Both sides as the caller would read them, long values cut: an
            // edit to a long document is one short changed row, not a reason
            // to resend the whole tree.
            let shown = shorten_long_values(md).len();
            if !report.is_empty() && report.render_len() * 100 > shown * DIFF_MAX_PERCENT_OF_FULL {
                SinceOutcome::Fallback("too_much_changed")
            } else {
                SinceOutcome::Diff(Box::new(report))
            }
        }
    }
}

// ── markdown row diff ────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    indent: usize,
    index: Option<u64>,
    /// Row text without indentation and without the `[N]` index.
    key: String,
    /// Role plus identifier or label; pairs a removed row with its changed
    /// replacement.
    identity: String,
    /// Row text as rendered, index included, without indentation.
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DiffReport {
    added: Vec<Row>,
    changed: Vec<Row>,
    /// The `[N]` each `changed` row had in the old read (same order), so a
    /// row whose index changed with it says so instead of reading as the
    /// element the old read had at its new index.
    changed_old_index: Vec<Option<u64>>,
    removed: Vec<Row>,
    reindexed: Vec<(u64, u64, i64)>,
    /// `role "name"` of the first old row of each `reindexed` run, so the
    /// line says which element moved.
    reindexed_labels: std::collections::HashMap<u64, String>,
    /// The new read was cut after this row: old rows past it were not
    /// reached, which says nothing about whether they still exist.
    cut_after: Option<u64>,
    /// Old rows past the new read's cut, left out of `removed`.
    unreached: usize,
    /// The old read was cut after this row.
    old_cut_after: Option<u64>,
    /// New rows past the old read's cut, left out of `added`: the old read
    /// never got that far, so they need not be new.
    first_read: Vec<Row>,
}

#[derive(Debug, PartialEq, Eq)]
enum DiffError {
    TooLarge,
}

fn parse_rows(md: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- ") {
            let indent = line.len() - trimmed.len();
            let (index, after) = match rest.strip_prefix('[') {
                Some(r) => match r.split_once(']') {
                    Some((n, tail)) if n.parse::<u64>().is_ok() => {
                        (n.parse::<u64>().ok(), tail.trim_start())
                    }
                    _ => (None, rest),
                },
                None => (None, rest),
            };
            rows.push(Row {
                indent,
                index,
                identity: identity_of(indent, after),
                key: after.to_owned(),
                text: rest.to_owned(),
            });
        } else if let Some(last) = rows.last_mut() {
            if !line.trim().is_empty() {
                // Continuation of a multi-line row (an action name with a
                // newline in it).
                last.key.push('\n');
                last.key.push_str(line);
                last.text.push('\n');
                last.text.push_str(line);
            }
        }
    }
    rows
}

fn identity_of(indent: usize, after_index: &str) -> String {
    let role = after_index.split_whitespace().next().unwrap_or("");
    // Outside quoted fields: a name or value can say `id=` too.
    let id = find_unquoted(after_index, "id=").map(|p| {
        let tail = &after_index[p + 3..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == ']')
            .unwrap_or(tail.len());
        &tail[..end]
    });
    // A quoted string right after the role is the title; after `=` it is the
    // value, which changes and so cannot identify the row.
    let label = after_index
        .split_once(char::is_whitespace)
        .map(|(_, tail)| tail.trim_start())
        .and_then(|tail| tail.strip_prefix('"'))
        .map(|tail| closing_quote(tail).map_or(tail, |end| &tail[..end]));
    format!("{indent}|{role}|{}", id.or(label).unwrap_or(""))
}

/// Diff two reads row by row. `old_cut` / `new_cut` say a read was cut short.
/// A cut read is assumed to be the leading part of the full tree: the Linux
/// walk is depth-first and renders rows in visit order (atspi/native.rs), and
/// the macOS and Windows walks say they truncate depth-first (ax/tree.rs and
/// the Windows max_elements description; not run here). The rows after the
/// last row both reads share are therefore the only ones a cut can hide:
/// there, an old row the cut new read lacks was not reached, and a new row
/// the cut old read lacks was not read before. Neither is reported as removed
/// or added
/// (A/B test of 62b714f, 2026-11-03: toolbar rows with no `[N]` came out as
/// removed, and run_steps listed menu items past its cut baseline as added).
fn diff_markdown(
    old_md: &str,
    new_md: &str,
    old_cut: bool,
    new_cut: bool,
) -> Result<DiffReport, DiffError> {
    let old = parse_rows(old_md);
    let new = parse_rows(new_md);
    let ops = diff_ops(&old, &new, old_cut || new_cut)?;

    let mut report = DiffReport::default();
    if new_cut {
        report.cut_after = new.iter().filter_map(|row| row.index).max();
    }
    if old_cut {
        report.old_cut_after = old.iter().filter_map(|row| row.index).max();
    }
    let mut dels: Vec<usize> = Vec::new();
    let mut inss: Vec<usize> = Vec::new();
    // Unchanged rows whose index moved, grouped into runs of equal delta.
    let mut run: Option<(u64, u64, i64)> = None;

    // `last` marks the hunk after the last shared row, where a cut hides rows.
    let flush_hunk =
        |dels: &mut Vec<usize>, inss: &mut Vec<usize>, report: &mut DiffReport, last: bool| {
            let mut used = vec![false; dels.len()];
            // The old row each new row is paired with.
            let mut paired: Vec<Option<usize>> = vec![None; inss.len()];
            for (k, &i) in inss.iter().enumerate() {
                let pair = dels
                    .iter()
                    .enumerate()
                    .find(|(d, &o)| !used[*d] && old[o].identity == new[i].identity);
                if let Some((d, &o)) = pair {
                    used[d] = true;
                    paired[k] = Some(o);
                }
            }
            // Both reads reached a paired row, so only rows after the last pair
            // can sit past a cut.
            let reached_old = used.iter().rposition(|&u| u).map_or(0, |d| d + 1);
            let reached_new = paired
                .iter()
                .rposition(Option::is_some)
                .map_or(0, |k| k + 1);
            for (k, &i) in inss.iter().enumerate() {
                if let Some(o) = paired[k] {
                    report.changed.push(new[i].clone());
                    report.changed_old_index.push(old[o].index);
                } else if last && old_cut && k >= reached_new {
                    report.first_read.push(new[i].clone());
                } else {
                    report.added.push(new[i].clone());
                }
            }
            for (d, &o) in dels.iter().enumerate() {
                if used[d] {
                    continue;
                }
                if last && new_cut && d >= reached_old {
                    report.unreached += 1;
                } else {
                    report.removed.push(old[o].clone());
                }
            }
            dels.clear();
            inss.clear();
        };

    for op in ops {
        match op {
            Op::Del(o) => dels.push(o),
            Op::Ins(n) => inss.push(n),
            Op::Equal(o, n) => {
                flush_hunk(&mut dels, &mut inss, &mut report, false);
                if let (Some(oi), Some(ni)) = (old[o].index, new[n].index) {
                    let delta = ni as i64 - oi as i64;
                    match (&mut run, delta) {
                        (Some((_, last_old, d)), delta2) if *d == delta2 && delta2 != 0 => {
                            *last_old = oi;
                        }
                        _ => {
                            if let Some(done) = run.take() {
                                report.reindexed.push(done);
                            }
                            if delta != 0 {
                                run = Some((oi, oi, delta));
                                report.reindexed_labels.insert(oi, row_label(&old[o].key));
                            }
                        }
                    }
                }
            }
        }
    }
    flush_hunk(&mut dels, &mut inss, &mut report, true);
    if let Some(done) = run.take() {
        report.reindexed.push(done);
    }
    Ok(report)
}

/// `push button "Open Menu"` out of a row key (role, then its first quoted
/// string, as the row escapes it).
fn row_label(key: &str) -> String {
    let open = closing_quote(key);
    let role = key[..open.unwrap_or(key.len())].trim();
    let role = role.split(" [").next().unwrap_or(role).trim();
    let name = open
        .map(|open| &key[open + 1..])
        .map(|rest| closing_quote(rest).map_or(rest, |end| &rest[..end]));
    match name {
        Some(name) if !name.is_empty() => format!("{role} \"{name}\""),
        _ => role.to_owned(),
    }
}

enum Op {
    Equal(usize, usize),
    Del(usize),
    Ins(usize),
}

/// `cut`: one read was cut short, so the two reads end at different places
/// in the tree and their last rows are not matched up.
fn diff_ops(old: &[Row], new: &[Row], cut: bool) -> Result<Vec<Op>, DiffError> {
    let mut prefix = 0;
    while prefix < old.len()
        && prefix < new.len()
        && old[prefix].key == new[prefix].key
        && old[prefix].indent == new[prefix].indent
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while !cut
        && suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix].key == new[new.len() - 1 - suffix].key
        && old[old.len() - 1 - suffix].indent == new[new.len() - 1 - suffix].indent
    {
        suffix += 1;
    }
    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];
    let (n, m) = (a.len(), b.len());
    if n.saturating_mul(m) > DIFF_CELL_LIMIT {
        return Err(DiffError::TooLarge);
    }
    let same = |x: &Row, y: &Row| x.key == y.key && x.indent == y.indent;

    let mut ops: Vec<Op> = (0..prefix).map(|i| Op::Equal(i, i)).collect();
    // dp[i][j] = LCS length of a[i..] and b[j..].
    let width = m + 1;
    let mut dp = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * width + j] = if same(&a[i], &b[j]) {
                dp[(i + 1) * width + j + 1] + 1
            } else {
                dp[(i + 1) * width + j].max(dp[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if same(&a[i], &b[j]) {
            ops.push(Op::Equal(prefix + i, prefix + j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * width + j] >= dp[i * width + j + 1] {
            ops.push(Op::Del(prefix + i));
            i += 1;
        } else {
            ops.push(Op::Ins(prefix + j));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(prefix + i));
        i += 1;
    }
    while j < m {
        ops.push(Op::Ins(prefix + j));
        j += 1;
    }
    for k in 0..suffix {
        ops.push(Op::Equal(old.len() - suffix + k, new.len() - suffix + k));
    }
    Ok(ops)
}

impl DiffReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    fn render_len(&self) -> usize {
        self.added
            .iter()
            .chain(&self.changed)
            .chain(&self.removed)
            .chain(&self.first_read)
            .map(|r| shorten_long_values(&r.text).len() + 3)
            .sum()
    }

    fn render(&self, since: &str, snapshot_id: Option<&str>, focus: Option<&str>) -> String {
        let new_id = snapshot_id.unwrap_or("the new snapshot");
        if self.is_empty() && self.first_read.is_empty() {
            let mut out = match self.cut_after {
                Some(last) => format!(
                    "no change since {since} among the rows this read reached (it stops at row [{last}])"
                ),
                None => format!("no change since {since}"),
            };
            if let Some(focus) = focus {
                out.push_str(&format!("; focused element is {focus}"));
            }
            out.push_str(&format!(
                ". Rows and indices are as before; use snapshot_id {new_id} in element_tokens ({new_id}:N)."
            ));
            return out;
        }
        // A count says how many of its rows have no `[N]` (the window's own
        // `frame` row, a label), so it matches the `[N]` rows a reader
        // counts (round 2 of the first-call test: "4 changed" with three
        // `~ [N]` rows and the frame). Round 5 read "(1 of them a row with no [N])" as
        // the row that gained an `[N]`, so it says "a row with no [N]".
        let count = |rows: &[Row], what: &str| {
            let unindexed = rows.iter().filter(|row| row.index.is_none()).count();
            if unindexed == 0 {
                format!("{} {what}", rows.len())
            } else {
                format!("{} {what} ({unindexed} of them a row with no [N])", rows.len())
            }
        };
        let mut out = format!(
            "since {since}: {}, {}, {}. Use element_tokens {new_id}:N for every row except - rows; - ids are from {since}.\n",
            count(&self.added, "added"),
            count(&self.changed, "changed"),
            count(&self.removed, "removed")
        );
        for row in &self.added {
            out.push_str(&format!("+ {}\n", row.text));
        }
        for (row, old_index) in self.changed.iter().zip(&self.changed_old_index) {
            // A changed row whose index changed too: without the note,
            // `~ [19] push button "Undo"` read as the old [19] (Open Menu)
            // turning into Undo, when Undo had just gained an index.
            let note = match (*old_index, row.index) {
                (old, new) if old == new => String::new(),
                (Some(old), _) => format!(" (was [{old}])"),
                (None, _) => " (had no [N] before)".to_owned(),
            };
            out.push_str(&format!("~ {}{note}\n", row.text));
        }
        for row in &self.removed {
            out.push_str(&format!("- {}\n", row.text));
        }
        if self.unreached > 0 {
            out.push_str(&format!(
                "{} later row(s) of {since} were not reached by this read, which stops at row [{}]; they are not removed.\n",
                self.unreached,
                self.cut_after.unwrap_or_default()
            ));
        }
        if !self.first_read.is_empty() {
            out.push_str(&format!(
                "{since} stopped at row [{}] (cut short), so these {} row(s) past it are read for the first time, not added:\n",
                self.old_cut_after.unwrap_or_default(),
                self.first_read.len()
            ));
            for row in &self.first_read {
                out.push_str(&format!("  {}\n", row.text));
            }
        }
        const MAX_RUNS: usize = 20;
        for (first_old, last_old, delta) in self.reindexed.iter().take(MAX_RUNS) {
            let label = self
                .reindexed_labels
                .get(first_old)
                .map(String::as_str)
                .unwrap_or("");
            if first_old == last_old {
                out.push_str(&format!(
                    "reindexed: [{first_old}] {label} -> [{}]\n",
                    *first_old as i64 + delta
                ));
            } else {
                out.push_str(&format!(
                    "reindexed: [{first_old}-{last_old}] -> [{}-{}] (from {label})\n",
                    *first_old as i64 + delta,
                    *last_old as i64 + delta
                ));
            }
        }
        if self.reindexed.len() > MAX_RUNS {
            out.push_str("reindexed: more shifted ranges not listed; pass full_output or omit since for a fresh index.\n");
        }
        out.truncate(out.trim_end().len());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: Value) -> ViewOptions {
        ViewOptions::from_args(&args).unwrap()
    }

    #[test]
    fn display_only_is_advertised_in_every_backend_schema() {
        let schema = extend_input_schema(json!({"type": "object", "properties": {}}));
        assert_eq!(schema["properties"]["display_only"]["type"], "boolean");
    }

    #[test]
    fn display_only_requires_a_screenshot_only_read() {
        assert_eq!(display_only(&json!({})).ok(), Some(false));
        assert_eq!(
            display_only(&json!({"display_only": false})).ok(),
            Some(false)
        );
        assert_eq!(
            display_only(&json!({"display_only": true, "include_accessibility_tree": false})).ok(),
            Some(true)
        );
        for args in [
            json!({"display_only": true}),
            json!({"display_only": true, "include_accessibility_tree": true}),
        ] {
            let refusal = display_only(&args).unwrap_err();
            assert_eq!(
                serde_json::to_value(refusal).unwrap(),
                serde_json::to_value(ToolResult::error(DISPLAY_ONLY_NEEDS_NO_TREE)).unwrap()
            );
        }
    }

    #[test]
    fn display_only_response_carries_the_frame_note() {
        let mut structured = json!({"screenshot_width": 800});
        mark_display_only(&mut structured);
        assert_eq!(structured["display_only"], true);
        assert_eq!(structured["frame_note"], DISPLAY_ONLY_FRAME_NOTE);
    }

    const OLD: &str = "- [0] AXWindow \"Doc\"\n  - [1] AXTextField \"Name\" = \"a\" [id=name]\n  - AXStaticText = \"hello\"\n  - [2] AXButton \"Save\" [id=save actions=[press]]\n  - [3] AXButton \"Cancel\" [id=cancel actions=[press]]";

    #[test]
    fn identical_trees_report_no_change() {
        let report = diff_markdown(OLD, OLD, false, false).unwrap();
        assert!(report.is_empty());
        let text = report.render(
            "s00000001",
            Some("s00000002"),
            Some("[1] AXTextField \"Name\""),
        );
        assert!(text.starts_with("no change since s00000001; focused element is [1]"));
        assert!(text.contains("s00000002:N"));
    }

    #[test]
    fn value_edit_is_a_change_and_removal_and_addition_are_reported() {
        let new = "- [0] AXWindow \"Doc\"\n  - [1] AXTextField \"Name\" = \"ab\" [id=name]\n  - AXStaticText = \"hello\"\n  - [2] AXButton \"Save\" [id=save actions=[press]]\n  - [3] AXButton \"Help\" [id=help actions=[press]]";
        let report = diff_markdown(OLD, new, false, false).unwrap();
        assert_eq!(report.changed.len(), 1);
        assert_eq!(report.changed[0].index, Some(1));
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].index, Some(3));
        let text = report.render("sA", Some("sB"), None);
        assert!(text.contains("~ [1] AXTextField \"Name\" = \"ab\""));
        assert!(text.contains("+ [3] AXButton \"Help\""));
        assert!(text.contains("- [3] AXButton \"Cancel\""));
    }

    #[test]
    fn inserted_row_reports_reindexed_range() {
        let new = "- [0] AXWindow \"Doc\"\n  - [1] AXTextField \"Name\" = \"a\" [id=name]\n  - AXStaticText = \"hello\"\n  - [2] AXCheckBox \"New\" [id=new]\n  - [3] AXButton \"Save\" [id=save actions=[press]]\n  - [4] AXButton \"Cancel\" [id=cancel actions=[press]]";
        let report = diff_markdown(OLD, new, false, false).unwrap();
        assert_eq!(report.added.len(), 1);
        assert!(report.removed.is_empty());
        assert_eq!(report.reindexed, vec![(2, 3, 1)]);
        assert!(report
            .render("sA", Some("sB"), None)
            .contains("reindexed: [2-3] -> [3-4]"));
    }

    #[test]
    fn display_only_text_changes_are_seen() {
        let new = OLD.replace("hello", "goodbye");
        let report = diff_markdown(OLD, &new, false, false).unwrap();
        assert_eq!(report.changed.len(), 1);
        assert_eq!(report.changed[0].index, None);
    }

    #[test]
    fn multiline_action_rows_stay_one_row() {
        let md = "- [0] AXWindow\n  - [1] AXButton \"x\" [actions=[press,name:move\ntarget:0x0\nselector:(null)]]\n  - [2] AXButton \"y\"";
        assert_eq!(parse_rows(md).len(), 3);
        assert!(diff_markdown(md, md, false, false).unwrap().is_empty());
    }

    #[test]
    fn options_parse_and_defaults() {
        let o = opts(json!({}));
        assert_eq!(o.tree_format, TreeFormat::Markdown);
        assert!(!o.verbose && !o.full_output && o.since.is_none());
        assert_eq!(
            opts(json!({"full_output": true})).tree_format,
            TreeFormat::Both
        );
        assert_eq!(
            opts(json!({"tree_format": "elements"})).tree_format,
            TreeFormat::Elements
        );
        assert!(ViewOptions::from_args(&json!({"tree_format": "xml"})).is_err());
        assert!(ViewOptions::from_args(&json!({"since": 3})).is_err());
        // full_output is a full read, never a diff.
        assert!(opts(json!({"full_output": true, "since": "s00000001"}))
            .since
            .is_none());
    }

    #[test]
    fn max_elements_precedence() {
        let o = opts(json!({}));
        assert_eq!(
            o.max_elements(&json!({}), 2000, false),
            DEFAULT_MAX_ELEMENTS
        );
        assert_eq!(
            o.max_elements(&json!({"max_elements": 900}), 2000, false),
            900
        );
        assert_eq!(o.max_elements(&json!({}), 2000, true), 2000);
        let full = opts(json!({"full_output": true}));
        assert_eq!(full.max_elements(&json!({}), 5000, false), 5000);
    }

    #[test]
    fn query_reads_search_past_a_small_node_budget() {
        let o = opts(json!({}));
        assert_eq!(o.max_elements(&json!({"query": "Log"}), 2000, false), 2000);
        assert_eq!(
            o.max_elements(&json!({"query": "Log", "max_elements": 80}), 2000, false),
            2000
        );
        assert_eq!(
            o.max_elements(&json!({"query": "Log", "max_elements": 3000}), 2000, false),
            3000
        );
        // A blank query is no query.
        assert_eq!(
            o.max_elements(&json!({"query": "  ", "max_elements": 80}), 2000, false),
            80
        );
    }

    fn payload(sid: &str, md: &str) -> (Vec<Content>, Value) {
        let content = vec![
            Content::image_png("AAAA".into()),
            Content::text(format!("window_id=7 pid=9 size=10x10 elements=4\n\n{md}")),
        ];
        let structured = json!({
            "window_id": 7, "pid": 9, "snapshot_id": sid,
            "tree_markdown": md,
            "elements": [{"element_index": 1}, {"element_index": 2}],
            "_note": "n", "background_input": {"routes": []},
            "element_count": 4
        });
        (content, structured)
    }

    fn ctx(window_id: u64) -> ViewContext<'static> {
        ViewContext {
            pid: 9,
            window_id,
            key: ViewKey {
                query: None,
                max_elements: 250,
                max_depth: None,
            },
            focus_probe: None,
        }
    }

    fn text_of(content: &[Content]) -> String {
        content
            .iter()
            .find_map(|c| match c {
                Content::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn default_read_is_markdown_only_without_noise() {
        let (mut content, mut s) = payload("s000000a1", OLD);
        apply(&opts(json!({})), &ctx(7001), &mut content, &mut s);
        assert!(s.get("elements").is_none());
        assert!(s.get("_note").is_none());
        assert!(s.get("background_input").is_none());
        assert_eq!(s["tree_markdown"], OLD);
        assert_eq!(s["tree_format"], "markdown");
        let text = text_of(&content);
        assert!(text.starts_with("window_id=7 pid=9 size=10x10 elements=4 snapshot_id=s000000a1\n"));
        assert!(text.contains("element_token for row [N] = s000000a1:N"));
        assert!(text.contains("row [1] is \"s000000a1:1\""));
        assert!(!text.contains("this read stops at row"));
        assert!(text.ends_with(OLD));
    }

    #[test]
    fn truncated_read_names_its_last_row() {
        let (mut content, mut s) = payload("s000000f4", OLD);
        s["truncated"] = json!(true);
        s["truncation_reason"] = json!("timeout");
        apply(&opts(json!({})), &ctx(7101), &mut content, &mut s);
        let last = parse_rows(OLD)
            .iter()
            .filter_map(|row| row.index)
            .max()
            .unwrap();
        assert!(text_of(&content).contains(&format!(
            "this read stops at row [{last}], so rows past it from a larger read are not in s000000f4"
        )));
    }

    #[test]
    fn since_null_strings_mean_no_baseline_and_truncated_diffs_say_where_they_stop() {
        for spelled in ["null", "None", " "] {
            assert_eq!(opts(json!({ "since": spelled })).since, None, "{spelled:?}");
        }
        let window = 7103;
        let (mut c1, mut s1) = payload("s000000f5", OLD);
        apply(&opts(json!({})), &ctx(window), &mut c1, &mut s1);
        let (mut c2, mut s2) = payload("s000000f6", OLD);
        s2["truncated"] = json!(true);
        apply(
            &opts(json!({"since": "latest"})),
            &ctx(window),
            &mut c2,
            &mut s2,
        );
        assert_eq!(s2["since_status"], "no_change");
        assert!(text_of(&c2).contains(
            "this read stops at row [3]: rows past it from a larger read are not in s000000f6"
        ));
    }

    #[test]
    fn since_latest_diffs_against_the_last_read_of_the_same_view() {
        let window = 7102;
        // No earlier read: full read, stated.
        let (mut c0, mut s0) = payload("s000000f0", OLD);
        apply(
            &opts(json!({"since": "latest"})),
            &ctx(window),
            &mut c0,
            &mut s0,
        );
        assert_eq!(s0["since_status"], "no_baseline");
        assert_eq!(s0["tree_markdown"], OLD);
        assert!(text_of(&c0).contains("since=latest: no earlier read of this window"));

        // A read with another view is not the baseline; the last same-view read is.
        let mut narrow = ctx(window);
        narrow.key.query = Some("Save".into());
        let (mut c1, mut s1) = payload("s000000f1", OLD);
        apply(&opts(json!({})), &narrow, &mut c1, &mut s1);

        let edited = OLD.replace("\"a\"", "\"ab\"");
        let (mut c2, mut s2) = payload("s000000f2", &edited);
        apply(
            &opts(json!({"since": "latest"})),
            &ctx(window),
            &mut c2,
            &mut s2,
        );
        assert_eq!(s2["since_status"], "diff");
        assert_eq!(s2["since"], "s000000f0");
        assert!(text_of(&c2).contains("since s000000f0:"));

        let (mut c3, mut s3) = payload("s000000f3", &edited);
        apply(
            &opts(json!({"since": "LATEST"})),
            &ctx(window),
            &mut c3,
            &mut s3,
        );
        assert_eq!(s3["since_status"], "no_change");
        assert_eq!(s3["since"], "s000000f2");
    }

    #[test]
    fn full_output_is_untouched() {
        let (mut content, mut s) = payload("s000000a2", OLD);
        let before = s.clone();
        let text_before = text_of(&content);
        apply(
            &opts(json!({"full_output": true})),
            &ctx(7002),
            &mut content,
            &mut s,
        );
        assert_eq!(s, before);
        assert_eq!(text_of(&content), text_before);
        assert_eq!(s["elements"], before["elements"]);
        assert_eq!(s["_note"], before["_note"]);
        assert_eq!(s["background_input"], before["background_input"]);
        assert_eq!(s["tree_markdown"], OLD);
    }

    fn long_value_tree(doc: &str) -> String {
        format!(
            "- [0] frame \"KWrite\" [actions=[]]\n  - [1] text \"f.txt\" value=\"{doc}\" [actions=[]]\n  - [2] push button \"Save\" [actions=[Press,Set Focus]]\n  - [3] push button \"Open\" [actions=[Press]]\n  - [4] push button \"New\" [actions=[Press]]"
        )
    }

    #[test]
    fn long_values_are_cut_in_reads_but_diffed_whole() {
        // A 300-line KWrite document as its text area's value (one_line
        // escapes the newlines).
        let doc: String = (1..=300)
            .map(|i| format!("scroll test line {i} november words\\n"))
            .collect();
        let chars = doc.chars().count();
        let md = long_value_tree(&doc);
        let (mut content, mut s) = payload("s0000f101", &md);
        apply(&opts(json!({})), &ctx(7101), &mut content, &mut s);
        let text = text_of(&content);
        assert!(
            text.contains(&format!("...\" ({chars} chars) [actions=[]]")),
            "{text}"
        );
        assert!(
            text.contains("full_output:true returns them whole"),
            "{text}"
        );
        assert!(text.len() < 1200, "{}", text.len());
        let shown = s["tree_markdown"].as_str().unwrap();
        assert!(shown.len() < 1000, "{}", shown.len());
        assert!(shown.contains("[2] push button \"Save\""));

        // An edit near the end of the document is a changed row in a short
        // diff, not a full re-read.
        let edited = long_value_tree(&doc.replace("line 299 ", "line 2x9 "));
        let (mut content, mut s) = payload("s0000f102", &edited);
        apply(
            &opts(json!({"since": "s0000f101"})),
            &ctx(7101),
            &mut content,
            &mut s,
        );
        assert_eq!(s["since_status"], "diff", "{s}");
        assert_eq!(s["diff_counts"]["changed"], 1);
        assert!(s["tree_diff"].as_str().unwrap().len() < 1000);
        assert!(text_of(&content).len() < 1200);

        // full_output returns the value whole.
        let (mut content, mut s) = payload("s0000f103", &md);
        apply(
            &opts(json!({"full_output": true})),
            &ctx(7102),
            &mut content,
            &mut s,
        );
        assert!(text_of(&content).contains(&doc));
        assert_eq!(s["tree_markdown"], md);
    }

    #[test]
    fn a_count_the_platform_wrote_is_kept_when_the_value_is_cut() {
        // The platform read 4,096 of 24,492 characters and said so; the cut
        // keeps that instead of counting the shown, escaped text.
        let doc = "line\\n".repeat(700);
        let md = format!(
            "- [1] text \"f.txt\" value=\"{doc}...\" (24492 chars, the first 4096 read) [actions=[]]"
        );
        let short = shorten_long_values(&md);
        assert!(
            short.ends_with("...\" (24492 chars, the first 4096 read) [actions=[]]"),
            "{short}"
        );
        assert_eq!(short.matches("chars").count(), 1, "{short}");
        // A whole long value with a platform count keeps it too.
        let md = format!("- [1] text \"f.txt\" value=\"{doc}\" (3500 chars) [actions=[]]");
        let short = shorten_long_values(&md);
        assert!(
            short.ends_with("...\" (3500 chars) [actions=[]]"),
            "{short}"
        );
        // Text inside a value that only looks like a count is not one.
        let md = format!(
            "- [1] entry \"q\" value=\"{}\" (5 chars) and more\" [actions=[]]",
            "y".repeat(250)
        );
        let short = shorten_long_values(&md);
        assert!(short.ends_with("...\" (270 chars) [actions=[]]"), "{short}");
    }

    #[test]
    fn short_values_and_quotes_inside_values_are_left_alone() {
        let md = "- [1] text \"a\" value=\"say \"hi\" [actions=[x]] twice\" [actions=[]]";
        assert!(matches!(
            shorten_long_values(md),
            std::borrow::Cow::Borrowed(_)
        ));
        let long = format!(
            "- [1] entry \"q\" value=\"{}\" [actions=[]]",
            "\"x\" ".repeat(80)
        );
        let short = shorten_long_values(&long);
        assert!(short.ends_with("...\" (320 chars) [actions=[]]"), "{short}");
    }

    #[test]
    fn a_cut_read_does_not_call_unreached_rows_removed() {
        let old = "- [0] W \"Doc\"\n  - [1] push button \"A\" [actions=[Press]]\n  - [2] push button \"B\" [actions=[Press]]\n  - [3] menu item \"Settings\" [actions=[Show Menu]]";
        let new = "- [0] W \"Doc\"\n  - [1] push button \"A\" [actions=[Press]]";
        assert_eq!(
            diff_markdown(old, new, false, false).unwrap().removed.len(),
            2
        );
        let report = diff_markdown(old, new, false, true).unwrap();
        assert!(report.removed.is_empty());
        let text = report.render("s1", Some("s2"), None);
        assert!(
            text.starts_with(
                "no change since s1 among the rows this read reached (it stops at row [1])"
            ),
            "{text}"
        );

        // A real removal before the cut is still reported, with the count
        // of rows the cut read did not reach.
        let new = "- [0] W \"Doc\"\n  - [2] push button \"B\" [actions=[Press]]";
        let report = diff_markdown(old, new, false, true).unwrap();
        let text = report.render("s1", Some("s2"), None);
        assert!(text.contains("- [1] push button \"A\""), "{text}");
        assert!(!text.contains("Settings"), "{text}");
        assert!(
            text.contains(
                "1 later row(s) of s1 were not reached by this read, which stops at row [2]"
            ),
            "{text}"
        );
    }

    /// A Kate window as a timed-out walk sees it: many rows first, then
    /// toolbar rows with no `[N]` (containers, disabled buttons), then the
    /// menu bar, which the walk reaches last (A/B test, 2026-11-03).
    /// `find_bar` inserts a row at [1], shifting every later index by one.
    fn kate_rows(find_bar: bool) -> Vec<String> {
        let mut rows = vec!["- [0] frame \"kate-scratch.txt - Kate\"".to_owned()];
        let mut n = 1;
        if find_bar {
            rows.push(format!("  - [{n}] label \"Find\" [actions=[]]"));
            n += 1;
        }
        for line in 0..40 {
            rows.push(format!(
                "  - [{n}] list item \"line {line}\" [actions=[Toggle]]"
            ));
            n += 1;
        }
        rows.push("  - tool bar = \"Main Toolbar\"".to_owned());
        rows.push("    - push button = \"Undo\" (disabled)".to_owned());
        rows.push("    - push button = \"Redo\" (disabled)".to_owned());
        rows.push("  - tool bar = \"Hamburger Menu Toolbar\"".to_owned());
        rows.push(format!(
            "    - [{n}] push button \"Open Menu\" [actions=[Press]]"
        ));
        n += 1;
        rows.push("  - menu bar".to_owned());
        for name in ["File", "Edit", "Settings", "Help"] {
            rows.push(format!(
                "    - [{n}] menu item \"{name}\" [actions=[Show Menu]]"
            ));
            n += 1;
        }
        rows
    }

    /// The rows of `rows` up to and including the first one containing `last`.
    fn cut_at(rows: &[String], last: &str) -> String {
        let end = rows.iter().position(|row| row.contains(last)).unwrap();
        rows[..=end].join("\n")
    }

    /// Read `md` as snapshot `sid` of `window`, optionally `since:"latest"`
    /// and optionally cut short, through the same path a tool call takes.
    fn read(window: u64, sid: &str, md: &str, cut: bool, since: bool) -> (String, Value) {
        let (mut content, mut s) = payload(sid, md);
        if cut {
            s["truncated"] = json!(true);
            s["truncation_reason"] = json!("timeout");
        }
        let args = if since {
            json!({"since": "latest"})
        } else {
            json!({})
        };
        apply(&opts(args), &ctx(window), &mut content, &mut s);
        (text_of(&content), s)
    }

    #[test]
    fn a_cut_since_read_lists_no_unreached_row_as_removed() {
        // A/B fixes-1: the re-read stopped before the toolbars; their rows
        // have no [N], so the index rule kept them as "- tool bar ...".
        let window = 7104;
        let full = kate_rows(false).join("\n");
        read(window, "s0000f201", &full, false, false);
        let cut = cut_at(&kate_rows(false), "line 39");
        let (text, s) = read(window, "s0000f202", &cut, true, true);
        assert_eq!(s["since_status"], "no_change", "{text}");
        assert!(!text.contains("\n- "), "{text}");
        assert!(text.contains("among the rows this read reached"), "{text}");
    }

    #[test]
    fn a_cut_since_read_after_a_reindex_lists_no_unreached_row_as_removed() {
        // A/B fixes-2: a row inserted at [1] moved every index up by one, so
        // the cut read's last row [44] is the old [43], and the old [44]
        // "Settings", never reached, came out as removed.
        let window = 7105;
        let full = kate_rows(false).join("\n");
        read(window, "s0000f211", &full, false, false);
        let cut = cut_at(&kate_rows(true), "\"Edit\"");
        let (text, s) = read(window, "s0000f212", &cut, true, true);
        assert_eq!(s["since_status"], "diff", "{text}");
        assert!(text.contains("+ [1] label \"Find\""), "{text}");
        assert!(
            !text.contains("Settings") && !text.contains("Help"),
            "{text}"
        );
        assert_eq!(s["diff_counts"]["removed"], 0, "{text}");
        assert!(text.contains("this read stops at row [44]"), "{text}");
    }

    #[test]
    fn rows_past_a_cut_baseline_are_not_called_added() {
        // A/B fixes-1: run_steps took its baseline with a walk that timed out
        // before the menu bar's last items; its end read reached them and
        // listed "+ [53] menu item \"Settings\"" as added.
        let window = 7106;
        let cut = cut_at(&kate_rows(false), "\"Edit\"");
        read(window, "s0000f221", &cut, true, false);
        let full = kate_rows(false).join("\n");
        let (text, s) = read(window, "s0000f222", &full, false, true);
        assert!(
            !text.contains("+ [44]") && !text.contains("+ [45]"),
            "{text}"
        );
        assert_eq!(s["diff_counts"]["added"], 0, "{text}");
        // The caller never saw them, so they are listed, as first reads.
        assert!(text.contains("[44] menu item \"Settings\""), "{text}");
        assert!(text.contains("s0000f221 stopped at row [43]"), "{text}");
    }

    #[test]
    fn a_reindexed_row_is_named() {
        // Kate's toolbar after typing: Undo became enabled and indexed, so
        // Open Menu moved from [1] to [2] (desc-test round 1, item 6).
        let old = "- [0] W \"Doc\"\n  - push button = \"Undo\" (disabled)\n  - [1] push button \"Open Menu\" [actions=[]]";
        let new = "- [0] W \"Doc\"\n  - [1] push button \"Undo\" [actions=[Press]]\n  - [2] push button \"Open Menu\" [actions=[]]";
        let report = diff_markdown(old, new, false, false).unwrap();
        let text = report.render("s1", Some("s2"), None);
        assert!(
            text.contains("reindexed: [1] push button \"Open Menu\" -> [2]"),
            "{text}"
        );
    }

    // Round 2 of the first-call test, KWrite after typing: `~ [19] push
    // button "Undo"` read as the old [19] (Open Menu) turning into Undo, and
    // "4 changed" counted the frame row, which has no [N].
    #[test]
    fn a_changed_row_says_when_its_index_changed_and_counts_say_which_have_none() {
        let old = "- frame = \"notes.txt - KWrite\"\n  - [0] text \"notes.txt\" value=\"a\"\n  - [1] push button \"1:1\" [actions=[Press]]\n  - push button = \"Undo\" (disabled)\n  - [2] push button \"Open Menu\" [actions=[]]";
        let new = "- frame = \"notes.txt * - KWrite\"\n  - [0] text \"notes.txt\" value=\"ab\"\n  - [1] push button \"1:2\" [actions=[Press]]\n  - [2] push button \"Undo\" [actions=[Press]]\n  - [3] push button \"Open Menu\" [actions=[]]";
        let report = diff_markdown(old, new, false, false).unwrap();
        let text = report.render("s1", Some("s2"), None);
        assert!(
            text.starts_with("since s1: 0 added, 4 changed (1 of them a row with no [N]), 0 removed."),
            "{text}"
        );
        assert!(
            text.contains("\n~ [2] push button \"Undo\" [actions=[Press]] (had no [N] before)\n"),
            "{text}"
        );
        assert!(
            text.contains("\n~ [0] text \"notes.txt\" value=\"ab\"\n"),
            "{text}"
        );
        assert!(
            text.contains("\n~ frame = \"notes.txt * - KWrite\"\n"),
            "{text}"
        );
        assert!(
            text.contains("reindexed: [2] push button \"Open Menu\" -> [3]"),
            "{text}"
        );

        // The other way: a control that loses its index, and one whose
        // index moves while its row changes.
        let report = diff_markdown(new, old, false, false).unwrap();
        let text = report.render("s2", Some("s3"), None);
        assert!(
            text.contains("\n~ push button = \"Undo\" (disabled) (was [2])\n"),
            "{text}"
        );
        let moved_old = "- [0] W\n  - [1] check box \"Grid\" (unchecked)";
        let moved_new =
            "- [0] W\n  - [1] push button \"New\"\n  - [2] check box \"Grid\" (checked)";
        let report = diff_markdown(moved_old, moved_new, false, false).unwrap();
        let text = report.render("s4", Some("s5"), None);
        assert!(
            text.starts_with("since s4: 1 added, 1 changed, 0 removed."),
            "{text}"
        );
        assert!(
            text.contains("\n~ [2] check box \"Grid\" (checked) (was [1])"),
            "{text}"
        );
    }

    #[test]
    fn elements_format_drops_markdown() {
        let (mut content, mut s) = payload("s000000a3", OLD);
        apply(
            &opts(json!({"tree_format": "elements"})),
            &ctx(7003),
            &mut content,
            &mut s,
        );
        assert!(s.get("tree_markdown").is_none());
        assert_eq!(s["elements"].as_array().unwrap().len(), 2);
        assert!(!text_of(&content).contains("AXButton"));
    }

    #[test]
    fn since_returns_diff_then_falls_back() {
        let window = 7004;
        let (mut c1, mut s1) = payload("s000000b1", OLD);
        apply(&opts(json!({})), &ctx(window), &mut c1, &mut s1);

        // Unchanged tree.
        let (mut c2, mut s2) = payload("s000000b2", OLD);
        apply(
            &opts(json!({"since": "s000000b1"})),
            &ctx(window),
            &mut c2,
            &mut s2,
        );
        assert_eq!(s2["since_status"], "no_change");
        assert!(s2.get("tree_markdown").is_none());
        assert!(text_of(&c2).contains("no change since s000000b1"));

        // One edit, chained against the previous snapshot.
        let edited = OLD.replace("\"a\"", "\"ab\"");
        let (mut c3, mut s3) = payload("s000000b3", &edited);
        apply(
            &opts(json!({"since": "s000000b2"})),
            &ctx(window),
            &mut c3,
            &mut s3,
        );
        assert_eq!(s3["since_status"], "diff");
        assert_eq!(s3["diff_counts"]["changed"], 1);
        assert!(text_of(&c3).contains("~ [1] AXTextField"));
        assert!(!text_of(&c3).contains("AXButton \"Save\""));
        assert!(s3.get("elements").is_none());

        // Unknown snapshot falls back to the full read.
        let (mut c4, mut s4) = payload("s000000b4", &edited);
        apply(
            &opts(json!({"since": "s0000dead"})),
            &ctx(window),
            &mut c4,
            &mut s4,
        );
        assert_eq!(s4["since_status"], "unknown_snapshot");
        assert_eq!(s4["tree_markdown"], edited);
        assert!(text_of(&c4).contains("unknown or expired snapshot_id; full read follows"));

        // Another window's snapshot is not a baseline.
        let (mut c5, mut s5) = payload("s000000b5", &edited);
        apply(
            &opts(json!({"since": "s000000b4"})),
            &ctx(window + 1),
            &mut c5,
            &mut s5,
        );
        assert_eq!(s5["since_status"], "other_window");
    }

    #[test]
    fn a_rewritten_tree_falls_back_to_full() {
        let window = 7005;
        let (mut c1, mut s1) = payload("s000000c1", OLD);
        apply(&opts(json!({})), &ctx(window), &mut c1, &mut s1);
        let other = "- [0] AXWindow \"Other\"\n  - [1] AXList \"x\"\n  - [2] AXButton \"Q\"\n  - [3] AXButton \"R\"\n  - [4] AXButton \"S\"";
        let (mut c2, mut s2) = payload("s000000c2", other);
        apply(
            &opts(json!({"since": "s000000c1"})),
            &ctx(window),
            &mut c2,
            &mut s2,
        );
        assert_eq!(s2["since_status"], "too_much_changed");
        assert_eq!(s2["tree_markdown"], other);
    }

    #[test]
    fn node_budget_truncation_is_stated() {
        let (mut content, mut s) = payload("s000000d1", OLD);
        s["truncated"] = json!(true);
        s["truncation_reason"] = json!("node_budget");
        s["nodes_pending"] = json!(40);
        apply(&opts(json!({})), &ctx(7006), &mut content, &mut s);
        let text = text_of(&content);
        assert!(text.contains("Tree truncated at max_elements=250 (40 more node(s) not shown)"));
        assert!(s["truncation_hint"]
            .as_str()
            .unwrap()
            .contains("larger max_elements"));
    }

    #[test]
    fn walk_diagnostics_are_dropped_unless_verbose() {
        let (mut content, mut s) = payload("s000000d2", OLD);
        s["nodes_visited"] = json!(261);
        s["walk_elapsed_ms"] = json!(149);
        apply(&opts(json!({})), &ctx(7016), &mut content, &mut s);
        assert!(s.get("nodes_visited").is_none());
        assert!(s.get("walk_elapsed_ms").is_none());
        let (mut content, mut s) = payload("s000000d3", OLD);
        s["nodes_visited"] = json!(261);
        apply(
            &opts(json!({"verbose": true})),
            &ctx(7017),
            &mut content,
            &mut s,
        );
        assert_eq!(s["nodes_visited"], 261);
    }

    #[test]
    fn degraded_snapshots_keep_background_input() {
        let (mut content, mut s) = payload("s000000e1", OLD);
        s["degraded"] = json!(true);
        apply(&opts(json!({})), &ctx(7007), &mut content, &mut s);
        assert!(s.get("background_input").is_some());
        let (mut content, mut s) = payload("s000000e2", OLD);
        apply(
            &opts(json!({"verbose": true})),
            &ctx(7008),
            &mut content,
            &mut s,
        );
        assert!(s.get("background_input").is_some());
        assert!(s.get("_note").is_some());
    }
}

/// Rows from the Linux AT-SPI walk escape every app string inside a quoted
/// field with `untrusted_text::inline`, so `\"` and `\\` never end a field.
/// macOS and Windows rows do not escape.
#[cfg(test)]
mod escaped_row_tests {
    use super::*;

    #[test]
    fn a_value_written_inside_a_name_does_not_hide_the_real_value() {
        // RENDER-5: the name carries ` value="` and 250 characters, so a
        // cutter that takes the first ` value="` cuts inside the name and the
        // real value (an IBAN) never shows.
        let row = format!(
            "- [5] text \"Total value=\"{}\" value=\"Pay 5000 EUR to DE00 1234\" [actions=[]]",
            "A".repeat(250)
        );
        let short = shorten_long_values(&row);
        assert!(short.contains("DE00 1234"), "{short}");
    }

    #[test]
    fn a_cut_value_never_ends_inside_an_escape() {
        // A quote at character 200 of the shown value: the cut kept the
        // backslash of `\"` and dropped its quote.
        let shown = format!("{}\\\"{}", "A".repeat(199), "B".repeat(100));
        let row = format!("- [5] text \"f\" value=\"{shown}\" (300 chars) [actions=[]]");
        let short = shorten_long_values(&row);
        assert!(
            short.contains(&format!("value=\"{}...\" (300 chars) [actions=[]]", "A".repeat(199))),
            "{short}"
        );
        // A bidi override escaped as `\u{202e}` across the cut.
        let shown = format!("{}\\u{{202e}}{}", "A".repeat(197), "B".repeat(100));
        let row = format!("- [5] text \"f\" value=\"{shown}\" (298 chars) [actions=[]]");
        let short = shorten_long_values(&row);
        assert!(
            short.contains(&format!("value=\"{}...\" (298 chars)", "A".repeat(197))),
            "{short}"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_label() {
        assert_eq!(
            identity_of(2, "frame \"Pay \\\"Bob\\\" now\" [actions=[]]"),
            "2|frame|Pay \\\"Bob\\\" now"
        );
        assert_eq!(
            row_label("push button \"Send \\\"now\\\"\" [actions=[press]]"),
            "push button \"Send \\\"now\\\"\""
        );
        let md = "- [27] AXMenuBar\n  - [700] AXMenuBarItem \"Say \\\"Hi\\\"\" [actions=[press]]\n";
        assert_eq!(
            menu_path_in(md, 700),
            Some(vec!["Say \\\"Hi\\\"".to_owned()])
        );
    }

    // macOS and Windows rows do not escape; a title that ends in a
    // backslash still reads as before.
    #[test]
    fn unescaped_rows_still_parse() {
        assert_eq!(
            identity_of(0, "AXButton \"C:\\\" [actions=[press]]"),
            "0|AXButton|C:\\"
        );
        assert_eq!(
            row_label("Button \"C:\\\" [actions=[invoke]]"),
            "Button \"C:\\\""
        );
        assert_eq!(
            identity_of(0, "Edit \"Address\" [value=\"C:\\Users\" id=addr actions=[]]"),
            "0|Edit|addr"
        );
        assert_eq!(closing_quote("plain\" rest"), Some(5));
        assert_eq!(closing_quote("C:\\"), None);
    }

    #[test]
    fn an_id_written_inside_a_name_is_not_the_row_id() {
        assert_eq!(
            identity_of(0, "AXButton \"see id=fake\" [id=real actions=[press]]"),
            "0|AXButton|real"
        );
    }
}

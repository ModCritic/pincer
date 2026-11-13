//! Opt-in plain-text `tools/call` results (`PINCER_MCP_RESULTS=text`).
//!
//! Claude Code (checked in 2.1.308) hands the model
//! `JSON.stringify(structuredContent)` whenever a result carries one and drops
//! the text blocks. That JSON repeats the text (the tree, the summary),
//! escapes every newline and quote in it, and adds metadata the model rarely
//! needs. Its MCP SDK also rejects a result without structured content from a
//! tool that advertised an `outputSchema`, so the two halves change together:
//! in text mode `tools/list` advertises no `outputSchema`, and every
//! `tools/call` result is content only, one rendered text block followed by
//! the original image (and other non-text) blocks.
//!
//! Each transport applies this to the finished response, after the result was
//! held to its contract (`crate::mcp_result`) and after the call was observed,
//! so session history and telemetry read the same result in either mode. Any
//! other value of the variable, or none, leaves responses byte-identical.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{json, Map, Value};

use crate::protocol::{Response, ResponseBody};

pub const RESULTS_ENV: &str = "PINCER_MCP_RESULTS";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultsMode {
    /// `outputSchema` advertised and `structuredContent` sent, as always.
    #[default]
    Structured,
    /// Content-only results rendered as text.
    Text,
}

impl ResultsMode {
    /// Only the exact value `text` opts in.
    pub fn from_env_value(value: Option<&std::ffi::OsStr>) -> Self {
        if value.is_some_and(|value| value == "text") {
            Self::Text
        } else {
            Self::Structured
        }
    }
}

static CONFIGURED: LazyLock<ResultsMode> =
    LazyLock::new(|| ResultsMode::from_env_value(std::env::var_os(RESULTS_ENV).as_deref()));

/// The mode this process was started with, read once.
pub fn configured() -> ResultsMode {
    *CONFIGURED
}

/// Shape one finished MCP response for the client under the configured mode.
pub fn for_client(method: &str, response: Response) -> Response {
    apply(configured(), method, response)
}

/// Shape one finished MCP response for the client. Only `tools/list` and
/// `tools/call` results change, and only in [`ResultsMode::Text`].
pub fn apply(mode: ResultsMode, method: &str, mut response: Response) -> Response {
    if mode == ResultsMode::Text {
        if let ResponseBody::Result { result } = &mut response.body {
            match method {
                "tools/list" => strip_output_schemas(result),
                "tools/call" => render_tool_result(result),
                _ => {}
            }
        }
    }
    response
}

/// Remove `outputSchema` from every tool entry of a `tools/list` result.
pub fn strip_output_schemas(result: &mut Value) {
    if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
            tool.remove("outputSchema");
        }
    }
}

/// Turn one `tools/call` result into content only: the rendered text first,
/// then the original non-text blocks in order. `isError`, `_meta` and any
/// other envelope field stay as they were. A result without structured
/// content keeps its content as is.
pub fn render_tool_result(result: &mut Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let blocks = match object.get("content") {
        None => Vec::new(),
        Some(Value::Array(blocks)) => blocks.clone(),
        // A malformed content field is left for the client to report.
        Some(_) => return,
    };
    let Some(structured) = object.remove("structuredContent") else {
        return;
    };
    if structured.is_null() {
        return;
    }
    let is_error = object.get("isError").and_then(Value::as_bool) == Some(true);
    let mut texts = Vec::new();
    let mut others = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            texts.push(block["text"].as_str().unwrap_or_default().to_owned());
        } else {
            others.push(block);
        }
    }
    let text = render_text(&texts.join("\n"), &structured, is_error);
    let mut content = Vec::with_capacity(others.len() + 1);
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    content.extend(others);
    object.insert("content".into(), Value::Array(content));
}

/// Render the result's text (`text`, its text blocks joined with newlines)
/// together with its structured payload.
///
/// - An error with a `code`: the text, `[code: …]`, then the other fields.
/// - An action result (`effect` plus `summary` or `route`): the producer's
///   text, one `[effect: …; delivery: …]` line, evidence details the text
///   does not already say, then the other fields. `route` is never shown:
///   names such as `global_input` read as "the real pointer moved".
/// - Anything else: the text, then every field.
///
/// The fields after the text are deduplicated one top-level key at a time:
/// a key is left out only when the text already says its whole value (see
/// [`skip_field`]), and a key that stays is printed whole. Nothing inside an
/// object or an array is dropped for being in the text, so a list or object
/// in the tail is never a partial copy that reads as complete.
pub fn render_text(text: &str, structured: &Value, is_error: bool) -> String {
    render_text_with(text, structured, is_error, TOKEN_SCANS_BEFORE_INDEX)
}

/// [`render_text`], with [`Shown::has_token`] indexing the text after
/// `scans_before_index` scans (tests compare both paths).
fn render_text_with(
    text: &str,
    structured: &Value,
    is_error: bool,
    scans_before_index: u32,
) -> String {
    let mut lines = Vec::new();
    let shown = Shown::new(text, scans_before_index);
    let Value::Object(fields) = structured else {
        push_body(&mut lines, text);
        if !value_shown("", structured, &shown) {
            lines.push(render_value(structured, false, &shown));
        }
        return lines.join("\n");
    };
    let code = fields
        .get("code")
        .and_then(Value::as_str)
        .filter(|_| is_error);
    let effect = fields.get("effect").and_then(Value::as_str);
    let action = effect.is_some()
        && (fields.get("summary").is_some_and(Value::is_string)
            || fields.get("route").is_some_and(Value::is_string));
    match (code, effect) {
        (Some(code), _) => {
            push_body(&mut lines, text);
            lines.push(format!("[code: {code}]"));
            push_fields(&mut lines, fields, &shown, &["code"]);
        }
        (None, Some(effect)) if action => {
            render_action(&mut lines, fields, effect, text, scans_before_index)
        }
        _ => {
            push_body(&mut lines, text);
            push_fields(&mut lines, fields, &shown, &[]);
        }
    }
    lines.join("\n")
}

fn render_action(
    lines: &mut Vec<String>,
    fields: &Map<String, Value>,
    effect: &str,
    text: &str,
    scans_before_index: u32,
) {
    let summary = fields
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // `summary` is a copy of the producer's first text block, so the text
    // normally contains it and carries any later blocks as well.
    let body = if text.is_empty() {
        summary.to_owned()
    } else if summary.is_empty() || text.contains(summary) {
        text.to_owned()
    } else {
        format!("{summary}\n{text}")
    };
    push_body(lines, &body);
    let shown = Shown::new(&body, scans_before_index);

    let delivery = fields.get("delivery").and_then(Value::as_object);
    let mut tags = vec![format!("effect: {effect}")];
    if let Some(mode) = delivery
        .and_then(|delivery| delivery.get("mode"))
        .and_then(Value::as_str)
        .filter(|mode| !matches!(*mode, "not_applicable" | "unknown"))
    {
        tags.push(format!("delivery: {mode}"));
    }
    if let Some(count) = delivery
        .and_then(|delivery| delivery.get("delivered_count"))
        .filter(|count| !count.is_null())
    {
        tags.push(format!("delivered_count: {count}"));
    }
    lines.push(format!("[{}]", tags.join("; ")));

    let details = fields
        .get("evidence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|evidence| evidence.get("detail").and_then(Value::as_str));
    for detail in details {
        let said = normalized(detail);
        if !said.is_empty() && !shown.normalized.contains(&said) {
            push_multiline(lines, "evidence", detail);
        }
    }

    // Whatever the bracket line did not show goes through the generic tail;
    // `route` is dropped on purpose, not shown anywhere.
    let mut rest = fields.clone();
    for key in ["summary", "effect", "route", "evidence"] {
        rest.remove(key);
    }
    if let Some(Value::Object(delivery)) = rest.get_mut("delivery") {
        delivery.remove("mode");
        delivery.remove("delivered_count");
    }
    push_fields(lines, &rest, &shown, &[]);
}

fn push_body(lines: &mut Vec<String>, text: &str) {
    let text = text.trim_end();
    if !text.is_empty() {
        lines.push(text.to_owned());
    }
}

/// [`Shown::has_token`] scans the text this many times before it indexes
/// it: an index costs a few scans to build, and most results check only a
/// handful of tokens.
const TOKEN_SCANS_BEFORE_INDEX: u32 = 16;

/// The text the rendered result already shows, as written and normalized.
struct Shown<'a> {
    raw: &'a str,
    normalized: String,
    /// Scans [`Shown::has_token`] makes before it builds `starts`.
    scans_left: Cell<u32>,
    /// Where tokens may start in `raw`, built after `scans_left` scans. A
    /// result checks each leaf of each container with `has_token`, which
    /// scanned the whole text per leaf: about 3 ms for list_apps' 800
    /// entries.
    starts: OnceCell<TokenStarts>,
    /// Lines where tokens under three bytes and `key=value` flags occur as
    /// whole tokens. The index cannot serve those, so each would scan the
    /// whole text again; they repeat a lot (`0`, `1`, `running=true` once
    /// per app) and are few, so the map stays small.
    memo: RefCell<HashMap<String, Vec<usize>>>,
    /// Byte offset where each line of `raw` starts, built on first use.
    line_starts: OnceCell<Vec<usize>>,
}

impl<'a> Shown<'a> {
    fn new(raw: &'a str, scans_before_index: u32) -> Self {
        Self {
            raw,
            normalized: normalized(raw),
            scans_left: Cell::new(scans_before_index),
            starts: OnceCell::new(),
            memo: RefCell::new(HashMap::new()),
            line_starts: OnceCell::new(),
        }
    }

    /// The line of `raw` that byte offset `at` is on, counted from 0.
    fn line_of(&self, at: usize) -> usize {
        let starts = self.line_starts.get_or_init(|| {
            std::iter::once(0)
                .chain(self.raw.match_indices('\n').map(|(at, _)| at + 1))
                .collect()
        });
        starts.partition_point(|&start| start <= at) - 1
    }

    /// The lines (ascending, no repeats) holding each offset `offsets` yields.
    fn lines_of(&self, offsets: impl Iterator<Item = usize>) -> Vec<usize> {
        let mut lines: Vec<usize> = offsets.map(|at| self.line_of(at)).collect();
        lines.dedup();
        lines
    }

    /// `compute`'s lines for `key`, computed once.
    fn memoized(&self, key: String, compute: impl FnOnce() -> Vec<usize>) -> Vec<usize> {
        if let Some(known) = self.memo.borrow().get(&key) {
            return known.clone();
        }
        let lines = compute();
        self.memo.borrow_mut().insert(key, lines.clone());
        lines
    }

    /// Lines where the text states the flag `key` as `key=value` or
    /// `key: value`.
    fn flag_lines(&self, key: &str, value: bool) -> Vec<usize> {
        self.memoized(format!("{key}\u{0}{value}"), || {
            let mut lines = self.lines_of(
                [format!("{key}={value}"), format!("{key}: {value}")]
                    .iter()
                    .flat_map(|needle| assignments(needle, self.raw)),
            );
            lines.sort_unstable();
            lines.dedup();
            lines
        })
    }

    /// Offsets in `raw` where `token` may start, ascending, a superset of
    /// its matches. `None` (scan instead) for a token under three bytes and
    /// until the scans before indexing are used up.
    fn candidate_starts(&self, token: &str) -> Option<&[u32]> {
        let key: [u8; 3] = token.as_bytes().get(..3)?.try_into().ok()?;
        if self.starts.get().is_none() {
            let left = self.scans_left.get();
            if left > 0 || u32::try_from(self.raw.len()).is_err() {
                self.scans_left.set(left.saturating_sub(1));
                return None;
            }
        }
        Some(
            self.starts
                .get_or_init(|| TokenStarts::new(self.raw))
                .candidates(key),
        )
    }

    /// A long string the text already says, allowing for case, spacing,
    /// quoting and trailing punctuation (a tree, a summary, a window title
    /// in the header, a suggestion reworded only in its punctuation). Short
    /// strings stay: they are mostly ids and enum values.
    fn restates(&self, value: &str) -> bool {
        let value_normalized = normalized(value);
        value.chars().count() >= 16
            && !value_normalized.is_empty()
            && self.normalized.contains(&value_normalized)
    }

    /// `token` appears in the text as a whole token: not run into a longer
    /// number (`1354` matches `2560x1354`) or a longer word.
    fn has_token(&self, token: &str) -> bool {
        if token.len() < 3 {
            return !self.token_lines(token).is_empty();
        }
        self.find_token(token, |_| true)
    }

    /// The lines where `token` appears as a whole token.
    fn token_lines(&self, token: &str) -> Vec<usize> {
        let scan = || {
            let mut offsets = Vec::new();
            self.find_token(token, |at| {
                offsets.push(at);
                false
            });
            self.lines_of(offsets.into_iter())
        };
        if token.len() < 3 {
            self.memoized(token.to_owned(), scan)
        } else {
            scan()
        }
    }

    /// Call `found` with the offset of each whole-token match of `token`,
    /// leftmost first, until it returns `true`; whether it did.
    fn find_token(&self, token: &str, mut found: impl FnMut(usize) -> bool) -> bool {
        let (Some(first), Some(last)) = (token.chars().next(), token.chars().next_back()) else {
            return false;
        };
        let whole = |at: usize| {
            let before = self.raw[..at].chars().next_back();
            let after = self.raw[at + token.len()..].chars().next();
            !before.is_some_and(|c| continues(first, c))
                && !after.is_some_and(|c| continues(last, c))
        };
        let Some(candidates) = self.candidate_starts(token) else {
            return self
                .raw
                .match_indices(token)
                .any(|(at, _)| whole(at) && found(at));
        };
        // The same matches `match_indices` yields: leftmost first, and none
        // overlapping the previous one ("a-a" in "xa-a-a" is only at 1).
        let mut free_from = 0;
        for &at in candidates {
            let at = at as usize;
            if at < free_from || !self.raw.as_bytes()[at..].starts_with(token.as_bytes()) {
                continue;
            }
            free_from = at + token.len();
            if whole(at) && found(at) {
                return true;
            }
        }
        false
    }
}

/// Every offset of a text where a UTF-8 character starts (and three bytes
/// follow), grouped by a hash of those three bytes, each group ascending: a
/// counting sort, two passes and no per-group allocation. A group can hold
/// other byte triples that share its hash; callers compare the text anyway.
struct TokenStarts {
    /// Group `g` is `offsets[group_starts[g]..group_starts[g + 1]]`.
    group_starts: Vec<u32>,
    offsets: Vec<u32>,
    bits: u32,
}

impl TokenStarts {
    fn new(raw: &str) -> Self {
        let bytes = raw.as_bytes();
        let bits = bytes.len().next_power_of_two().max(2).trailing_zeros();
        let starts = || {
            bytes
                .windows(3)
                .enumerate()
                // A UTF-8 continuation byte never starts a match.
                .filter(|(_, window)| (window[0] as i8) >= -0x40)
                .map(move |(at, window)| {
                    (
                        at as u32,
                        Self::group([window[0], window[1], window[2]], bits),
                    )
                })
        };
        let mut group_starts = vec![0u32; (1 << bits) + 1];
        for (_, group) in starts() {
            group_starts[group + 1] += 1;
        }
        for group in 1..group_starts.len() {
            group_starts[group] += group_starts[group - 1];
        }
        let mut next = group_starts.clone();
        let mut offsets = vec![0u32; *group_starts.last().unwrap_or(&0) as usize];
        for (at, group) in starts() {
            offsets[next[group] as usize] = at;
            next[group] += 1;
        }
        Self {
            group_starts,
            offsets,
            bits,
        }
    }

    /// Fibonacci hashing of the three bytes into `bits` bits.
    fn group(key: [u8; 3], bits: u32) -> usize {
        let value = u64::from(u32::from_le_bytes([key[0], key[1], key[2], 0]));
        (value.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize
    }

    fn candidates(&self, key: [u8; 3]) -> &[u32] {
        let group = Self::group(key, self.bits);
        &self.offsets[self.group_starts[group] as usize..self.group_starts[group + 1] as usize]
    }
}

/// Whether `neighbor` would run on from a token that ends in `edge`.
fn continues(edge: char, neighbor: char) -> bool {
    if edge.is_ascii_digit() {
        neighbor.is_ascii_digit()
    } else if edge.is_alphanumeric() || edge == '_' {
        neighbor.is_alphanumeric() || neighbor == '_'
    } else {
        false
    }
}

/// Lowercase, quotes removed, whitespace collapsed, trailing punctuation
/// trimmed, so two spellings of one sentence compare equal.
fn normalized(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for word in value.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(
            word.chars()
                .filter(|c| !matches!(c, '"' | '\'' | '`'))
                .flat_map(char::to_lowercase),
        );
    }
    out.trim_end_matches(['.', ',', ';', ':', '!', '?', ' '])
        .to_owned()
}

/// Metadata the rendered result already conveys another way, or an echo of
/// the call's own budget: the tree shows its format, an image block carries
/// its own `mimeType`, and `timeout_ms` only repeats the walk budget (a walk
/// cut by it says so through `truncated` and the text).
const NOISE_KEYS: &[&str] = &["tree_format", "screenshot_mime_type", "timeout_ms"];

/// One `key: value` line per field, in the object's key order. A field is
/// left out whole or printed whole, never in part.
fn push_fields(
    lines: &mut Vec<String>,
    fields: &Map<String, Value>,
    shown: &Shown,
    except: &[&str],
) {
    for (key, value) in fields {
        if except.contains(&key.as_str())
            || skip_field(key, value, fields, shown)
            || restated_assignment(key, value, shown.raw)
            || restated_scalar(key, value, shown)
        {
            continue;
        }
        match value {
            Value::Array(items) if items.iter().any(|item| item.is_object() || item.is_array()) => {
                lines.push(format!("{key}:"));
                lines.extend(
                    items
                        .iter()
                        .map(|item| format!("- {}", render_value(item, true, shown))),
                );
            }
            Value::String(value) if value.contains('\n') => push_multiline(lines, key, value),
            _ => lines.push(format!("{key}: {}", render_value(value, false, shown))),
        }
    }
}

/// `key: first line`, then each further line indented, with no escaping.
fn push_multiline(lines: &mut Vec<String>, key: &str, value: &str) {
    let mut rows = value.trim_end_matches('\n').lines();
    lines.push(format!("{key}: {}", rows.next().unwrap_or_default()));
    lines.extend(rows.map(|row| format!("  {row}")));
}

/// A top-level field to leave out: noise, an empty value, a default flag,
/// a copy of a sibling, or a value the text already says whole.
fn skip_field(key: &str, value: &Value, siblings: &Map<String, Value>, shown: &Shown) -> bool {
    if NOISE_KEYS.contains(&key) {
        return true;
    }
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty() || (!protected(key, value) && shown.restates(text)),
        Value::Array(items) => items.is_empty() || value_shown(key, value, shown),
        Value::Object(fields) => fields.is_empty() || value_shown(key, value, shown),
        Value::Bool(true) => key.ends_with("_complete") || key.ends_with("_valid"),
        Value::Bool(false) => key == "truncated",
        Value::Number(_) => duplicate_geometry(key, value, siblings),
    }
}

/// Flat window geometry next to a `bounds` object that holds it (the
/// legacy `x`/`y`/`width`/`height` aliases of a window record).
fn duplicate_geometry(key: &str, value: &Value, siblings: &Map<String, Value>) -> bool {
    matches!(key, "x" | "y" | "width" | "height")
        && siblings.get("bounds").and_then(|bounds| bounds.get(key)) == Some(value)
}

/// A field the dedupe rules must keep whatever the text says: an error or a
/// warning, a cut tree, or an incomplete walk.
fn protected(key: &str, value: &Value) -> bool {
    let flagged = key.contains("error") || key.contains("warning");
    (flagged && !matches!(value, Value::Null | Value::Bool(false)))
        || (key == "truncated" && value == &Value::Bool(true))
        || (key.ends_with("_complete") && value == &Value::Bool(false))
}

/// Flags a text may leave unsaid because they hold their usual value: a
/// window line that does not say it is off screen is on screen, and Linux
/// apps are never `active` (list_apps reserves the field). Any other flag
/// counts as said only as `key=value` or `key: value`.
const USUAL_FLAGS: &[(&str, bool)] = &[("is_on_screen", true), ("active", false)];

/// The text already says the whole of an object or array. An array of
/// objects or arrays is a list of records, each checked on its own; any
/// other value is one record. Never true for a value holding something
/// [`protected`].
fn value_shown(key: &str, value: &Value, shown: &Shown) -> bool {
    match value {
        Value::Array(items) if items.iter().any(|item| item.is_object() || item.is_array()) => {
            items.iter().all(|item| record_shown(key, item, shown))
        }
        _ => record_shown(key, value, shown),
    }
}

/// The text says every leaf of one record:
///
/// - a string or number of 3+ characters appears anywhere as a whole token
///   (a long string may also differ in case, spacing, quoting and trailing
///   punctuation); at least one such leaf is required;
/// - a shorter string or number (`0`, `22`, `on`), and a flag stated as
///   `key=value` or `key: value`, appears on a line that shows one of the
///   record's longer values, since a short value elsewhere in the text
///   ("4 ms") says nothing about this record;
/// - a flag in [`USUAL_FLAGS`], a null and an empty string need nothing.
///
/// This rests on producers keeping each record's short values on the same
/// line as its ids (list_windows' `z=0` after the window_id), which every
/// list in this crate's tests and the Linux tools does.
fn record_shown(key: &str, record: &Value, shown: &Shown) -> bool {
    let mut leaves = Vec::new();
    if !collect_leaves(key, record, &mut leaves) {
        return false;
    }
    let mut identifying = Vec::new();
    let mut short = Vec::new();
    for (key, leaf) in leaves {
        match leaf {
            Value::Null => {}
            Value::String(text) if text.is_empty() => {}
            Value::String(text) if text.chars().count() >= 3 => {
                if shown.has_token(text) {
                    identifying.push(text.clone());
                } else if !shown.restates(text) {
                    return false;
                }
            }
            Value::Number(number) if number.to_string().len() >= 3 => {
                let number = number.to_string();
                if !shown.has_token(&number) {
                    return false;
                }
                identifying.push(number);
            }
            Value::Bool(flag) if USUAL_FLAGS.contains(&(key, *flag)) => {}
            leaf => short.push((key, leaf)),
        }
    }
    if identifying.is_empty() {
        return false;
    }
    if short.is_empty() {
        return true;
    }
    let mut lines: Vec<usize> = identifying
        .iter()
        .flat_map(|token| shown.token_lines(token))
        .collect();
    lines.sort_unstable();
    lines.dedup();
    short.into_iter().all(|(key, leaf)| {
        let leaf_lines = match leaf {
            Value::Bool(flag) => shown.flag_lines(key, *flag),
            Value::Number(number) => shown.token_lines(&number.to_string()),
            Value::String(text) => shown.token_lines(text),
            _ => Vec::new(),
        };
        leaf_lines
            .iter()
            .any(|line| lines.binary_search(line).is_ok())
    })
}

/// Gather a record's leaves with the key each sits under (an array item
/// takes its array's key). `false` when anything inside it is
/// [`protected`].
fn collect_leaves<'v>(
    key: &'v str,
    value: &'v Value,
    leaves: &mut Vec<(&'v str, &'v Value)>,
) -> bool {
    if protected(key, value) {
        return false;
    }
    match value {
        Value::Object(fields) => fields.iter().all(|(key, value)| {
            NOISE_KEYS.contains(&key.as_str()) || collect_leaves(key, value, leaves)
        }),
        Value::Array(items) => items.iter().all(|item| collect_leaves(key, item, leaves)),
        leaf => {
            leaves.push((key, leaf));
            true
        }
    }
}

/// A top-level scalar the text already prints as `key=value`, such as the
/// `window_id=… pid=… snapshot_id=…` header of get_window_state.
fn restated_assignment(key: &str, value: &Value, text: &str) -> bool {
    let value = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => return false,
    };
    assignments(&format!("{key}={value}"), text)
        .next()
        .is_some()
}

/// Offsets where `needle` (`key=value`) stands as a whole assignment: no
/// word character before it, none or a `.` after it.
fn assignments<'t>(needle: &'t str, text: &'t str) -> impl Iterator<Item = usize> + 't {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(needle).filter_map(move |(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + needle.len()..].chars().next();
        (!before.is_some_and(word) && !after.is_some_and(|c| word(c) || c == '.')).then_some(at)
    })
}

/// A top-level string or number of 3+ characters that the text shows as a
/// whole token next to its own name: `pid` and `launcher_pid` for
/// "(pid 90758)", `since` for "no change since s00000001". The name match
/// takes the whole key or its last `_` segment.
fn restated_scalar(key: &str, value: &Value, shown: &Shown) -> bool {
    if protected(key, value) {
        return false;
    }
    let value = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => return false,
    };
    let last = key.rsplit('_').next().unwrap_or(key);
    value.chars().count() >= 3
        && shown.has_token(&value)
        && (shown.has_token(key) || shown.has_token(last))
}

/// Stands in for a multi-line string inside a kept value that the text
/// already shows whole (run_steps' observation holds the window's whole
/// tree as `tree_markdown`, which the text prints above it).
const SAME_AS_TEXT: &str = "(same as the text above)";

/// One value on a single line: objects as `{k: v, …}`, arrays as `[a, …]`.
/// Printed whole: inside it only null and empty fields, [`NOISE_KEYS`] and
/// the flat geometry copy of a sibling `bounds` are left out, none of which
/// depends on the text. A multi-line string the text restates keeps its
/// key, with [`SAME_AS_TEXT`] in place of a second, escaped copy.
fn render_value(value: &Value, nested: bool, shown: &Shown) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) if nested && value.contains('\n') && shown.restates(value) => {
            SAME_AS_TEXT.to_owned()
        }
        Value::String(value) => scalar_string(value, nested),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| render_value(item, true, shown))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .filter(|(key, value)| {
                    !empty_or_noise(key, value) && !duplicate_geometry(key, value, fields)
                })
                .map(|(key, value)| format!("{key}: {}", render_value(value, true, shown)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn empty_or_noise(key: &str, value: &Value) -> bool {
    NOISE_KEYS.contains(&key)
        || match value {
            Value::Null => true,
            Value::String(value) => value.is_empty(),
            Value::Array(items) => items.is_empty(),
            Value::Object(fields) => fields.is_empty(),
            _ => false,
        }
}

/// A string unquoted unless it could be misread: surrounding whitespace, a
/// leading quote or bracket, a control character, a word or number that
/// reads as another type, or (inside `{…}` and `[…]`) a separator.
fn scalar_string(value: &str, nested: bool) -> String {
    let ambiguous = value.trim() != value
        || value.starts_with(['"', '[', '{'])
        || value.chars().any(char::is_control)
        || matches!(value, "true" | "false" | "null")
        || value.parse::<f64>().is_ok()
        || (nested && value.contains([',', '{', '}', '[', ']']));
    if ambiguous {
        serde_json::to_string(value).unwrap_or_else(|_| value.to_owned())
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    //! Fixtures are trimmed copies of real Linux results captured from the
    //! installed binary against Kate.

    use super::*;

    fn text_of(result: &Value) -> &str {
        result["content"][0]["text"].as_str().unwrap()
    }

    fn rendered(result: Value) -> Value {
        let response = apply(
            ResultsMode::Text,
            "tools/call",
            Response::ok(json!(1), result),
        );
        serde_json::to_value(response).unwrap()["result"].clone()
    }

    #[test]
    fn only_the_exact_value_text_opts_in() {
        use std::ffi::OsStr;
        assert_eq!(
            ResultsMode::from_env_value(Some(OsStr::new("text"))),
            ResultsMode::Text
        );
        for other in ["", "TEXT", "text ", "1", "structured"] {
            assert_eq!(
                ResultsMode::from_env_value(Some(OsStr::new(other))),
                ResultsMode::Structured
            );
        }
        assert_eq!(ResultsMode::from_env_value(None), ResultsMode::Structured);
    }

    #[test]
    fn tools_list_loses_only_its_output_schemas() {
        let list = json!({
            "tools": [
                {"name": "click", "inputSchema": {"type": "object"}, "outputSchema": {"type": "object"}},
                {"name": "get_config", "inputSchema": {"type": "object"}},
            ],
            "schema_version": "1",
        });
        let response = apply(
            ResultsMode::Text,
            "tools/list",
            Response::ok(json!(1), list),
        );
        let result = serde_json::to_value(response).unwrap()["result"].clone();

        assert_eq!(
            result,
            json!({
                "tools": [
                    {"name": "click", "inputSchema": {"type": "object"}},
                    {"name": "get_config", "inputSchema": {"type": "object"}},
                ],
                "schema_version": "1",
            })
        );
    }

    #[test]
    fn launch_app_keeps_the_window_id_the_next_call_needs() {
        let result = rendered(json!({
            "content": [{"type": "text", "text": "✅ Launched kate (pid 90758) in background."}],
            "structuredContent": {
                "active": false, "bundle_id": null, "launcher_pid": 90758, "name": "kate",
                "pid": 90758, "running": true,
                "windows": [{
                    "app_name": "kate", "bounds": {"height": 1354, "width": 2560, "x": 0, "y": 22},
                    "height": 1354, "is_on_screen": true, "pid": 90758,
                    "title": "kate-scratch.txt  — Kate", "width": 2560, "window_id": 94371847,
                    "x": 0, "y": 22, "z_index": 5,
                }],
            },
        }));

        assert!(result.get("structuredContent").is_none());
        assert_eq!(
            text_of(&result),
            "✅ Launched kate (pid 90758) in background.\n\
             active: false\n\
             name: kate\n\
             running: true\n\
             windows:\n\
             - {app_name: kate, bounds: {height: 1354, width: 2560, x: 0, y: 22}, \
             is_on_screen: true, pid: 90758, title: kate-scratch.txt  — Kate, \
             window_id: 94371847, z_index: 5}"
        );
    }

    #[test]
    fn an_action_result_ends_with_its_effect_line_and_new_evidence() {
        let summary = "Typed 5 character(s) into element [23] (via targeted AT-SPI); it reads back 'hello\nLine two.\n'.";
        let result = rendered(json!({
            "content": [{"type": "text", "text": summary}],
            "structuredContent": {
                "delivery": {"mode": "background"},
                "effect": "confirmed",
                "evidence": [
                    {"kind": "value_readback"},
                    {"detail": "element [23] reads back the typed text", "kind": "value_readback"},
                ],
                "route": "accessibility",
                "summary": summary,
            },
        }));

        assert_eq!(
            text_of(&result),
            format!(
                "{}\n[effect: confirmed; delivery: background]\n\
                 evidence: element [23] reads back the typed text",
                summary.trim_end()
            )
        );
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn an_action_result_omits_a_delivery_mode_that_says_nothing() {
        let summary =
            "Agent cursor 'probe' moved to (600.0, 400.0); the user pointer was unchanged.";
        let result = rendered(json!({
            "content": [{"type": "text", "text": summary}],
            "structuredContent": {
                "delivery": {"mode": "not_applicable"},
                "effect": "unverifiable",
                "route": "synthetic_events",
                "summary": summary,
            },
        }));

        assert_eq!(
            text_of(&result),
            format!("{summary}\n[effect: unverifiable]")
        );
    }

    #[test]
    fn a_refusal_names_its_code_and_drops_what_the_text_repeats() {
        let result = rendered(json!({
            "content": [{"type": "text", "text": "Background delivery is not available: the requested target has no focus-free input backend. Retry this action with delivery_mode:\"foreground\"."}],
            "isError": true,
            "_meta": {"trace": "kept"},
            "structuredContent": {
                "code": "background_unavailable",
                "detail": "the requested target has no focus-free input backend",
                "escalation": {"reason": "background input is unavailable on this surface", "recommended": "foreground"},
                "suggestion": "Retry this action with delivery_mode:\"foreground\".",
            },
        }));

        assert_eq!(result["isError"], true);
        assert_eq!(result["_meta"], json!({"trace": "kept"}));
        assert_eq!(
            text_of(&result),
            "Background delivery is not available: the requested target has no focus-free \
             input backend. Retry this action with delivery_mode:\"foreground\".\n\
             [code: background_unavailable]\n\
             escalation: {reason: background input is unavailable on this surface, recommended: foreground}"
        );
    }

    #[test]
    fn a_window_state_keeps_its_image_after_the_text_and_drops_restated_metadata() {
        let header =
            "window_id=94371847 pid=90758 elements=1 walk_ms=907 snapshot_id=s00000001\n\n";
        let tree = "- frame = \"kate-scratch.txt  — Kate\"\n  - [0] push button \"Save\" [actions=[Press]]\n";
        let image = json!({"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="});
        let result = rendered(json!({
            "content": [{"type": "text", "text": format!("{header}{tree}")}, image.clone()],
            "structuredContent": {
                "app_name": "kate", "bounds_complete": true, "capture_id": "capture_53dc_0",
                "coordinate_frame": "window", "element_count": 1, "elements_complete": true,
                "frame_scale": 0.57578125, "pid": 90758, "returned_element_count": 1,
                "screenshot_frame_valid": true, "screenshot_height": 780,
                "screenshot_mime_type": "image/png", "screenshot_width": 1474,
                "snapshot_id": "s00000001", "timeout_ms": 2000, "total_element_count": 1,
                "tree_format": "markdown", "tree_markdown": tree, "truncated": false,
                "window_bounds": {"height": 1354, "width": 2560, "x": 0, "y": 22},
                "window_id": 94371847, "window_title": "kate-scratch.txt  — Kate",
            },
        }));

        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1], image);
        assert_eq!(
            text_of(&result),
            format!(
                "{header}{}\n\
                 app_name: kate\n\
                 capture_id: capture_53dc_0\n\
                 coordinate_frame: window\n\
                 element_count: 1\n\
                 frame_scale: 0.57578125\n\
                 returned_element_count: 1\n\
                 screenshot_height: 780\n\
                 screenshot_width: 1474\n\
                 total_element_count: 1\n\
                 window_bounds: {{height: 1354, width: 2560, x: 0, y: 22}}",
                tree.trim_end()
            )
        );
    }

    fn kate_window(z_index: Value, is_on_screen: bool) -> Value {
        json!({
            "app_name": "kate", "bounds": {"height": 1354, "width": 2560, "x": 0, "y": 22},
            "height": 1354, "is_on_screen": is_on_screen, "pid": 90758,
            "title": "kate-scratch.txt  — Kate", "width": 2560, "window_id": 94371847,
            "x": 0, "y": 22, "z_index": z_index,
        })
    }

    /// list_windows prints each window on a text line, so the structured
    /// copy of the same window adds nothing.
    #[test]
    fn a_window_the_text_already_lists_is_dropped() {
        let text = "Found 1 window(s):\n  window_id=94371847 pid=90758 \"kate-scratch.txt  — Kate\" 2560x1354+0+22 z=5 app=kate";
        let result = rendered(json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": {"windows": [kate_window(json!(5), true)]},
        }));

        assert_eq!(text_of(&result), text);
    }

    /// The 3eefd3e list_windows line had no z_index, and the renderer still
    /// dropped the record that carried it: a field the text does not show
    /// keeps the whole record.
    #[test]
    fn a_window_whose_z_index_the_text_omits_is_kept_whole() {
        let text = "Found 1 windows:\n  window_id=94371847 pid=90758 \"kate-scratch.txt  — Kate\" 2560x1354+0+22 app=kate";
        let rendered = render_text(
            text,
            &json!({"windows": [kate_window(json!(5), true)]}),
            false,
        );

        assert_eq!(
            rendered,
            format!(
                "{text}\nwindows:\n\
                 - {{app_name: kate, bounds: {{height: 1354, width: 2560, x: 0, y: 22}}, \
                 is_on_screen: true, pid: 90758, title: kate-scratch.txt  — Kate, \
                 window_id: 94371847, z_index: 5}}"
            )
        );
    }

    /// A flag holding its usual value needs no words; any other flag needs
    /// `key=value` or `key: value` in the text.
    #[test]
    fn a_flag_counts_as_shown_only_when_usual_or_stated() {
        let line = "  window_id=94371847 pid=90758 \"kate-scratch.txt  — Kate\" 2560x1354+0+22 z=5 app=kate";
        let hidden = json!({"windows": [kate_window(json!(5), false)]});

        assert!(render_text(line, &hidden, false).contains("is_on_screen: false"));
        assert_eq!(
            render_text(&format!("{line} is_on_screen=false"), &hidden, false),
            format!("{line} is_on_screen=false")
        );
        let null_z = json!({"windows": [kate_window(Value::Null, true)]});
        assert_eq!(
            render_text(&line.replace("z=5", "z=?"), &null_z, false),
            line.replace("z=5", "z=?")
        );
    }

    /// health_report at 3eefd3e: the text names every check, but its status
    /// only as an icon. Each check stays whole with its name and message;
    /// before, four lost their name and eight their message.
    #[test]
    fn a_list_the_text_says_in_part_is_kept_whole() {
        let checks = json!([
            {"name": "binary_version", "status": "pass", "message": "pincer 0.35.4"},
            {"name": "platform_supported", "status": "pass", "message": "Arch Linux (x86_64)",
             "data": {"architecture": "x86_64", "os_version": "Arch Linux"}},
            {"name": "tcc_accessibility", "status": "skip", "message": "not applicable on Linux"},
        ]);
        let text = "✅ pincer 0.35.4 on linux — ok\n  \
                    ✅ binary_version: pincer 0.35.4\n  \
                    ✅ platform_supported: Arch Linux (x86_64)\n  \
                    ⏭ tcc_accessibility: not applicable on Linux";
        let rendered = render_text(text, &json!({"checks": checks, "overall": "ok"}), false);

        assert_eq!(
            rendered,
            format!(
                "{text}\nchecks:\n\
                 - {{message: pincer 0.35.4, name: binary_version, status: pass}}\n\
                 - {{data: {{architecture: x86_64, os_version: Arch Linux}}, \
                 message: Arch Linux (x86_64), name: platform_supported, status: pass}}\n\
                 - {{message: not applicable on Linux, name: tcc_accessibility, status: skip}}\n\
                 overall: ok"
            )
        );

        // With each status written out, the text carries the whole list.
        let text = text
            .replace("binary_version:", "binary_version (pass):")
            .replace("platform_supported:", "platform_supported (pass):")
            .replace("tcc_accessibility:", "tcc_accessibility (skip):");
        assert_eq!(render_text(&text, &json!({"checks": checks}), false), text);
    }

    /// get_accessibility_tree at 3eefd3e: the text printed argv[0] and the
    /// structured list the kernel's 15-character name, so the processes
    /// that matched were dropped and the rest stayed: 13 of 17 read as the
    /// whole list. Now the list stays whole; the windows, all in the text,
    /// go.
    #[test]
    fn processes_the_text_names_differently_keep_the_whole_list() {
        let text = "3 running process(es), 1 visible window(s)\n\
                    - python3 (pid 1)\n\
                    - /usr/lib/at-spi-bus-launcher (pid 15)\n\
                    - kwrite (pid 166)\n\n\
                    Windows:\n\
                    - pid=166 window_id=16777223 \"notes.txt  — KWrite\" 640x480+960+494";
        let structured = json!({
            "processes": [
                {"name": "python3", "pid": 1},
                {"name": "at-spi-bus-laun", "pid": 15},
                {"name": "kwrite", "pid": 166},
            ],
            "windows": [{"window_id": 16777223, "pid": 166, "title": "notes.txt  — KWrite",
                         "x": 960, "y": 494, "width": 640, "height": 480}],
        });

        assert_eq!(
            render_text(text, &structured, false),
            format!(
                "{text}\nprocesses:\n\
                 - {{name: python3, pid: 1}}\n\
                 - {{name: at-spi-bus-laun, pid: 15}}\n\
                 - {{name: kwrite, pid: 166}}"
            )
        );
    }

    /// run_script at 3eefd3e kept `value: {clicks: 4}`: `label` was dropped
    /// for being in the text and `clicks` kept for being short.
    #[test]
    fn a_returned_object_is_dropped_or_kept_whole() {
        let value = json!({"clicks": 4, "label": "Count: 5   Strokes: 0"});
        let text = "run_script: ok in 4 ms, 0 driver call(s)\n\
                    return value: {\"clicks\":4,\"label\":\"Count: 5   Strokes: 0\"}";
        assert_eq!(
            render_text(text, &json!({"ok": true, "value": value}), false),
            format!("{text}\nok: true")
        );

        let text = "run_script: ok in 4 ms\nlabel read: Count: 5   Strokes: 0";
        assert_eq!(
            render_text(text, &json!({"value": value}), false),
            format!("{text}\nvalue: {{clicks: 4, label: Count: 5   Strokes: 0}}")
        );
    }

    /// run_steps' observation holds flags and an app_name the text never
    /// says, so it stays whole; the tree inside it, which the text prints
    /// in full, keeps its key with a pointer instead of an escaped copy.
    #[test]
    fn a_kept_object_points_to_a_tree_the_text_shows() {
        let tree = "- frame = \"Pincer Test Pad\"\n  - [5] button \"Count\" [actions=[click]]\n";
        let text = format!("run_steps: 1/1 steps ok\nobservation: full read (below)\n{tree}");
        let rendered = render_text(
            &text,
            &json!({"observation": {
                "ok": true,
                "state": {"app_name": "Pincer-test-pad", "elements_complete": true,
                          "tree_markdown": tree},
            }}),
            false,
        );

        assert_eq!(
            rendered,
            format!(
                "{}\nobservation: {{ok: true, state: {{app_name: Pincer-test-pad, \
                 elements_complete: true, tree_markdown: (same as the text above)}}}}",
                text.trim_end()
            )
        );
    }

    /// A short value proves nothing on its own: an object whose only leaves
    /// are `0` and `1` stays even when the text has both.
    #[test]
    fn short_values_alone_do_not_drop_a_field() {
        let text = "moved 0 of 1";
        assert_eq!(
            render_text(text, &json!({"offset": {"x": 0, "y": 1}}), false),
            format!("{text}\noffset: {{x: 0, y: 1}}")
        );
    }

    /// set_config echoes each key it set as `key=value`, so nothing follows.
    #[test]
    fn set_config_echoes_its_keys_once() {
        let text = "Config updated: cursor.motion.style=comet_swoop (applies to sessions \
                    started from now on)";
        assert_eq!(
            render_text(text, &json!({"cursor.motion.style": "comet_swoop"}), false),
            text
        );
    }

    /// The click refusal's suggestion differs from the text only in its last
    /// punctuation mark (`".` against `";`).
    #[test]
    fn a_string_reworded_only_in_case_spacing_or_punctuation_is_dropped() {
        let text = "Background delivery is not available: the requested target has no \
                    focus-free input backend; the remaining input route can only deliver to \
                    the globally focused widget. Retry this action with \
                    delivery_mode:\"foreground\"; Pincer will activate the target window, \
                    confirm it holds the input focus, and deliver real input to it.";
        let rendered = render_text(
            text,
            &json!({
                "code": "background_unavailable",
                "detail": "the requested target has no focus-free input backend; the remaining input route can only deliver to the globally focused widget",
                "escalation": {
                    "reason": "background input is unavailable on this surface; retry this action with delivery_mode:\"foreground\".",
                    "recommended": "foreground",
                },
                "suggestion": "Retry this action with delivery_mode:\"foreground\".",
            }),
            true,
        );

        assert!(!rendered.contains("suggestion:"), "{rendered}");
        // The reason's first clause is not in the text, so it stays.
        assert!(rendered.ends_with(
            "[code: background_unavailable]\nescalation: {reason: background input is \
             unavailable on this surface; retry this action with delivery_mode:\"foreground\"., \
             recommended: foreground}"
        ));
    }

    #[test]
    fn a_scalar_the_text_shows_beside_its_name_is_dropped() {
        let text = "window_id=94371847 pid=90758 elements=60 walk_ms=993 snapshot_id=s00000003\n\n\
                    no change since s00000002. Rows and indices are as before.";
        let rendered = render_text(
            text,
            &json!({
                "app_name": "kate",
                "since": "s00000002",
                "since_status": "no_change",
                "timeout_ms": 1000,
            }),
            false,
        );

        assert_eq!(
            rendered,
            format!("{text}\napp_name: kate\nsince_status: no_change")
        );
    }

    /// An id the text never shows keeps its container, whatever else matches.
    #[test]
    fn a_container_with_an_unseen_id_stays() {
        let text = "Launched kate (pid 90758) at 2560x1354+0+22.";
        let rendered = render_text(
            text,
            &json!({"windows": [{"app_name": "kate", "pid": 90758, "window_id": 94371847}]}),
            false,
        );

        assert!(rendered.contains("window_id: 94371847"), "{rendered}");
    }

    #[test]
    fn errors_warnings_and_cut_trees_survive_every_dedupe() {
        let text = "Walk cut at 250 of 900 nodes in window 94371847: surface_identity_unproven. \
                    The bounds phase ran out of time before every frame was read.";
        let rendered = render_text(
            text,
            &json!({
                "screenshot_error": {"code": "surface_identity_unproven", "window_id": 94371847},
                "walk": {"nodes_total": 900, "nodes_visited": 250, "truncated": true},
                "warning": "The bounds phase ran out of time before every frame was read.",
                "window": {"elements_complete": false, "nodes_total": 900, "nodes_visited": 250},
            }),
            false,
        );

        assert!(
            rendered.contains("screenshot_error: {code: surface_identity_unproven"),
            "{rendered}"
        );
        assert!(rendered.contains("walk: {"), "{rendered}");
        assert!(rendered.contains("warning: The bounds phase"), "{rendered}");
        assert!(
            rendered.contains("window: {elements_complete: false"),
            "{rendered}"
        );
    }

    /// A cut tree must stay visible: `truncated` is dropped only when false.
    #[test]
    fn a_truncated_tree_says_so() {
        let text = render_text(
            "window_id=1 pid=2 elements=250",
            &json!({"elements_complete": false, "total_element_count": 900, "truncated": true}),
            false,
        );
        assert_eq!(
            text,
            "window_id=1 pid=2 elements=250\nelements_complete: false\ntotal_element_count: 900\ntruncated: true"
        );
    }

    /// Deterministic pseudo-random numbers (xorshift64), so the large-result
    /// tests need no extra crate.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }

        /// A short string from a small alphabet, so pieces of it recur, run
        /// into each other and overlap ("a-a" inside "a-a-a").
        fn scrap(&mut self) -> String {
            let alphabet = ['a', '-', '1', '_', ' ', 'x', '.', '9', 'é', '/'];
            (0..1 + self.below(9))
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }

        fn word(&mut self) -> String {
            const WORDS: &[&str] = &[
                "kworker/R-rcu_gp",
                "org.kde.kwrite",
                "notes.txt  — KWrite",
                "Ünïcödé Ñame",
                "日本語テキスト",
                "a-a",
                "xa-a-a",
                "x1354",
                "1354",
                "2560x1354",
                "pid",
                "launcher_pid",
                "foo_bar",
                "ab",
                "(pid 90758)",
                "90758",
                "snapshot_id=s00000001",
                "The quick brown fox jumps over the lazy dog",
                "",
            ];
            match self.below(3) {
                0 => self.scrap(),
                1 => format!("{}{}", self.pick(WORDS), self.scrap()),
                _ => self.pick(WORDS).to_owned(),
            }
        }

        fn number(&mut self) -> Value {
            match self.below(4) {
                0 => json!(self.below(100)),
                1 => json!(self.below(100_000)),
                2 => json!(1354),
                _ => json!(self.below(10_000) as f64 / 8.0),
            }
        }

        fn leaf(&mut self) -> Value {
            match self.below(6) {
                0 | 1 => json!(self.word()),
                2 | 3 => self.number(),
                4 => json!(self.below(2) == 0),
                _ => Value::Null,
            }
        }

        fn value(&mut self, depth: usize) -> Value {
            match if depth == 0 { 0 } else { self.below(4) } {
                0 | 1 => self.leaf(),
                2 => Value::Array((0..self.below(4)).map(|_| self.value(depth - 1)).collect()),
                _ => {
                    const KEYS: &[&str] = &[
                        "name",
                        "pid",
                        "title",
                        "id",
                        "x",
                        "width",
                        "bundle_id",
                        "error",
                        "truncated",
                        "bounds",
                        "windows",
                        "detail",
                        "elements_complete",
                    ];
                    let mut fields = Map::new();
                    for _ in 0..1 + self.below(5) {
                        fields.insert(self.pick(KEYS).to_owned(), self.value(depth - 1));
                    }
                    Value::Object(fields)
                }
            }
        }
    }

    const LISTED: usize = 50;

    /// A list_apps-sized result whose text mentions much of what its
    /// structured half holds, whole, run into other tokens, or not at all.
    fn large_varied_result(rng: &mut Rng) -> (String, Value) {
        let mut text = String::from("Found many entries:\n");
        let mut apps = Vec::new();
        for _ in 0..900 {
            let name = rng.word();
            let pid = rng.number();
            let joiner = rng.pick(&[" ", "", "-", "(", ")", "=", "x", "_", "\n", "  "]);
            match rng.below(4) {
                0 => text.push_str(&format!("- {name} (pid {pid}){joiner}")),
                1 => text.push_str(&format!("{name}{joiner}{pid}{joiner}")),
                2 => text.push_str(&format!("{}{joiner}", rng.word())),
                _ => {}
            }
            if rng.below(8) == 0 {
                text.push('\n');
            }
            let mut app = json!({"name": name, "pid": pid});
            if rng.below(2) == 0 {
                app["detail"] = rng.value(3);
            }
            apps.push(app);
        }
        // A list the text carries whole, as list_windows' does.
        let listed: Vec<Value> = (0..LISTED)
            .map(|i| {
                text.push_str(&format!("\n- listed-app-{i} (pid {})", 70_000 + i));
                json!({"name": format!("listed-app-{i}"), "pid": 70_000 + i})
            })
            .collect();
        let structured = json!({
            "apps": apps,
            "listed": listed,
            "processes": (0..300).map(|_| rng.value(3)).collect::<Vec<_>>(),
            "pid": rng.number(),
            "since": rng.word(),
            "summary": rng.word(),
            "window": rng.value(4),
        });
        (text, structured)
    }

    /// The indexed `has_token` answers exactly what the plain scan answers,
    /// including the scan's leftmost, non-overlapping matches.
    #[test]
    fn indexed_tokens_match_the_scan() {
        let scan = Shown::new("xa-a-a 2560x1354 a-a-a", u32::MAX);
        let indexed = Shown::new("xa-a-a 2560x1354 a-a-a", 0);
        for token in ["a-a", "1354", "2560x1354", "a-a-a", "xa-", "-a-", "x13"] {
            assert_eq!(indexed.has_token(token), scan.has_token(token), "{token}");
        }
        // The scan's non-overlapping matches miss the second "a-a" here.
        assert!(!Shown::new("xa-a-a", u32::MAX).has_token("a-a"));
        assert!(!Shown::new("xa-a-a", 0).has_token("a-a"));

        let mut rng = Rng(0x5eed_1234_abcd_0001);
        let (text, _) = large_varied_result(&mut rng);
        assert!(text.len() > 10_000, "{}", text.len());
        let scan = Shown::new(&text, u32::MAX);
        let indexed = Shown::new(&text, 0);
        let boundaries: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
        let (mut found, mut missing) = (0, 0);
        for _ in 0..20_000 {
            let token = match rng.below(3) {
                0 => {
                    let start = boundaries[rng.below(boundaries.len())];
                    let end = boundaries
                        .get(boundaries.partition_point(|&at| at <= start) + rng.below(12))
                        .copied()
                        .unwrap_or(text.len());
                    text[start..end].to_owned()
                }
                1 => rng.word(),
                _ => rng.scrap(),
            };
            let expected = scan.has_token(&token);
            assert_eq!(indexed.has_token(&token), expected, "{token:?}");
            if expected {
                found += 1;
            } else {
                missing += 1;
            }
        }
        assert!(
            found > 1000 && missing > 1000,
            "{found} found, {missing} missing"
        );
    }

    /// Large varied results render the same with and without the index, as
    /// plain results, action results and refusals.
    #[test]
    fn large_results_render_the_same_with_the_token_index() {
        let mut rng = Rng(0x0dd_ba11_c0ff_ee00);
        for round in 0..6 {
            let (text, mut structured) = large_varied_result(&mut rng);
            let is_error = round == 2;
            match round {
                1 | 4 => {
                    structured["effect"] = json!("unverifiable");
                    structured["summary"] = json!(text.lines().nth(3).unwrap_or_default());
                    structured["evidence"] = json!([{"detail": rng.word()}, {"detail": "pid"}]);
                    structured["delivery"] = json!({"mode": "foreground", "delivered_count": 3});
                }
                2 => structured["code"] = json!("window_not_found"),
                _ => {}
            }
            let scanned = render_text_with(&text, &structured, is_error, u32::MAX);
            let indexed = render_text_with(&text, &structured, is_error, 0);
            assert_eq!(indexed, scanned, "round {round}");
            assert_eq!(render_text(&text, &structured, is_error), scanned);
            // The text decides what is dropped, one top-level key at a time:
            // the list it carries goes, against unrelated text of the same
            // length it stays, and a list is never cut short.
            let unrelated = render_text_with(&"#".repeat(text.len()), &structured, is_error, 0);
            assert_eq!(rows_after(&scanned, "listed:"), None, "round {round}");
            assert_eq!(
                rows_after(&unrelated, "listed:"),
                Some(LISTED),
                "round {round}"
            );
            assert_eq!(rows_after(&scanned, "apps:"), Some(900), "round {round}");
            assert_eq!(rows_after(&unrelated, "apps:"), Some(900), "round {round}");
        }
    }

    /// The `- ` rows under the line `header`, or `None` without it.
    fn rows_after(rendered: &str, header: &str) -> Option<usize> {
        let mut lines = rendered.lines().skip_while(|line| *line != header);
        lines.next()?;
        Some(lines.take_while(|line| line.starts_with("- ")).count())
    }

    /// `key=value` is matched as a whole assignment, never inside another.
    #[test]
    fn a_restated_assignment_needs_word_boundaries() {
        assert!(restated_assignment(
            "pid",
            &json!(90758),
            "window_id=1 pid=90758 x"
        ));
        assert!(!restated_assignment("x", &json!(0), "max=0"));
        assert!(!restated_assignment("pid", &json!(9), "pid=90758"));
        assert!(!restated_assignment("scale", &json!(1), "scale=1.5"));
    }

    #[test]
    fn strings_are_quoted_only_when_they_could_be_misread() {
        assert_eq!(
            scalar_string("kate-scratch.txt  — Kate", true),
            "kate-scratch.txt  — Kate"
        );
        assert_eq!(scalar_string("Untitled, edited", false), "Untitled, edited");
        assert_eq!(
            scalar_string("Untitled, edited", true),
            "\"Untitled, edited\""
        );
        assert_eq!(scalar_string("42", false), "\"42\"");
        assert_eq!(scalar_string(" padded", false), "\" padded\"");
    }

    #[test]
    fn multi_line_fields_indent_their_continuation_lines() {
        let text = render_text("", &json!({"note": "first\nsecond\n"}), false);
        assert_eq!(text, "note: first\n  second");
    }

    #[test]
    fn a_result_without_structured_content_is_unchanged() {
        let plain = json!({"content": [{"type": "text", "text": "ok"}], "isError": false});
        assert_eq!(rendered(plain.clone()), plain);
    }

    #[test]
    fn structured_mode_leaves_every_response_byte_identical() {
        let call = json!({
            "content": [{"type": "text", "text": "Pressed key 'Return'."}],
            "structuredContent": {"effect": "unverifiable", "route": "synthetic_events", "summary": "Pressed key 'Return'."},
        });
        let list = json!({"tools": [{"name": "click", "outputSchema": {"type": "object"}}]});
        for (method, result) in [("tools/call", call), ("tools/list", list)] {
            let before = serde_json::to_string(&Response::ok(json!(7), result.clone())).unwrap();
            let after = serde_json::to_string(&apply(
                ResultsMode::Structured,
                method,
                Response::ok(json!(7), result),
            ))
            .unwrap();
            assert_eq!(before, after);
        }
    }

    #[test]
    fn other_methods_are_untouched_in_text_mode() {
        let result = json!({"structuredContent": {"a": 1}, "content": []});
        let response = apply(
            ResultsMode::Text,
            "resources/read",
            Response::ok(json!(1), result.clone()),
        );
        assert_eq!(serde_json::to_value(response).unwrap()["result"], result);
    }
}

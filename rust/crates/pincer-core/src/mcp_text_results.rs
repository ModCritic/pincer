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
pub fn render_text(text: &str, structured: &Value, is_error: bool) -> String {
    let mut lines = Vec::new();
    let shown = Shown::new(text);
    let Value::Object(fields) = structured else {
        push_body(&mut lines, text);
        if let Some(value) = render_value(structured, &shown, false, false) {
            lines.push(value);
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
        (None, Some(effect)) if action => render_action(&mut lines, fields, effect, text),
        _ => {
            push_body(&mut lines, text);
            push_fields(&mut lines, fields, &shown, &[]);
        }
    }
    lines.join("\n")
}

fn render_action(lines: &mut Vec<String>, fields: &Map<String, Value>, effect: &str, text: &str) {
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
    let shown = Shown::new(&body);

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

/// The text the rendered result already shows, as written and normalized.
struct Shown<'a> {
    raw: &'a str,
    normalized: String,
}

impl<'a> Shown<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            raw,
            normalized: normalized(raw),
        }
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
        let (Some(first), Some(last)) = (token.chars().next(), token.chars().next_back()) else {
            return false;
        };
        self.raw.match_indices(token).any(|(at, _)| {
            let before = self.raw[..at].chars().next_back();
            let after = self.raw[at + token.len()..].chars().next();
            !before.is_some_and(|c| continues(first, c))
                && !after.is_some_and(|c| continues(last, c))
        })
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

/// One `key: value` line per field, in the object's key order.
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
                let rows: Vec<String> = items
                    .iter()
                    .filter(|item| !container_restated(key, item, shown))
                    .filter_map(|item| render_value(item, shown, true, protected(key, value)))
                    .collect();
                if !rows.is_empty() {
                    lines.push(format!("{key}:"));
                    lines.extend(rows.into_iter().map(|row| format!("- {row}")));
                }
            }
            Value::String(value) if value.contains('\n') => push_multiline(lines, key, value),
            _ => {
                if let Some(value) = render_value(value, shown, false, protected(key, value)) {
                    lines.push(format!("{key}: {value}"));
                }
            }
        }
    }
}

/// `key: first line`, then each further line indented, with no escaping.
fn push_multiline(lines: &mut Vec<String>, key: &str, value: &str) {
    let mut rows = value.trim_end_matches('\n').lines();
    lines.push(format!("{key}: {}", rows.next().unwrap_or_default()));
    lines.extend(rows.map(|row| format!("  {row}")));
}

/// Fields that would only repeat the text or state a default.
fn skip_field(key: &str, value: &Value, siblings: &Map<String, Value>, shown: &Shown) -> bool {
    if NOISE_KEYS.contains(&key) {
        return true;
    }
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty() || (!protected(key, value) && shown.restates(text)),
        Value::Array(items) => {
            items.is_empty()
                || items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|item| shown.restates(item)))
                || container_restated(key, value, shown)
        }
        Value::Object(fields) => fields.is_empty() || container_restated(key, value, shown),
        Value::Bool(true) => key.ends_with("_complete") || key.ends_with("_valid"),
        Value::Bool(false) => key == "truncated",
        // Flat window geometry next to a `bounds` object that holds it.
        Value::Number(_) => {
            matches!(key, "x" | "y" | "width" | "height")
                && siblings.get("bounds").and_then(|bounds| bounds.get(key)) == Some(value)
        }
    }
}

/// A field the dedupe rules must keep whatever the text says: an error or a
/// warning, a cut tree, or an incomplete walk.
fn protected(key: &str, value: &Value) -> bool {
    let flagged = key.contains("error") || key.contains("warning");
    (flagged && !matches!(value, Value::Null | Value::Bool(false)))
        || (key == "truncated" && value == &Value::Bool(true))
        || (key.ends_with("_complete") && value == &Value::Bool(false))
}

/// An object or array whose identifying leaves the text already shows: at
/// least two strings or numbers of 3+ characters, each one a whole token in
/// the text (or a long string it restates). Shorter leaves and booleans do
/// not count either way, so a skipped container takes its flags with it.
fn container_restated(key: &str, value: &Value, shown: &Shown) -> bool {
    let mut leaves = Vec::new();
    collect_leaves(key, value, &mut leaves)
        && leaves.len() >= 2
        && leaves
            .iter()
            .all(|leaf| shown.has_token(leaf) || shown.restates(leaf))
}

/// Gather a container's qualifying leaves. `false` when anything inside it
/// is [`protected`].
fn collect_leaves(key: &str, value: &Value, leaves: &mut Vec<String>) -> bool {
    if protected(key, value) {
        return false;
    }
    match value {
        Value::String(value) if value.chars().count() >= 3 => leaves.push(value.clone()),
        Value::Number(value) if value.to_string().len() >= 3 => leaves.push(value.to_string()),
        Value::Object(fields) => {
            return fields.iter().all(|(key, value)| {
                NOISE_KEYS.contains(&key.as_str()) || collect_leaves(key, value, leaves)
            })
        }
        Value::Array(items) => return items.iter().all(|item| collect_leaves(key, item, leaves)),
        _ => {}
    }
    true
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
    let needle = format!("{key}={value}");
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(&needle).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + needle.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(|c| word(c) || c == '.')
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

/// One value on a single line: objects as `{k: v, …}`, arrays as `[a, …]`.
/// `None` when nothing in it is left to show. Inside a `guarded` value (one
/// under a [`protected`] key) only empty fields and noise are dropped.
fn render_value(value: &Value, shown: &Shown, nested: bool, guarded: bool) -> Option<String> {
    match value {
        Value::Null => Some("null".to_owned()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) => Some(scalar_string(value, nested)),
        Value::Array(items) => Some(format!(
            "[{}]",
            items
                .iter()
                .filter(|item| {
                    guarded
                        || !(item.is_object() || item.is_array())
                        || !container_restated("", item, shown)
                })
                .filter_map(|item| render_value(item, shown, true, guarded))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Value::Object(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .filter(|(key, value)| {
                    if guarded {
                        !empty_or_noise(key, value)
                    } else {
                        !skip_field(key, value, fields, shown)
                    }
                })
                .filter_map(|(key, value)| {
                    render_value(value, shown, true, guarded || protected(key, value))
                        .map(|value| format!("{key}: {value}"))
                })
                .collect();
            (!parts.is_empty()).then(|| format!("{{{}}}", parts.join(", ")))
        }
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

    /// list_windows prints each window on a text line, so the structured
    /// copy of the same window adds nothing.
    #[test]
    fn a_window_the_text_already_lists_is_dropped() {
        let text = "Found 1 windows:\n  window_id=94371847 pid=90758 \"kate-scratch.txt  — Kate\" 2560x1354+0+22 app=kate";
        let result = rendered(json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": {"windows": [{
                "app_name": "kate", "bounds": {"height": 1354, "width": 2560, "x": 0, "y": 22},
                "height": 1354, "is_on_screen": true, "pid": 90758,
                "title": "kate-scratch.txt  — Kate", "width": 2560, "window_id": 94371847,
                "x": 0, "y": 22, "z_index": 5,
            }]},
        }));

        assert_eq!(text_of(&result), text);
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

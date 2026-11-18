//! type_text, and the read-back and result helpers it shares with set_value.

use super::*;

/// Structured payload for a `type_text` response. Keeps the legacy
/// `path`/`characters`/`verified` fields for back-compat and adds the cross-tool
/// `effect` tri-state. Linux's AT-SPI `insertText` return value acknowledges
/// the method call but does not read the widget value back, so it and every
/// keystroke / XSendEvent / XTest / Wayland rung are `"unverifiable"` (the
/// caller confirms through a separate observation). No escalation is
/// attached to a successful delivery: the public projection renders one as
/// `delivery_failed`, which is wrong for text that landed but was not read
/// back. Mirrors the macOS `type_text` contract.
pub(super) fn type_text_structured(path: &str, characters: usize, verified: bool) -> Value {
    let s = json!({
        "path": path,
        "characters": characters,
        "verified": verified,
        "effect": if verified { "confirmed" } else { "unverifiable" },
    });
    // No escalation on a successful-but-unverified delivery: the public
    // projection renders a foreground escalation as `delivery_failed`, which
    // made models re-type text that had already landed. Refusals carry their own.
    let _ = verified;
    s
}

/// Structured payload for the Electron/Chromium AX-echo case: the AT-SPI
/// `insertText` rung returned success on a Chromium embedder, but on those
/// surfaces the a11y bridge can accept and echo the write while the renderer
/// never observes it — so a path=="ax" "confirm" is a shim echo, not ground
/// truth. Mirrors the macOS `type_text` `ax_echo_surface` branch: report
/// effect:"unverifiable" + escalation:{recommended:"px"} (a renderer-focus
/// problem — pixel-focus the field, not foreground-activate it).
fn type_text_structured_electron(text_len: usize) -> Value {
    json!({
        "path": "ax",
        "characters": text_len,
        "verified": false,
        "effect": "unverifiable",
        "escalation": {
            "recommended": "px",
            "reason": "Electron/web surface: the AX write was echoed but the \
                       renderer may not have observed it. Confirm via the screenshot; \
                       if it didn't land, re-type with the element px action (pass x,y \
                       to pixel-focus the field, then type)."
        }
    })
}

/// [`type_text_ax_result`] for an element-addressed insert: the snapshot
/// object is read back (as `set_value` does) and, when the typed text is now
/// part of its value, the result is `effect: confirmed` with a
/// `value_readback` evidence item instead of `unverifiable`.
pub(super) async fn type_text_ax_result_verified(
    pid: u32,
    xid: u64,
    idx: usize,
    text: &str,
    route: &str,
    password_before: Option<usize>,
) -> ToolResult {
    let text_len = text.chars().count();
    if is_chromium_embedder(pid) {
        return type_text_ax_result(pid, text_len, route);
    }
    let typed = text.to_owned();
    let readback = tokio::task::spawn_blocking(move || {
        // The caret sits after the insert: the text there, with a little
        // context, proves it even past the 4,096 characters a value read
        // returns. A field whose caret read misses it (a spin button that
        // reformats "-40", a masked password) is read whole as before.
        let before = typed.chars().count() + READBACK_CONTEXT_CHARS;
        if let Some((around, more_before, more_after)) =
            crate::atspi::read_text_around_caret_in(pid, xid, idx, before, READBACK_CONTEXT_CHARS)
        {
            if !typed.trim().is_empty() && around.contains(typed.as_str()) {
                // Marked as a piece of the field, so a short one is not
                // quoted as its whole value.
                return Some(format!(
                    "{}{around}{}",
                    if more_before { "..." } else { "" },
                    if more_after { "..." } else { "" }
                ));
            }
        }
        crate::atspi::read_value_in(pid, xid, idx)
    })
    .await
    .ok()
    .flatten();
    type_text_readback_result(pid, idx, text, route, readback, password_before)
}

/// Characters of context quoted on each side of typed text in a read-back.
const READBACK_CONTEXT_CHARS: usize = 20;
/// A read-back up to this long is quoted whole.
const READBACK_WHOLE_CHARS: usize = 80;
/// Typed text longer than this is confirmed without quoting it back.
const READBACK_QUOTED_TEXT_CHARS: usize = 60;

/// One line of `seen` for a result, shown inside double quotes: the app's
/// text escaped by `untrusted_text::inline` (`\"`, `\\`, `\n`, `\u{2028}`),
/// so it can neither end its quotes nor start a line.
fn readback_one_line(seen: &str) -> String {
    pincer_core::untrusted_text::inline(seen).into_owned()
}

/// A `set_value` or combo box read-back for the result text: the first
/// [`READBACK_WHOLE_CHARS`] characters, escaped and quoted, with the full
/// length named when it was cut. The structured result keeps it whole.
pub(super) fn readback_quoted(seen: &str) -> String {
    quote_untrusted(seen, READBACK_WHOLE_CHARS)
}

/// What a confirmed read-back quotes: the whole value when it is short, else
/// the typed text with [`READBACK_CONTEXT_CHARS`] of context on each side
/// (`...dog; HELLO 001 The...`). `None` when the typed text is too long to be
/// worth echoing.
fn readback_excerpt(seen: &str, text: &str) -> Option<String> {
    if seen.chars().count() <= READBACK_WHOLE_CHARS {
        return Some(readback_one_line(seen));
    }
    if text.chars().count() > READBACK_QUOTED_TEXT_CHARS {
        return None;
    }
    let Some(at) = seen.find(text) else {
        // Agreed numerically, not by substring: quote the start.
        let head: String = seen.chars().take(READBACK_WHOLE_CHARS).collect();
        return Some(format!("{}...", readback_one_line(&head)));
    };
    let before: Vec<char> = seen[..at].chars().collect();
    let lead_start = before.len().saturating_sub(READBACK_CONTEXT_CHARS);
    let lead: String = before[lead_start..].iter().collect();
    let tail: String = seen[at + text.len()..]
        .chars()
        .take(READBACK_CONTEXT_CHARS)
        .collect();
    let cut_after = seen[at + text.len()..].chars().count() > READBACK_CONTEXT_CHARS;
    Some(format!(
        "{}{}{}{}{}",
        if lead_start > 0 { "..." } else { "" },
        readback_one_line(&lead),
        readback_one_line(text),
        readback_one_line(&tail),
        if cut_after { "..." } else { "" }
    ))
}

/// For a password field, how many mask characters it holds before a
/// `type_text` insert, so the read-back can be compared by length (its text
/// is masked). `None` for any other element, and for a password field that
/// shows its text in the clear (that one reads back normally).
pub(super) async fn password_length_before(pid: u32, xid: u64, idx: usize) -> Option<usize> {
    if is_chromium_embedder(pid) {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        if !crate::atspi::is_password_in(pid, xid, idx) {
            return None;
        }
        let before = crate::atspi::read_value_in(pid, xid, idx).unwrap_or_default();
        masked_length(&before)
    })
    .await
    .ok()
    .flatten()
}

/// The number of characters `seen` masks when it is a password field's
/// masked text: empty (an empty field), or one repeated character that is
/// neither alphanumeric nor whitespace (Qt's and GTK's "●", "•", "*").
/// `None` for anything else, such as a password shown in the clear.
pub(super) fn masked_length(seen: &str) -> Option<usize> {
    let mut chars = seen.chars();
    let Some(first) = chars.next() else {
        return Some(0);
    };
    if first.is_alphanumeric() || first.is_whitespace() {
        return None;
    }
    chars.all(|c| c == first).then(|| seen.chars().count())
}

/// Whether `masks` mask characters stand for `text`. Qt masks each UTF-16
/// unit ("a\u{1F600}b" read back as four masks on the Qt probe); GTK is
/// assumed to mask each character, as its entry draws one invisible char per
/// character (not checked against a GTK app here).
pub(super) fn mask_count_fits(masks: usize, text: &str) -> bool {
    masks == text.chars().count() || masks == text.encode_utf16().count()
}

/// The typed text is confirmed when the element's fresh value contains it
/// (an insert at the caret) or agrees with it numerically (a spin button
/// that reformats "-40" as "-40.0").
pub(super) fn type_text_readback_result(
    pid: u32,
    idx: usize,
    text: &str,
    route: &str,
    readback: Option<String>,
    password_before: Option<usize>,
) -> ToolResult {
    let text_len = text.chars().count();
    if let (Some(before), Some(seen)) = (password_before, readback.as_deref()) {
        if !seen.contains(text) {
            if let Some(after) = masked_length(seen) {
                return type_text_masked_result(idx, text, route, before, after);
            }
        }
    }
    match readback {
        Some(seen)
            if !text.trim().is_empty() && (seen.contains(text) || values_agree(text, &seen)) =>
        {
            let excerpt = readback_excerpt(&seen, text);
            let shown = match &excerpt {
                Some(excerpt) => format!("it reads back \"{excerpt}\""),
                None => "it reads back the typed text in place".to_owned(),
            };
            let mut structured = json!({
                "path": "ax",
                "characters": text_len,
                "verified": true,
                "effect": "confirmed",
                "evidence": [{
                    "kind": "value_readback",
                    "detail": format!("element [{idx}] reads back the typed text"),
                }],
            });
            if let Some(excerpt) = excerpt {
                structured["readback"] = json!(excerpt);
            }
            ToolResult::text(format!(
                "Typed {text_len} character(s) into element [{idx}] ({route}); {shown}."
            ))
            .with_structured(structured)
        }
        Some(seen) => {
            let mut result = type_text_ax_result(pid, text_len, route);
            let head: String = seen.chars().take(READBACK_WHOLE_CHARS).collect();
            let shown = if seen.chars().count() > READBACK_WHOLE_CHARS {
                format!("{}...", readback_one_line(&head))
            } else {
                readback_one_line(&head)
            };
            if let Some(structured) = result.structured_content.as_mut() {
                structured["readback"] = json!(shown);
            }
            result
                .content
                .push(pincer_core::protocol::Content::text(format!(
                    "The element reads back \"{shown}\", which does not contain the typed text \
                     at its caret or in its first 4,096 characters; confirm with \
                     get_window_state."
                )));
            result
        }
        None => type_text_ax_result(pid, text_len, route),
    }
}

/// `type_text` into a password field: its text reads back as mask
/// characters, so only its length can be compared. The insert landed when the
/// field grew by exactly the typed text's length (a masked-length match).
fn type_text_masked_result(
    idx: usize,
    text: &str,
    route: &str,
    before: usize,
    after: usize,
) -> ToolResult {
    let text_len = text.chars().count();
    let grew_by_text = after
        .checked_sub(before)
        .is_some_and(|grown| mask_count_fits(grown, text));
    let mut structured = json!({
        "path": "ax",
        "characters": text_len,
        "verified": grew_by_text,
        "effect": if grew_by_text { "confirmed" } else { "unverifiable" },
        "readback_masked": true,
        "masked_length_before": before,
        "masked_length": after,
    });
    let text = if grew_by_text {
        structured["evidence"] = json!([{
            "kind": "value_readback",
            "detail": format!(
                "password field element [{idx}] grew from {before} to {after} mask characters, \
                 the length of the typed text"
            ),
        }]);
        format!(
            "Typed {text_len} character(s) into password field element [{idx}] ({route}); it \
             grew from {before} to {after} mask characters, the length of the typed text (a \
             masked-length match: a password field shows only its length)."
        )
    } else {
        format!(
            "Typed {text_len} character(s) into password field element [{idx}] ({route}). The \
             field is masked, so the typed text can't be read back: it holds {after} mask \
             characters, {before} before typing. Not verified."
        )
    };
    ToolResult::text(text).with_structured(structured)
}

/// Build the success `ToolResult` for an AT-SPI insert. The EditableText
/// method's boolean is a delivery acknowledgement, not a fresh value readback,
/// so native widgets remain `unverifiable`. Chromium embedders additionally
/// recommend the pixel rung because their accessibility bridge can acknowledge
/// a write the renderer never observes.
pub(super) fn type_text_ax_result(pid: u32, text_len: usize, route: &str) -> ToolResult {
    if is_chromium_embedder(pid) {
        return ToolResult::text(format!(
            "📨 Sent (unverified) {text_len} character(s) ({route}). Electron/web \
             surface: the AX layer accepts and echoes the write but the renderer may \
             not have observed it, so the driver cannot confirm via AX. Verify via the \
             screenshot; if it didn't land, re-type with the px form (pass x,y to \
             pixel-focus the field)."
        ))
        .with_structured(type_text_structured_electron(text_len));
    }
    ToolResult::text(format!("Typed {text_len} character(s) ({route})."))
        .with_structured(type_text_structured("ax", text_len, false))
}

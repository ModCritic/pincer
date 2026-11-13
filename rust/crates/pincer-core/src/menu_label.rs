// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! How `invoke_menu` compares a path segment with a menu label, and what it
//! says when a segment is not found, on every platform.

/// A menu label as path segments compare it: trimmed, with three periods
/// read as the ellipsis character (U+2026). Menus write "Find…" with the
/// ellipsis on macOS and in KDE and GTK apps alike, while a caller types
/// "Find...", so either spelling finds either (round 2 of the first-call
/// test: Opus had to guess the character for KWrite's Edit > Find…).
pub fn menu_label_key(label: &str) -> String {
    label.trim().replace("...", "\u{2026}")
}

/// Whether menu label `label` is path segment `segment`.
pub fn menu_label_matches(label: &str, segment: &str) -> bool {
    menu_label_key(label) == menu_label_key(segment)
}

/// Most labels a missed segment lists, so a long menu cannot flood the reply.
pub const MAX_LISTED_LABELS: usize = 40;

/// The refusal for a path segment that is not found: what was asked for,
/// where, and the labels that are there, so the next call can use the real
/// label instead of a guess or a full tree read (LibreOffice calls it "To
/// Next Sheet", not "Next Sheet"). `labels` are the entries of the level
/// that missed, in menu order; blank ones and repeats next to each other are
/// left out. The tool's name is not in it: each platform adds its prefix.
pub fn missing_segment_message(depth: usize, path: &[String], labels: &[String]) -> String {
    let wanted = path.get(depth).map(String::as_str).unwrap_or_default();
    let location = if depth == 0 {
        "in the menu bar".to_owned()
    } else {
        format!("under \"{}\"", path[..depth.min(path.len())].join(" > "))
    };
    let mut items: Vec<&str> = labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .collect();
    items.dedup();
    if items.is_empty() {
        return format!(
            "path segment {depth} \"{wanted}\" was not found {location}, which lists no titled items (the app may fill this menu only while it is open)"
        );
    }
    let more = items.len().saturating_sub(MAX_LISTED_LABELS);
    let listed = items
        .iter()
        .take(MAX_LISTED_LABELS)
        .map(|label| format!("\"{label}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!(
        "path segment {depth} \"{wanted}\" was not found {location}. Items there: {listed}{more}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_periods_and_the_ellipsis_match_each_other() {
        assert!(menu_label_matches("Find\u{2026}", "Find..."));
        assert!(menu_label_matches("Find...", "Find\u{2026}"));
        assert!(menu_label_matches(" Save As... ", "Save As\u{2026}"));
        assert!(menu_label_matches("Find\u{2026}", "Find\u{2026}"));
        assert!(!menu_label_matches("Find", "Find\u{2026}"));
        assert!(!menu_label_matches("Find..", "Find\u{2026}"));
        // Case still counts.
        assert!(!menu_label_matches("find\u{2026}", "Find..."));
        assert_eq!(menu_label_key("Save As..."), "Save As\u{2026}");
    }

    #[test]
    fn a_missed_segment_lists_the_labels_at_its_level() {
        let path: Vec<String> = ["Sheet", "Navigate", "Next Sheet"]
            .map(String::from)
            .to_vec();
        let labels: Vec<String> = ["To Next Sheet", "", "To Previous Sheet"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            missing_segment_message(2, &path, &labels),
            "path segment 2 \"Next Sheet\" was not found under \"Sheet > Navigate\". Items there: \"To Next Sheet\", \"To Previous Sheet\""
        );
        assert!(missing_segment_message(0, &path, &["File".into()]).contains("in the menu bar"));
        assert!(missing_segment_message(1, &path, &[]).contains("lists no titled items"));
        let many: Vec<String> = (0..45).map(|i| format!("Item {i}")).collect();
        assert!(missing_segment_message(1, &path, &many).ends_with("and 5 more"));
    }
}

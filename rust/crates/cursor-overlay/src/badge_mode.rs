// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! How much the session badge under an agent cursor shows.
//!
//! `cursor.badges` lives in `~/.pincer/config.json`
//! (`{"cursor": {"badges": "minimal"}}`) next to the saved motion defaults.
//! Unlike the motion defaults it applies live: every cursor reads the
//! process-wide value on each frame, so a `set_config` write changes cursors
//! that are already on screen.
//!
//! - `minimal` (default): no badge at rest. A short chip appears for a
//!   foreground-delivery moment or a held key combo (such as ⌘C), and the
//!   session name appears only on hover or while two or more agent cursors
//!   are on screen. Nothing shows while the cursor is moving.
//! - `verbose`: the earlier badge. The name shows on reveal, and delivery and
//!   target chips follow every action.
//! - `off`: no badge.

use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

/// Config key this module owns.
pub const BADGES_KEY: &str = "cursor.badges";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BadgeMode {
    #[default]
    Minimal,
    Verbose,
    Off,
}

impl BadgeMode {
    pub const ALL: [BadgeMode; 3] = [BadgeMode::Minimal, BadgeMode::Verbose, BadgeMode::Off];

    pub const fn as_str(self) -> &'static str {
        match self {
            BadgeMode::Minimal => "minimal",
            BadgeMode::Verbose => "verbose",
            BadgeMode::Off => "off",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str().eq_ignore_ascii_case(value.trim()))
    }

    const fn to_u8(self) -> u8 {
        match self {
            BadgeMode::Minimal => 0,
            BadgeMode::Verbose => 1,
            BadgeMode::Off => 2,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => BadgeMode::Verbose,
            2 => BadgeMode::Off,
            _ => BadgeMode::Minimal,
        }
    }
}

fn allowed() -> String {
    BadgeMode::ALL.map(BadgeMode::as_str).join(", ")
}

/// Parse a config value. `null` and `"default"` mean the built-in default.
pub fn parse_value(value: &Value) -> Result<Option<BadgeMode>, String> {
    if value.is_null() || value.as_str() == Some("default") {
        return Ok(None);
    }
    let name = value
        .as_str()
        .ok_or_else(|| format!("{BADGES_KEY} must be a string, got {value}"))?;
    BadgeMode::parse(name).map(Some).ok_or_else(|| {
        format!(
            "unknown {BADGES_KEY} value `{name}`; expected one of {}",
            allowed()
        )
    })
}

/// Read the saved mode from `path`. A missing file or an invalid value falls
/// back to the default; a bad value is logged, never fatal.
pub fn load_from(path: &Path) -> BadgeMode {
    let saved = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|json| json.get("cursor")?.get("badges").cloned());
    match saved.map(|value| parse_value(&value)) {
        Some(Ok(mode)) => mode.unwrap_or_default(),
        Some(Err(error)) => {
            tracing::warn!("ignoring saved {BADGES_KEY}: {error}");
            BadgeMode::default()
        }
        None => BadgeMode::default(),
    }
}

/// Validate and persist one write to `path`, keeping every other key in the
/// file. Nothing is written when validation fails.
pub fn set_at(path: &Path, value: &Value) -> Result<BadgeMode, String> {
    let mode = parse_value(value)?;
    let mut root: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let object = root.as_object_mut().expect("root is an object");
    let cursor = object.entry("cursor").or_insert_with(|| json!({}));
    if !cursor.is_object() {
        *cursor = json!({});
    }
    let cursor = cursor.as_object_mut().expect("cursor is an object");
    match mode {
        Some(mode) => {
            cursor.insert("badges".into(), json!(mode.as_str()));
        }
        None => {
            cursor.remove("badges");
        }
    }
    if cursor.is_empty() {
        object.remove("cursor");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let body = serde_json::to_string_pretty(&root).map_err(|error| error.to_string())?;
    std::fs::write(path, body)
        .map_err(|error| format!("failed to persist {BADGES_KEY}: {error}"))?;
    Ok(mode.unwrap_or_default())
}

// Process-wide mode every cursor reads per frame. It starts at the default so
// unit tests never read the user's file; the daemon loads it at startup
// (`CursorConfig::from_args`) and on every config write.
static ACTIVE: AtomicU8 = AtomicU8::new(0);

/// The mode cursors paint with right now.
pub fn active() -> BadgeMode {
    BadgeMode::from_u8(ACTIVE.load(Ordering::Relaxed))
}

pub fn set_active(mode: BadgeMode) {
    ACTIVE.store(mode.to_u8(), Ordering::Relaxed);
}

/// Load `~/.pincer/config.json` into the process-wide value.
pub fn load_active() -> BadgeMode {
    let mode = read_saved();
    set_active(mode);
    mode
}

/// What is saved on disk right now (for `get_config`).
pub fn read_saved() -> BadgeMode {
    crate::motion_defaults::default_config_path()
        .map(|path| load_from(&path))
        .unwrap_or_default()
}

/// Apply a `cursor.badges` write in a `set_config` call: validate, persist,
/// and switch every cursor at once. Returns the key when it was written.
pub fn apply_config_args(args: &Value) -> Result<Option<String>, String> {
    let Some(value) = args.get(BADGES_KEY) else {
        return Ok(None);
    };
    let path = crate::motion_defaults::default_config_path()
        .ok_or_else(|| "$HOME is not set".to_string())?;
    let mode = set_at(&path, value)?;
    set_active(mode);
    Ok(Some(format!("{BADGES_KEY}={}", mode.as_str())))
}

/// Input-schema property to merge into each platform's `set_config` tool.
pub fn config_schema_properties() -> Map<String, Value> {
    let mut properties = Map::new();
    let mut values: Vec<&str> = BadgeMode::ALL.map(BadgeMode::as_str).to_vec();
    values.push("default");
    properties.insert(
        BADGES_KEY.to_owned(),
        json!({
            "type": "string",
            "enum": values,
            "description": "Agent cursor badge. minimal (default): no badge at rest; a brief chip for a foreground moment or a key combo, and the session name on hover or when two or more agent cursors are on screen. verbose: the session name and delivery/target chips on every action. off: no badge. Applies immediately to every cursor."
        }),
    );
    properties
}

/// A held key combo as the minimal badge draws it: one keycap per key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCombo {
    /// Modifier labels in display order, then the main key, such as
    /// `["Ctrl", "Shift", "Z"]`, or `["⇧", "⌘", "Z"]` on macOS. A key that
    /// is itself `+` stays one part.
    pub keys: Vec<String>,
    /// macOS spelling: modifier symbols sit side by side with no `+`.
    pub mac: bool,
}

impl KeyCombo {
    /// The combo as one line of text, such as `Ctrl+Shift+Z` or `⇧⌘Z`.
    pub fn text(&self) -> String {
        self.keys.join(if self.mac { "" } else { "+" })
    }
}

/// Format a key combo for the minimal badge, such as `⌘C` on macOS or
/// `Ctrl+C` elsewhere. `None` unless at least one modifier is held with a
/// non-modifier key: a bare key press does not earn a chip.
pub fn format_key_combo(keys: &[String]) -> Option<String> {
    format_key_combo_for(keys, cfg!(target_os = "macos"))
}

pub(crate) fn format_key_combo_for(keys: &[String], mac: bool) -> Option<String> {
    key_combo_parts_for(keys, mac).map(|combo| combo.text())
}

/// Split a key combo into the keys the badge draws, with the same order,
/// spelling and rules as [`format_key_combo`].
pub fn key_combo_parts(keys: &[String]) -> Option<KeyCombo> {
    key_combo_parts_for(keys, cfg!(target_os = "macos"))
}

pub(crate) fn key_combo_parts_for(keys: &[String], mac: bool) -> Option<KeyCombo> {
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Modifier {
        Control,
        Option,
        Shift,
        Command,
    }
    let mut modifiers = Vec::new();
    let mut main = None;
    for key in keys {
        let lower = key.trim().to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "cmd" | "command" | "meta" | "super" | "win" | "windows" => Some(Modifier::Command),
            "ctrl" | "control" => Some(Modifier::Control),
            "alt" | "option" | "opt" => Some(Modifier::Option),
            "shift" => Some(Modifier::Shift),
            "" => continue,
            _ => None,
        };
        match modifier {
            Some(modifier) if !modifiers.contains(&modifier) => modifiers.push(modifier),
            Some(_) => {}
            None => main = Some(key.trim().to_owned()),
        }
    }
    let main = main?;
    if modifiers.is_empty() {
        return None;
    }
    modifiers.sort();
    let key_label = {
        let mut chars = main.chars();
        match (chars.next(), chars.next()) {
            (Some(only), None) => only.to_uppercase().collect::<String>(),
            _ => {
                let mut word = main.replace(['_', '-'], " ");
                if let Some(first) = word.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                word
            }
        }
    };
    let mut parts: Vec<String> = modifiers
        .into_iter()
        .map(|modifier| {
            match (mac, modifier) {
                (true, Modifier::Control) => "⌃",
                (true, Modifier::Option) => "⌥",
                (true, Modifier::Shift) => "⇧",
                (true, Modifier::Command) => "⌘",
                (false, Modifier::Control) => "Ctrl",
                (false, Modifier::Option) => "Alt",
                (false, Modifier::Shift) => "Shift",
                (false, Modifier::Command) => "Win",
            }
            .to_owned()
        })
        .collect();
    parts.push(key_label);
    Some(KeyCombo { keys: parts, mac })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn default_mode_is_minimal_and_values_round_trip() {
        assert_eq!(BadgeMode::default(), BadgeMode::Minimal);
        for mode in BadgeMode::ALL {
            assert_eq!(BadgeMode::parse(mode.as_str()), Some(mode));
            assert_eq!(BadgeMode::from_u8(mode.to_u8()), mode);
        }
        assert_eq!(parse_value(&json!("VERBOSE")), Ok(Some(BadgeMode::Verbose)));
        assert_eq!(parse_value(&json!("default")), Ok(None));
        assert_eq!(parse_value(&Value::Null), Ok(None));
        assert!(parse_value(&json!("loud"))
            .unwrap_err()
            .contains("minimal, verbose, off"));
        assert!(parse_value(&json!(true)).is_err());
    }

    #[test]
    fn saved_mode_persists_beside_motion_defaults_and_clears() {
        let dir = std::env::temp_dir().join(format!("pincer-badge-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        crate::motion_defaults::set_key_at(
            &path,
            crate::motion_defaults::STYLE_KEY,
            &json!("magnetic"),
        )
        .unwrap();
        assert_eq!(load_from(&path), BadgeMode::Minimal);

        assert_eq!(set_at(&path, &json!("verbose")), Ok(BadgeMode::Verbose));
        assert_eq!(load_from(&path), BadgeMode::Verbose);
        let saved = crate::motion_defaults::load_from(&path);
        assert_eq!(
            saved.style,
            Some(crate::motion::MotionStyle::Magnetic),
            "badge writes keep motion defaults"
        );

        assert!(set_at(&path, &json!("loud")).is_err());
        assert_eq!(
            load_from(&path),
            BadgeMode::Verbose,
            "invalid writes change nothing"
        );

        assert_eq!(set_at(&path, &json!("default")), Ok(BadgeMode::Minimal));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("badges"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_combos_need_a_modifier_and_use_platform_spelling() {
        assert_eq!(
            format_key_combo_for(&keys(&["cmd", "c"]), true).as_deref(),
            Some("⌘C")
        );
        assert_eq!(
            format_key_combo_for(&keys(&["shift", "cmd", "z"]), true).as_deref(),
            Some("⇧⌘Z")
        );
        assert_eq!(
            format_key_combo_for(&keys(&["ctrl", "c"]), false).as_deref(),
            Some("Ctrl+C")
        );
        assert_eq!(
            format_key_combo_for(&keys(&["alt", "page_down"]), false).as_deref(),
            Some("Alt+Page down")
        );
        assert_eq!(format_key_combo_for(&keys(&["return"]), true), None);
        assert_eq!(format_key_combo_for(&keys(&["cmd"]), true), None);
        assert_eq!(format_key_combo_for(&[], true), None);
    }

    #[test]
    fn key_combo_parts_keep_the_text_order_and_spelling() {
        let parts = |values: &[&str], mac: bool| {
            key_combo_parts_for(&keys(values), mac).map(|combo| combo.keys)
        };
        assert_eq!(
            parts(&["z", "shift", "ctrl"], false),
            Some(vec!["Ctrl".into(), "Shift".into(), "Z".into()])
        );
        assert_eq!(
            parts(&["alt", "page_down"], false),
            Some(vec!["Alt".into(), "Page down".into()])
        );
        assert_eq!(
            parts(&["win", "alt", "shift", "ctrl", "k"], false),
            Some(vec![
                "Ctrl".into(),
                "Alt".into(),
                "Shift".into(),
                "Win".into(),
                "K".into()
            ])
        );
        assert_eq!(
            parts(&["shift", "cmd", "z"], true),
            Some(vec!["⇧".into(), "⌘".into(), "Z".into()])
        );
        assert_eq!(
            parts(&["cmd", "ctrl", "opt", "shift", "k"], true),
            Some(vec![
                "⌃".into(),
                "⌥".into(),
                "⇧".into(),
                "⌘".into(),
                "K".into()
            ])
        );
        assert_eq!(parts(&["ctrl", "ctrl", "c"], false).unwrap().len(), 2);
        assert_eq!(parts(&["return"], false), None);
        assert_eq!(parts(&["shift"], true), None);

        // The parts and the text always agree.
        for (values, mac) in [
            (&["ctrl", "c"][..], false),
            (&["shift", "cmd", "z"][..], true),
            (&["alt", "page_down"][..], false),
        ] {
            assert_eq!(
                key_combo_parts_for(&keys(values), mac).map(|combo| combo.text()),
                format_key_combo_for(&keys(values), mac)
            );
        }
    }

    #[test]
    fn a_plus_key_stays_one_part() {
        let linux = key_combo_parts_for(&keys(&["ctrl", "+"]), false).unwrap();
        assert_eq!(linux.keys, vec!["Ctrl".to_owned(), "+".to_owned()]);
        assert!(!linux.mac);
        assert_eq!(linux.text(), "Ctrl++");
        let shifted = key_combo_parts_for(&keys(&["ctrl", "shift", "+"]), false).unwrap();
        assert_eq!(shifted.keys, ["Ctrl", "Shift", "+"]);
        let mac = key_combo_parts_for(&keys(&["cmd", "+"]), true).unwrap();
        assert_eq!(mac.keys, vec!["⌘".to_owned(), "+".to_owned()]);
        assert!(mac.mac);
        assert_eq!(mac.text(), "⌘+");
    }
}

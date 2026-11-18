//! The one table of keyboard modifier names.
//!
//! Tool calls name modifiers in several vocabularies: X11 (`ctrl`, `super`),
//! macOS (`option`, `cmd`, `command`) and Windows (`win`, `windows`). Every
//! input route parses them here and then asks for its own encoding (an X
//! keysym, an X/XKB state bit, an evdev code), so no route can treat a name
//! as a modifier that another route reads as a plain key.

/// A keyboard modifier, whatever name the caller used for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    /// The Super (logo, Windows, Command) key.
    Super,
}

/// Every accepted spelling, compared case-insensitively.
const NAMES: [(&str, Modifier); 11] = [
    ("ctrl", Modifier::Ctrl),
    ("control", Modifier::Ctrl),
    ("shift", Modifier::Shift),
    ("alt", Modifier::Alt),
    ("option", Modifier::Alt),
    ("super", Modifier::Super),
    ("cmd", Modifier::Super),
    ("command", Modifier::Super),
    ("meta", Modifier::Super),
    ("windows", Modifier::Super),
    ("win", Modifier::Super),
];

/// The accepted spellings, for error messages.
pub const ACCEPTED_NAMES: &str =
    "ctrl/control, shift, alt/option, super/cmd/command/meta/windows/win";

impl Modifier {
    /// Parse a modifier name, ignoring case and surrounding whitespace.
    /// `None` for anything that is not a modifier (a plain key name).
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        NAMES
            .iter()
            .find(|(spelling, _)| spelling.eq_ignore_ascii_case(name))
            .map(|(_, modifier)| *modifier)
    }

    /// The canonical name: ctrl, shift, alt or super.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ctrl => "ctrl",
            Self::Shift => "shift",
            Self::Alt => "alt",
            Self::Super => "super",
        }
    }

    /// The X keysym of the left-hand key (Control_L, Shift_L, Alt_L,
    /// Super_L).
    pub fn keysym(self) -> u32 {
        match self {
            Self::Ctrl => 0xFFE3,
            Self::Shift => 0xFFE1,
            Self::Alt => 0xFFE9,
            Self::Super => 0xFFEB,
        }
    }

    /// The X keysym name of the left-hand key, as `wtype -k` takes it.
    pub fn keysym_name(self) -> &'static str {
        match self {
            Self::Ctrl => "Control_L",
            Self::Shift => "Shift_L",
            Self::Alt => "Alt_L",
            Self::Super => "Super_L",
        }
    }

    /// The modifier's bit in an X11 core event state and in an XKB
    /// modifier mask: ShiftMask, ControlMask, Mod1Mask (Alt), Mod4Mask
    /// (Super).
    pub fn state_mask(self) -> u16 {
        match self {
            Self::Shift => 1,
            Self::Ctrl => 4,
            Self::Alt => 8,
            Self::Super => 64,
        }
    }

    /// The Linux evdev key code of the left-hand key
    /// (`linux/input-event-codes.h`).
    pub fn evdev_code(self) -> u32 {
        match self {
            Self::Ctrl => 29,
            Self::Shift => 42,
            Self::Alt => 56,
            Self::Super => 125,
        }
    }
}

/// Whether `name` names a modifier.
pub fn is_modifier(name: &str) -> bool {
    Modifier::parse(name).is_some()
}

/// Parse a list of modifier names, in order and without repeats. `Err`
/// holds the names that are not modifiers.
pub fn parse_modifiers<S: AsRef<str>>(names: &[S]) -> Result<Vec<Modifier>, Vec<String>> {
    let mut modifiers = Vec::with_capacity(names.len());
    let mut unknown = Vec::new();
    for name in names {
        match Modifier::parse(name.as_ref()) {
            Some(modifier) if !modifiers.contains(&modifier) => modifiers.push(modifier),
            Some(_) => {}
            None => unknown.push(name.as_ref().to_owned()),
        }
    }
    if unknown.is_empty() {
        Ok(modifiers)
    } else {
        Err(unknown)
    }
}

/// [`parse_modifiers`], as canonical names.
pub fn canonical_modifier_names<S: AsRef<str>>(names: &[S]) -> Result<Vec<String>, Vec<String>> {
    parse_modifiers(names)
        .map(|modifiers| modifiers.into_iter().map(|m| m.name().to_owned()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_parses_case_insensitively() {
        for (names, expected) in [
            (&["ctrl", "control"][..], Modifier::Ctrl),
            (&["shift"][..], Modifier::Shift),
            (&["alt", "option"][..], Modifier::Alt),
            (
                &["super", "cmd", "command", "meta", "windows", "win"][..],
                Modifier::Super,
            ),
        ] {
            for name in names {
                for spelling in [name.to_string(), name.to_uppercase(), format!(" {name} ")] {
                    assert_eq!(Modifier::parse(&spelling), Some(expected), "{spelling:?}");
                }
            }
        }
    }

    #[test]
    fn plain_keys_are_not_modifiers() {
        for name in ["s", "a", "f4", "tab", "hyper", "fn", "mod4", "ctrl_l", ""] {
            assert_eq!(Modifier::parse(name), None, "{name:?}");
            assert!(!is_modifier(name));
        }
    }

    #[test]
    fn encodings_name_the_left_hand_keys() {
        assert_eq!(Modifier::Ctrl.keysym(), 0xFFE3);
        assert_eq!(Modifier::Shift.keysym(), 0xFFE1);
        assert_eq!(Modifier::Alt.keysym(), 0xFFE9);
        assert_eq!(Modifier::Super.keysym(), 0xFFEB);
        assert_eq!(Modifier::Super.keysym_name(), "Super_L");
        assert_eq!(Modifier::Alt.evdev_code(), 56);
        assert_eq!(Modifier::Super.evdev_code(), 125);
        let mask: u16 = [Modifier::Ctrl, Modifier::Shift, Modifier::Alt, Modifier::Super]
            .iter()
            .map(|m| m.state_mask())
            .fold(0, |mask, bit| mask | bit);
        assert_eq!(mask, 1 | 4 | 8 | 64);
    }

    #[test]
    fn lists_parse_in_order_without_repeats_and_name_unknowns() {
        assert_eq!(
            parse_modifiers(&["Command", "shift", "cmd", "control"]),
            Ok(vec![Modifier::Super, Modifier::Shift, Modifier::Ctrl])
        );
        assert_eq!(
            canonical_modifier_names(&["option", "WIN"]),
            Ok(vec!["alt".to_owned(), "super".to_owned()])
        );
        assert_eq!(
            parse_modifiers(&["ctrl", "hyper", "fn"]),
            Err(vec!["hyper".to_owned(), "fn".to_owned()])
        );
        assert_eq!(parse_modifiers::<&str>(&[]), Ok(vec![]));
    }
}

use super::is_enter_key;
use super::press_key_chord;

#[test]
fn unmodified_press_stays_on_the_single_key_route() {
    assert_eq!(press_key_chord(&[], "return"), None);
}

#[test]
fn enter_key_gate_accepts_the_documented_enter_spelling() {
    assert!(is_enter_key("enter"));
    assert!(is_enter_key("Enter"));
    assert!(is_enter_key("ENTER"));
}

#[test]
fn enter_key_gate_accepts_the_documented_return_spelling() {
    // `key_name_to_keysym` resolves "return" and "enter" to the same
    // physical key (XK_Return, 0xFF0D); the pty short-circuit must not
    // drop a documented spelling on terminals (they discard synthetic
    // XSendEvent keys, so a dropped short-circuit loses the keypress).
    assert!(is_enter_key("return"));
    assert!(is_enter_key("Return"));
    assert!(is_enter_key("RETURN"));
}

#[test]
fn enter_key_gate_rejects_non_enter_keys() {
    assert!(!is_enter_key("tab"));
    assert!(!is_enter_key("escape"));
    assert!(!is_enter_key("space"));
    assert!(!is_enter_key(""));
}

#[test]
fn modifiers_are_promoted_to_a_chord_in_order() {
    assert_eq!(
        press_key_chord(&["ctrl".to_owned()], "s"),
        Some(vec!["ctrl".to_owned(), "s".to_owned()])
    );
    assert_eq!(
        press_key_chord(&["ctrl".to_owned(), "shift".to_owned()], "t"),
        Some(vec!["ctrl".to_owned(), "shift".to_owned(), "t".to_owned()])
    );
}

#[test]
fn hotkey_keys_take_every_modifier_spelling() {
    use super::split_hotkey_keys;
    let keys = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
    // ["option","s"] used to send a bare "s": option was read as the key.
    assert_eq!(
        split_hotkey_keys(&keys(&["option", "s"])).unwrap(),
        (vec!["alt".to_owned()], "s".to_owned())
    );
    assert_eq!(
        split_hotkey_keys(&keys(&["Command", "shift", "z"])).unwrap(),
        (vec!["super".to_owned(), "shift".to_owned()], "z".to_owned())
    );
    assert_eq!(
        split_hotkey_keys(&keys(&["windows", "e"])).unwrap(),
        (vec!["super".to_owned()], "e".to_owned())
    );
}

#[test]
fn hotkey_refuses_a_chord_whose_extra_key_is_not_a_modifier() {
    use super::split_hotkey_keys;
    let keys = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
    let refusal = split_hotkey_keys(&keys(&["hyper", "s"])).unwrap_err();
    let structured = refusal.structured_content.as_ref().unwrap();
    assert_eq!(structured["code"], "invalid_arguments");
    assert_eq!(structured["unknown"], serde_json::json!(["hyper"]));
    let text = format!("{:?}", refusal.content);
    assert!(text.contains("hyper") && text.contains("plus one key"), "{text}");
    assert!(split_hotkey_keys(&keys(&["ctrl", "shift"])).is_err());
}

#[test]
fn modifier_arguments_are_canonical_or_refused() {
    use super::modifier_arg;
    let names = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
    assert_eq!(
        modifier_arg("click", "modifier", &names(&["cmd", "Option", "control"])).unwrap(),
        ["super", "alt", "ctrl"]
    );
    let refusal = modifier_arg("press_key", "modifiers", &names(&["ctrl", "fn"])).unwrap_err();
    let structured = refusal.structured_content.unwrap();
    assert_eq!(structured["code"], "invalid_arguments");
    assert_eq!(structured["field"], "modifiers");
    assert_eq!(structured["unknown"], serde_json::json!(["fn"]));
}

#[test]
fn wm_chords_read_every_super_and_alt_spelling() {
    use super::{wm_chord_kind, WmChord};
    let mods = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
    assert_eq!(
        wm_chord_kind("f4", &mods(&["option"])),
        Some(WmChord::CloseWindow)
    );
    assert_eq!(
        wm_chord_kind("e", &mods(&["windows"])),
        Some(WmChord::Unavailable)
    );
    assert_eq!(wm_chord_kind("s", &mods(&["ctrl"])), None);
}

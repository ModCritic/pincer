use super::*;

#[test]
fn a_file_chooser_keeping_the_basename_counts_as_committed() {
    assert!(path_normalised_to_basename(
        "/home/user/Desktop/tone.wav",
        "tone.wav"
    ));
    assert!(!path_normalised_to_basename(
        "/home/user/Desktop/tone.wav",
        "other.wav"
    ));
    assert!(!path_normalised_to_basename("tone.wav", "tone.wav"));
    let ok = set_value_result(
        3,
        "/home/user/Desktop/tone.wav",
        "ax",
        Some("tone.wav".into()),
        false,
        None,
    );
    let s = ok.structured_content.unwrap();
    assert_eq!(s["verified"], true);
    assert_eq!(s["effect"], "confirmed");
}

#[test]
fn readback_agreement_is_numeric_aware() {
    assert!(values_agree("40", "40.0"));
    assert!(values_agree("40", " 40 "));
    assert!(values_agree("abc", "abc"));
    assert!(!values_agree("40", "41"));
    assert!(!values_agree("40", "forty"));
}

#[test]
fn set_value_result_is_verified_only_on_matching_readback() {
    let ok = set_value_result(3, "40", "click_type_mpx", Some("40.0".into()), false, None);
    let s = ok.structured_content.unwrap();
    assert_eq!(s["verified"], true);
    assert_eq!(s["effect"], "confirmed");
    let noop = set_value_result(3, "40", "ax", Some("10.0".into()), false, None);
    let s = noop.structured_content.unwrap();
    assert_eq!(s["verified"], false);
    assert_eq!(s["effect"], "suspected_noop");
    assert_eq!(s["readback"], "10.0");
    let blind = set_value_result(3, "40", "ax", None, false, None);
    assert_eq!(blind.structured_content.unwrap()["effect"], "unverifiable");
}

fn result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            pincer_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn masked_text_is_one_repeated_symbol_or_empty() {
    assert_eq!(masked_length("\u{25cf}\u{25cf}\u{25cf}"), Some(3));
    assert_eq!(masked_length("\u{2022}\u{2022}"), Some(2));
    assert_eq!(masked_length("****"), Some(4));
    assert_eq!(masked_length(""), Some(0));
    assert_eq!(masked_length("hunter22"), None);
    assert_eq!(masked_length("aaaa"), None);
    assert_eq!(masked_length("\u{25cf}*"), None);
    assert_eq!(masked_length("   "), None);
    // Qt masks UTF-16 units: "a" + U+1F600 + "b" read back as four masks.
    assert!(mask_count_fits(4, "a\u{1F600}b"));
    assert!(mask_count_fits(3, "a\u{1F600}b"));
    assert!(!mask_count_fits(5, "a\u{1F600}b"));
}

#[test]
fn a_password_field_with_the_values_length_in_masks_is_a_masked_length_match() {
    let masks = "\u{25cf}".repeat(8);
    let result = set_value_result(8, "hunter22", "ax", Some(masks), true, None);
    let s = result.structured_content.clone().unwrap();
    assert_eq!(s["verified"], true);
    assert_eq!(s["effect"], "confirmed");
    assert_eq!(s["readback_masked"], true);
    assert_eq!(s["masked_length"], 8);
    assert_eq!(s["evidence"][0]["kind"], "value_readback");
    assert!(s.get("escalation").is_none());
    let text = result_text(&result);
    assert!(text.contains("masked-length match"), "{text}");
    assert!(!text.contains("may not have committed"), "{text}");
    assert!(!text.contains("hunter22"), "the password is echoed: {text}");
}

#[test]
fn a_password_field_with_another_length_is_unreadable_not_a_noop() {
    let result = set_value_result(8, "hunter22", "ax", Some("\u{25cf}".repeat(5)), true, None);
    let s = result.structured_content.clone().unwrap();
    assert_eq!(s["verified"], false);
    assert_eq!(s["effect"], "unverifiable");
    assert_eq!(s["readback_masked"], true);
    assert_eq!(s["masked_length"], 5);
    assert!(s.get("escalation").is_none());
    let text = result_text(&result);
    assert!(text.contains("can't be read back"), "{text}");
    assert!(!text.contains("may not have committed"), "{text}");
    assert!(!text.contains("hunter22"), "the password is echoed: {text}");
    // An empty password field after a non-empty write: still not a no-op claim.
    let empty = set_value_result(8, "hunter22", "ax", Some(String::new()), true, None);
    assert_eq!(empty.structured_content.unwrap()["effect"], "unverifiable");
}

#[test]
fn masks_in_a_field_that_is_not_a_password_field_still_disagree() {
    let result = set_value_result(8, "hunter22", "ax", Some("\u{25cf}".repeat(8)), false, None);
    assert_eq!(
        result.structured_content.unwrap()["effect"],
        "suspected_noop"
    );
    // A password field showing its text in the clear compares as text.
    let shown = set_value_result(8, "hunter22", "ax", Some("hunter21".into()), true, None);
    assert_eq!(
        shown.structured_content.unwrap()["effect"],
        "suspected_noop"
    );
    let plain = set_value_result(8, "hunter22", "ax", Some("hunter22".into()), true, None);
    assert_eq!(plain.structured_content.unwrap()["effect"], "confirmed");
}

#[test]
fn an_index_of_a_closed_window_is_a_stale_token() {
    let error: anyhow::Error = crate::atspi::native::WindowClosed { xid: 90177604 }.into();
    let result = input_error_result(error);
    assert_eq!(result.is_error, Some(true));
    let text = result_text(&result);
    assert!(text.contains("window 90177604 has closed"), "{text}");
    assert!(text.contains("nothing was done"), "{text}");
    let s = result.structured_content.unwrap();
    assert_eq!(s["code"], "stale_element_token");
    assert_eq!(s["effect"], "none");
    assert_eq!(s["window_id"], 90177604);
}

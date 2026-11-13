use super::*;

#[test]
fn typed_text_is_confirmed_only_when_read_back() {
    let confirmed = type_text_readback_result(
        1,
        3,
        "-40",
        "via targeted AT-SPI",
        Some("-40.0".into()),
        None,
    );
    let s = confirmed.structured_content.clone().unwrap();
    assert_eq!(s["effect"], "confirmed");
    assert_eq!(s["verified"], true);
    assert_eq!(s["evidence"][0]["kind"], "value_readback");
    let contained = type_text_readback_result(1, 3, "ab", "r", Some("xxabyy".into()), None);
    assert_eq!(contained.structured_content.unwrap()["effect"], "confirmed");
    let mismatch = type_text_readback_result(1, 3, "ab", "r", Some("zz".into()), None);
    let s = mismatch.structured_content.unwrap();
    assert_eq!(s["effect"], "unverifiable");
    assert_eq!(s["readback"], "zz");
    let blind = type_text_readback_result(1, 3, "ab", "r", None, None);
    assert_eq!(blind.structured_content.unwrap()["effect"], "unverifiable");
}

#[test]
fn a_read_back_quotes_the_typed_text_with_a_little_context() {
    // Round 1 quoted the first ~200 characters of the whole document.
    let doc = format!(
        "{}HELLO FROM T2 {}",
        "001 The quick brown fox jumps over the lazy dog.\n".repeat(3),
        "002 The quick brown fox jumps over the lazy dog.\n".repeat(50)
    );
    let result = type_text_readback_result(1, 0, "HELLO FROM T2 ", "r", Some(doc), None);
    let s = result.structured_content.clone().unwrap();
    assert_eq!(s["effect"], "confirmed");
    assert_eq!(
        s["readback"],
        "... over the lazy dog.\\nHELLO FROM T2 002 The quick brown ..."
    );
    // A short value is quoted whole.
    let short = type_text_readback_result(1, 0, "ab", "r", Some("xxab".into()), None);
    assert_eq!(short.structured_content.unwrap()["readback"], "xxab");
    // Long typed text is confirmed without echoing it.
    let typed = "z".repeat(100);
    let long = type_text_readback_result(
        1,
        0,
        &typed,
        "r",
        Some(format!("{}{typed}", "a".repeat(100))),
        None,
    );
    let s = long.structured_content.clone().unwrap();
    assert_eq!(s["effect"], "confirmed");
    assert!(s.get("readback").is_none());
    let said = format!("{:?}", long.content);
    assert!(
        said.contains("reads back the typed text in place"),
        "{said}"
    );
}

#[test]
fn typing_into_a_password_field_is_judged_by_how_much_it_grew() {
    let masks = |n: usize| Some("\u{25cf}".repeat(n));
    let grew = type_text_readback_result(1, 8, "xyz", "via targeted AT-SPI", masks(11), Some(8));
    let s = grew.structured_content.clone().unwrap();
    assert_eq!(s["effect"], "confirmed");
    assert_eq!(s["verified"], true);
    assert_eq!(s["readback_masked"], true);
    assert_eq!(s["masked_length_before"], 8);
    assert_eq!(s["masked_length"], 11);
    assert_eq!(s["evidence"][0]["kind"], "value_readback");
    assert!(s.get("readback").is_none());
    let text = format!("{:?}", grew.content);
    assert!(text.contains("masked-length match"), "{text}");
    assert!(!text.contains("does not contain"), "{text}");
    assert!(
        !text.contains("xyz"),
        "the typed password is echoed: {text}"
    );

    let off = type_text_readback_result(1, 8, "xyz", "r", masks(3), Some(8));
    let s = off.structured_content.clone().unwrap();
    assert_eq!(s["effect"], "unverifiable");
    assert_eq!(s["verified"], false);
    let text = format!("{:?}", off.content);
    assert!(text.contains("can't be read back"), "{text}");
    assert!(!text.contains("does not contain"), "{text}");

    // Not a password field: the old comparison stands.
    let plain = type_text_readback_result(1, 8, "xyz", "r", masks(11), None);
    assert_eq!(
        plain.structured_content.unwrap()["readback"],
        "\u{25cf}".repeat(11)
    );
}

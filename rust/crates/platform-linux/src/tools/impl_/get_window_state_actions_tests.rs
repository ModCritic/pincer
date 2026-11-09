use super::*;
use crate::atspi::AtspiNode;

fn node(actions: Vec<String>) -> AtspiNode {
    AtspiNode {
        element_index: Some(1),
        role: "button".to_owned(),
        name: Some("ok".to_owned()),
        value: None,
        checked: None,
        enabled: Some(true),
        selected: None,
        description: None,
        actions,
        element_key: 1,
        identity: None,
        depth: 0,
        parent_element_index: None,
        in_web_content: false,
        object_ref: None,
    }
}

#[test]
fn element_entry_omits_actions_when_empty() {
    let n = node(Vec::new());
    let entry = build_element_entry(&n, None, None).unwrap();
    assert!(entry.get("actions").is_none());
}

#[test]
fn element_entry_filters_blank_action_names() {
    let n = node(vec![
        "Press".to_owned(),
        "".to_owned(),
        "   ".to_owned(),
        "Open".to_owned(),
    ]);
    let entry = build_element_entry(&n, None, None).unwrap();
    assert_eq!(entry["actions"], json!(["Press", "Open"]));
}

#[test]
fn a_role_search_says_it_searched_roles_not_names() {
    let note = query_note(Some("button"), true, None, "- frame = \"Probe\"\n").unwrap();
    assert!(note.contains("query \"button\" names roles"));
    assert!(note.contains("without a walk"));
    assert!(note.contains("names were not searched"));
}

#[test]
fn a_query_with_no_match_on_a_cut_walk_says_the_rest_was_never_searched() {
    let note = query_note(Some("combo"), false, Some(Some("timeout")), "").unwrap();
    assert!(note.contains("No row matches query \"combo\""));
    assert!(note.contains("rows past the cut were not searched"));
    assert!(note.contains("larger timeout_ms"));
    // A match, a complete walk, or no query: nothing to add.
    let rows = "- frame = \"Probe\"\n  - [3] combo box \"Groceries\"\n";
    assert_eq!(
        query_note(Some("combo"), false, Some(Some("timeout")), rows),
        None
    );
    assert_eq!(query_note(Some("combo"), false, None, ""), None);
    assert_eq!(query_note(None, false, Some(Some("timeout")), ""), None);
    assert_eq!(query_note(Some("  "), true, None, ""), None);
}

#[test]
fn the_description_says_query_filters_and_the_limits_bound_the_walk() {
    use pincer_core::tool::Tool;
    let tool = GetWindowStateTool {
        state: ToolState::new(),
    };
    let def = tool.def();
    assert!(def.description.contains(
        "`timeout_ms` (when omitted, 2000 ms for a window's first read and 1000 ms after), \
         `max_elements` and `max_depth` bound the walk of the window; `query` only filters its rows."
    ));
    assert!(!def.description.contains("narrow with `query`"));
    let query = def.input_schema["properties"]["query"]["description"]
        .as_str()
        .unwrap();
    assert!(query.contains("does not shorten the walk"));
    assert!(query.contains("role words"));
}

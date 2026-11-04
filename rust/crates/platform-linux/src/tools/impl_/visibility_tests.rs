use super::*;

#[test]
fn framed_elements_lead_and_keep_their_relative_order() {
    let elements = vec![
        json!({"element_index": 0, "role": "menu"}),
        json!({"element_index": 1, "role": "push button", "frame": {"x": 1, "y": 1, "w": 2, "h": 2}}),
        json!({"element_index": 2, "role": "menu"}),
        json!({"element_index": 3, "role": "paragraph", "frame": {"x": 1, "y": 9, "w": 2, "h": 2}}),
    ];
    let ordered: Vec<u64> = framed_elements_first(elements)
        .iter()
        .map(|e| e["element_index"].as_u64().unwrap())
        .collect();
    assert_eq!(ordered, vec![1, 3, 0, 2]);
}

#[test]
fn a_frameless_popup_returns_only_the_menu_entries_drawn_inside_it() {
    let elements = vec![
        json!({"element_index": 1, "role": "push button", "frame": {"x": 5, "y": 5, "w": 20, "h": 20}}),
        json!({"element_index": 2, "role": "menu item", "label": "Paragraph...", "frame": {"x": 2, "y": 30, "w": 180, "h": 22}}),
        json!({"element_index": 3, "role": "menu item", "label": "elsewhere", "frame": {"x": 400, "y": 30, "w": 180, "h": 22}}),
        json!({"element_index": 4, "role": "menu", "label": "Format"}),
    ];
    let kept: Vec<u64> = popup_menu_elements(elements.clone(), (0, 0), 200, 400)
        .iter()
        .map(|e| e["element_index"].as_u64().unwrap())
        .collect();
    assert_eq!(kept, vec![2]);
    assert_eq!(
        popup_menu_elements(elements[..1].to_vec(), (0, 0), 200, 400).len(),
        1
    );
}

#[test]
fn a_combo_popup_returns_the_rows_of_the_list_that_fills_it() {
    // Qt: the combo list lives under the combo box inside the dialog; the
    // main window's menubar entries are elsewhere on the screen.
    let elements = vec![
        json!({"element_index": 1, "role": "menu", "label": "Audio", "frame": {"x": -600, "y": -300, "w": 50, "h": 20}}),
        json!({"element_index": 7, "role": "list", "frame": {"x": 1, "y": 1, "w": 198, "h": 118}}),
        json!({"element_index": 8, "role": "list item", "label": "Desktop", "parent_index": 7, "frame": {"x": 2, "y": 2, "w": 190, "h": 22}}),
        json!({"element_index": 9, "role": "list item", "label": "user", "parent_index": 7, "frame": {"x": 2, "y": 26, "w": 190, "h": 22}}),
        json!({"element_index": 12, "role": "push button", "label": "Open", "frame": {"x": 20, "y": 20, "w": 60, "h": 20}}),
    ];
    let kept: Vec<u64> = popup_menu_elements(elements, (0, 0), 200, 120)
        .iter()
        .map(|e| e["element_index"].as_u64().unwrap())
        .collect();
    assert_eq!(kept, vec![8, 9]);
}

#[test]
fn a_list_popup_without_an_indexed_container_keeps_its_item_rows_only() {
    let elements = vec![
        json!({"element_index": 1, "role": "menu", "label": "Audio", "frame": {"x": 1, "y": 1, "w": 50, "h": 20}}),
        json!({"element_index": 8, "role": "list item", "label": "Desktop", "frame": {"x": 2, "y": 2, "w": 190, "h": 22}}),
        json!({"element_index": 9, "role": "tree item", "label": "user", "frame": {"x": 2, "y": 26, "w": 190, "h": 22}}),
        json!({"element_index": 12, "role": "push button", "label": "Open", "frame": {"x": 20, "y": 20, "w": 60, "h": 20}}),
    ];
    let kept: Vec<u64> = popup_menu_elements(elements, (0, 0), 200, 120)
        .iter()
        .map(|e| e["element_index"].as_u64().unwrap())
        .collect();
    assert_eq!(kept, vec![8, 9]);
}

#[test]
fn menu_roles_take_the_real_press() {
    for role in ["menu", "Menu Item", "check menu item", "radio menu item"] {
        assert!(element_is_menu_role(role), "{role}");
    }
    for role in ["menu bar", "push button", "text"] {
        assert!(!element_is_menu_role(role), "{role}");
    }
}

#[test]
fn overlays_name_the_follow_up_call_per_dialog_and_popup() {
    let overlays = WindowOverlays {
        dialogs: vec![json!({
            "window_id": 71, "title": "Position and Size", "transient_for": 3, "modal": true,
            "bounds": {"x": 100, "y": 100, "width": 400, "height": 300}
        })],
        popups: vec![json!({
            "window_id": 72, "title": "", "pid": 5,
            "bounds": {"x": 10, "y": 30, "width": 200, "height": 400}
        })],
        covers_window: true,
        window_rect: Some((0, 0, 800, 600)),
    };
    let note = overlays.follow_up(5).unwrap();
    assert!(
        note.contains("dialog \"Position and Size\" (window_id 71, transient of window 3, modal)"),
        "{note}"
    );
    assert!(
        note.contains("get_window_state(pid=5, window_id=71)"),
        "{note}"
    );
    assert!(
        note.contains("popup (window_id 72, bounds x=10 y=30 200x400)"),
        "{note}"
    );
    assert!(rects_intersect((0, 0, 800, 600), (100, 100, 400, 300)));
    assert!(!rects_intersect((0, 0, 800, 600), (800, 0, 10, 10)));
}

#[test]
fn a_cross_process_dialog_names_its_real_owning_pid_in_the_follow_up() {
    // GIMP's "Export Image as JPEG" dialog spawned as a separate process
    // (owning_pid 999) but is transient-for the target pid (5)'s window.
    let overlays = WindowOverlays {
        dialogs: vec![json!({
            "window_id": 90, "title": "Export Image as JPEG", "transient_for": 10,
            "modal": false,
            "bounds": {"x": 0, "y": 0, "width": 300, "height": 200},
            "owning_pid": 999,
        })],
        popups: vec![],
        covers_window: false,
        window_rect: Some((0, 0, 800, 600)),
    };
    let note = overlays.follow_up(5).unwrap();
    assert!(
        note.contains("owned by a different process pid 999, not pid 5"),
        "{note}"
    );
    assert!(
        note.contains("get_window_state(pid=5, window_id=90)"),
        "{note}"
    );
}

fn popup_test_node(index: usize, role: &str, parent: Option<usize>) -> crate::atspi::AtspiNode {
    crate::atspi::AtspiNode {
        element_index: Some(index),
        role: role.into(),
        name: None,
        value: None,
        checked: None,
        enabled: None,
        selected: None,
        description: None,
        actions: Vec::new(),
        element_key: index as u64,
        identity: None,
        depth: 0,
        parent_element_index: parent,
        in_web_content: false,
        object_ref: None,
    }
}

fn kate_projects_popup() -> crate::input::PopupWindow {
    crate::input::PopupWindow {
        window: 98566169,
        x: 312,
        y: 56,
        width: 523,
        height: 258,
        title: String::new(),
        pid: Some(62856),
    }
}

#[test]
fn a_popup_read_prints_the_open_menu_not_the_main_window() {
    // Kate with its Projects menu open (2026-11-03): the walk is
    // application-wide, the popup's items hang under the menubar item.
    let md = "- frame = \"kate-scratch.txt  — Kate\"\n\
              \x20   - [17] list item \"\" [actions=[Toggle]]\n\
              \x20   - [25] text \"kate-scratch.txt\" value=\"alpha\" [actions=[]]\n\
              \x20 - menu bar = \"\"\n\
              \x20   - [51] menu item \"Projects\" [actions=[Show Menu]]\n\
              \x20     - popup menu = \"Kate\"\n\
              \x20       - menu item = \"Close All Projects\" (disabled)\n\
              \x20       - [52] menu item \"Reload All Projects\" [actions=[Press]]\n\
              \x20       - [53] menu item \"Project TODOs\" [actions=[Press]]\n\
              \x20   - [54] menu item \"LSP Client\" [actions=[Show Menu]]\n\
              \x20 - [57] push button \"New\" [actions=[Press,Set Focus]]\n";
    let nodes = vec![
        popup_test_node(17, "list item", None),
        popup_test_node(25, "text", None),
        popup_test_node(51, "menu item", None),
        popup_test_node(52, "menu item", Some(51)),
        popup_test_node(53, "menu item", Some(51)),
        popup_test_node(54, "menu item", None),
        popup_test_node(57, "push button", None),
    ];
    let bounds = vec![
        // A breadcrumb separator of the main window, under the menu.
        (17, 520, 120, 20, 30),
        (25, 100, 169, 2359, 1170),
        (51, 312, 22, 87, 34),
        (52, 312, 240, 523, 36),
        (53, 312, 278, 523, 36),
        (54, 405, 22, 106, 34),
        (57, 6, 56, 80, 30),
    ];
    let (popup_md, rows) =
        popup_tree_markdown(md, &nodes, &bounds, &kate_projects_popup(), true)
            .expect("popup rows");
    assert_eq!(
        popup_md,
        "- popup menu = \"Kate\"\n\
         \x20 - menu item = \"Close All Projects\" (disabled)\n\
         \x20 - [52] menu item \"Reload All Projects\" [actions=[Press]]\n\
         \x20 - [53] menu item \"Project TODOs\" [actions=[Press]]\n"
    );
    let mut rows: Vec<usize> = rows.into_iter().collect();
    rows.sort_unstable();
    assert_eq!(rows, vec![52, 53]);
}

#[test]
fn a_popup_with_no_accessible_items_is_reported_as_such() {
    // KWrite's text-area context menu: Qt publishes no node for it, so
    // nothing of the walk is drawn inside the popup.
    let md = "- frame = \"kwrite-scratch.txt  — KWrite\"\n\
              \x20 - [0] text \"kwrite-scratch.txt\" [actions=[]]\n\
              \x20 - [16] push button \"New\" [actions=[Press,Set Focus]]\n";
    let nodes = vec![
        popup_test_node(0, "text", None),
        popup_test_node(16, "push button", None),
    ];
    let bounds = vec![(0, 960, 538, 600, 360), (16, 966, 491, 80, 30)];
    let popup = crate::input::PopupWindow {
        window: 102760480,
        x: 1160,
        y: 747,
        width: 532,
        height: 366,
        title: "KWrite".into(),
        pid: Some(93219),
    };
    assert!(popup_tree_markdown(md, &nodes, &bounds, &popup, true).is_none());
}

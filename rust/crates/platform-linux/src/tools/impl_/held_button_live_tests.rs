//! Live checks of the held-button release on a private Xvfb. The test body
//! runs in a child copy of the test binary with `DISPLAY` set to that server
//! (see `crate::input::xlib_live_tests`).

use super::*;
use crate::input::xlib_live_tests::{assert_child_passed, in_child, run_child, Server};
use std::path::Path;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, KeyButMask, WindowClass};
use x11rb::rust_connection::RustConnection;

/// Whether the core pointer has its left button down.
fn left_button_down(conn: &RustConnection, root: u32) -> bool {
    let mask = conn
        .query_pointer(root)
        .expect("QueryPointer request")
        .reply()
        .expect("QueryPointer reply")
        .mask;
    mask.contains(KeyButMask::BUTTON1)
}

/// A mapped 200x200 window at (10, 10), destroyed again by the caller.
fn test_window(conn: &RustConnection, root: u32) -> u32 {
    let window = conn.generate_id().expect("window id");
    conn.create_window(
        0,
        window,
        root,
        10,
        10,
        200,
        200,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new(),
    )
    .expect("CreateWindow");
    conn.map_window(window).expect("MapWindow");
    conn.get_input_focus()
        .expect("sync request")
        .reply()
        .expect("sync");
    window
}

/// A window that closes while a button is held on it (its app quit or
/// crashed) must not leave the button pressed. mouse_button_up releases the
/// foreground hold's real XTest button where the pointer is, clears a
/// background hold (its synthetic press reached that window only), and says
/// the window was gone.
#[test]
#[ignore = "starts its own Xvfb; run inside a sandbox"]
fn a_held_button_is_released_after_its_window_closed() {
    const MARKER: &str = "HELD_BUTTON_RELEASED_AFTER_WINDOW_CLOSED";
    if in_child() {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(release_after_window_closed());
        println!("{MARKER}");
        return;
    }
    let server = Server::start(Path::new("Xvfb"));
    let output = run_child(
        "tools::impl_::held_button_live_tests::a_held_button_is_released_after_its_window_closed",
        &server.display,
    );
    assert_child_passed(&output, MARKER);
}

async fn release_after_window_closed() {
    let (conn, screen_num) = RustConnection::connect(None).expect("connect");
    let root = conn.setup().roots[screen_num].root;
    let state = ToolState::new();
    let tool = MouseButtonUpTool {
        state: state.clone(),
    };
    for foreground in [true, false] {
        let window = test_window(&conn, root);
        if foreground {
            crate::input::send_button_xtest_desktop(Some((50, 50)), 1, true).expect("XTest press");
            assert!(left_button_down(&conn, root), "the XTest press is down");
        }
        let cursor = format!("held-foreground-{foreground}");
        state.mouse_hold.lock().unwrap().insert(
            cursor.clone(),
            MouseHoldState {
                pid: std::process::id(),
                xid: u64::from(window),
                button: 1,
                x: 40.0,
                y: 40.0,
                foreground,
            },
        );
        conn.destroy_window(window).expect("DestroyWindow");
        conn.get_input_focus()
            .expect("sync request")
            .reply()
            .expect("sync");

        let result = tool.invoke(json!({ "cursor_id": cursor })).await;
        let text = format!("{:?}", result.content);
        assert_ne!(
            result.is_error,
            Some(true),
            "foreground={foreground}: {text}"
        );
        assert!(text.contains("is gone"), "foreground={foreground}: {text}");
        assert!(
            !state.mouse_hold.lock().unwrap().contains_key(&cursor),
            "foreground={foreground}: the hold is cleared"
        );
        assert!(
            !left_button_down(&conn, root),
            "foreground={foreground}: no button is left pressed"
        );
        let structured = result.structured_content.expect("structured result");
        assert_eq!(structured["held"], false, "{structured}");
        assert_eq!(structured["window_gone"], true, "{structured}");
    }
}

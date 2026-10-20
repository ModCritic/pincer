//! Transport-level tests for the removed capture-scope inputs: `start_session`
//! refuses `capture_scope`, and the persistent config key stays retired.

#![cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]

use cua_driver_testkit::RawDriver;
use serde_json::{json, Value};

fn call(driver: &mut RawDriver, id: u64, name: &str, arguments: Value) -> Value {
    driver.send(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    }));
    driver.recv()
}

fn code(response: &Value) -> Option<&str> {
    response["result"]["structuredContent"]["code"].as_str()
}

#[test]
fn start_session_refuses_the_removed_capture_scope_input() {
    let mut driver = RawDriver::spawn().expect("spawn source-built driver");
    driver.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    driver.recv();
    let response = call(
        &mut driver,
        2,
        "start_session",
        json!({"session": "removed-capture-scope", "capture_scope": "window"}),
    );
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(code(&response), Some("invalid_arguments"), "{response}");
}

#[test]
fn persistent_capture_scope_key_is_retired() {
    let mut driver = RawDriver::spawn().expect("spawn source-built driver");
    driver.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    driver.recv();
    let response = call(
        &mut driver,
        2,
        "set_config",
        json!({"key": "capture_scope", "value": "desktop"}),
    );
    assert_eq!(code(&response), Some("config_key_retired"));
    assert_eq!(
        response["result"]["structuredContent"]["replacement"],
        "action.target"
    );
}

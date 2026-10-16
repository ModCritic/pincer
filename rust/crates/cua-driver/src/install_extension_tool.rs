//! `install_extension` MCP tool.
//!
//! This build ships no installable extensions. The optional perception
//! extension (the visual-region parser behind `parse_visual_regions`) was
//! removed together with its installer, so the tool stays in `tools/list` only
//! to give callers a clear, stable refusal instead of an unknown-tool error.
//! It never reads a catalog, touches the network, or writes to disk.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::OnceLock;

use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
};

pub(crate) fn register_host_tools(registry: &mut ToolRegistry) {
    registry.register(Box::new(InstallExtensionTool));
}

struct InstallExtensionTool;

static DEF: OnceLock<ToolDef> = OnceLock::new();

fn refusal(name: Option<&str>) -> String {
    match name {
        Some("perception" | "cua-perception") => {
            "the perception extension is not available in this build".to_owned()
        }
        Some(other) => {
            format!("unknown extension \"{other}\"; no extensions are available in this build")
        }
        None => "install_extension requires a name; no extensions are available in this build"
            .to_owned(),
    }
}

#[async_trait]
impl Tool for InstallExtensionTool {
    fn def(&self) -> &ToolDef {
        DEF.get_or_init(|| ToolDef {
            name: "install_extension".to_owned(),
            description: "Refuses every request: this build ships no optional extensions."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Extension name."},
                    "confirm": {"type": "boolean", "description": "Ignored; nothing can be installed."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        ToolResult::error(refusal(args.get("name").and_then(Value::as_str)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn perception_is_refused_with_a_clear_message() {
        for name in ["perception", "cua-perception"] {
            for confirm in [false, true] {
                let result = InstallExtensionTool
                    .invoke(serde_json::json!({"name": name, "confirm": confirm}))
                    .await;
                assert!(result.is_error.unwrap_or(false));
                let text = serde_json::to_string(&result.content).unwrap();
                assert!(
                    text.contains("the perception extension is not available in this build"),
                    "{text}"
                );
            }
        }
    }

    #[tokio::test]
    async fn every_other_name_is_refused() {
        let result = InstallExtensionTool
            .invoke(serde_json::json!({"name": "anything"}))
            .await;
        assert!(result.is_error.unwrap_or(false));
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(
            text.contains("no extensions are available in this build"),
            "{text}"
        );
    }

    #[test]
    fn the_tool_is_advertised_as_inert() {
        let def = InstallExtensionTool.def();
        assert!(def.read_only);
        assert!(!def.destructive);
        assert!(!def.open_world);
    }
}

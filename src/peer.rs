//! Glue between the engine's view of a client and the MCP SDK's.
//!
//! A tool the catalog defines is run by the engine, which reports progress through its own
//! `PeerChannel`. This forwards those notifications to the client of the current call and
//! turns the engine's result into the SDK's.

use std::time::Duration;

use async_trait::async_trait;
use mcp_tools::peer::PeerChannel;
use mcp_types::tool::{ContentItem, ToolResult};
use rmcp::model::{CallToolResult, ContentBlock, ProgressNotificationParam, ProgressToken};
use rmcp::service::{Peer, RoleServer};
use serde_json::Value;

/// Forwards the engine's progress notifications; asks the client nothing.
pub struct ProgressChannel {
    peer: Peer<RoleServer>,
}

impl ProgressChannel {
    pub fn new(peer: Peer<RoleServer>) -> Self {
        Self { peer }
    }
}

#[async_trait]
impl PeerChannel for ProgressChannel {
    async fn notify(&self, method: &str, params: Value) {
        if method != "notifications/progress" {
            return;
        }
        let token = params
            .get("progressToken")
            .cloned()
            .and_then(|token| serde_json::from_value::<ProgressToken>(token).ok());
        let (Some(token), Some(progress)) = (token, params.get("progress").and_then(Value::as_f64))
        else {
            return;
        };
        let mut notification = ProgressNotificationParam::new(token, progress);
        if let Some(total) = params.get("total").and_then(Value::as_f64) {
            notification = notification.with_total(total);
        }
        if let Some(message) = params.get("message").and_then(Value::as_str) {
            notification = notification.with_message(message);
        }
        let _ = self.peer.notify_progress(notification).await;
    }

    async fn request(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> Result<Value, String> {
        Err("this server asks the client nothing in the middle of a call".to_string())
    }
}

/// The engine's result of a tool call, as the SDK's.
pub fn convert_result(result: ToolResult) -> CallToolResult {
    let content: Vec<ContentBlock> = result
        .content
        .iter()
        .filter_map(|item| match item {
            ContentItem::Text { text } => Some(ContentBlock::text(text.clone())),
            // Anything else the engine can carry is passed through when the SDK reads it.
            other => serde_json::to_value(other)
                .ok()
                .and_then(|value| serde_json::from_value::<ContentBlock>(value).ok()),
        })
        .collect();
    let mut converted = if result.is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    converted.structured_content = result.structured_content;
    converted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_engine_result_keeps_its_text_structure_and_error_flag() {
        let converted = convert_result(ToolResult {
            content: vec![ContentItem::text("echo: hello")],
            structured_content: Some(json!({"exit": 0})),
            is_error: false,
            meta: None,
        });
        let value = serde_json::to_value(&converted).unwrap();
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "echo: hello");
        assert_eq!(value["structuredContent"]["exit"], 0);
        assert_ne!(value["isError"], true);

        let failed = convert_result(ToolResult::error("it broke"));
        let value = serde_json::to_value(&failed).unwrap();
        assert_eq!(value["isError"], true);
        assert_eq!(value["content"][0]["text"], "it broke");
    }
}

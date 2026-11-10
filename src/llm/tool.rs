use std::sync::Arc;

use async_openai::types::responses::{FunctionTool, InputItem, Tool};
use serenity::all::{ChannelId, MessageId, UserId};

use crate::app::context::NelfieContext;
use crate::llm::user::UserContextScope;
use crate::typst_render::TypstDraftStore;

#[derive(Clone)]
pub struct ToolExecutionContext {
    pub channel_id: ChannelId,
    pub reply_to_message_id: Option<MessageId>,
    pub request_author_id: Option<UserId>,
    pub user_context_scope: UserContextScope,
    pub allow_mainline_model_change: bool,
    pub image_generation_enabled: bool,
    pub response_round: usize,
    pub typst_drafts: Arc<TypstDraftStore>,
}

pub struct ToolExecutionResult {
    pub text: String,
    pub input_items: Vec<InputItem>,
    pub input_bytes: usize,
    pub review_preview_id: Option<String>,
}

impl ToolExecutionResult {
    pub fn text(text: String) -> Self {
        Self {
            text,
            input_items: Vec::new(),
            input_bytes: 0,
            review_preview_id: None,
        }
    }
}

#[async_trait::async_trait]
pub trait LMTool: Send + Sync {
    fn define(&self) -> Tool {
        Tool::Function(FunctionTool {
            name: self.name(),
            description: Some(self.description()),
            parameters: Some(self.json_schema()),
            strict: Some(false),
            defer_loading: None,
        })
    }
    fn define_for_request(&self, _context: ToolExecutionContext) -> Tool {
        self.define()
    }
    fn json_schema(&self) -> serde_json::Value;
    fn description(&self) -> String;
    fn name(&self) -> String;
    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: NelfieContext,
    ) -> Result<String, String>;
    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        ob_ctx: NelfieContext,
        _context: ToolExecutionContext,
    ) -> Result<String, String> {
        self.execute(args, ob_ctx).await
    }

    async fn execute_with_context_result(
        &self,
        args: serde_json::Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, String> {
        self.execute_with_context(args, ob_ctx, context)
            .await
            .map(ToolExecutionResult::text)
    }
}

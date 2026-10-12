use async_openai::types::responses::{FunctionTool, Tool};

use crate::app::context::NelfieContext;
use crate::llm::user::UserContextScope;

#[derive(Clone, Copy)]
pub struct ToolExecutionContext {
    pub user_context_scope: UserContextScope,
    pub allow_mainline_model_change: bool,
    pub image_generation_enabled: bool,
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
}

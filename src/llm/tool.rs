use async_openai::types::responses::{FunctionTool, Tool};

use crate::app::context::NelfieContext;

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
    fn json_schema(&self) -> serde_json::Value;
    fn description(&self) -> String;
    fn name(&self) -> String;
    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: NelfieContext,
    ) -> Result<String, String>;
}

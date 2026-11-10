use async_openai::types::responses::{FunctionTool, Tool};

use crate::{
    app::context::NelfieContext,
    llm::{
        models::{ImageGenerationModels, Models},
        tool::{LMTool, ToolExecutionContext},
    },
};

const DESCRIPTION: &str = concat!(
    "Inspect or change the current user's selected model. Use get to read selections and set ",
    "only when the user asks to switch models. Valid targets and model IDs are supplied by ",
    "the schema and can vary by API round. A change applies from the next API round; choose ",
    "an image-generation model before a round that includes image generation. Mainline ",
    "reasoning effort stays low; this tool does not change it."
);

pub struct ModelControlTool;

impl ModelControlTool {
    pub fn new() -> Self {
        Self
    }

    fn mainline_models() -> Vec<String> {
        Models::list()
            .into_iter()
            .map(|model| model.to_string())
            .collect()
    }

    fn image_generation_models() -> Vec<String> {
        ImageGenerationModels::list()
            .into_iter()
            .map(|model| model.to_string())
            .collect()
    }

    fn schema(image_generation_enabled: bool) -> serde_json::Value {
        let mut targets = vec!["mainline"];
        if !image_generation_enabled {
            targets.push("image_generation");
        }
        let model_names = Self::mainline_models()
            .into_iter()
            .chain(Self::image_generation_models())
            .collect::<Vec<_>>();

        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get", "set"],
                    "description": "Use get to inspect selections; use set to change one model and provide its target and model ID."
                },
                "target": {
                    "type": "string",
                    "enum": targets,
                    "description": "Use only a target listed for this API round: mainline is the chat model and cannot be changed by scheduled responses; image_generation is the built-in image model. Omit for get to inspect both."
                },
                "model": {
                    "type": "string",
                    "enum": model_names,
                    "description": "Model ID for the selected target. Required by set; omit for get. The schema lists IDs for both targets, so choose a model valid for the selected target."
                },
                "$explain": {
                    "type": "string",
                    "description": "A brief explanation of this model selection."
                }
            },
            "required": ["action"]
        })
    }
}

#[async_trait::async_trait]
impl LMTool for ModelControlTool {
    fn define_for_request(&self, context: ToolExecutionContext) -> Tool {
        Tool::Function(FunctionTool {
            name: self.name(),
            description: Some(self.description()),
            parameters: Some(Self::schema(context.image_generation_enabled)),
            strict: Some(false),
            defer_loading: None,
        })
    }

    fn json_schema(&self) -> serde_json::Value {
        Self::schema(false)
    }

    fn description(&self) -> String {
        DESCRIPTION.to_owned()
    }

    fn name(&self) -> String {
        "model-control".to_string()
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ob_ctx: NelfieContext,
    ) -> Result<String, String> {
        Err("Model selection requires an active user context".to_string())
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<String, String> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or("Missing or invalid 'action'".to_string())?;
        let target = args.get("target").and_then(serde_json::Value::as_str);

        if action == "get" {
            let settings = ob_ctx
                .user_contexts
                .get_or_create_scoped(context.user_context_scope);
            let value = match target {
                Some("mainline") => serde_json::json!({
                    "target": "mainline",
                    "selected": settings.main_model.to_string(),
                    "available": Self::mainline_models(),
                    "reasoning_effort": "low"
                }),
                Some("image_generation") => serde_json::json!({
                    "target": "image_generation",
                    "selected": settings.image_generation_model.to_string(),
                    "available": Self::image_generation_models()
                }),
                None => serde_json::json!({
                    "mainline": {
                        "selected": settings.main_model.to_string(),
                        "available": Self::mainline_models(),
                        "reasoning_effort": "low"
                    },
                    "image_generation": {
                        "selected": settings.image_generation_model.to_string(),
                        "available": Self::image_generation_models()
                    }
                }),
                Some(other) => return Err(format!("Unsupported model target: {other}")),
            };
            return Ok(value.to_string());
        }

        if action != "set" {
            return Err(format!("Unsupported action: {action}. Use get or set."));
        }

        let target = target.ok_or("Missing 'target' for set action".to_string())?;
        let model_name = args
            .get("model")
            .and_then(serde_json::Value::as_str)
            .ok_or("Missing 'model' for set action".to_string())?;

        match target {
            "mainline" => {
                if !context.allow_mainline_model_change {
                    return Err(
                        "Mainline model changes are unavailable for scheduled responses"
                            .to_string(),
                    );
                }
                let model = Models::from_name(model_name)
                    .ok_or_else(|| format!("Unsupported mainline model: {model_name}"))?;
                ob_ctx
                    .user_contexts
                    .set_model_scoped(context.user_context_scope, model);
                Ok(serde_json::json!({
                    "target": target,
                    "selected": model.to_string(),
                    "reasoning_effort": "low",
                    "applies_from": "next API round"
                })
                .to_string())
            }
            "image_generation" => {
                if context.image_generation_enabled {
                    return Err(
                        "Select the image model before the image-generation round".to_string()
                    );
                }
                let model = ImageGenerationModels::from_name(model_name)
                    .ok_or_else(|| format!("Unsupported image-generation model: {model_name}"))?;
                ob_ctx
                    .user_contexts
                    .set_image_generation_model_scoped(context.user_context_scope, model);
                Ok(serde_json::json!({
                    "target": target,
                    "selected": model.to_string(),
                    "applies_from": "next API round"
                })
                .to_string())
            }
            other => Err(format!("Unsupported model target: {other}")),
        }
    }
}

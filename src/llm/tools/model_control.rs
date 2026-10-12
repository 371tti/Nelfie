use async_openai::types::responses::{FunctionTool, Tool};

use crate::{
    app::context::NelfieContext,
    llm::{
        models::{ImageGenerationModels, Models},
        tool::{LMTool, ToolExecutionContext},
    },
};

const DESCRIPTION: &str = concat!(
    "Read or change the user's selected model. Targets: mainline for the Responses LLM, or ",
    "image_generation for the built-in image tool. Before generating or editing an image, ",
    "call this tool with action=get and target=image_generation, or action=set and ",
    "target=image_generation if the user requested a specific image model. The image tool ",
    "becomes available on the next API round with that selection. Image model selection is ",
    "limited to gpt-image-2.5-flare (default) and gpt-image-2.5-sunburst. Mainline selection ",
    "is limited to the available LLM model IDs. Mainline changes apply from the next API ",
    "round; reasoning effort remains low. Do not call image generation in the same round as ",
    "changing its model."
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
                    "description": "Read current selections or change one."
                },
                "target": {
                    "type": "string",
                    "enum": targets,
                    "description": "mainline changes the Responses LLM; image_generation changes the built-in image tool model."
                },
                "model": {
                    "type": "string",
                    "enum": model_names,
                    "description": "A model ID valid for the chosen target."
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

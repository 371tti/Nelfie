use async_openai::types::responses::{
    EasyInputContent, EasyInputMessage, ImageDetail, InputContent, InputImageContent, InputItem,
    InputTextContent, Role,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use serenity::all::{CreateAttachment, CreateMessage};

use crate::{
    app::context::NelfieContext,
    llm::tool::{LMTool, ToolExecutionContext, ToolExecutionResult},
    typst_render::{
        TypstOutputFormat, TypstRenderContent, TypstRenderInput, load_registry_asset,
        load_registry_assets, render, source_from_asset,
    },
};

pub struct TypstRenderTool;

impl TypstRenderTool {
    pub fn new() -> Self {
        Self
    }

    async fn run(
        &self,
        args: Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, String> {
        context.typst_drafts.ensure_capacity()?;

        let source = args.get("source").and_then(Value::as_str);
        let source_attachment_id = args
            .get("source_attachment_id")
            .and_then(Value::as_str)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|error| format!("Invalid 'source_attachment_id': {error}"))
            })
            .transpose()?;
        if source.is_some() == source_attachment_id.is_some() {
            return Err("Specify exactly one of source or source_attachment_id.".to_string());
        }
        if source.is_some_and(|source| source.trim().is_empty()) {
            return Err("Typst source must not be empty.".to_string());
        }
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .map(TypstOutputFormat::parse)
            .transpose()?
            .unwrap_or_default();

        let asset_ids = args
            .get("asset_ids")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .map(|value| {
                        value
                            .as_str()
                            .ok_or_else(|| {
                                "asset_ids must contain retained file or profile image asset IDs.".to_string()
                            })?
                            .parse::<u64>()
                            .map_err(|error| format!("Invalid asset attachment ID: {error}"))
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .transpose()?
            .unwrap_or_default();
        if asset_ids.len() > 64 {
            return Err("A Typst render can use at most 64 asset attachments.".to_string());
        }

        let (source_filename, source) = if let Some(source) = source {
            ("main.typ".to_string(), source.to_string())
        } else {
            source_from_asset(
                load_registry_asset(&ob_ctx, source_attachment_id.expect("validated source id"))
                    .await?,
            )?
        };
        let assets = load_registry_assets(&ob_ctx, &asset_ids, source_attachment_id).await?;
        let input = TypstRenderInput {
            source_filename,
            source,
            assets,
            format,
        };

        let rendered = render(input).await?;
        let page_count = match &rendered.content {
            TypstRenderContent::Pdf(_) => None,
            TypstRenderContent::Pages(pages) => Some(pages.len()),
        };
        let warnings = rendered.warnings.clone();
        let warning_count = warnings.len();
        let (draft_id, preview_pages) = context
            .typst_drafts
            .insert(format, context.response_round, rendered)?;
        let input_bytes = preview_pages
            .iter()
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        let input_items = preview_input_items(&preview_pages, &draft_id);

        Ok(ToolExecutionResult {
            text: json!({
                "status": "preview_ready",
                "draft_id": draft_id,
                "format": format,
                "page_count": page_count,
                "warning_count": warning_count,
                "warnings": warnings,
                "note": "PNG page previews for this exact draft are attached to the next model request. Review every page. This draft has not been sent to Discord. Pass this draft_id to typst-publish to publish these exact reviewed bytes.",
            })
            .to_string(),
            input_items,
            input_bytes,
            review_preview_id: Some(draft_id),
        })
    }
}

impl Default for TypstRenderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LMTool for TypstRenderTool {
    fn name(&self) -> String {
        "typst-render".to_string()
    }

    fn description(&self) -> String {
        "Render Typst and return PNG page previews for review without posting anything to Discord. Every render gets a distinct draft_id, and its preview labels carry that ID. For a user-uploaded .typ file, pass its attachment ID as source_attachment_id; do not read or copy its source text. The selected format controls the published file. Inspect every page and warning, then pass the draft_id of the exact preview you chose to typst-publish in a later response round. Images and fonts can be passed by retained attachment IDs, including IDs returned by get_user_profile. Imports may use only official @preview packages; arbitrary URLs and host files are unavailable.".to_string()
    }

    fn json_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Typst source code. Use only when the user did not provide a .typ attachment; otherwise pass its attachment ID as source_attachment_id.",
                },
                "source_attachment_id": {
                    "type": "string",
                    "description": "Discord attachment ID of the user's UTF-8 .typ file. Use this instead of reading or copying the file's contents into source.",
                },
                "asset_ids": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Retained Discord attachment IDs or reusable profile image IDs returned by get_user_profile, for images, fonts, or files imported by the source.",
                },
                "format": {
                    "type": "string",
                    "enum": ["png", "svg", "pdf"],
                    "description": "Final Discord output format. Defaults to png; select svg or pdf only when explicitly requested. Preview is always PNG.",
                },
            },
            "required": [],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, _args: Value, _ob_ctx: NelfieContext) -> Result<String, String> {
        Err("The Typst renderer requires request context.".to_string())
    }

    async fn execute_with_context(
        &self,
        args: Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<String, String> {
        self.run(args, ob_ctx, context)
            .await
            .map(|result| result.text)
    }

    async fn execute_with_context_result(
        &self,
        args: Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, String> {
        self.run(args, ob_ctx, context).await
    }
}

pub struct TypstPublishTool;

impl TypstPublishTool {
    pub fn new() -> Self {
        Self
    }

    async fn run(
        &self,
        args: Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, String> {
        let http = ob_ctx
            .discord_client
            .try_open()
            .ok_or_else(|| "Discord connection is unavailable.".to_string())?;
        let draft_id = args
            .get("draft_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing or invalid 'draft_id' parameter".to_string())?;
        let (format, content, warnings) =
            context
                .typst_drafts
                .take_reviewed(draft_id, context.response_round)?;
        let warning_count = warnings.len();

        let (message_ids, filename, page_count) = match content {
            TypstRenderContent::Pdf(pdf) => {
                let message = context
                    .channel_id
                    .send_message(
                        &http.http,
                        CreateMessage::new()
                            .content("レビュー済みのTypst PDFです。")
                            .add_file(CreateAttachment::bytes(pdf, "document.pdf")),
                    )
                    .await
                    .map_err(|error| format!("Typst PDFをDiscordへ送信できません: {error}"))?;
                (
                    vec![message.id.to_string()],
                    "document.pdf".to_string(),
                    None,
                )
            }
            TypstRenderContent::Pages(pages) => {
                let page_count = pages.len();
                let messages = crate::discord::message_delivery::send_typst_pages(
                    &http.http,
                    context.channel_id,
                    format.extension(),
                    pages,
                )
                .await
                .map_err(|error| format!("TypstページをDiscordへ送信できません: {error}"))?;
                (
                    messages
                        .into_iter()
                        .map(|message_id| message_id.to_string())
                        .collect(),
                    format!("page-*.{}", format.extension()),
                    Some(page_count),
                )
            }
        };

        Ok(ToolExecutionResult::text(
            json!({
                "status": "published",
                "format": format,
                "channel_id": context.channel_id.to_string(),
                "message_ids": message_ids,
                "filename": filename,
                "page_count": page_count,
                "warning_count": warning_count,
            })
            .to_string(),
        ))
    }
}

impl Default for TypstPublishTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LMTool for TypstPublishTool {
    fn name(&self) -> String {
        "typst-publish".to_string()
    }

    fn description(&self) -> String {
        "Publish one explicitly selected Typst draft to the current Discord channel. Pass the draft_id from the exact preview you reviewed. The tool rejects unknown drafts and drafts whose previews have not been passed to the model in an earlier response round.".to_string()
    }

    fn json_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "draft_id": {
                    "type": "string",
                    "description": "The exact draft_id returned by typst-render for the preview selected for publication.",
                },
            },
            "required": ["draft_id"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, _args: Value, _ob_ctx: NelfieContext) -> Result<String, String> {
        Err("The Typst publisher requires request context.".to_string())
    }

    async fn execute_with_context(
        &self,
        args: Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<String, String> {
        self.run(args, ob_ctx, context)
            .await
            .map(|result| result.text)
    }

    async fn execute_with_context_result(
        &self,
        args: Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, String> {
        self.run(args, ob_ctx, context).await
    }
}

fn preview_input_items(pages: &[Vec<u8>], draft_id: &str) -> Vec<InputItem> {
    let mut content = Vec::with_capacity(pages.len() * 2 + 1);
    content.push(InputContent::InputText(InputTextContent {
        text: format!(
            "Generated Typst PNG previews for draft `{draft_id}` ({} pages). Inspect the complete visual result before publishing this exact draft.",
            pages.len(),
        ),
    }));
    for (index, page) in pages.iter().enumerate() {
        content.push(InputContent::InputText(InputTextContent {
            text: format!("Draft `{draft_id}`, page {} of {}.", index + 1, pages.len()),
        }));
        content.push(InputContent::InputImage(InputImageContent {
            detail: ImageDetail::High,
            file_id: None,
            image_url: Some(format!("data:image/png;base64,{}", STANDARD.encode(page))),
        }));
    }
    vec![InputItem::EasyMessage(EasyInputMessage {
        role: Role::User,
        content: EasyInputContent::ContentList(content),
        ..Default::default()
    })]
}

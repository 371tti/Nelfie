use std::str::FromStr;

use async_openai::types::responses::{
    EasyInputContent, EasyInputMessage, ImageDetail, InputContent, InputFileArgs,
    InputImageContent, InputItem, InputTextContent, Role,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use serenity::all::ChannelId;

use crate::{
    app::context::NelfieContext,
    discord::attachments::AttachmentRef,
    llm::tool::{LMTool, ToolExecutionContext, ToolExecutionResult},
};

const MAX_TEXT_READ_BYTES: usize = 32 * 1024;
const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

pub struct FileTool;

impl FileTool {
    pub fn new() -> Self {
        Self
    }

    fn target_channel(args: &Value, default: ChannelId) -> Result<ChannelId, String> {
        args.get("channel_id")
            .and_then(Value::as_str)
            .map(|value| {
                ChannelId::from_str(value).map_err(|error| format!("Invalid 'channel_id': {error}"))
            })
            .unwrap_or(Ok(default))
    }

    fn attachment_id(args: &Value) -> Result<u64, String> {
        args.get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing or invalid 'attachment_id' parameter".to_string())?
            .parse::<u64>()
            .map_err(|error| format!("Invalid 'attachment_id': {error}"))
    }

    fn resolve_ref(
        args: &Value,
        ob_ctx: &NelfieContext,
        current_channel: ChannelId,
    ) -> Result<AttachmentRef, String> {
        let attachment_id = Self::attachment_id(args)?;
        let channel = args
            .get("channel_id")
            .and_then(Value::as_str)
            .map(|value| {
                ChannelId::from_str(value).map_err(|error| format!("Invalid 'channel_id': {error}"))
            })
            .transpose()?;
        ob_ctx
            .attachment_registry
            .find(attachment_id, channel)
            .ok_or_else(|| {
                format!(
                    "添付ID `{attachment_id}` は保持中の参照にありません。file listで参照を確認してください。"
                )
            })
            .or_else(|error| {
                if channel.is_none() {
                    ob_ctx
                        .attachment_registry
                        .find(attachment_id, Some(current_channel))
                        .ok_or(error)
                } else {
                    Err(error)
                }
            })
    }

    async fn run(
        &self,
        args: Value,
        ob_ctx: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, String> {
        let operation = args
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing or invalid 'operation' parameter".to_string())?;

        match operation {
            "list" => {
                let channel_id = Self::target_channel(&args, context.channel_id)?;
                let files = ob_ctx
                    .attachment_registry
                    .list(channel_id)
                    .into_iter()
                    .map(|item| item.to_json())
                    .collect::<Vec<_>>();
                Ok(ToolExecutionResult::text(
                    json!({
                        "channel_id": channel_id.to_string(),
                        "attachments": files,
                    })
                    .to_string(),
                ))
            }
            "read" => {
                let reference = Self::resolve_ref(&args, &ob_ctx, context.channel_id)?;
                if !is_text_attachment(&reference.filename) {
                    return Err(format!(
                        "`{}` は読み取り可能なテキスト形式ではありません。",
                        reference.filename
                    ));
                }
                let http = ob_ctx
                    .discord_client
                    .try_open()
                    .ok_or_else(|| "Discord接続が利用できません。".to_string())?;
                let (filename, _, bytes) = ob_ctx
                    .attachment_registry
                    .fetch_message_attachment(&http.http, &reference)
                    .await?;
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| format!("`{filename}` はUTF-8テキストではありません。"))?;
                let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                if offset > text.len() || !text.is_char_boundary(offset) {
                    return Err(format!(
                        "offset `{offset}` は `{filename}` の有効なUTF-8境界ではありません。"
                    ));
                }
                let mut end = offset.saturating_add(MAX_TEXT_READ_BYTES).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                Ok(ToolExecutionResult::text(
                    json!({
                        "filename": filename,
                        "attachment_id": reference.attachment_id.to_string(),
                        "offset": offset,
                        "next_offset": (end < text.len()).then_some(end),
                        "content": &text[offset..end],
                    })
                    .to_string(),
                ))
            }
            "attach" => {
                let reference = Self::resolve_ref(&args, &ob_ctx, context.channel_id)?;
                if reference
                    .size
                    .is_some_and(|size| size > MAX_ATTACHMENT_BYTES)
                {
                    return Err(format!(
                        "`{}` は25 MiBを超えているため添付できません。",
                        reference.filename
                    ));
                }
                let http = ob_ctx
                    .discord_client
                    .try_open()
                    .ok_or_else(|| "Discord接続が利用できません。".to_string())?;
                let (filename, content_type, size, url) = ob_ctx
                    .attachment_registry
                    .latest_attachment(&http.http, &reference)
                    .await?;
                if size > MAX_ATTACHMENT_BYTES {
                    return Err(format!("`{filename}` は25 MiBを超えています。"));
                }
                if url.is_empty() {
                    return Err(format!("`{filename}` のDiscord URLを取得できません。"));
                }

                let content = if reference.source_url.is_some() {
                    if !is_image_attachment(&filename, content_type.as_deref()) {
                        return Err(format!("`{filename}` は画像ファイルではありません。"));
                    }
                    let bytes = ob_ctx
                        .attachment_registry
                        .fetch_message_attachment(&http.http, &reference)
                        .await?
                        .2;
                    let actual_size = bytes.len();
                    let content_type = content_type.as_deref().unwrap_or("image/png");
                    let data_url = format!(
                        "data:{content_type};base64,{}",
                        STANDARD.encode(bytes)
                    );
                    let content = InputContent::InputImage(InputImageContent {
                        detail: ImageDetail::Auto,
                        file_id: None,
                        image_url: Some(data_url),
                    });
                    let input = InputItem::EasyMessage(EasyInputMessage {
                        role: Role::User,
                        content: EasyInputContent::ContentList(vec![
                            InputContent::InputText(InputTextContent {
                                text: format!("添付ファイル: {filename}"),
                            }),
                            content,
                        ]),
                        ..Default::default()
                    });
                    return Ok(ToolExecutionResult {
                        text: json!({
                            "status": "attached_to_next_request",
                            "filename": filename,
                            "attachment_id": reference.attachment_id.to_string(),
                            "size": actual_size,
                        })
                        .to_string(),
                        input_items: vec![input],
                        input_bytes: actual_size,
                        review_preview_id: None,
                    });
                } else if is_image_attachment(&filename, content_type.as_deref()) {
                    InputContent::InputImage(InputImageContent {
                        detail: ImageDetail::Auto,
                        file_id: None,
                        image_url: Some(url),
                    })
                } else {
                    if !is_responses_file(&filename) {
                        return Err(format!("`{filename}` はResponses APIの添付対象外です。"));
                    }
                    InputContent::InputFile(
                        InputFileArgs::default()
                            .file_url(url)
                            .build()
                            .map_err(|error| format!("ファイル入力を構築できません: {error}"))?,
                    )
                };
                let input_bytes = size as usize;
                let input = InputItem::EasyMessage(EasyInputMessage {
                    role: Role::User,
                    content: EasyInputContent::ContentList(vec![
                        InputContent::InputText(InputTextContent {
                            text: format!("添付ファイル: {filename}"),
                        }),
                        content,
                    ]),
                    ..Default::default()
                });
                Ok(ToolExecutionResult {
                    text: json!({
                        "status": "attached_to_next_request",
                        "filename": filename,
                        "attachment_id": reference.attachment_id.to_string(),
                        "size": size,
                    })
                    .to_string(),
                    input_items: vec![input],
                    input_bytes,
                    review_preview_id: None,
                })
            }
            other => Err(format!(
                "Unsupported operation `{other}`. Use list, read, or attach."
            )),
        }
    }
}

impl Default for FileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LMTool for FileTool {
    fn name(&self) -> String {
        "file".to_string()
    }

    fn description(&self) -> String {
        "List, read, or attach files from Discord messages, including reusable profile images returned by get_user_profile. Use list with a channel_id to inspect retained attachments; use read for UTF-8 text files; use attach to include a Responses API-supported file or image in the next model request. For Typst, pass image IDs from get_user_profile or file list to typst-render.asset_ids; for a .typ file, pass its attachment ID directly to typst-render.source_attachment_id instead of reading and copying the source text.".to_string()
    }

    fn json_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["list", "read", "attach"],
                },
                "channel_id": {
                    "type": "string",
                    "description": "Optional Discord channel ID. Defaults to the current channel for list. For read/attach, omit to search retained attachment IDs across channels.",
                },
                "attachment_id": {
                    "type": "string",
                    "description": "Retained Discord attachment or profile image asset ID, required for read and attach.",
                },
                "offset": {
                    "type": "integer",
                    "description": "UTF-8 byte offset returned by the previous read result.",
                },
            },
            "required": ["operation"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, _args: Value, _ob_ctx: NelfieContext) -> Result<String, String> {
        Err("The file tool requires request context.".to_string())
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

fn extension(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension)
}

fn is_text_attachment(filename: &str) -> bool {
    matches!(
        extension(filename).to_ascii_lowercase().as_str(),
        "txt" | "md" | "csv" | "json" | "yaml" | "yml" | "toml" | "typ"
    )
}

fn is_image_attachment(filename: &str, content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| {
            matches!(
                value.trim(),
                "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif"
            )
        })
        || matches!(
            extension(filename).to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "gif"
        )
}

fn is_responses_file(filename: &str) -> bool {
    matches!(
        extension(filename).to_ascii_lowercase().as_str(),
        "pdf"
            | "xla"
            | "xlb"
            | "xlc"
            | "xlm"
            | "xls"
            | "xlsx"
            | "xlt"
            | "xlw"
            | "csv"
            | "tsv"
            | "iif"
            | "doc"
            | "docx"
            | "dot"
            | "odt"
            | "rtf"
            | "ppt"
            | "pptx"
            | "pot"
            | "ppa"
            | "pps"
            | "pwz"
            | "wiz"
            | "asm"
            | "bat"
            | "c"
            | "cc"
            | "conf"
            | "cpp"
            | "cxx"
            | "def"
            | "dic"
            | "eml"
            | "h"
            | "hh"
            | "htm"
            | "html"
            | "ics"
            | "ifb"
            | "in"
            | "ksh"
            | "list"
            | "log"
            | "markdown"
            | "mht"
            | "mhtml"
            | "mime"
            | "mjs"
            | "nws"
            | "pl"
            | "rst"
            | "s"
            | "srt"
            | "text"
            | "vcf"
            | "vtt"
            | "yaml"
            | "yml"
            | "toml"
            | "txt"
            | "md"
            | "json"
            | "xml"
            | "py"
            | "js"
            | "css"
            | "sh"
            | "sql"
    )
}

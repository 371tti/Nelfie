use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::Arc,
};

use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::responses::{
        CreateResponseArgs, EasyInputContent, EasyInputMessage, FunctionCallOutput,
        FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, ImageDetail, InputContent,
        InputImageContent, InputItem, InputMessage, InputParam, InputRole, Item, MessageItem,
        MessageType, OutputItem, OutputMessage, OutputMessageContent, Reasoning,
        ResponseStreamEvent, SummaryPart, Tool, ToolChoiceOptions, ToolChoiceParam, WebSearchTool,
    },
};
use log::{debug, error, info, warn};
use serenity::futures::StreamExt;
use tokio::sync::mpsc;

use crate::{
    app::config::{ModelResponseParams, Models},
    app::context::NelfieContext,
};

pub use async_openai::types::responses::Role;

pub struct LMClient {
    pub client: Client<OpenAIConfig>,
}

/// LMのクライアント
/// レスポンス投げて返すための抽象レイヤ
impl LMClient {
    pub fn new(client: Client<OpenAIConfig>) -> Self {
        Self { client }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn generate_response(
        &self,
        ob_ctx: NelfieContext,
        lm_context: &LMContext,
        max_tokens: Option<u32>,
        tools: Option<Arc<HashMap<String, Box<dyn LMTool>>>>,
        state_mpsc: Option<mpsc::Sender<String>>,
        delta_mpsc: Option<mpsc::Sender<String>>,
        parameters: Option<ModelResponseParams>,
    ) -> Result<LMContext, Box<dyn std::error::Error + Send + Sync>> {
        debug!("Generating response with context: {:?}", lm_context);

        let tools = tools.unwrap_or_default();
        let state_send = |s: String| {
            if let Some(tx) = state_mpsc.as_ref() {
                let _ = tx.clone().try_send(s);
            }
        };

        let delta_send = |s: String| {
            if let Some(tx) = delta_mpsc.as_ref() {
                let _ = tx.clone().try_send(s);
            }
        };

        let mut tool_defs = tools
            .values()
            .map(|tool| tool.define())
            .collect::<Vec<Tool>>();

        // OpenAI built-in browser tool
        tool_defs.push(Tool::WebSearch(WebSearchTool::default()));

        let request_parameters = parameters.unwrap_or_else(|| Models::default().to_parameter());

        let mut tool_choice = ToolChoiceParam::Mode(ToolChoiceOptions::Auto);
        let mut delta_context = LMContext::new();
        let mut token_count = 0usize;

        for i in 0..10 {
            let context = lm_context.generate_context_with(&delta_context);
            debug!("Iteration {}: Generated context", i);

            let request = CreateResponseArgs::default()
                .model(request_parameters.model.clone())
                .input(context)
                .max_output_tokens(max_tokens.unwrap_or(100))
                .parallel_tool_calls(true)
                .tools(tool_defs.clone())
                .tool_choice(tool_choice.clone())
                .reasoning(Reasoning {
                    effort: Some(request_parameters.reasoning_effort.clone()),
                    summary: None,
                })
                .build()?;

            let mut stream = self.client.responses().create_stream(request).await?;

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        if should_ignore_stream_deserialize_error(&err) {
                            warn!(
                                "Ignored known web_search_call stream schema mismatch: {}",
                                err
                            );
                            continue;
                        }
                        return Err(Box::new(err) as Box<dyn std::error::Error + Send + Sync>);
                    }
                };

                match chunk {
                    ResponseStreamEvent::ResponseCreated(e) => {
                        state_send(format!("Response created (seq {})", e.sequence_number));
                        info!("Response created (seq {})", e.sequence_number);
                    }
                    ResponseStreamEvent::ResponseQueued(e) => {
                        state_send(format!("Response queued... (seq {})", e.sequence_number));
                        info!("Response queued (seq {})", e.sequence_number);
                    }
                    ResponseStreamEvent::ResponseInProgress(e) => {
                        state_send(format!(
                            "Response in progress... (seq {})",
                            e.sequence_number
                        ));
                        info!("Response in progress (seq {})", e.sequence_number);
                    }
                    ResponseStreamEvent::ResponseCompleted(e) => {
                        info!("Response completed (seq {})", e.sequence_number);
                        break;
                    }

                    ResponseStreamEvent::ResponseFailed(e) => {
                        error!(
                            "Response failed (seq {}): {:?}",
                            e.sequence_number, e.response
                        );
                        return Err(Box::new(io::Error::other("Response failed")));
                    }
                    ResponseStreamEvent::ResponseIncomplete(e) => {
                        error!(
                            "Response incomplete (seq {}): {:?}",
                            e.sequence_number, e.response
                        );
                        return Err(Box::new(io::Error::other("Response incomplete")));
                    }

                    ResponseStreamEvent::ResponseOutputItemDone(e) => match e.item {
                        OutputItem::Message(output_message) => {
                            let text = extract_output_message_text(&output_message);
                            if !text.is_empty() {
                                delta_context.add_text(text, Role::Assistant);
                            }
                        }
                        OutputItem::FunctionCall(function_tool_call) => {
                            state_send(format!("Function tool call: {}", function_tool_call.name));
                            delta_context.add_input_item(Item::FunctionCall(function_tool_call));
                        }
                        OutputItem::FileSearchCall(file_search_tool_call) => {
                            delta_context
                                .add_input_item(Item::FileSearchCall(file_search_tool_call));
                        }
                        OutputItem::WebSearchCall(web_search_tool_call) => {
                            state_send("OpenAI browser(web search) in progress...".to_string());
                            delta_context.add_input_item(Item::WebSearchCall(web_search_tool_call));
                        }
                        OutputItem::ComputerCall(computer_tool_call) => {
                            delta_context.add_input_item(Item::ComputerCall(computer_tool_call));
                        }
                        OutputItem::Reasoning(reasoning) => {
                            delta_context.add_input_item(Item::Reasoning(reasoning));
                        }
                        other => {
                            warn!("Unhandled output item: {:?}", other);
                        }
                    },

                    ResponseStreamEvent::ResponseOutputTextDelta(e) => {
                        delta_send(e.delta);
                        token_count += 1;
                        state_send(format!("Generating... ({} tokens)", token_count));
                    }

                    ResponseStreamEvent::ResponseRefusalDone(e) => {
                        state_send(e.refusal);
                    }

                    ResponseStreamEvent::ResponseReasoningSummaryPartDone(e) => {
                        let summary_text = match e.part {
                            SummaryPart::SummaryText(content) => content.text,
                        };
                        state_send(summary_text);
                    }

                    ResponseStreamEvent::ResponseError(e) => {
                        error!(
                            "Error (seq {}): {:?} - {} ({:?})",
                            e.sequence_number, e.code, e.message, e.param
                        );
                        return Err(Box::new(io::Error::other(e.message)));
                    }

                    other => {
                        debug!("Unhandled stream event: {:?}", other);
                    }
                }
            }

            let mut outputs = Vec::new();
            let uncompleted_tool_calls = delta_context.get_uncompleted_tool_calls();

            if uncompleted_tool_calls.is_empty() {
                break;
            }

            for tool_call in uncompleted_tool_calls {
                debug!("Executing tool call: {:?}", tool_call);
                let name = tool_call.name.clone();
                let args = tool_call.arguments.clone();
                let call_id = tool_call.call_id.clone();

                let v_args: serde_json::Value =
                    serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);

                let explain = v_args
                    .as_object()
                    .and_then(|o| o.get("$explain"))
                    .and_then(|o| o.as_str());

                if let Some(explain) = explain {
                    state_send(format!("Executing tool: {} - {}", name, explain));
                } else {
                    state_send(format!("Executing tool: {}", name));
                }

                if let Some(tool) = tools.get(&name) {
                    let exec_result = tool.execute(v_args, ob_ctx.clone()).await;
                    debug!("Tool {} executed with result: {:?}", name, exec_result);

                    let output = match exec_result {
                        Ok(res) => FunctionCallOutputItemParam {
                            call_id: call_id.clone(),
                            output: FunctionCallOutput::Text(res),
                            id: None,
                            status: None,
                        },
                        Err(err) => FunctionCallOutputItemParam {
                            call_id: call_id.clone(),
                            output: FunctionCallOutput::Text(format!("Error: {}", err)),
                            id: None,
                            status: None,
                        },
                    };

                    outputs.push(output);
                }
            }

            for output in outputs {
                delta_context.add_input_item(Item::FunctionCallOutput(output));
            }

            if i == 8 {
                tool_choice = ToolChoiceParam::Mode(ToolChoiceOptions::None);
            }
        }

        Ok(delta_context)
    }
}

fn should_ignore_stream_deserialize_error(err: &OpenAIError) -> bool {
    let OpenAIError::JSONDeserialize(parse_err, body) = err else {
        return false;
    };

    let parse_err_str = parse_err.to_string();
    if !parse_err_str.contains("missing field `action`") {
        return false;
    }

    body.contains("\"type\":\"response.output_item.added\"") && body.contains("\"web_search_call\"")
}

fn extract_output_message_text(output_message: &OutputMessage) -> String {
    output_message
        .content
        .iter()
        .filter_map(|content| match content {
            OutputMessageContent::OutputText(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<String>>()
        .join("")
}

/// コンテキスト実態
/// リングバッファで管理
#[derive(Debug, Clone)]
pub struct LMContext {
    pub buf: VecDeque<InputItem>,
    pub max_len: usize,
}

impl Default for LMContext {
    fn default() -> Self {
        Self::new()
    }
}

impl LMContext {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            max_len: 64,
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn set_max_len(&mut self, max_len: usize) {
        self.max_len = max_len;
    }

    pub fn generate_context(&self) -> InputParam {
        InputParam::Items(self.buf.iter().cloned().collect())
    }

    pub fn generate_context_with(&self, additional: &LMContext) -> InputParam {
        let mut combined = self.buf.clone();
        for item in additional.buf.iter() {
            combined.push_back(item.clone());
        }
        InputParam::Items(combined.into())
    }

    pub fn extend(&mut self, other: &LMContext) {
        for item in other.buf.iter() {
            if !is_history_item(item) {
                continue;
            }
            self.buf.push_back(item.clone());
        }
        self.trim_len();
    }

    pub fn trim_len(&mut self) {
        self.drop_incomplete_tool_pairs();

        while self.buf.len() > self.max_len {
            self.pop_oldest_history_group();
            self.drop_incomplete_tool_pairs();
        }
    }

    fn pop_oldest_history_group(&mut self) {
        let Some(front) = self.buf.front() else {
            return;
        };

        if let Some(call_id) = function_tool_call_id(front)
            .or_else(|| function_tool_output_call_id(front))
            .map(ToOwned::to_owned)
        {
            self.buf
                .retain(|item| !is_function_tool_pair_item(item, &call_id));
            return;
        }

        self.buf.pop_front();
    }

    fn drop_incomplete_tool_pairs(&mut self) {
        let call_ids = self
            .buf
            .iter()
            .filter_map(function_tool_call_id)
            .map(ToOwned::to_owned)
            .collect::<HashSet<_>>();
        let output_ids = self
            .buf
            .iter()
            .filter_map(function_tool_output_call_id)
            .map(ToOwned::to_owned)
            .collect::<HashSet<_>>();

        self.buf.retain(|item| {
            if let Some(call_id) = function_tool_call_id(item) {
                return output_ids.contains(call_id);
            }

            if let Some(call_id) = function_tool_output_call_id(item) {
                return call_ids.contains(call_id);
            }

            true
        });
    }

    pub fn add_text(&mut self, text: String, role: Role) {
        self.buf.push_back(InputItem::EasyMessage(EasyInputMessage {
            r#type: MessageType::Message,
            role,
            content: EasyInputContent::Text(text),
            phase: None,
        }));
    }

    pub fn add_user_text_with_images(&mut self, text: String, image_urls: Vec<String>) {
        let mut contents = vec![InputContent::from(text)];
        for url in image_urls {
            contents.push(InputContent::InputImage(InputImageContent {
                detail: ImageDetail::Low,
                file_id: None,
                image_url: Some(url),
            }));
        }

        self.buf
            .push_back(InputItem::Item(Item::Message(MessageItem::Input(
                InputMessage {
                    content: contents,
                    role: InputRole::User,
                    status: None,
                },
            ))));
    }

    pub fn add_input_item(&mut self, item: Item) {
        self.buf.push_back(InputItem::Item(item));
    }

    pub fn get_latest(&self) -> Option<&InputItem> {
        self.buf.back()
    }

    pub fn get_result(&self) -> String {
        for item in self.buf.iter().rev() {
            if let Some(text) = extract_text_from_item(item)
                && !text.is_empty()
            {
                return text;
            }
        }
        String::new()
    }

    pub fn get_uncompleted_tool_calls(&self) -> Vec<FunctionToolCall> {
        let call_id_list = self
            .buf
            .iter()
            .filter_map(|item| {
                if let InputItem::Item(Item::FunctionCallOutput(call)) = item {
                    Some(call.call_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<String>>();

        self.buf
            .iter()
            .filter_map(|item| {
                if let InputItem::Item(Item::FunctionCall(call)) = item
                    && !call_id_list.contains(&call.call_id)
                {
                    return Some(call.clone());
                }
                None
            })
            .collect()
    }

    pub fn get_latest_discord_send_content(&self) -> Option<String> {
        self.buf.iter().rev().find_map(|item| {
            let InputItem::Item(Item::FunctionCall(call)) = item else {
                return None;
            };

            if call.name != "discord-tool" {
                return None;
            }

            let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
            let operation = args.get("operation").and_then(|v| v.as_str());
            if operation != Some("send_message") {
                return None;
            }

            args.get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
    }
}

fn is_history_item(item: &InputItem) -> bool {
    matches!(
        item,
        InputItem::EasyMessage(_)
            | InputItem::Item(
                Item::Message(_) | Item::FunctionCall(_) | Item::FunctionCallOutput(_)
            )
    )
}

fn function_tool_call_id(item: &InputItem) -> Option<&str> {
    if let InputItem::Item(Item::FunctionCall(call)) = item {
        Some(&call.call_id)
    } else {
        None
    }
}

fn function_tool_output_call_id(item: &InputItem) -> Option<&str> {
    if let InputItem::Item(Item::FunctionCallOutput(output)) = item {
        Some(&output.call_id)
    } else {
        None
    }
}

fn is_function_tool_pair_item(item: &InputItem, call_id: &str) -> bool {
    function_tool_call_id(item) == Some(call_id)
        || function_tool_output_call_id(item) == Some(call_id)
}

fn extract_text_from_item(item: &InputItem) -> Option<String> {
    match item {
        InputItem::EasyMessage(msg) => Some(match &msg.content {
            EasyInputContent::Text(text) => text.clone(),
            EasyInputContent::ContentList(list) => list
                .iter()
                .filter_map(|content| match content {
                    InputContent::InputText(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<String>>()
                .join(""),
        }),
        InputItem::Item(Item::Message(msg)) => match msg {
            MessageItem::Output(output) => Some(
                output
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        OutputMessageContent::OutputText(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<String>>()
                    .join(""),
            ),
            MessageItem::Input(input) => Some(
                input
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        InputContent::InputText(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<String>>()
                    .join(""),
            ),
        },
        _ => None,
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
    fn json_schema(&self) -> serde_json::Value;
    fn description(&self) -> String;
    fn name(&self) -> String;
    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: NelfieContext,
    ) -> Result<String, String>;
}

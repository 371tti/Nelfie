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
        MessageType, OutputItem, OutputMessageContent, Reasoning, ResponseStreamEvent, SummaryPart,
        Tool, ToolChoiceOptions, ToolChoiceParam, WebSearchTool,
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
            let mut response_output_items = Vec::new();

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        if let Some(stream_err) = stream_error_from_deserialize_error(&err) {
                            state_send(stream_err.to_string());
                            return Err(
                                Box::new(stream_err) as Box<dyn std::error::Error + Send + Sync>
                            );
                        }

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
                            response_output_items.push((
                                e.output_index,
                                InputItem::Item(Item::Message(MessageItem::Output(output_message))),
                            ));
                        }
                        OutputItem::FunctionCall(function_tool_call) => {
                            state_send(format!("Function tool call: {}", function_tool_call.name));
                            response_output_items.push((
                                e.output_index,
                                InputItem::Item(Item::FunctionCall(function_tool_call)),
                            ));
                        }
                        OutputItem::FileSearchCall(file_search_tool_call) => {
                            response_output_items.push((
                                e.output_index,
                                InputItem::Item(Item::FileSearchCall(file_search_tool_call)),
                            ));
                        }
                        OutputItem::WebSearchCall(web_search_tool_call) => {
                            state_send("OpenAI browser(web search) in progress...".to_string());
                            response_output_items.push((
                                e.output_index,
                                InputItem::Item(Item::WebSearchCall(web_search_tool_call)),
                            ));
                        }
                        OutputItem::ComputerCall(computer_tool_call) => {
                            response_output_items.push((
                                e.output_index,
                                InputItem::Item(Item::ComputerCall(computer_tool_call)),
                            ));
                        }
                        OutputItem::Reasoning(reasoning) => {
                            response_output_items.push((
                                e.output_index,
                                InputItem::Item(Item::Reasoning(reasoning)),
                            ));
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

            response_output_items.sort_by_key(|(output_index, _)| *output_index);
            for (_, item) in response_output_items {
                delta_context.buf.push_back(item);
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

fn stream_error_from_deserialize_error(err: &OpenAIError) -> Option<io::Error> {
    let OpenAIError::JSONDeserialize(_, body) = err else {
        return None;
    };

    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("error") {
        return None;
    }

    let error = value.get("error")?;
    let error_type = error
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown_error");
    let code = error
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown_code");
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("OpenAI stream returned an error event");
    let sequence = value
        .get("sequence_number")
        .and_then(|v| v.as_i64())
        .map(|seq| format!(", seq {seq}"))
        .unwrap_or_default();

    Some(io::Error::other(format!(
        "OpenAI stream error ({error_type}/{code}{sequence}): {message}"
    )))
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
        let mut sanitized = self.clone();
        sanitized.sanitize_tool_history();
        InputParam::Items(sanitized.buf.into())
    }

    pub fn generate_context_with(&self, additional: &LMContext) -> InputParam {
        let mut combined = self.buf.clone();
        for item in additional.buf.iter() {
            combined.push_back(item.clone());
        }
        let mut sanitized = LMContext {
            buf: combined,
            max_len: self.max_len,
        };
        sanitized.sanitize_tool_history();
        InputParam::Items(sanitized.buf.into())
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
        self.sanitize_tool_history();

        while self.buf.len() > self.max_len {
            self.pop_oldest_history_group();
            self.sanitize_tool_history();
        }
    }

    fn pop_oldest_history_group(&mut self) {
        let Some(front) = self.buf.front() else {
            return;
        };

        if let Some(reasoning_id) = reasoning_item_id(front).map(ToOwned::to_owned) {
            let call_ids = self.reasoning_group_call_ids(&reasoning_id);
            self.buf.retain(|item| {
                reasoning_item_id(item) != Some(reasoning_id.as_str())
                    && !function_tool_pair_matches_any(item, &call_ids)
            });
            return;
        }

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

    fn sanitize_tool_history(&mut self) {
        self.drop_incomplete_tool_pairs();
        self.drop_function_calls_missing_required_reasoning();
        self.drop_incomplete_tool_pairs();
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

    fn drop_function_calls_missing_required_reasoning(&mut self) {
        let mut invalid_call_ids = HashSet::new();
        let mut current_reasoning = false;

        for item in self.buf.iter() {
            if reasoning_item_id(item).is_some() {
                current_reasoning = true;
                continue;
            }

            if is_message_item(item) {
                current_reasoning = false;
                continue;
            }

            if let Some(call) = function_tool_call(item)
                && function_call_requires_reasoning(call)
                && !current_reasoning
            {
                invalid_call_ids.insert(call.call_id.clone());
            }
        }

        if invalid_call_ids.is_empty() {
            return;
        }

        self.buf
            .retain(|item| !function_tool_pair_matches_any(item, &invalid_call_ids));
    }

    fn reasoning_group_call_ids(&self, reasoning_id: &str) -> HashSet<String> {
        let mut call_ids = HashSet::new();
        let mut in_group = false;

        for item in self.buf.iter() {
            if reasoning_item_id(item) == Some(reasoning_id) {
                in_group = true;
                continue;
            }

            if !in_group {
                continue;
            }

            if reasoning_item_id(item).is_some() || is_message_item(item) {
                break;
            }

            if let Some(call_id) = function_tool_call_id(item) {
                call_ids.insert(call_id.to_string());
            }
        }

        call_ids
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
                Item::Message(_)
                    | Item::Reasoning(_)
                    | Item::FunctionCall(_)
                    | Item::FunctionCallOutput(_)
            )
    )
}

fn is_message_item(item: &InputItem) -> bool {
    matches!(
        item,
        InputItem::EasyMessage(_) | InputItem::Item(Item::Message(_))
    )
}

fn reasoning_item_id(item: &InputItem) -> Option<&str> {
    if let InputItem::Item(Item::Reasoning(reasoning)) = item {
        Some(&reasoning.id)
    } else {
        None
    }
}

fn function_tool_call(item: &InputItem) -> Option<&FunctionToolCall> {
    if let InputItem::Item(Item::FunctionCall(call)) = item {
        Some(call)
    } else {
        None
    }
}

fn function_tool_call_id(item: &InputItem) -> Option<&str> {
    function_tool_call(item).map(|call| call.call_id.as_str())
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

fn function_tool_pair_matches_any(item: &InputItem, call_ids: &HashSet<String>) -> bool {
    function_tool_call_id(item)
        .or_else(|| function_tool_output_call_id(item))
        .is_some_and(|call_id| call_ids.contains(call_id))
}

fn function_call_requires_reasoning(call: &FunctionToolCall) -> bool {
    call.id.as_deref().is_some_and(|id| id.starts_with("fc_"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses::ReasoningItem;

    fn reasoning_item(id: &str) -> InputItem {
        InputItem::Item(Item::Reasoning(ReasoningItem {
            id: id.to_string(),
            summary: Vec::new(),
            content: None,
            encrypted_content: None,
            status: None,
        }))
    }

    fn function_call_item(call_id: &str, item_id: Option<&str>) -> InputItem {
        InputItem::Item(Item::FunctionCall(FunctionToolCall {
            arguments: "{}".to_string(),
            call_id: call_id.to_string(),
            namespace: None,
            name: "test-tool".to_string(),
            id: item_id.map(ToOwned::to_owned),
            status: None,
        }))
    }

    fn function_output_item(call_id: &str) -> InputItem {
        InputItem::Item(Item::FunctionCallOutput(FunctionCallOutputItemParam {
            call_id: call_id.to_string(),
            output: FunctionCallOutput::Text("ok".to_string()),
            id: None,
            status: None,
        }))
    }

    fn input_items(input: InputParam) -> Vec<InputItem> {
        match input {
            InputParam::Items(items) => items,
            InputParam::Text(_) => panic!("expected item input"),
        }
    }

    #[test]
    fn extend_preserves_reasoning_for_function_call_history() {
        let mut delta = LMContext::new();
        delta.buf.push_back(reasoning_item("rs_1"));
        delta
            .buf
            .push_back(function_call_item("call_1", Some("fc_1")));
        delta.buf.push_back(function_output_item("call_1"));

        let mut history = LMContext::new();
        history.extend(&delta);

        assert!(
            history
                .buf
                .iter()
                .any(|item| reasoning_item_id(item) == Some("rs_1"))
        );
        assert!(
            history
                .buf
                .iter()
                .any(|item| function_tool_call_id(item) == Some("call_1"))
        );
        assert!(
            history
                .buf
                .iter()
                .any(|item| function_tool_output_call_id(item) == Some("call_1"))
        );
    }

    #[test]
    fn generate_context_drops_required_reasoning_call_when_reasoning_is_missing() {
        let mut history = LMContext::new();
        history
            .buf
            .push_back(function_call_item("call_1", Some("fc_1")));
        history.buf.push_back(function_output_item("call_1"));

        let items = input_items(history.generate_context());

        assert!(items.is_empty());
    }

    #[test]
    fn generate_context_keeps_function_call_without_openai_item_id() {
        let mut history = LMContext::new();
        history.buf.push_back(function_call_item("call_1", None));
        history.buf.push_back(function_output_item("call_1"));

        let items = input_items(history.generate_context());

        assert_eq!(items.len(), 2);
    }

    #[test]
    fn trim_drops_reasoning_tool_group_together() {
        let mut history = LMContext::new();
        history.set_max_len(2);
        history.buf.push_back(reasoning_item("rs_1"));
        history
            .buf
            .push_back(function_call_item("call_1", Some("fc_1")));
        history.buf.push_back(function_output_item("call_1"));
        history.add_text("later".to_string(), Role::User);

        history.trim_len();

        assert!(
            history
                .buf
                .iter()
                .all(|item| reasoning_item_id(item) != Some("rs_1"))
        );
        assert!(
            history
                .buf
                .iter()
                .all(|item| function_tool_call_id(item) != Some("call_1"))
        );
        assert_eq!(history.get_result(), "later");
    }
}

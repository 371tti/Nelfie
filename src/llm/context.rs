use std::collections::{HashSet, VecDeque};

use async_openai::types::responses::{
    EasyInputContent, EasyInputMessage, FunctionToolCall, ImageDetail, InputContent,
    InputImageContent, InputItem, InputMessage, InputParam, InputRole, Item, MessageItem,
    MessageType, OutputMessageContent,
};

pub use async_openai::types::responses::Role;

#[derive(Clone, Debug)]
pub struct DiscordSendRecord {
    pub content: String,
    pub private: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptCacheUsage {
    pub input_tokens: u32,
    pub cached_tokens: u32,
}

impl PromptCacheUsage {
    pub fn hit(self) -> bool {
        self.cached_tokens > 0
    }
}

#[derive(Debug, Clone)]
pub struct LMContext {
    pub buf: VecDeque<InputItem>,
    pub discord_image_sources: Vec<DiscordImageSource>,
    last_response_total_tokens: Option<u32>,
    last_response_cache_usage: Option<PromptCacheUsage>,
}

#[derive(Debug, Clone)]
pub struct LMContextCompaction {
    source_items: Vec<InputItem>,
    summary_context: LMContext,
}

#[derive(Debug, Clone)]
pub struct DiscordImageSource {
    pub channel_id: u64,
    pub message_id: u64,
    pub attachment_id: u64,
    pub captured_at_unix: u64,
    pub url: String,
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
            discord_image_sources: Vec::new(),
            last_response_total_tokens: None,
            last_response_cache_usage: None,
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.discord_image_sources.clear();
        self.last_response_total_tokens = None;
        self.last_response_cache_usage = None;
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
            discord_image_sources: Vec::new(),
            last_response_total_tokens: self.last_response_total_tokens,
            last_response_cache_usage: self.last_response_cache_usage,
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
        self.discord_image_sources
            .extend(other.discord_image_sources.iter().cloned());
        if other.last_response_total_tokens.is_some() {
            self.last_response_total_tokens = other.last_response_total_tokens;
        }
        if other.last_response_cache_usage.is_some() {
            self.last_response_cache_usage = other.last_response_cache_usage;
        }
        self.sanitize_tool_history();
        self.prune_discord_image_sources_to_present_urls();
    }

    pub fn record_response_total_tokens(&mut self, total_tokens: u32) {
        self.last_response_total_tokens = Some(total_tokens);
    }

    pub fn record_response_usage(
        &mut self,
        total_tokens: u32,
        input_tokens: u32,
        cached_tokens: u32,
    ) {
        self.last_response_total_tokens = Some(
            self.last_response_total_tokens
                .unwrap_or_default()
                .saturating_add(total_tokens),
        );

        let cache_usage = self.last_response_cache_usage.get_or_insert_default();
        cache_usage.input_tokens = cache_usage.input_tokens.saturating_add(input_tokens);
        cache_usage.cached_tokens = cache_usage.cached_tokens.saturating_add(cached_tokens);
    }

    pub fn response_total_tokens(&self) -> Option<u32> {
        self.last_response_total_tokens
    }

    pub fn response_cache_usage(&self) -> Option<PromptCacheUsage> {
        self.last_response_cache_usage
    }

    pub fn plan_compaction(&self, token_limit: u32) -> Option<LMContextCompaction> {
        if token_limit == 0 || self.last_response_total_tokens? < token_limit {
            return None;
        }

        let mut sanitized = self.clone();
        sanitized.sanitize_tool_history();
        let turns = history_turn_ranges(&sanitized.buf);
        if turns.len() < 2 {
            return None;
        }

        let total_weight = sanitized
            .buf
            .iter()
            .map(estimated_item_weight)
            .sum::<usize>();
        let target_weight = total_weight / 2;
        let mut cumulative_weight = 0usize;
        let mut best_boundary = None::<(usize, usize)>;

        for &(start, end) in turns.iter().take(turns.len() - 1) {
            cumulative_weight += sanitized
                .buf
                .range(start..end)
                .map(estimated_item_weight)
                .sum::<usize>();
            let distance = cumulative_weight.abs_diff(target_weight);
            if best_boundary.is_none_or(|(_, best_distance)| distance < best_distance) {
                best_boundary = Some((end, distance));
            }
        }

        let (boundary, _) = best_boundary?;
        let source_items = sanitized
            .buf
            .iter()
            .take(boundary)
            .cloned()
            .collect::<Vec<_>>();
        let mut summary_context = LMContext::new();
        summary_context.buf = source_items.iter().cloned().collect();
        for item in &mut summary_context.buf {
            redact_images_for_compaction(item);
        }

        Some(LMContextCompaction {
            source_items,
            summary_context,
        })
    }

    pub fn apply_compaction(&mut self, compaction: &LMContextCompaction, summary: &str) -> bool {
        let summary = summary.trim();
        if summary.is_empty() {
            return false;
        }

        self.sanitize_tool_history();
        let prefix_matches = self
            .buf
            .iter()
            .take(compaction.source_items.len())
            .eq(compaction.source_items.iter());
        if !prefix_matches || self.buf.len() <= compaction.source_items.len() {
            return false;
        }

        self.buf.drain(..compaction.source_items.len());
        self.buf.push_front(text_message_item(
            format!(
                "Summary of earlier conversation context. Treat this as background memory, not \
                 as a new user request:\n{summary}"
            ),
            Role::Developer,
        ));
        self.last_response_total_tokens = None;
        self.last_response_cache_usage = None;
        self.prune_discord_image_sources_to_present_urls();
        true
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

    pub fn add_text(&mut self, text: String, role: Role) {
        self.buf.push_back(text_message_item(text, role));
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

    pub fn add_user_text_with_discord_images(
        &mut self,
        text: String,
        images: Vec<DiscordImageSource>,
    ) {
        let image_urls = images.iter().map(|image| image.url.clone()).collect();
        self.add_user_text_with_images(text, image_urls);
        self.discord_image_sources.extend(images);
    }

    pub fn replace_image_url(&mut self, old_url: &str, new_url: &str) {
        for item in &mut self.buf {
            replace_image_url_in_item(item, old_url, new_url);
        }
    }

    pub fn remove_image_url(&mut self, url: &str) {
        for item in &mut self.buf {
            remove_image_url_from_item(item, url);
        }
        self.prune_discord_image_sources_to_present_urls();
    }

    pub fn prune_discord_image_sources_to_present_urls(&mut self) {
        let urls = self
            .buf
            .iter()
            .flat_map(image_urls_in_item)
            .map(ToOwned::to_owned)
            .collect::<HashSet<String>>();
        self.discord_image_sources
            .retain(|source| urls.contains(&source.url));
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

    pub fn get_latest_discord_send_record(&self) -> Option<DiscordSendRecord> {
        self.buf.iter().rev().find_map(|item| {
            let InputItem::Item(Item::FunctionCall(call)) = item else {
                return None;
            };

            if call.name != "discord-tool" {
                return None;
            }

            let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
            let operation = args.get("operation").and_then(|v| v.as_str());
            let private = match operation {
                Some("send_message") => false,
                Some("send_ephemeral_message") => true,
                _ => return None,
            };

            let content = args.get("content").and_then(|v| v.as_str())?.to_string();
            Some(DiscordSendRecord { content, private })
        })
    }
}

impl LMContextCompaction {
    pub fn summary_context(&self) -> &LMContext {
        &self.summary_context
    }

    pub fn source_item_count(&self) -> usize {
        self.source_items.len()
    }
}

fn text_message_item(text: String, role: Role) -> InputItem {
    InputItem::EasyMessage(EasyInputMessage {
        r#type: MessageType::Message,
        role,
        content: EasyInputContent::Text(text),
        phase: None,
    })
}

fn history_turn_ranges(items: &VecDeque<InputItem>) -> Vec<(usize, usize)> {
    if items.is_empty() {
        return Vec::new();
    }

    let mut starts = vec![0usize];
    for (index, item) in items.iter().enumerate().skip(1) {
        if starts_history_turn(item) {
            starts.push(index);
        }
    }

    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            (
                *start,
                starts.get(index + 1).copied().unwrap_or(items.len()),
            )
        })
        .collect()
}

fn starts_history_turn(item: &InputItem) -> bool {
    match item {
        InputItem::EasyMessage(message) => message.role != Role::Assistant,
        InputItem::Item(Item::Message(MessageItem::Input(_))) => true,
        _ => false,
    }
}

fn estimated_item_weight(item: &InputItem) -> usize {
    serde_json::to_vec(item)
        .map(|serialized| serialized.len().max(1))
        .unwrap_or(1)
}

fn redact_images_for_compaction(item: &mut InputItem) {
    match item {
        InputItem::EasyMessage(message) => {
            if let EasyInputContent::ContentList(contents) = &mut message.content {
                redact_images_in_contents(contents);
            }
        }
        InputItem::Item(Item::Message(MessageItem::Input(message))) => {
            redact_images_in_contents(&mut message.content);
        }
        _ => {}
    }
}

fn redact_images_in_contents(contents: &mut Vec<InputContent>) {
    let image_count = contents
        .iter()
        .filter(|content| matches!(content, InputContent::InputImage(_)))
        .count();
    if image_count == 0 {
        return;
    }

    contents.retain(|content| !matches!(content, InputContent::InputImage(_)));
    contents.push(InputContent::from(format!(
        "[{image_count} image attachment(s) omitted from background compaction input]"
    )));
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

fn image_urls_in_item(item: &InputItem) -> Vec<&str> {
    match item {
        InputItem::EasyMessage(msg) => match &msg.content {
            EasyInputContent::Text(_) => Vec::new(),
            EasyInputContent::ContentList(list) => image_urls_in_input_contents(list),
        },
        InputItem::Item(Item::Message(MessageItem::Input(input))) => {
            image_urls_in_input_contents(&input.content)
        }
        _ => Vec::new(),
    }
}

fn image_urls_in_input_contents(contents: &[InputContent]) -> Vec<&str> {
    contents
        .iter()
        .filter_map(|content| match content {
            InputContent::InputImage(image) => image.image_url.as_deref(),
            _ => None,
        })
        .collect()
}

fn replace_image_url_in_item(item: &mut InputItem, old_url: &str, new_url: &str) {
    match item {
        InputItem::EasyMessage(msg) => {
            if let EasyInputContent::ContentList(list) = &mut msg.content {
                replace_image_url_in_input_contents(list, old_url, new_url);
            }
        }
        InputItem::Item(Item::Message(MessageItem::Input(input))) => {
            replace_image_url_in_input_contents(&mut input.content, old_url, new_url);
        }
        _ => {}
    }
}

fn remove_image_url_from_item(item: &mut InputItem, url: &str) {
    match item {
        InputItem::EasyMessage(msg) => {
            if let EasyInputContent::ContentList(list) = &mut msg.content {
                remove_image_url_from_input_contents(list, url);
            }
        }
        InputItem::Item(Item::Message(MessageItem::Input(input))) => {
            remove_image_url_from_input_contents(&mut input.content, url);
        }
        _ => {}
    }
}

fn replace_image_url_in_input_contents(
    contents: &mut [InputContent],
    old_url: &str,
    new_url: &str,
) {
    for content in contents {
        let InputContent::InputImage(image) = content else {
            continue;
        };

        if image.image_url.as_deref() == Some(old_url) {
            image.image_url = Some(new_url.to_string());
        }
    }
}

fn remove_image_url_from_input_contents(contents: &mut Vec<InputContent>, url: &str) {
    contents.retain(|content| match content {
        InputContent::InputImage(image) => image.image_url.as_deref() != Some(url),
        _ => true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses::{
        FunctionCallOutput, FunctionCallOutputItemParam, ReasoningItem,
    };

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

    fn discord_tool_call_item(operation: &str, content: &str) -> InputItem {
        InputItem::Item(Item::FunctionCall(FunctionToolCall {
            arguments: serde_json::json!({
                "operation": operation,
                "content": content,
                "channel_id": "123",
                "user_id": "456",
            })
            .to_string(),
            call_id: format!("call_{operation}"),
            namespace: None,
            name: "discord-tool".to_string(),
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
    fn compaction_uses_response_tokens_and_keeps_tool_pairs_atomic() {
        let mut history = LMContext::new();
        history.add_text("old request".to_string(), Role::User);
        history.buf.push_back(function_call_item("call_1", None));
        history.buf.push_back(function_output_item("call_1"));
        history.add_text("middle request".to_string(), Role::User);
        history.add_text("recent request".to_string(), Role::User);

        history.record_response_total_tokens(999);
        assert!(history.plan_compaction(1_000).is_none());

        history.record_response_total_tokens(1_000);
        let compaction = history.plan_compaction(1_000).unwrap();
        let has_call = compaction
            .source_items
            .iter()
            .any(|item| function_tool_call_id(item) == Some("call_1"));
        let has_output = compaction
            .source_items
            .iter()
            .any(|item| function_tool_output_call_id(item) == Some("call_1"));
        assert_eq!(has_call, has_output);
    }

    #[test]
    fn applying_compaction_preserves_messages_appended_while_summarizing() {
        let mut history = LMContext::new();
        for message in ["old one", "old two", "recent one", "recent two"] {
            history.add_text(message.to_string(), Role::User);
        }
        history.record_response_total_tokens(10_000);
        let compaction = history.plan_compaction(10_000).unwrap();

        history.add_text("arrived while summarizing".to_string(), Role::User);
        assert!(history.apply_compaction(&compaction, "important earlier details"));

        assert_eq!(history.get_result(), "arrived while summarizing");
        assert!(
            extract_text_from_item(history.buf.front().unwrap())
                .unwrap()
                .contains("important earlier details")
        );
        assert_eq!(history.response_total_tokens(), None);
    }

    #[test]
    fn compaction_rejects_a_plan_after_history_is_cleared() {
        let mut history = LMContext::new();
        for message in ["old one", "old two", "recent"] {
            history.add_text(message.to_string(), Role::User);
        }
        history.record_response_total_tokens(10_000);
        let compaction = history.plan_compaction(10_000).unwrap();

        history.clear();
        history.add_text("new conversation".to_string(), Role::User);

        assert!(!history.apply_compaction(&compaction, "stale summary"));
        assert_eq!(history.get_result(), "new conversation");
    }

    #[test]
    fn passive_history_merge_does_not_erase_last_response_usage() {
        let mut history = LMContext::new();
        history.record_response_usage(12_345, 10_000, 4_096);
        let mut passive_message = LMContext::new();
        passive_message.add_text("passive".to_string(), Role::User);

        history.extend(&passive_message);

        assert_eq!(history.response_total_tokens(), Some(12_345));
        assert_eq!(
            history.response_cache_usage(),
            Some(PromptCacheUsage {
                input_tokens: 10_000,
                cached_tokens: 4_096,
            })
        );
    }

    #[test]
    fn response_usage_aggregates_tool_call_rounds() {
        let mut response = LMContext::new();
        response.record_response_usage(1_200, 1_000, 0);
        response.record_response_usage(1_400, 1_100, 768);

        assert_eq!(response.response_total_tokens(), Some(2_600));
        let cache = response.response_cache_usage().unwrap();
        assert_eq!(cache.input_tokens, 2_100);
        assert_eq!(cache.cached_tokens, 768);
        assert!(cache.hit());
    }

    #[test]
    fn history_is_not_rolled_by_item_count() {
        let mut history = LMContext::new();
        let mut messages = LMContext::new();
        for index in 0..100 {
            messages.add_text(format!("message {index}"), Role::User);
        }

        history.extend(&messages);

        assert_eq!(history.buf.len(), 100);
    }

    #[test]
    fn compaction_input_replaces_images_with_text_markers() {
        let image_url = "https://cdn.discordapp.com/attachments/1/2/image.png?ex=ffff";
        let mut history = LMContext::new();
        history.add_user_text_with_images("old image".to_string(), vec![image_url.to_string()]);
        history.add_text("recent request".to_string(), Role::User);
        history.record_response_total_tokens(10_000);

        let compaction = history.plan_compaction(10_000).unwrap();
        let serialized =
            serde_json::to_string(&compaction.summary_context().generate_context()).unwrap();

        assert!(!serialized.contains(image_url));
        assert!(serialized.contains("image attachment(s) omitted"));
    }

    #[test]
    fn latest_discord_send_record_marks_channel_message_public() {
        let mut history = LMContext::new();
        history
            .buf
            .push_back(discord_tool_call_item("send_message", "public"));

        let record = history.get_latest_discord_send_record().unwrap();

        assert_eq!(record.content, "public");
        assert!(!record.private);
    }

    #[test]
    fn latest_discord_send_record_marks_ephemeral_message_private() {
        let mut history = LMContext::new();
        history
            .buf
            .push_back(discord_tool_call_item("send_ephemeral_message", "private"));

        let record = history.get_latest_discord_send_record().unwrap();

        assert_eq!(record.content, "private");
        assert!(record.private);
    }
}

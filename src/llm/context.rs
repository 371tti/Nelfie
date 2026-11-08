use std::collections::{HashSet, VecDeque};

use async_openai::types::responses::{
    EasyInputContent, EasyInputMessage, FunctionCallOutput, FunctionToolCall, ImageDetail,
    InputContent, InputImageContent, InputItem, InputMessage, InputParam, InputRole, Item,
    MessageItem, MessageType, OutputMessageContent,
};
use tiktoken_rs::o200k_base_singleton;

pub use async_openai::types::responses::Role;

pub const SUMMARY_INPUT_MAX_BYTES: usize = 32 * 1024;
const COMPACTION_RETAINED_TURNS: usize = 2;
const COMPACTION_TARGET_NUMERATOR: u32 = 3;
const COMPACTION_TARGET_DENOMINATOR: u32 = 4;
// Reserve headroom for the model-side image expansion; this is not an exact image token count.
const ESTIMATED_IMAGE_TOKENS: usize = 1_024;

pub fn compaction_target_tokens(trigger_tokens: u32) -> usize {
    (trigger_tokens as u64 * COMPACTION_TARGET_NUMERATOR as u64
        / COMPACTION_TARGET_DENOMINATOR as u64) as usize
}

#[derive(Clone, Debug)]
pub struct DiscordSendRecord {
    pub content: String,
    pub private: bool,
    pub channel_id: Option<u64>,
    pub message_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct LMContext {
    pub buf: VecDeque<InputItem>,
    pub discord_image_sources: Vec<DiscordImageSource>,
}

#[derive(Debug, Clone)]
pub struct LMContextCompaction {
    source_items: Vec<InputItem>,
    summary_contexts: Vec<LMContext>,
    oversized_first_turn: bool,
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
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.discord_image_sources.clear();
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
        self.sanitize_tool_history();
        self.prune_discord_image_sources_to_present_urls();
    }

    pub fn serialized_input_bytes(&self) -> usize {
        context_input_bytes(self)
    }

    pub fn estimated_input_tokens(&self) -> usize {
        self.buf
            .iter()
            .map(estimated_item_tokens)
            .fold(0usize, usize::saturating_add)
    }

    pub fn trim_to_recent_token_limit(&mut self, token_limit: usize) -> (usize, usize) {
        self.sanitize_tool_history();
        let item_token_estimates = self
            .buf
            .iter()
            .map(estimated_item_tokens)
            .collect::<Vec<_>>();
        let estimated_total = item_token_estimates
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add);
        if estimated_total <= token_limit {
            return (0, estimated_total);
        }

        let item_count = self.buf.len();
        let turns = history_turn_ranges(&self.buf);
        let mut keep = vec![false; item_count];
        let mut retained_tokens = 0usize;

        for (index, item) in self.buf.iter().enumerate() {
            if is_pinned_instruction(item) {
                keep[index] = true;
                retained_tokens = retained_tokens.saturating_add(item_token_estimates[index]);
            }
        }

        let mut retained_turns = 0usize;
        for &(start, end) in turns.iter().rev() {
            let turn_tokens = (start..end)
                .filter(|index| !keep[*index])
                .map(|index| item_token_estimates[index])
                .fold(0usize, usize::saturating_add);
            if retained_turns > 0 && retained_tokens.saturating_add(turn_tokens) > token_limit {
                break;
            }

            for item_index in start..end {
                keep[item_index] = true;
            }
            retained_tokens = retained_tokens.saturating_add(turn_tokens);
            retained_turns += 1;
        }

        let before = self.buf.len();
        self.buf = std::mem::take(&mut self.buf)
            .into_iter()
            .zip(keep)
            .filter_map(|(item, keep)| keep.then_some(item))
            .collect();
        self.prune_discord_image_sources_to_present_urls();
        (before.saturating_sub(self.buf.len()), retained_tokens)
    }

    pub fn plan_compaction(
        &self,
        trigger_token_limit: u32,
        target_token_limit: usize,
    ) -> Option<LMContextCompaction> {
        let estimated_tokens = self.estimated_input_tokens();
        if trigger_token_limit == 0 || estimated_tokens < trigger_token_limit as usize {
            return None;
        }

        self.plan_compaction_batch_if_needed(target_token_limit, estimated_tokens)
    }

    pub fn plan_compaction_batch(&self, target_token_limit: usize) -> Option<LMContextCompaction> {
        let estimated_tokens = self.estimated_input_tokens();
        self.plan_compaction_batch_if_needed(target_token_limit, estimated_tokens)
    }

    fn plan_compaction_batch_if_needed(
        &self,
        target_token_limit: usize,
        estimated_tokens: usize,
    ) -> Option<LMContextCompaction> {
        if estimated_tokens <= target_token_limit {
            return None;
        }

        let mut sanitized = self.clone();
        sanitized.sanitize_tool_history();
        let turns = history_turn_ranges(&sanitized.buf);
        if turns.len() <= COMPACTION_RETAINED_TURNS {
            return None;
        }

        let eligible_turns = turns.len() - COMPACTION_RETAINED_TURNS;
        let mut source_end = 0usize;
        let mut source_items = Vec::new();
        let mut source_weight = 2usize;
        let mut summary_contexts = Vec::new();
        let mut oversized_first_turn = false;
        let safe_source_bytes = SUMMARY_INPUT_MAX_BYTES.saturating_sub(1_024);

        for &(start, end) in turns.iter().take(eligible_turns) {
            let turn_items = sanitized.buf.range(start..end).cloned().collect::<Vec<_>>();
            let turn_weight = turn_items
                .iter()
                .map(serialized_item_bytes)
                .map(|weight| weight.saturating_add(1))
                .fold(0usize, usize::saturating_add);
            let candidate_weight = source_weight.saturating_add(turn_weight);

            if candidate_weight <= safe_source_bytes {
                source_weight = candidate_weight;
                source_items.extend(turn_items);
                source_end = end;
                continue;
            }

            if source_end == 0 {
                source_end = end;
                source_items.extend(turn_items);
                summary_contexts =
                    chunk_oversized_turn_context(&source_items, SUMMARY_INPUT_MAX_BYTES);
                oversized_first_turn = true;
            }
            break;
        }

        if source_end == 0 {
            return None;
        }
        if summary_contexts.is_empty() {
            let summary_context = summary_context_for_items(&source_items);
            if context_input_bytes(&summary_context) <= SUMMARY_INPUT_MAX_BYTES {
                summary_contexts.push(summary_context);
            } else {
                summary_contexts =
                    chunk_oversized_turn_context(&source_items, SUMMARY_INPUT_MAX_BYTES);
                oversized_first_turn = true;
            }
        }

        Some(LMContextCompaction {
            source_items,
            summary_contexts,
            oversized_first_turn,
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
        self.buf.push_front(compaction_summary_item(summary));
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
            let tool_output = self.buf.iter().rev().find_map(|item| {
                let InputItem::Item(Item::FunctionCallOutput(output)) = item else {
                    return None;
                };
                if output.call_id != call.call_id {
                    return None;
                }
                let FunctionCallOutput::Text(output) = &output.output else {
                    return None;
                };
                serde_json::from_str::<serde_json::Value>(output).ok()
            });
            if tool_output
                .as_ref()
                .and_then(|output| output.get("status"))
                .and_then(serde_json::Value::as_str)
                != Some("ok")
            {
                return None;
            }
            let read_snowflake = |key: &str| {
                tool_output
                    .as_ref()
                    .and_then(|output| output.get(key))
                    .and_then(|value| {
                        value
                            .as_u64()
                            .or_else(|| value.as_str().and_then(|value| value.parse::<u64>().ok()))
                    })
            };
            Some(DiscordSendRecord {
                content,
                private,
                channel_id: read_snowflake("channel_id"),
                message_id: read_snowflake("message_id"),
            })
        })
    }
}

impl LMContextCompaction {
    pub fn summary_contexts(&self) -> &[LMContext] {
        &self.summary_contexts
    }

    pub fn source_item_count(&self) -> usize {
        self.source_items.len()
    }

    pub fn estimated_token_savings(&self, summary: &str) -> usize {
        let source_tokens = self
            .source_items
            .iter()
            .map(estimated_item_tokens)
            .fold(0usize, usize::saturating_add);
        source_tokens.saturating_sub(estimated_item_tokens(&compaction_summary_item(summary)))
    }

    pub fn oversized_first_turn(&self) -> bool {
        self.oversized_first_turn
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

fn compaction_summary_item(summary: &str) -> InputItem {
    text_message_item(
        format!(
            "Summary of earlier conversation context. Treat this as background memory, not \
             as a new user request:\n{summary}"
        ),
        Role::Developer,
    )
}

fn estimated_item_tokens(item: &InputItem) -> usize {
    let serialized = match serde_json::to_string(item) {
        Ok(serialized) => serialized,
        Err(_) => return usize::MAX,
    };
    // Use an explicit encoding: tiktoken-rs does not map the configured GPT-6 models yet.
    let text_tokens = o200k_base_singleton()
        .encode_with_special_tokens(&serialized)
        .len();
    let image_reserve = image_urls_in_item(item)
        .len()
        .saturating_mul(ESTIMATED_IMAGE_TOKENS);
    text_tokens.saturating_add(image_reserve)
}

fn is_pinned_instruction(item: &InputItem) -> bool {
    match item {
        InputItem::EasyMessage(message) => {
            message.role == Role::System || message.role == Role::Developer
        }
        InputItem::Item(Item::Message(MessageItem::Input(message))) => {
            message.role == InputRole::System || message.role == InputRole::Developer
        }
        _ => false,
    }
}

fn history_turn_ranges(items: &VecDeque<InputItem>) -> Vec<(usize, usize)> {
    let Some(first_user_turn) = items.iter().position(starts_history_turn) else {
        return Vec::new();
    };

    let mut starts = vec![0usize];
    for (index, item) in items.iter().enumerate().skip(first_user_turn + 1) {
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
        InputItem::EasyMessage(message) => message.role == Role::User,
        InputItem::Item(Item::Message(MessageItem::Input(message))) => {
            message.role == InputRole::User
        }
        _ => false,
    }
}

fn summary_context_for_items(items: &[InputItem]) -> LMContext {
    let mut summary_context = LMContext::new();
    summary_context.buf = items.iter().cloned().collect();
    for item in &mut summary_context.buf {
        redact_images_for_compaction(item);
    }
    summary_context.sanitize_tool_history();
    summary_context
}

fn chunk_oversized_turn_context(items: &[InputItem], max_bytes: usize) -> Vec<LMContext> {
    let redacted_items = summary_context_for_items(items).buf;
    let serialized = serde_json::to_string(&redacted_items).unwrap_or_default();
    let mut chunk_bytes = (max_bytes / 3).max(1);

    loop {
        let text_chunks = split_utf8_chunks(&serialized, chunk_bytes);
        let total_chunks = text_chunks.len();
        let contexts = text_chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| {
                let mut context = LMContext::new();
                context.add_text(
                    format!(
                        "Ordered segment {} of {} from the earliest historical turn. The body \
                         continues across segments and boundaries may occur inside serialized \
                         JSON. Use this segment only as source material; combine facts in segment \
                         order.\n\n{}",
                        index + 1,
                        total_chunks,
                        chunk
                    ),
                    Role::User,
                );
                context
            })
            .collect::<Vec<_>>();

        if contexts
            .iter()
            .all(|context| context_input_bytes(context) <= max_bytes)
            || chunk_bytes == 1
        {
            return contexts;
        }
        chunk_bytes = (chunk_bytes / 2).max(1);
    }
}

pub(crate) fn split_utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut end = 0usize;

    for (index, character) in text.char_indices() {
        let next_end = index + character.len_utf8();
        if next_end - start > max_bytes && end > start {
            chunks.push(&text[start..end]);
            start = index;
        }
        end = next_end;
    }

    if start < text.len() {
        chunks.push(&text[start..]);
    }
    if chunks.is_empty() {
        chunks.push("");
    }
    chunks
}

fn context_input_bytes(context: &LMContext) -> usize {
    serde_json::to_vec(&context.generate_context())
        .map(|serialized| serialized.len())
        .unwrap_or(usize::MAX)
}

fn serialized_item_bytes(item: &InputItem) -> usize {
    serde_json::to_vec(item)
        .map(|serialized| serialized.len())
        .unwrap_or(usize::MAX)
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
        reasoning.id.as_deref()
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
            id: Some(id.to_string()),
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

    fn discord_send_call_item(call_id: &str, operation: &str, content: &str) -> InputItem {
        InputItem::Item(Item::FunctionCall(FunctionToolCall {
            arguments: serde_json::json!({
                "operation": operation,
                "content": content
            })
            .to_string(),
            call_id: call_id.to_string(),
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
    fn latest_discord_send_record_reads_message_target_from_tool_result() {
        let mut context = LMContext::new();
        context
            .buf
            .push_back(discord_send_call_item("call_1", "send_message", "hello"));
        context
            .buf
            .push_back(InputItem::Item(Item::FunctionCallOutput(
                FunctionCallOutputItemParam {
                    call_id: "call_1".to_string(),
                    output: FunctionCallOutput::Text(
                        serde_json::json!({
                        "status": "ok",
                            "channel_id": "123",
                            "message_id": "456"
                        })
                        .to_string(),
                    ),
                    id: None,
                    status: None,
                },
            )));

        let record = context.get_latest_discord_send_record().unwrap();
        assert_eq!(record.content, "hello");
        assert!(!record.private);
        assert_eq!(record.channel_id, Some(123));
        assert_eq!(record.message_id, Some(456));
    }

    #[test]
    fn failed_discord_send_is_not_treated_as_a_delivered_message() {
        let mut context = LMContext::new();
        context
            .buf
            .push_back(discord_send_call_item("call_1", "send_message", "hello"));
        context
            .buf
            .push_back(InputItem::Item(Item::FunctionCallOutput(
                FunctionCallOutputItemParam {
                    call_id: "call_1".to_string(),
                    output: FunctionCallOutput::Text("Error: missing permission".to_string()),
                    id: None,
                    status: None,
                },
            )));

        assert!(context.get_latest_discord_send_record().is_none());
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
    fn compaction_uses_local_token_estimate_and_keeps_tool_pairs_atomic() {
        let mut history = LMContext::new();
        history.add_text("old request".to_string(), Role::User);
        history.buf.push_back(function_call_item("call_1", None));
        history.buf.push_back(function_output_item("call_1"));
        history.add_text("middle request".to_string(), Role::User);
        history.add_text("recent request".to_string(), Role::User);

        let estimate = history.estimated_input_tokens() as u32;
        assert!(
            history
                .plan_compaction(estimate.saturating_add(1), 0)
                .is_none()
        );

        let compaction = history.plan_compaction(estimate, 0).unwrap();
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
        let compaction = history.plan_compaction(1, 0).unwrap();

        history.add_text("arrived while summarizing".to_string(), Role::User);
        assert!(history.apply_compaction(&compaction, "important earlier details"));

        assert_eq!(history.get_result(), "arrived while summarizing");
        assert!(
            extract_text_from_item(history.buf.front().unwrap())
                .unwrap()
                .contains("important earlier details")
        );
    }

    #[test]
    fn compaction_rejects_a_plan_after_history_is_cleared() {
        let mut history = LMContext::new();
        for message in ["old one", "old two", "recent"] {
            history.add_text(message.to_string(), Role::User);
        }
        let compaction = history.plan_compaction(1, 0).unwrap();

        history.clear();
        history.add_text("new conversation".to_string(), Role::User);

        assert!(!history.apply_compaction(&compaction, "stale summary"));
        assert_eq!(history.get_result(), "new conversation");
    }

    #[test]
    fn compaction_input_replaces_images_with_text_markers() {
        let image_url = "https://cdn.discordapp.com/attachments/1/2/image.png?ex=ffff";
        let mut history = LMContext::new();
        history.add_user_text_with_images("old image".to_string(), vec![image_url.to_string()]);
        history.add_text("middle request".to_string(), Role::User);
        history.add_text("recent request".to_string(), Role::User);
        let compaction = history.plan_compaction(1, 0).unwrap();
        let serialized =
            serde_json::to_string(&compaction.summary_contexts()[0].generate_context()).unwrap();

        assert!(!serialized.contains(image_url));
        assert!(serialized.contains("image attachment(s) omitted"));
    }
}

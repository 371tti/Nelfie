use std::str::FromStr;

use serde_json::json;
use serenity::all::{
    ButtonStyle, ChannelId, CreateActionRow, CreateButton, CreateInputText, CreateMessage,
    CreateModal, InputTextStyle,
};

use crate::llm::tool::LMTool;

pub const MODAL_TRIGGER_PREFIX: &str = "modal_builder:open:";
const MODAL_TRIGGER_VERSION: &str = "v1:";
pub const MODAL_SUBMIT_PREFIX: &str = "modal_builder:submit:";
const DISCORD_CUSTOM_ID_LIMIT: usize = 100;
const DISCORD_MODAL_TITLE_LIMIT: usize = 45;
const DISCORD_INPUT_LABEL_LIMIT: usize = 45;
const DISCORD_INPUT_PLACEHOLDER_LIMIT: usize = 100;
const DISCORD_INPUT_VALUE_LIMIT: usize = 4000;
const DISCORD_INPUT_LENGTH_LIMIT: u16 = 4000;
const DISCORD_MODAL_INPUT_LIMIT: usize = 5;
const DISCORD_BUTTON_LABEL_LIMIT: usize = 80;

#[derive(Clone, Debug)]
pub struct ModalInputSpec {
    pub label: String,
    pub custom_id: String,
    pub style: InputTextStyle,
    pub placeholder: Option<String>,
    pub value: Option<String>,
    pub required: bool,
    pub min_length: Option<u16>,
    pub max_length: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct ModalSpec {
    pub title: String,
    pub logical_custom_id: String,
    pub inputs: Vec<ModalInputSpec>,
}

pub struct ModalBuilderTool;

impl ModalBuilderTool {
    pub fn new() -> ModalBuilderTool {
        ModalBuilderTool {}
    }

    fn get_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Result<&'a str, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Missing or invalid '{key}' parameter"))
    }

    fn parse_modal_spec(args: &serde_json::Value) -> Result<ModalSpec, String> {
        let title = Self::get_str_arg(args, "title")?.to_string();
        let logical_custom_id = Self::get_str_arg(args, "custom_id")?.to_string();
        let inputs = args
            .get("inputs")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "Missing or invalid 'inputs' parameter".to_string())?;

        if inputs.is_empty() {
            return Err("'inputs' must contain at least one field".to_string());
        }

        let mut parsed_inputs = Vec::with_capacity(inputs.len());
        for (idx, input) in inputs.iter().enumerate() {
            let label = input
                .get("label")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("inputs[{idx}].label is required"))?
                .to_string();
            let input_custom_id = input
                .get("custom_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("inputs[{idx}].custom_id is required"))?
                .to_string();

            let style = match input
                .get("style")
                .and_then(|v| v.as_str())
                .unwrap_or("short")
            {
                "short" => InputTextStyle::Short,
                "paragraph" => InputTextStyle::Paragraph,
                other => {
                    return Err(format!(
                        "inputs[{idx}].style must be 'short' or 'paragraph', got '{other}'"
                    ));
                }
            };

            let min_length = input
                .get("min_length")
                .and_then(|v| v.as_u64())
                .map(|v| {
                    u16::try_from(v)
                        .map_err(|_| format!("inputs[{idx}].min_length must be <= 65535"))
                })
                .transpose()?;

            let max_length = input
                .get("max_length")
                .and_then(|v| v.as_u64())
                .map(|v| {
                    u16::try_from(v)
                        .map_err(|_| format!("inputs[{idx}].max_length must be <= 65535"))
                })
                .transpose()?;

            parsed_inputs.push(ModalInputSpec {
                label,
                custom_id: input_custom_id,
                style,
                placeholder: input
                    .get("placeholder")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                value: input
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                required: input
                    .get("required")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                min_length,
                max_length,
            });
        }

        let spec = ModalSpec {
            title,
            logical_custom_id,
            inputs: parsed_inputs,
        };
        validate_modal_spec(&spec)?;
        Ok(spec)
    }

    fn build_modal_payload(spec: &ModalSpec, effective_custom_id: &str) -> serde_json::Value {
        let components = spec
            .inputs
            .iter()
            .map(|input| {
                json!({
                    "type": 1,
                    "components": [{
                        "type": 4,
                        "custom_id": input.custom_id,
                        "label": input.label,
                        "style": match input.style {
                            InputTextStyle::Paragraph => 2,
                            _ => 1,
                        },
                        "required": input.required,
                        "placeholder": input.placeholder,
                        "value": input.value,
                        "min_length": input.min_length,
                        "max_length": input.max_length,
                    }],
                })
            })
            .collect::<Vec<serde_json::Value>>();

        json!({
            "title": spec.title,
            "custom_id": effective_custom_id,
            "logical_custom_id": spec.logical_custom_id,
            "components": components,
        })
    }
}

fn validate_modal_spec(spec: &ModalSpec) -> Result<(), String> {
    validate_nonempty_len("title", &spec.title, DISCORD_MODAL_TITLE_LIMIT)?;
    validate_nonempty_len(
        "custom_id",
        &spec.logical_custom_id,
        DISCORD_CUSTOM_ID_LIMIT,
    )?;

    if spec.inputs.is_empty() {
        return Err("'inputs' must contain at least one field".to_string());
    }
    if spec.inputs.len() > DISCORD_MODAL_INPUT_LIMIT {
        return Err(format!(
            "'inputs' must contain at most {DISCORD_MODAL_INPUT_LIMIT} fields"
        ));
    }

    for (idx, input) in spec.inputs.iter().enumerate() {
        validate_nonempty_len(
            &format!("inputs[{idx}].label"),
            &input.label,
            DISCORD_INPUT_LABEL_LIMIT,
        )?;
        validate_nonempty_len(
            &format!("inputs[{idx}].custom_id"),
            &input.custom_id,
            DISCORD_CUSTOM_ID_LIMIT,
        )?;
        if let Some(placeholder) = &input.placeholder {
            validate_len(
                &format!("inputs[{idx}].placeholder"),
                placeholder,
                DISCORD_INPUT_PLACEHOLDER_LIMIT,
            )?;
        }
        if let Some(value) = &input.value {
            validate_len(
                &format!("inputs[{idx}].value"),
                value,
                DISCORD_INPUT_VALUE_LIMIT,
            )?;
        }
        if let Some(min_length) = input.min_length
            && min_length > DISCORD_INPUT_LENGTH_LIMIT
        {
            return Err(format!(
                "inputs[{idx}].min_length must be <= {DISCORD_INPUT_LENGTH_LIMIT}"
            ));
        }
        if let Some(max_length) = input.max_length
            && max_length > DISCORD_INPUT_LENGTH_LIMIT
        {
            return Err(format!(
                "inputs[{idx}].max_length must be <= {DISCORD_INPUT_LENGTH_LIMIT}"
            ));
        }
        if let (Some(min_length), Some(max_length)) = (input.min_length, input.max_length)
            && min_length > max_length
        {
            return Err(format!(
                "inputs[{idx}].min_length must be <= inputs[{idx}].max_length"
            ));
        }
    }

    Ok(())
}

fn validate_nonempty_len(name: &str, value: &str, max_chars: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    validate_len(name, value, max_chars)
}

fn validate_len(name: &str, value: &str, max_chars: usize) -> Result<(), String> {
    let len = value.chars().count();
    if len > max_chars {
        return Err(format!(
            "{name} must be <= {max_chars} characters, got {len}"
        ));
    }
    Ok(())
}

fn build_submit_custom_id(spec: &ModalSpec) -> Result<String, String> {
    let submit_custom_id = format!("{}{}", MODAL_SUBMIT_PREFIX, spec.logical_custom_id);
    validate_len(
        "modal submit custom_id",
        &submit_custom_id,
        DISCORD_CUSTOM_ID_LIMIT,
    )?;
    Ok(submit_custom_id)
}

fn encode_modal_trigger_custom_id(spec: &ModalSpec) -> Result<String, String> {
    ensure_stateless_trigger_compatible(spec)?;

    let encoded_inputs = spec
        .inputs
        .iter()
        .map(|input| {
            format!(
                "{}{}{}{}",
                encode_len_token(&input.label),
                encode_len_token(&input.custom_id),
                match input.style {
                    InputTextStyle::Paragraph => "p",
                    _ => "s",
                },
                if input.required { "1" } else { "0" }
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let trigger_custom_id = format!(
        "{}{}{}{}{}:{}",
        MODAL_TRIGGER_PREFIX,
        MODAL_TRIGGER_VERSION,
        encode_len_token(&spec.title),
        encode_len_token(&spec.logical_custom_id),
        spec.inputs.len(),
        encoded_inputs
    );
    validate_len(
        "modal trigger custom_id",
        &trigger_custom_id,
        DISCORD_CUSTOM_ID_LIMIT,
    )?;
    Ok(trigger_custom_id)
}

fn ensure_stateless_trigger_compatible(spec: &ModalSpec) -> Result<(), String> {
    for (idx, input) in spec.inputs.iter().enumerate() {
        if input.placeholder.is_some()
            || input.value.is_some()
            || input.min_length.is_some()
            || input.max_length.is_some()
        {
            return Err(format!(
                "inputs[{idx}] uses placeholder/value/min_length/max_length, but build_and_send_trigger is stateless and stores the modal in Discord custom_id only. Omit these fields or use build_modal for payload JSON."
            ));
        }
    }
    Ok(())
}

pub fn decode_modal_trigger_custom_id(
    trigger_custom_id: &str,
) -> Result<Option<(ModalSpec, String)>, String> {
    let Some(body) = trigger_custom_id.strip_prefix(MODAL_TRIGGER_PREFIX) else {
        return Ok(None);
    };
    let Some(body) = body.strip_prefix(MODAL_TRIGGER_VERSION) else {
        return Err("Unsupported modal trigger version".to_string());
    };

    let mut offset = 0usize;
    let title = parse_len_token(body, &mut offset)?;
    let logical_custom_id = parse_len_token(body, &mut offset)?;
    let input_count = parse_count(body, &mut offset)?;
    if input_count > DISCORD_MODAL_INPUT_LIMIT {
        return Err(format!(
            "Modal trigger input count must be <= {DISCORD_MODAL_INPUT_LIMIT}"
        ));
    }

    let mut inputs = Vec::with_capacity(input_count);
    for idx in 0..input_count {
        let label = parse_len_token(body, &mut offset)?;
        let custom_id = parse_len_token(body, &mut offset)?;
        let style = match read_ascii_marker(body, &mut offset)? {
            "s" => InputTextStyle::Short,
            "p" => InputTextStyle::Paragraph,
            other => {
                return Err(format!(
                    "Invalid modal trigger input style at index {idx}: {other}"
                ));
            }
        };
        let required = match read_ascii_marker(body, &mut offset)? {
            "1" => true,
            "0" => false,
            other => {
                return Err(format!(
                    "Invalid modal trigger required flag at index {idx}: {other}"
                ));
            }
        };

        inputs.push(ModalInputSpec {
            label,
            custom_id,
            style,
            placeholder: None,
            value: None,
            required,
            min_length: None,
            max_length: None,
        });
    }

    if offset != body.len() {
        return Err("Invalid trailing data in modal trigger payload".to_string());
    }

    let spec = ModalSpec {
        title,
        logical_custom_id,
        inputs,
    };
    validate_modal_spec(&spec)?;
    let submit_custom_id = build_submit_custom_id(&spec)?;
    Ok(Some((spec, submit_custom_id)))
}

fn encode_len_token(value: &str) -> String {
    format!("{}:{}", value.chars().count(), value)
}

fn parse_len_token(input: &str, offset: &mut usize) -> Result<String, String> {
    let rest = input
        .get(*offset..)
        .ok_or_else(|| "Invalid modal trigger offset".to_string())?;
    let Some(colon_idx) = rest.find(':') else {
        return Err("Invalid length token in modal trigger payload".to_string());
    };
    let len = rest[..colon_idx]
        .parse::<usize>()
        .map_err(|_| "Invalid length token in modal trigger payload".to_string())?;

    let start = *offset + colon_idx + 1;
    let value_rest = input
        .get(start..)
        .ok_or_else(|| "Invalid length token start in modal trigger payload".to_string())?;
    let mut end = start;
    let mut count = 0usize;
    for (idx, ch) in value_rest.char_indices() {
        if count == len {
            break;
        }
        end = start + idx + ch.len_utf8();
        count += 1;
    }

    if count != len {
        return Err("Length token exceeds modal trigger payload".to_string());
    }

    *offset = end;
    Ok(input[start..end].to_string())
}

fn parse_count(input: &str, offset: &mut usize) -> Result<usize, String> {
    let rest = input
        .get(*offset..)
        .ok_or_else(|| "Invalid modal trigger count offset".to_string())?;
    let Some(colon_idx) = rest.find(':') else {
        return Err("Invalid input count in modal trigger payload".to_string());
    };
    let count = rest[..colon_idx]
        .parse::<usize>()
        .map_err(|_| "Invalid input count in modal trigger payload".to_string())?;
    *offset += colon_idx + 1;
    Ok(count)
}

fn read_ascii_marker<'a>(input: &'a str, offset: &mut usize) -> Result<&'a str, String> {
    let marker = input
        .get(*offset..*offset + 1)
        .ok_or_else(|| "Unexpected end of modal trigger payload".to_string())?;
    *offset += 1;
    Ok(marker)
}

impl Default for ModalBuilderTool {
    fn default() -> Self {
        Self::new()
    }
}

pub fn build_create_modal(spec: &ModalSpec, effective_custom_id: &str) -> CreateModal {
    let rows = spec
        .inputs
        .iter()
        .map(|input| {
            let mut text_input = CreateInputText::new(input.style, &input.label, &input.custom_id)
                .required(input.required);

            if let Some(placeholder) = &input.placeholder {
                text_input = text_input.placeholder(placeholder);
            }
            if let Some(value) = &input.value {
                text_input = text_input.value(value);
            }
            if let Some(min_length) = input.min_length {
                text_input = text_input.min_length(min_length);
            }
            if let Some(max_length) = input.max_length {
                text_input = text_input.max_length(max_length);
            }

            CreateActionRow::InputText(text_input)
        })
        .collect::<Vec<CreateActionRow>>();

    CreateModal::new(effective_custom_id, &spec.title).components(rows)
}

#[async_trait::async_trait]
impl LMTool for ModalBuilderTool {
    fn name(&self) -> String {
        "modal-builder-tool".to_string()
    }

    fn description(&self) -> String {
        "Build valid Discord modal definitions. Use build_modal to generate payload JSON, or build_and_send_trigger to post a stateless button that opens the modal when clicked. build_and_send_trigger stores the modal definition inside Discord custom_id, so keep title, custom_id, labels, and input IDs short and do not use placeholder/value/min_length/max_length there.".to_string()
    }

    fn json_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Operation to run. Use build_and_send_trigger for actual Discord-side modal opening flow. It is stateless and limited by Discord custom_id length.",
                    "enum": ["build_modal", "build_and_send_trigger"]
                },
                "channel_id": {
                    "type": "string",
                    "description": "Target channel ID. Required for build_and_send_trigger."
                },
                "title": {
                    "type": "string",
                    "description": "Modal title. Max 45 characters."
                },
                "custom_id": {
                    "type": "string",
                    "description": "Logical modal custom ID. Max 100 characters for build_modal. For build_and_send_trigger it is prefixed internally and must be much shorter."
                },
                "inputs": {
                    "type": "array",
                    "description": "List of text input definitions.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "description": "Input label. Max 45 characters."
                            },
                            "custom_id": {
                                "type": "string",
                                "description": "Input custom ID. Max 100 characters."
                            },
                            "style": {
                                "type": "string",
                                "description": "Input style. 'short' or 'paragraph'. Defaults to 'short'.",
                                "enum": ["short", "paragraph"]
                            },
                            "placeholder": {
                                "type": "string",
                                "description": "Optional placeholder text. Supported by build_modal only; build_and_send_trigger rejects it to avoid storage."
                            },
                            "value": {
                                "type": "string",
                                "description": "Optional prefilled value. Supported by build_modal only; build_and_send_trigger rejects it to avoid storage."
                            },
                            "required": {
                                "type": "boolean",
                                "description": "Whether this input is required. Defaults to true."
                            },
                            "min_length": {
                                "type": "integer",
                                "description": "Optional minimum length. Supported by build_modal only; build_and_send_trigger rejects it to avoid storage."
                            },
                            "max_length": {
                                "type": "integer",
                                "description": "Optional maximum length. Supported by build_modal only; build_and_send_trigger rejects it to avoid storage."
                            }
                        },
                        "required": ["label", "custom_id"]
                    }
                },
                "trigger_label": {
                    "type": "string",
                    "description": "Button label used by build_and_send_trigger. Defaults to 'Open modal'. Max 80 characters."
                },
                "trigger_message": {
                    "type": "string",
                    "description": "Message body posted with the trigger button. Defaults to a short guide text."
                }
            },
            "required": ["operation", "title", "custom_id", "inputs"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: crate::app::context::NelfieContext,
    ) -> Result<String, String> {
        let operation = args
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'operation' parameter".to_string())?;

        match operation {
            "build_modal" => {
                let spec = Self::parse_modal_spec(&args)?;
                let payload = Self::build_modal_payload(&spec, &spec.logical_custom_id);

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "modal": payload,
                    "note": "Payload generated only. To open it in Discord, use build_and_send_trigger.",
                });

                Ok(result.to_string())
            }
            "build_and_send_trigger" => {
                let spec = Self::parse_modal_spec(&args)?;
                let channel_id = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let trigger_custom_id = encode_modal_trigger_custom_id(&spec)?;
                let submit_custom_id = build_submit_custom_id(&spec)?;

                let trigger_label = args
                    .get("trigger_label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Open modal");
                validate_len("trigger_label", trigger_label, DISCORD_BUTTON_LABEL_LIMIT)?;
                let trigger_message = args
                    .get("trigger_message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("モーダルを開くには下のボタンを押してね。");

                let builder = CreateMessage::new()
                    .content(trigger_message)
                    .components(vec![CreateActionRow::Buttons(vec![
                        CreateButton::new(trigger_custom_id.clone())
                            .style(ButtonStyle::Primary)
                            .label(trigger_label),
                    ])]);

                let sent = channel_id
                    .send_message(ob_ctx.discord_client.open().http.clone(), builder)
                    .await
                    .map_err(|e| format!("Failed to send modal trigger message: {e}"))?;

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "channel_id": channel_id.to_string(),
                    "trigger_message_id": sent.id.to_string(),
                    "trigger_custom_id": trigger_custom_id,
                    "modal": Self::build_modal_payload(&spec, &submit_custom_id),
                    "note": "When a user clicks the button, the modal is opened via a stateless interaction response. No server-side modal storage is used.",
                });

                Ok(result.to_string())
            }
            other => Err(format!(
                "Unsupported 'operation': {other}. Use: build_modal or build_and_send_trigger."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_spec() -> ModalSpec {
        ModalSpec {
            title: "Feedback".to_string(),
            logical_custom_id: "fb".to_string(),
            inputs: vec![ModalInputSpec {
                label: "Comment".to_string(),
                custom_id: "comment".to_string(),
                style: InputTextStyle::Paragraph,
                placeholder: None,
                value: None,
                required: true,
                min_length: None,
                max_length: None,
            }],
        }
    }

    #[test]
    fn stateless_trigger_round_trips_modal_spec() {
        let spec = simple_spec();

        let trigger_id = encode_modal_trigger_custom_id(&spec).unwrap();
        let (decoded, submit_custom_id) = decode_modal_trigger_custom_id(&trigger_id)
            .unwrap()
            .unwrap();

        assert_eq!(submit_custom_id, "modal_builder:submit:fb");
        assert_eq!(decoded.title, spec.title);
        assert_eq!(decoded.logical_custom_id, spec.logical_custom_id);
        assert_eq!(decoded.inputs.len(), 1);
        assert_eq!(decoded.inputs[0].label, "Comment");
        assert_eq!(decoded.inputs[0].custom_id, "comment");
        assert_eq!(decoded.inputs[0].style, InputTextStyle::Paragraph);
        assert!(decoded.inputs[0].required);
    }

    #[test]
    fn stateless_trigger_escapes_separators() {
        let mut spec = simple_spec();
        spec.title = "A|B".to_string();
        spec.logical_custom_id = "x,y".to_string();
        spec.inputs[0].label = "one;two".to_string();
        spec.inputs[0].custom_id = "field\\id".to_string();

        let trigger_id = encode_modal_trigger_custom_id(&spec).unwrap();
        let (decoded, _) = decode_modal_trigger_custom_id(&trigger_id)
            .unwrap()
            .unwrap();

        assert_eq!(decoded.title, "A|B");
        assert_eq!(decoded.logical_custom_id, "x,y");
        assert_eq!(decoded.inputs[0].label, "one;two");
        assert_eq!(decoded.inputs[0].custom_id, "field\\id");
    }

    #[test]
    fn stateless_trigger_rejects_fields_that_need_storage() {
        let mut spec = simple_spec();
        spec.inputs[0].placeholder = Some("Long helper text".to_string());

        let err = encode_modal_trigger_custom_id(&spec).unwrap_err();

        assert!(err.contains("stateless"));
    }

    #[test]
    fn stateless_trigger_rejects_custom_id_over_discord_limit() {
        let mut spec = simple_spec();
        spec.title = "T".repeat(45);
        spec.logical_custom_id = "L".repeat(45);
        spec.inputs[0].label = "I".repeat(45);
        spec.inputs[0].custom_id = "C".repeat(45);

        let err = encode_modal_trigger_custom_id(&spec).unwrap_err();

        assert!(err.contains("modal trigger custom_id"));
    }

    #[test]
    fn modal_validation_rejects_too_many_inputs() {
        let mut spec = simple_spec();
        spec.inputs = vec![spec.inputs[0].clone(); 6];

        let err = validate_modal_spec(&spec).unwrap_err();

        assert!(err.contains("at most 5"));
    }
}

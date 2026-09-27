use serenity::all::{
    CreateAttachment, CreateInteractionResponseFollowup, CreateMessage, EditInteractionResponse,
    ModalInteraction,
};

const DISCORD_MESSAGE_LIMIT: usize = 2000;
pub(super) const MODAL_PENDING_MESSAGE: &str = "採点中...";
pub(super) const MODAL_PUBLIC_RESPONSE_MESSAGE: &str = "-# 公開回答を送信しました。";
const MODAL_EMPTY_RESPONSE_MESSAGE: &str = "-# 完了しました。";

struct CodeAttachment {
    filename: String,
    content: String,
}

pub(super) async fn send_long_message(
    http: &serenity::http::Http,
    channel_id: serenity::all::ChannelId,
    text: &str,
    footer: &str,
) -> Result<(), serenity::Error> {
    let (text, attachments) = extract_code_blocks(text);
    let text = text.trim();

    if text.is_empty() {
        if !attachments.is_empty() {
            send_code_attachments(http, channel_id, &attachments).await?;
        }
        if !footer.is_empty() {
            channel_id
                .send_message(http, CreateMessage::new().content(footer))
                .await?;
        }
        return Ok(());
    }

    let total_len = discord_character_count(text) + 1 + discord_character_count(footer);
    if !footer.is_empty() && total_len <= DISCORD_MESSAGE_LIMIT {
        channel_id
            .send_message(
                http,
                CreateMessage::new().content(format!("{}\n{}", text, footer)),
            )
            .await?;
    } else {
        let chunks = split_message_chunks(text, DISCORD_MESSAGE_LIMIT);
        for chunk in chunks {
            if chunk.trim().is_empty() {
                continue;
            }
            channel_id
                .send_message(http, CreateMessage::new().content(chunk))
                .await?;
        }

        if !footer.is_empty() {
            channel_id
                .send_message(http, CreateMessage::new().content(footer))
                .await?;
        }
    }

    if !attachments.is_empty() {
        send_code_attachments(http, channel_id, &attachments).await?;
    }

    Ok(())
}

pub(super) async fn send_modal_ephemeral_response(
    http: &serenity::http::Http,
    modal: &ModalInteraction,
    text: &str,
    footer: &str,
) -> Result<(), serenity::Error> {
    let (text, attachments) = extract_code_blocks(text);
    let text = text.trim();

    if text.is_empty() {
        modal
            .edit_response(
                http,
                EditInteractionResponse::new().content(if footer.is_empty() {
                    MODAL_EMPTY_RESPONSE_MESSAGE
                } else {
                    footer
                }),
            )
            .await?;
    } else {
        let total_len = discord_character_count(text)
            + if footer.is_empty() {
                0
            } else {
                1 + discord_character_count(footer)
            };

        if !footer.is_empty() && total_len <= DISCORD_MESSAGE_LIMIT {
            modal
                .edit_response(
                    http,
                    EditInteractionResponse::new().content(format!("{}\n{}", text, footer)),
                )
                .await?;
        } else {
            let mut chunks = split_message_chunks(text, DISCORD_MESSAGE_LIMIT)
                .into_iter()
                .filter(|chunk| !chunk.trim().is_empty());

            if let Some(first_chunk) = chunks.next() {
                modal
                    .edit_response(http, EditInteractionResponse::new().content(first_chunk))
                    .await?;
            } else {
                modal
                    .edit_response(
                        http,
                        EditInteractionResponse::new().content(if footer.is_empty() {
                            MODAL_EMPTY_RESPONSE_MESSAGE
                        } else {
                            footer
                        }),
                    )
                    .await?;
            }

            for chunk in chunks {
                modal
                    .create_followup(
                        http,
                        CreateInteractionResponseFollowup::new()
                            .content(chunk)
                            .ephemeral(true),
                    )
                    .await?;
            }

            if !footer.is_empty() {
                modal
                    .create_followup(
                        http,
                        CreateInteractionResponseFollowup::new()
                            .content(footer)
                            .ephemeral(true),
                    )
                    .await?;
            }
        }
    }

    if !attachments.is_empty() {
        send_modal_code_attachments(http, modal, &attachments).await?;
    }

    Ok(())
}

async fn send_modal_code_attachments(
    http: &serenity::http::Http,
    modal: &ModalInteraction,
    attachments: &[CodeAttachment],
) -> Result<(), serenity::Error> {
    for attachment in attachments {
        if attachment.content.trim().is_empty() {
            continue;
        }

        let file = CreateAttachment::bytes(
            attachment.content.as_bytes().to_vec(),
            attachment.filename.clone(),
        );
        let message = CreateInteractionResponseFollowup::new()
            .content(format!("[code file] {}", attachment.filename))
            .add_file(file)
            .ephemeral(true);

        modal.create_followup(http, message).await?;
    }

    Ok(())
}

async fn send_code_attachments(
    http: &serenity::http::Http,
    channel_id: serenity::all::ChannelId,
    attachments: &[CodeAttachment],
) -> Result<(), serenity::Error> {
    for attachment in attachments {
        if attachment.content.trim().is_empty() {
            continue;
        }

        let file = CreateAttachment::bytes(
            attachment.content.as_bytes().to_vec(),
            attachment.filename.clone(),
        );
        let message = CreateMessage::new()
            .content(format!("[code file] {}", attachment.filename))
            .add_file(file);

        channel_id.send_message(http, message).await?;
    }

    Ok(())
}

fn extract_code_blocks(text: &str) -> (String, Vec<CodeAttachment>) {
    let mut out = String::with_capacity(text.len());
    let mut attachments = Vec::new();
    let mut remaining = text;
    let mut index = 1usize;

    while let Some(block) = next_fenced_code_block(remaining) {
        out.push_str(block.before);
        append_code_block(
            block.markdown,
            block.body,
            &mut out,
            &mut attachments,
            &mut index,
        );
        remaining = block.remaining;
    }

    out.push_str(remaining);
    (out, attachments)
}

struct FencedCodeBlock<'a> {
    before: &'a str,
    markdown: &'a str,
    body: &'a str,
    remaining: &'a str,
}

fn next_fenced_code_block(text: &str) -> Option<FencedCodeBlock<'_>> {
    let start = text.find("```")?;
    let body_start = start + 3;

    if let Some(relative_end) = text[body_start..].find("```") {
        let body_end = body_start + relative_end;
        let block_end = body_end + 3;
        Some(FencedCodeBlock {
            before: &text[..start],
            markdown: &text[start..block_end],
            body: &text[body_start..body_end],
            remaining: &text[block_end..],
        })
    } else {
        Some(FencedCodeBlock {
            before: &text[..start],
            markdown: &text[start..],
            body: &text[body_start..],
            remaining: "",
        })
    }
}

fn append_code_block(
    block: &str,
    raw: &str,
    out: &mut String,
    attachments: &mut Vec<CodeAttachment>,
    index: &mut usize,
) {
    let (lang, code) = split_code_block(raw);
    let code = code.trim_end_matches(['\n', '\r']);
    if code.trim().is_empty() {
        return;
    }

    if discord_character_count(block) <= DISCORD_MESSAGE_LIMIT {
        out.push_str(block);
        return;
    }

    let filename = build_code_filename(*index, lang);
    attachments.push(CodeAttachment {
        filename: filename.clone(),
        content: code.to_string(),
    });
    out.push_str(&format!("[code: {}]", filename));
    *index += 1;
}

fn split_code_block(raw: &str) -> (Option<&str>, &str) {
    let Some(newline) = raw.find('\n') else {
        return (None, raw);
    };

    let first = raw[..newline].trim();
    if first.is_empty() {
        (None, &raw[newline + 1..])
    } else {
        (Some(first), &raw[newline + 1..])
    }
}

fn build_code_filename(index: usize, lang: Option<&str>) -> String {
    let ext = lang.and_then(code_extension_for_language).unwrap_or("txt");
    format!("code-{}.{}", index, ext)
}

fn code_extension_for_language(lang: &str) -> Option<&'static str> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rs" | "rust" => Some("rs"),
        "py" | "python" => Some("py"),
        "js" | "javascript" => Some("js"),
        "ts" | "typescript" => Some("ts"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yml"),
        "md" | "markdown" => Some("md"),
        "sh" | "bash" | "shell" => Some("sh"),
        "ps1" | "powershell" => Some("ps1"),
        "c" => Some("c"),
        "cpp" | "c++" | "cc" => Some("cpp"),
        "h" => Some("h"),
        "go" => Some("go"),
        "java" => Some("java"),
        "kt" | "kotlin" => Some("kt"),
        "sql" => Some("sql"),
        "html" => Some("html"),
        "css" => Some("css"),
        _ => None,
    }
}

fn split_message_chunks(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut remaining = text;

    while let Some(block) = next_fenced_code_block(remaining) {
        append_plain_text(&mut out, &mut current, block.before, limit);

        if !current.is_empty()
            && discord_character_count(&current)
                .saturating_add(discord_character_count(block.markdown))
                > limit
        {
            flush_message_chunk(&mut out, &mut current);
        }

        current.push_str(block.markdown);
        remaining = block.remaining;
    }

    append_plain_text(&mut out, &mut current, remaining, limit);
    flush_message_chunk(&mut out, &mut current);
    out
}

fn append_plain_text(out: &mut Vec<String>, current: &mut String, segment: &str, limit: usize) {
    let mut rest = segment;

    while !rest.is_empty() {
        let current_len = discord_character_count(current);
        if current_len >= limit {
            flush_message_chunk(out, current);
            rest = rest.trim_start_matches(char::is_whitespace);
            continue;
        }

        let available = limit - current_len;
        if discord_character_count(rest) <= available {
            current.push_str(rest);
            break;
        }

        let (end, next_start) = split_plain_text_at_limit(rest, available);
        if end == 0 {
            if !current.is_empty() {
                flush_message_chunk(out, current);
                continue;
            }

            let Some(first_char) = rest.chars().next() else {
                break;
            };
            let first_char_len = first_char.len_utf8();
            current.push_str(&rest[..first_char_len]);
            rest = &rest[first_char_len..];
            flush_message_chunk(out, current);
            continue;
        }

        current.push_str(&rest[..end]);
        flush_message_chunk(out, current);
        rest = rest[next_start..].trim_start_matches(char::is_whitespace);
    }
}

fn split_plain_text_at_limit(text: &str, limit: usize) -> (usize, usize) {
    let mut count = 0usize;
    let mut hard_end = 0usize;
    let mut last_break: Option<(usize, usize)> = None;

    for (idx, ch) in text.char_indices() {
        let char_count = ch.len_utf16();
        if count.saturating_add(char_count) > limit {
            break;
        }

        count += char_count;
        hard_end = idx + ch.len_utf8();
        if ch.is_whitespace() {
            last_break = Some((idx, ch.len_utf8()));
        }
        if count == limit {
            break;
        }
    }

    match last_break {
        Some((break_idx, break_len)) if break_idx > 0 => (break_idx, break_idx + break_len),
        _ => (hard_end, hard_end),
    }
}

fn flush_message_chunk(out: &mut Vec<String>, current: &mut String) {
    let chunk = current.trim();
    if !chunk.is_empty() {
        out.push(chunk.to_string());
    }
    current.clear();
}

fn discord_character_count(text: &str) -> usize {
    text.encode_utf16().count()
}

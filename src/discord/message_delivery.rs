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

    let total_len = text.chars().count() + 1 + footer.chars().count();
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
        let total_len = text.chars().count()
            + if footer.is_empty() {
                0
            } else {
                1 + footer.chars().count()
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
    let mut cursor = 0usize;
    let mut index = 1usize;

    while let Some(rel_start) = text[cursor..].find("```") {
        let start = cursor + rel_start;
        out.push_str(&text[cursor..start]);

        let after_ticks = start + 3;
        let Some(rel_end) = text[after_ticks..].find("```") else {
            out.push_str(&text[start..]);
            return (out, attachments);
        };

        let end = after_ticks + rel_end;
        let raw = &text[after_ticks..end];
        let (lang, code) = split_code_block(raw);
        let code = code.trim_end_matches(['\n', '\r']);

        if !code.trim().is_empty() {
            let filename = build_code_filename(index, lang);
            attachments.push(CodeAttachment {
                filename: filename.clone(),
                content: code.to_string(),
            });
            out.push_str(&format!("[code: {}]", filename));
            index += 1;
        }

        cursor = end + 3;
    }

    out.push_str(&text[cursor..]);
    (out, attachments)
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
    let mut out = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        let mut count = 0usize;
        let mut last_break: Option<(usize, usize)> = None;
        let mut split_at: Option<(usize, usize)> = None;

        for (idx, ch) in rest.char_indices() {
            count += 1;
            if ch.is_whitespace() {
                last_break = Some((idx, ch.len_utf8()));
            }
            if count >= limit {
                split_at = Some((idx, ch.len_utf8()));
                break;
            }
        }

        let Some((idx, len)) = split_at else {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            break;
        };

        let (end, next_start) = match last_break {
            Some((break_idx, break_len)) if break_idx > 0 => (break_idx, break_idx + break_len),
            _ => (idx + len, idx + len),
        };

        let head = rest[..end].trim_end();
        if !head.is_empty() {
            out.push(head.to_string());
        }

        rest = rest[next_start..].trim_start_matches(|c: char| c.is_whitespace());
    }

    out
}

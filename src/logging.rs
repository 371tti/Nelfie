use std::io::Write as _;

const DEFAULT_FILTER: &str = "warn,nelfie=info";

pub fn init() {
    let env = env_logger::Env::default().default_filter_or(DEFAULT_FILTER);
    let mut builder = env_logger::Builder::from_env(env);

    builder.format(|buf, record| {
        let timestamp = buf.timestamp_millis();
        let level = record.level();
        let level_style = buf.default_level_style(level);
        let target = short_target(record.target());
        let message = sanitize_log_message(&record.args().to_string());

        writeln!(
            buf,
            "{timestamp} {level_style}{level:<5}{level_style:#} [{target}] {message}"
        )
    });

    let _ = builder.try_init();
}

fn short_target(target: &str) -> &str {
    target.strip_prefix("nelfie::").unwrap_or(target)
}

fn sanitize_log_message(message: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }

    let mut output = String::with_capacity(message.len());
    let mut state = State::Text;
    let mut pending_space = false;

    for character in message.chars() {
        match state {
            State::Text => match character {
                '\u{1b}' => state = State::Escape,
                value if value.is_whitespace() => {
                    pending_space = !output.is_empty();
                }
                value if value.is_control() => {}
                value => {
                    if pending_space {
                        output.push(' ');
                        pending_space = false;
                    }
                    output.push(value);
                }
            },
            State::Escape => {
                state = match character {
                    '[' => State::Csi,
                    ']' => State::Osc,
                    _ => State::Text,
                };
            }
            State::Csi => {
                if ('@'..='~').contains(&character) {
                    state = State::Text;
                }
            }
            State::Osc => match character {
                '\u{7}' => state = State::Text,
                '\u{1b}' => state = State::OscEscape,
                _ => {}
            },
            State::OscEscape => {
                state = if character == '\\' {
                    State::Text
                } else if character == '\u{1b}' {
                    State::OscEscape
                } else {
                    State::Osc
                };
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_application_targets_only() {
        assert_eq!(
            short_target("nelfie::discord::responses"),
            "discord::responses"
        );
        assert_eq!(
            short_target("serenity::gateway::shard"),
            "serenity::gateway::shard"
        );
    }

    #[test]
    fn log_messages_remove_ansi_escapes_and_control_characters() {
        assert_eq!(
            sanitize_log_message("first\n\u{1b}[31mred\u{1b}[0m\tlast\u{7}"),
            "first red last"
        );
        assert_eq!(
            sanitize_log_message("\u{1b}]0;window title\u{7}日本語"),
            "日本語"
        );
    }
}

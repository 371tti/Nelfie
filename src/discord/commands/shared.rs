use crate::app::context::NelfieContext;

pub(super) type Error = Box<dyn std::error::Error + Send + Sync>;
pub(super) type Context<'a> = poise::Context<'a, NelfieContext, Error>;

pub(super) fn preview_text(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, character) in input.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(character);
    }

    if out.is_empty() {
        "(empty)".to_string()
    } else {
        out
    }
}

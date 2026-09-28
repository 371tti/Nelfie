use std::error::Error;

use log::warn;
use serenity::all::{
    ActionRowComponent, CreateInteractionResponse, CreateInteractionResponseMessage, Interaction,
};

use crate::{
    app::context::NelfieContext,
    discord::{message_delivery::MODAL_PENDING_MESSAGE, responses::schedule_modal_response},
    llm::context::{LMContext, Role},
};

pub(super) async fn handle_interaction(
    ctx: &serenity::client::Context,
    interaction: &Interaction,
    ob_context: &NelfieContext,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match interaction {
        Interaction::Component(component) => {
            let trigger_id = component.data.custom_id.clone();
            if let Some(music_action) =
                trigger_id.strip_prefix(crate::voice::music::MUSIC_COMPONENT_PREFIX)
            {
                let Some((guild_raw, action)) = music_action.split_once(':') else {
                    component
                        .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
                        .await?;
                    return Ok(());
                };
                let parsed_guild = guild_raw
                    .parse::<u64>()
                    .ok()
                    .map(serenity::all::GuildId::new);
                let guild_id = parsed_guild.filter(|guild| Some(*guild) == component.guild_id);
                component
                    .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
                    .await?;
                if let Some(guild_id) = guild_id {
                    if let Err(error) = ob_context
                        .voice_system
                        .music
                        .handle_component(guild_id, component.message.id, action)
                        .await
                    {
                        warn!("music control interaction failed: {error}");
                    }
                }
                return Ok(());
            }
            let decoded_modal =
                crate::llm::tools::modal_builder::decode_modal_trigger_custom_id(&trigger_id);
            let Some((modal_spec, submit_custom_id)) = (match decoded_modal {
                Ok(decoded) => decoded,
                Err(e) => {
                    component
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .content(format!("モーダルを開けません: {e}"))
                                    .ephemeral(true),
                            ),
                        )
                        .await?;
                    return Ok(());
                }
            }) else {
                return Ok(());
            };

            let modal = crate::llm::tools::modal_builder::build_create_modal(
                &modal_spec,
                &submit_custom_id,
            );

            component
                .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                .await?;
        }
        Interaction::Modal(modal) => {
            if !modal
                .data
                .custom_id
                .starts_with(crate::llm::tools::modal_builder::MODAL_SUBMIT_PREFIX)
            {
                return Ok(());
            }

            let mut fields = serde_json::Map::new();
            for row in &modal.data.components {
                for component in &row.components {
                    if let ActionRowComponent::InputText(input) = component {
                        fields.insert(
                            input.custom_id.clone(),
                            serde_json::Value::String(input.value.clone().unwrap_or_default()),
                        );
                    }
                }
            }

            modal
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(MODAL_PENDING_MESSAGE)
                            .ephemeral(true),
                    ),
                )
                .await?;

            let mut lm_context = LMContext::new();
            lm_context.add_text(
                serde_json::json!({
                    "type": "modal_submit",
                    "modal_custom_id": modal.data.custom_id,
                    "user": modal.user.name,
                    "display_name": modal.user.display_name(),
                    "respondent_user_id": modal.user.id.to_string(),
                    "guild_id": modal.guild_id.map(|id| id.to_string()),
                    "channel_id": modal.channel_id.to_string(),
                    "fields": fields,
                })
                .to_string(),
                Role::User,
            );
            ob_context
                .chat_contexts
                .marge(modal.channel_id, &lm_context);

            schedule_modal_response(ctx, ob_context, modal);
        }
        _ => {}
    }

    Ok(())
}

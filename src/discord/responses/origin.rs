use serenity::all::{ChannelId, GuildId, Message, ModalInteraction, UserId};

use crate::{
    discord::ephemeral::EphemeralInteractionContext,
    llm::{
        context::{LMContext, Role},
        models::Models,
    },
};

#[derive(Clone, Debug)]
pub struct CronResponseRequest {
    pub cron_id: String,
    pub schedule: String,
    pub prompt: String,
    pub manual: bool,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
}

impl CronResponseRequest {
    pub(super) fn request_context(&self) -> LMContext {
        let mut lm_context = LMContext::new();
        lm_context.add_text(self.current_prompt_message(), Role::User);
        lm_context
    }

    pub(super) fn current_prompt_message(&self) -> String {
        format!(
            "This is an automated scheduled cron prompt. Execute the scheduled prompt below as \
             the current active user request.\n\
             \n\
             Rules for this turn:\n\
             - The scheduled prompt is an instruction to follow now, not metadata to summarize.\n\
             - Prior channel history is only background context. Do not answer older messages \
             unless the scheduled prompt explicitly asks for that.\n\
             - Do not mention that this was triggered by cron unless the scheduled prompt asks.\n\
             \n\
             cron_id: {}\n\
             cron_expression: {}\n\
             manual_test: {}\n\
             guild_id: {}\n\
             channel_id: {}\n\
             \n\
             <scheduled_prompt>\n\
             {}\n\
             </scheduled_prompt>",
            self.cron_id, self.schedule, self.manual, self.guild_id, self.channel_id, self.prompt
        )
    }

    pub(super) fn system_note(&self) -> String {
        format!(
            "This request was triggered by a registered cron schedule. \
             It is an automated scheduled prompt, not a live user mention. \
             The latest user message contains the scheduled prompt to execute now; follow that \
             prompt as the active request and treat earlier channel history as background only. \
             cron_id: {}, cron_expression: {}, manual_test: {}",
            self.cron_id, self.schedule, self.manual
        )
    }

    pub(super) fn failure_message(&self, reason: &str) -> String {
        format!(
            "Err: scheduled cron task failed: `{}`\nreason: {}\nschedule: `{}`",
            self.cron_id, reason, self.schedule
        )
    }
}

#[derive(Clone)]
pub(super) enum ResponseOrigin {
    User(UserResponseRequest),
    Cron(CronResponseRequest),
}

#[derive(Clone, Debug)]
pub(super) struct UserResponseRequest {
    pub(super) user_id: UserId,
    pub(super) guild_id: Option<GuildId>,
    pub(super) channel_id: ChannelId,
    pub(super) delivery: UserResponseDelivery,
}

#[derive(Clone, Debug)]
pub(super) enum UserResponseDelivery {
    Channel,
    ModalEphemeral(Box<ModalEphemeralDelivery>),
}

#[derive(Clone, Debug)]
pub(super) struct ModalEphemeralDelivery {
    modal_custom_id: String,
    pub(super) interaction: ModalInteraction,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum RateLimitTarget {
    User(UserId),
    GuildBot {
        guild_id: GuildId,
        bot_user_id: UserId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RateLimitBucket {
    General,
    Cron,
}

impl ResponseOrigin {
    pub(super) fn response_model(&self, user_model: Models) -> Models {
        match self {
            Self::User(_) => user_model,
            Self::Cron(_) => Models::CRON_MODEL,
        }
    }

    pub(super) fn log_source(&self) -> &'static str {
        match self {
            Self::User(user) => match &user.delivery {
                UserResponseDelivery::Channel => "message",
                UserResponseDelivery::ModalEphemeral(_) => "modal",
            },
            Self::Cron(cron) if cron.manual => "cron_test",
            Self::Cron(_) => "cron",
        }
    }

    pub(super) fn channel_id(&self) -> ChannelId {
        match self {
            Self::User(user) => user.channel_id,
            Self::Cron(cron) => cron.channel_id,
        }
    }

    pub(super) fn guild_id(&self) -> Option<GuildId> {
        match self {
            Self::User(user) => user.guild_id,
            Self::Cron(cron) => Some(cron.guild_id),
        }
    }

    pub(super) fn rate_limit_target(&self, bot_user_id: UserId) -> RateLimitTarget {
        match self {
            Self::User(user) => RateLimitTarget::User(user.user_id),
            Self::Cron(cron) => RateLimitTarget::GuildBot {
                guild_id: cron.guild_id,
                bot_user_id,
            },
        }
    }

    pub(super) fn rate_limit_bucket(&self) -> RateLimitBucket {
        match self {
            Self::User(_) => RateLimitBucket::General,
            Self::Cron(_) => RateLimitBucket::Cron,
        }
    }

    pub(super) fn current_request_context(&self) -> Option<LMContext> {
        match self {
            Self::User(_) => None,
            Self::Cron(cron) => Some(cron.request_context()),
        }
    }

    pub(super) fn triggers_context_compaction(&self) -> bool {
        matches!(self, Self::User(_))
    }

    pub(super) fn failure_message(&self, reason: &str) -> Option<String> {
        match self {
            Self::User(user) => user.delivery.failure_message(reason),
            Self::Cron(cron) => Some(cron.failure_message(reason)),
        }
    }

    pub(super) fn system_note(&self) -> Option<String> {
        match self {
            Self::User(user) => user.delivery.system_note(user.user_id),
            Self::Cron(cron) => Some(cron.system_note()),
        }
    }

    pub(super) fn thinking_label(&self) -> &'static str {
        match self {
            Self::User(_) => "-# Thinking...",
            Self::Cron(_) => "-# Scheduled task running...",
        }
    }

    pub(super) fn uses_public_progress(&self) -> bool {
        match self {
            Self::User(user) => user.delivery.uses_public_progress(),
            Self::Cron(_) => true,
        }
    }

    pub(super) fn modal_ephemeral_delivery(&self) -> Option<&ModalEphemeralDelivery> {
        match self {
            Self::User(user) => user.delivery.modal_ephemeral_delivery(),
            Self::Cron(_) => None,
        }
    }

    pub(super) fn ephemeral_interaction_context(&self) -> Option<EphemeralInteractionContext> {
        match self {
            Self::User(user) => user.delivery.ephemeral_interaction_context(user.user_id),
            Self::Cron(_) => None,
        }
    }
}

impl UserResponseRequest {
    pub(super) fn from_message(msg: &Message) -> Self {
        Self {
            user_id: msg.author.id,
            guild_id: msg.guild_id,
            channel_id: msg.channel_id,
            delivery: UserResponseDelivery::Channel,
        }
    }

    pub(super) fn from_modal(modal: &ModalInteraction) -> Self {
        Self {
            user_id: modal.user.id,
            guild_id: modal.guild_id,
            channel_id: modal.channel_id,
            delivery: UserResponseDelivery::ModalEphemeral(Box::new(ModalEphemeralDelivery {
                modal_custom_id: modal.data.custom_id.clone(),
                interaction: modal.clone(),
            })),
        }
    }
}

impl UserResponseDelivery {
    pub(super) fn uses_public_progress(&self) -> bool {
        matches!(self, Self::Channel)
    }

    pub(super) fn modal_ephemeral_delivery(&self) -> Option<&ModalEphemeralDelivery> {
        match self {
            Self::Channel => None,
            Self::ModalEphemeral(modal) => Some(modal),
        }
    }

    pub(super) fn ephemeral_interaction_context(
        &self,
        user_id: UserId,
    ) -> Option<EphemeralInteractionContext> {
        let modal = self.modal_ephemeral_delivery()?;
        Some(EphemeralInteractionContext {
            interaction: modal.interaction.clone(),
            respondent_user_id: user_id,
        })
    }

    pub(super) fn failure_message(&self, reason: &str) -> Option<String> {
        let modal = self.modal_ephemeral_delivery()?;
        Some(format!(
            "Err: modal response failed: `{}`\nreason: {}",
            modal.modal_custom_id, reason
        ))
    }

    pub(super) fn system_note(&self, user_id: UserId) -> Option<String> {
        self.modal_ephemeral_delivery()?;
        Some(format!(
            "This request was triggered by a Discord modal submission. \
             The respondent user_id is {}. \
             Treat this as a normal user response request for rate limits, model selection, \
             conversation history, and tool calls. \
             If the answer should be visible only to the respondent, respond normally; the final \
             assistant answer will edit the original ephemeral modal response. \
             If the answer should be public in the channel, use discord-tool operation \
             send_message, and do not repeat the same public body in the normal assistant reply. \
             You may also use discord-tool operation send_ephemeral_message to update the \
             respondent-only ephemeral response explicitly.",
            user_id
        ))
    }
}

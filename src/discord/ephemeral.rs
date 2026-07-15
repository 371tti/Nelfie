use std::future::Future;

use serenity::all::{ModalInteraction, UserId};

tokio::task_local! {
    static EPHEMERAL_INTERACTION: EphemeralInteractionContext;
}

#[derive(Clone, Debug)]
pub struct EphemeralInteractionContext {
    pub interaction: ModalInteraction,
    pub respondent_user_id: UserId,
}

pub async fn scope_ephemeral_interaction<F, T>(context: EphemeralInteractionContext, future: F) -> T
where
    F: Future<Output = T>,
{
    EPHEMERAL_INTERACTION.scope(context, future).await
}

pub(crate) fn current_ephemeral_interaction() -> Option<EphemeralInteractionContext> {
    EPHEMERAL_INTERACTION.try_with(Clone::clone).ok()
}

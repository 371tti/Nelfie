use std::error::Error;

use log::{error, info};
use serenity::{Client, all::GatewayIntents};
use songbird::SerenityInit;

use crate::{
    app::context::NelfieContext,
    discord::{
        client::DiscordClientHandles, command_registry, events::event_handler,
        voice_events::start_auto_leave_monitor,
    },
};

pub async fn start(ob_ctx: NelfieContext) -> Result<(), Box<dyn Error + Send + Sync>> {
    info!("Discord client starting");

    let framework_context = ob_ctx.clone();
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: command_registry::all(),
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some("!".into()),
                ..Default::default()
            },
            event_handler: |ctx, event, framework, data| {
                Box::pin(event_handler(ctx, event, framework, data))
            },
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let ob_ctx = framework_context.clone();
            Box::pin(async move {
                ob_ctx.discord_client.install(DiscordClientHandles {
                    http: ctx.http.clone(),
                    cache: ctx.cache.clone(),
                });

                if let Some(songbird_manager) = songbird::get(ctx).await {
                    ob_ctx.voice_system.set_songbird(songbird_manager);
                    start_auto_leave_monitor(ctx.clone(), ob_ctx.clone());
                }
                ob_ctx.cron_scheduler.start(ctx.clone(), ob_ctx.clone());

                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                info!("Discord commands registered");
                Ok(ob_ctx)
            })
        })
        .build();

    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::MESSAGE_CONTENT;

    let discord_client = Client::builder(ob_ctx.config.discord_token.clone(), intents)
        .register_songbird()
        .framework(framework);

    let monitoring = ob_ctx.monitoring.clone();
    tokio::spawn(async move {
        let mut client = match discord_client.await {
            Ok(client) => client,
            Err(error) => {
                error!("failed to create Discord client: {error}");
                monitoring.mark_gateway_disconnected();
                return;
            }
        };
        ob_ctx
            .discord_client
            .install_shard_manager(client.shard_manager.clone());
        if let Err(error) = client.start().await {
            error!("Discord client stopped with an error: {error}");
        }
        monitoring.mark_gateway_disconnected();
    });

    Ok(())
}

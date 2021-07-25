use chrono::Utc;
use scripty_macros::handle_serenity_error;
use scripty_metrics::Metrics;
use scripty_utils::START_TIME;
use serenity::{
    client::Context,
    framework::standard::{macros::command, CommandResult},
    model::prelude::Message,
};
use std::hint::unreachable_unchecked;

#[command("stats")]
#[bucket = "expensive"]
#[description = "Live statistics on the bot."]
async fn cmd_stats(ctx: &Context, msg: &Message) -> CommandResult {
    if let Err(e) = msg
        .channel_id
        .send_message(&ctx, |m| m.content("https://stats.imaskeleton.me"))
        .await
    {
        handle_serenity_error!(e);
    }

    Ok(())
}

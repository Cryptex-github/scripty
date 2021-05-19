use crate::bind;
use crate::globals::PgPoolKey;
use serenity::futures::TryStreamExt;
use serenity::model::id::ChannelId;
use serenity::prelude::Context;
use std::convert::TryInto;
use std::hint;
use std::sync::Arc;
use tracing::{debug, warn};

pub async fn auto_join(ctx: Arc<Context>) {
    let data = ctx.data.read().await;
    let pool = data.get::<PgPoolKey>().unwrap_or_else(|| unsafe {
        hint::unreachable_unchecked()
        // SAFETY: this should absolutely never happen if the DB pool is placed
        // in at initialization. if that were to happen, undefined behavior would result anyways
    });
    let mut query = sqlx::query!("SELECT * FROM guilds").fetch(pool);
    loop {
        match query.try_next().await {
            Ok(row) => match row {
                Some(row) => {
                    let guild_id = row.guild_id;

                    let already_connected = songbird::get(&ctx)
                        .await
                        .unwrap_or_else(|| unsafe {
                            hint::unreachable_unchecked() // SAFETY: this should absolutely never happen if Songbird is registered at client init.
                                                          // if it isn't registered, UB would result anyways
                        })
                        .get::<u64>(guild_id as u64)
                        .is_some();
                    if already_connected {
                        continue;
                    };

                    let vc_id = match row.default_bind {
                        Some(v) => v,
                        None => {
                            continue;
                        }
                    };
                    let result_id = match row.output_channel {
                        Some(v) => v,
                        None => {
                            continue;
                        }
                    };

                    if let Err(e) = bind::bind(
                        &ctx,
                        (vc_id as u64).into(),
                        (result_id as u64).into(),
                        (guild_id as u64).into(),
                    )
                    .await
                    {
                        warn!("failed to join VC in {}: {}", guild_id, e);
                        if let Err(e) = ChannelId(result_id.try_into().unwrap()).send_message(&ctx, |m | {
                            m.embed(|embed| {
                                embed
                                    .color(11534368)
                                    .description(format!("I can't join the voice chat you have set up! {}", e))
                                    .field("Need help fixing it?", "https://discord.gg/xSpNJSjNhq", true)
                                    .footer(|c| {
                                        c.text("This message will continually be sent until this is fixed.")
                                    })
                                    .title("Error while joining VC!")
                            })
                        }).await {
                            warn!("couldn't warn users about error in {}: {}", guild_id, e);
                            // if these queries fail so be it
                            let _ = sqlx::query!("DELETE FROM guilds WHERE guild_id = $1", guild_id).execute(pool).await;
                            let _ = sqlx::query!("DELETE FROM channels WHERE channel_id = $1", result_id).execute(pool).await;
                        }
                    } else {
                        debug!("joined VC in {} successfully", guild_id);
                    };
                }
                None => {
                    break;
                }
            },
            Err(_) => {
                continue;
            }
        }
    }
}
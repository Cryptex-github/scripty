use serenity::async_trait;
use serenity::client::bridge::gateway::ShardManager;
use serenity::model::id::{ChannelId, MessageId};
use serenity::prelude::{Context, TypeMapKey};
use songbird::driver::DecodeMode;
use songbird::model::id::UserId;
use songbird::model::payload::{ClientConnect, ClientDisconnect, Speaking};
use songbird::Event;
use songbird::{EventContext, EventHandler as VoiceEventHandler};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::RwLock;

pub static DECODE_TYPE: DecodeMode = DecodeMode::Decode;

pub enum ContextTypes<'a> {
    NoArc(&'a Context),
    WithArc(&'a Arc<Context>),
}

pub struct ShardManagerWrapper;

impl TypeMapKey for ShardManagerWrapper {
    type Value = Arc<RwLock<Arc<serenity::prelude::Mutex<ShardManager>>>>;
}

/// Gets the average websocket latency.
pub async fn get_avg_ws_latency(ctx: ContextTypes<'_>) -> (u128, u8) {
    let c = match ctx {
        ContextTypes::NoArc(c) => c,
        ContextTypes::WithArc(c) => c,
    };
    let data_read = c.data.read().await;

    let shard_manager_lock = data_read
        .get::<ShardManagerWrapper>()
        .expect("Expected shard manager in data map.")
        .clone();
    let shard_manager_guard = shard_manager_lock.read().await;
    let shard_manager = shard_manager_guard.lock().await;
    let mut total: u8 = 0;
    let mut latency: u128 = 0;
    for i in shard_manager.runners.lock().await.iter() {
        match i.1.latency {
            Some(l) => {
                total += 1;
                latency += l.as_millis();
            }
            None => {
                // ignore if no latency available
            }
        }
    }
    if total == 0 {
        // no shards ready
        latency = 0
    } else {
        latency = latency / total as u128; // scales to a arbitrary number of shards well
    }
    (latency, total)
}

pub async fn do_stats_update(ctx: Arc<Context>) {
    let shard_info = get_avg_ws_latency(ContextTypes::WithArc(&ctx)).await;

    ctx.cache.set_max_messages(0 as usize).await;
    let status_channel = ChannelId(791426352217587732);
    match status_channel
        .messages(&ctx.http, |retriever| {
            retriever.after(MessageId(0 as u64)).limit(25)
        })
        .await
    {
        Ok(m) => {
            if let Err(e) = status_channel.delete_messages(&ctx.http, m).await {
                println!("Failed to delete messages from status channel! {}", e);
            }
        }
        Err(e) => {
            println!("Failed to get most recent messages from channel! {}", e)
        }
    };
    let start = std::time::SystemTime::now();
    if let Err(why) = status_channel.broadcast_typing(&ctx.http).await {
        println!("Failed to get latency! {}", why);
    }
    let ping_time = match start.elapsed() {
        Ok(t) => t.as_millis(),
        Err(e) => {
            println!("Failed to get ping time! {}", e);
            return;
        }
    };
    let current_name = ctx.cache.current_user().await.name;
    let guild_count = ctx.cache.guild_count().await as u64;
    let user_count = ctx.cache.user_count().await as u64;
    let avg_ws_latency = if shard_info.0 == 0 {
        "NaN".to_string()
    } else {
        format!("{}", shard_info.0)
    };

    if let Err(e) = status_channel
        .send_message(&ctx.http, |m| {
            m.embed(|e| {
                e.title(format!("{}'s status", current_name))
                    .field("Guilds in Cache", guild_count, true)
                    .field("Users in Cache", user_count, true)
                    .field("Cached Messages", 0.to_string(), true)
                    .field("Message Send Latency", format!("{}ms", ping_time), true)
                    .field("Average WS Latency", format!("{}ms", avg_ws_latency), true)
                    .field("Shard Count", shard_info.1, true)
                    .field(
                        "Library",
                        "[serenity-rs](https://github.com/serenity-rs/serenity)",
                        true,
                    )
                    .field(
                        "Source Code",
                        "[Click me!](https://github.com/tazz4843/scripty)",
                        true,
                    )
                    .colour(serenity::utils::Colour::BLURPLE)
            })
        })
        .await
    {
        println!("Failed to update in status channel! {:?}", e);
    };
}

#[derive(Clone)]
pub struct Receiver {
    ssrc_map: Arc<RwLock<HashMap<u32, UserId>>>,
    audio_buffer: Arc<RwLock<Vec<i16>>>,
    encoded_audio_buffer: Arc<RwLock<Vec<u8>>>,
}

impl Receiver {
    pub fn new() -> Self {
        // You can manage state here, such as a buffer of audio packet bytes so
        // you can later store them in intervals.
        let ssrc_map = Arc::new(RwLock::new(HashMap::new()));
        let audio_buffer = Arc::new(RwLock::new(Vec::new()));
        let encoded_audio_buffer = Arc::new(RwLock::new(Vec::new()));
        Self {
            ssrc_map,
            audio_buffer,
            encoded_audio_buffer,
        }
    }
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    //noinspection SpellCheckingInspection
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use EventContext as Ctx;

        match ctx {
            Ctx::SpeakingStateUpdate(Speaking {
                speaking,
                ssrc,
                user_id,
                ..
            }) => {
                // Discord voice calls use RTP, where every sender uses a randomly allocated
                // *Synchronisation Source* (SSRC) to allow receivers to tell which audio
                // stream a received packet belongs to. As this number is not derived from
                // the sender's user_id, only Discord Voice Gateway messages like this one
                // inform us about which random SSRC a user has been allocated. Future voice
                // packets will contain *only* the SSRC.
                //
                // You can implement logic here so that you can differentiate users'
                // SSRCs and map the SSRC to the User ID and maintain this state.
                // Using this map, you can map the `ssrc` in `voice_packet`
                // to the user ID and handle their audio packets separately.
                println!(
                    "Speaking state update: user {:?} has SSRC {:?}, using {:?}",
                    user_id, ssrc, speaking,
                );
                if let Some(user_id) = user_id {
                    let mut map = self.ssrc_map.write().await;
                    map.insert(*ssrc, *user_id);
                } // otherwise just ignore it since we can't do anything about that
            }
            Ctx::SpeakingUpdate { ssrc, speaking } => {
                // You can implement logic here which reacts to a user starting
                // or stopping speaking.
                let user_id: Option<&UserId> = None;
                {
                    // block that will drop lock when exited
                    let map = self.ssrc_map.read().await;
                    let user_id = map.get(&ssrc);
                }
                let uid: u64 = match user_id {
                    Some(u) => u.to_string().parse().unwrap(),
                    None => 0,
                };
                if *speaking {
                    // user started speaking, reset buffer if not already
                    self.audio_buffer.write().await.clear();
                    self.encoded_audio_buffer.write().await.clear();
                } else {
                    if DECODE_TYPE == DecodeMode::Decode {
                        println!("Decode mode is DecodeMode::Decode");
                        let audio = self.audio_buffer.read().await.clone();

                        /*
                        match OpenOptions::new()
                            .write(true)
                            .create(true)
                            .open(format!("{}.pcm", ssrc))
                            .await
                        {
                            Ok(mut f) => {
                                for i in audio {
                                    if let Err(e) = f.write_i16(i).await {
                                        println!("Failed to write byte to file! {}", e);
                                    };
                                }
                            }
                            Err(e) => {
                                println!("Failed to open/create file! {}", e);
                            }
                        };
                        */
                        let args = [
                            "-f",
                            "s16be",
                            "-ar",
                            "8000",
                            "-ac",
                            "2",
                            "-acodec",
                            "pcm_s16le",
                            "-i",
                            "-",
                            &format!("{}.wav", ssrc),
                        ];

                        match Command::new("ffmpeg")
                            .args(&args)
                            .kill_on_drop(true)
                            .spawn()
                        {
                            Ok(p) => match p.stdin {
                                Some(mut stdin) => {
                                    for i in audio {
                                        if let Err(e) = stdin.write_i16(i).await {
                                            println!("Failed to write byte to file! {}", e);
                                        };
                                    }
                                } // at this point there's a file named "{}.wav" in the directory
                                None => {
                                    println!("Failed to open FFMPEG stdin!")
                                }
                            },
                            Err(e) => {
                                println!("Failed to spawn FFMPEG!");
                            }
                        }

                        // we now have a file named "{}.pcm" where {} is the user's SSRC.
                        // TODO: read and send to STT API
                        {
                            self.audio_buffer.write().await.clear(); // now to clear it
                        }
                    } else if DECODE_TYPE == DecodeMode::Decrypt {
                        println!("Decode mode is DecodeMode::Decrypt");
                        let audio = self.encoded_audio_buffer.read().await.clone();

                        match OpenOptions::new()
                            .write(true)
                            .create(true)
                            .open(format!("{}.opus", ssrc))
                            .await
                        {
                            Ok(mut f) => {
                                for i in audio {
                                    if let Err(e) = f.write_u8(i).await {
                                        println!("Failed to write byte to file! {}", e);
                                    };
                                }
                            }
                            Err(e) => {
                                println!("Failed to open/create file! {}", e);
                            }
                        };

                        // we now have a file named "{}.opus" where {} is the user's SSRC.
                        // TODO: read and send to STT API
                        {
                            self.audio_buffer.write().await.clear(); // now to clear it
                        }
                    } else {
                        println!("Decode mode is invalid!");
                    }
                }
                println!(
                    "Source {} (ID {}) has {} speaking.",
                    ssrc,
                    uid,
                    if *speaking { "started" } else { "stopped" },
                );
            }
            Ctx::VoicePacket {
                audio,
                packet,
                payload_offset,
                payload_end_pad,
            } => {
                // An event which fires for every received audio packet,
                // containing the decoded data.

                let uid: u64 = {
                    // block that will drop lock when exited
                    let map = self.ssrc_map.read().await;
                    match map.get(&packet.ssrc) {
                        Some(u) => u.to_string().parse().unwrap(),
                        None => 0,
                    }
                };
                if let Some(audio) = audio {
                    let aud = audio.clone();
                    println!("Audio packet: SSRC {}, user ID {}", packet.ssrc, uid);

                    {
                        // get exclusive write access to the audio buffer and write to it
                        let mut buf = self.audio_buffer.write().await;
                        let mut j: u32 = 0;
                        for i in aud {
                            buf.push(i);
                            /*
                            if j % 2 == 0 {
                                buf.push(i);
                            }
                             */
                            j += 1;
                        }
                    }

                    /*
                    let mut f = match tokio::fs::File::open(format!("{}.pcm", packet.ssrc)).await {
                        Ok(f) => f,
                        Err(e) => {
                            println!("Failed to open file! {}", e);
                            return None;
                        }
                    };
                    for i in audio {
                        f.write_i16(*i).await;
                    }

                    tokio::fs::write(format!("{}.pcm", packet.ssrc), &audio).await;
                     */
                } else {
                    println!("RTP packet, but no audio. Driver may not be configured to decode.");
                    let audio_range: &usize = &(packet.payload.len() - payload_end_pad);
                    println!("Audio range is {}", &audio_range);
                    let range = std::ops::Range {
                        start: payload_offset,
                        end: audio_range,
                    };
                    let mut buf = self.encoded_audio_buffer.write().await;
                    let mut counter: i64 = -1;
                    for i in &packet.payload {
                        counter += 1;
                        if counter <= *payload_offset as i64 {
                            continue;
                        } else if counter > *audio_range as i64 {
                            continue;
                        } else {
                            buf.push(i.clone())
                        }
                    }
                    println!(
                        "Audio packet sequence {:05}, SSRC {}, user ID {}",
                        packet.sequence.0, packet.ssrc, uid
                    );
                }
            }
            Ctx::RtcpPacket {
                packet,
                payload_offset,
                payload_end_pad,
            } => {
                // An event which fires for every received rtcp packet,
                // containing the call statistics and reporting information.
                // Probably ignorable for our purposes.
            }
            Ctx::ClientConnect(ClientConnect {
                audio_ssrc,
                video_ssrc,
                user_id,
                ..
            }) => {
                // You can implement your own logic here to handle a user who has joined the
                // voice channel e.g., allocate structures, map their SSRC to User ID.
                {
                    // block that will drop the lock when exited
                    let mut map = self.ssrc_map.write().await;
                    map.insert(*audio_ssrc, *user_id);
                }
                println!(
                    "Client connected: user {:?} has audio SSRC {:?}, video SSRC {:?}",
                    user_id, audio_ssrc, video_ssrc,
                );
            }
            Ctx::ClientDisconnect(ClientDisconnect { user_id, .. }) => {
                // You can implement your own logic here to handle a user who has left the
                // voice channel e.g., finalise processing of statistics etc.
                // You will typically need to map the User ID to their SSRC; observed when
                // speaking or connecting.
                let mut key: Option<u32> = None;
                {
                    let map = self.ssrc_map.read().await;
                    for i in map.iter() {
                        // walk the map to find the UserId
                        if i.1 == user_id {
                            key = Some(*i.0);
                            break;
                        };
                    }
                }
                match key {
                    Some(u) => {
                        {
                            let mut map = self.ssrc_map.write().await;
                            map.remove(&u);
                        }
                        println!("Removed {} from the user ID map.", u);
                    }
                    None => {
                        println!("Found no user with ID {} in the user ID map.", user_id);
                    }
                };

                println!("Client disconnected: user {:?}", user_id);
            }
            _ => {
                // We won't be registering this struct for any more event classes.
                unimplemented!()
            }
        }

        None
    }
}

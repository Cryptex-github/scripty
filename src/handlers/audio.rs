use crate::{decoder::Decoder, deepspeech::run_stt, utils::DECODE_TYPE};
use serenity::{async_trait, model::webhook::Webhook, prelude::{RwLock, Context}};
use songbird::{
    driver::DecodeMode,
    model::{
        id::UserId,
        payload::{ClientConnect, ClientDisconnect, Speaking},
    },
    Event, EventContext, EventHandler as VoiceEventHandler,
};
use std::{collections::{HashMap, BTreeSet}, process::Stdio, sync::Arc};
use tokio::{io::AsyncWriteExt, process::Command, task};
use uuid::Uuid;

fn do_check(
    user_id: &UserId,
    active_users: &tokio::sync::RwLockReadGuard<BTreeSet<UserId>>,
) -> bool {
    active_users.get(user_id).is_none()
}

#[derive(Clone)]
pub struct Receiver {
    ssrc_map: Arc<RwLock<HashMap<u32, UserId>>>,
    audio_buffer: Arc<RwLock<HashMap<u32, Vec<i16>>>>,
    encoded_audio_buffer: Arc<RwLock<HashMap<u32, Vec<i16>>>>,
    decoders: Arc<RwLock<HashMap<u32, Decoder>>>,
    active_users: Arc<RwLock<BTreeSet<UserId>>>,
    next_users: Arc<RwLock<BTreeSet<UserId>>>,
    webhook: Arc<Webhook>,
    context: Arc<Context>,
    premium_level: u8,
    max_users: u32, // seriously if it hits 65535 users in a VC wtf
}

impl Receiver {
    pub async fn new(webhook: Webhook, context: Arc<Context>, premium_level: u8) -> Self {
        // You can manage state here, such as a buffer of audio packet bytes so
        // you can later store them in intervals.
        let ssrc_map = Arc::new(RwLock::new(HashMap::new()));
        let audio_buffer = Arc::new(RwLock::new(HashMap::new()));
        let encoded_audio_buffer = Arc::new(RwLock::new(HashMap::new()));
        let decoders = Arc::new(RwLock::new(HashMap::new()));
        let webhook = Arc::new(webhook);
        let active_users = Arc::new(RwLock::new(BTreeSet::new()));
        let next_users = Arc::new(RwLock::new(BTreeSet::new()));
        Self {
            ssrc_map,
            audio_buffer,
            encoded_audio_buffer,
            decoders,
            webhook,
            context,
            premium_level,
            active_users,
            next_users,
            max_users: 10,
        }
    }
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    //noinspection SpellCheckingInspection
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use songbird::EventContext as Ctx;

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
                    if !do_check(&user_id, &self.active_users.read().await) {
                        println!("user failed the checks");
                        return None;
                    }

                    let mut map = self.ssrc_map.write().await;
                    map.insert(*ssrc, *user_id);
                    match DECODE_TYPE {
                        DecodeMode::Decrypt => {
                            {
                                let mut audio_buf = self.encoded_audio_buffer.write().await;
                                audio_buf.insert(*ssrc, Vec::new());
                            }
                            {
                                let mut decoders = self.decoders.write().await;
                                decoders.insert(*ssrc, Decoder::new());
                            }
                        }
                        DecodeMode::Decode => {
                            let mut audio_buf = self.audio_buffer.write().await;
                            audio_buf.insert(*ssrc, Vec::new());
                        }
                        _ => {
                            panic!("No supported decode mode found!")
                        }
                    }
                } // otherwise just ignore it since we can't do anything about that
            }
            Ctx::SpeakingUpdate { ssrc, speaking } => {
                // You can implement logic here which reacts to a user starting
                // or stopping speaking.
                let uid: u64 = {
                    let map = self.ssrc_map.read().await;
                    match map.get(ssrc) {
                        Some(u) => u.0,
                        None => 0,
                    }
                };
                if !do_check(&UserId(uid), &self.active_users.read().await) {
                    return None;
                };

                if !*speaking {
                    let audio = match DECODE_TYPE {
                        DecodeMode::Decrypt => {
                            {
                                let mut decoders = self.decoders.write().await;
                                decoders.insert(*ssrc, Decoder::new());
                            }
                            {
                                let mut buf = self.encoded_audio_buffer.write().await;
                                match buf.insert(*ssrc, Vec::new()) {
                                    Some(a) => a,
                                    None => {
                                        println!(
                                            "Didn't find a user with SSRC {} in the audio buffers.",
                                            ssrc
                                        );
                                        return None;
                                    }
                                }
                            }
                        }
                        DecodeMode::Decode => {
                            let mut buf = self.audio_buffer.write().await;
                            match buf.insert(*ssrc, Vec::new()) {
                                Some(a) => a,
                                None => {
                                    println!(
                                        "Didn't find a user with SSRC {} in the audio buffers.",
                                        ssrc
                                    );
                                    return None;
                                }
                            }
                        }
                        _ => {
                            println!("Decode mode is invalid!");
                            return None;
                        }
                    };
                    // all of this code reeks of https://www.youtube.com/watch?v=lIFE7h3m40U
                    let file_id = Uuid::new_v4();
                    let file_path = format!("{}.wav", file_id.as_u128());

                    // ffmpeg args
                    let args = [
                        "-f",
                        "s16le",
                        "-ar",
                        "48000",
                        "-ac",
                        "2",
                        "-i",
                        "-",
                        "-ac",
                        "1",
                        &file_path,
                    ];

                    /*
                    // sox args
                    let args = [
                        // INPUT FILE
                        // 16 bits
                        "-b",
                        "16",
                        // 16kHz sample rate
                        "-r",
                        "16000",
                        // stereo audio
                        "-c",
                        "2",
                        // raw PCM data
                        "-t",
                        "raw",
                        // little-endian
                        "-L",
                        // signed integers
                        "-e",
                        "signed-integer",
                        // stdin contains the file
                        "-",

                        // OUTPUT FILE
                        // 16 bits
                        "-b",
                        "16",
                        // 16kHz sample rate
                        "-r",
                        "48000",
                        // mono audio
                        "-c",
                        "1",
                        // signed integers
                        "-e",
                        "signed-integer",
                        // wav output
                        "-t",
                        "wav",
                        &file_path,

                        // EFFECTS
                        "speed",
                        "50"
                    ];
                    */

                    let mut child = match Command::new("ffmpeg")
                        .args(&args)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .kill_on_drop(true)
                        .spawn()
                    {
                        Err(e) => {
                            println!("Failed to spawn FFMPEG!");
                            return None;
                        }
                        Ok(c) => {
                            println!("Spawned FFMPEG!");
                            c
                        }
                    };

                    match child.stdin {
                        Some(ref mut stdin) => {
                            for i in audio {
                                if let Err(e) = stdin.write_i16(i).await {
                                    println!("Failed to write byte to FFMPEG stdin! {}", e);
                                    return None;
                                    // the audio's now corrupted, no point in continuing
                                    // plus if this happens once it'll happen every time after,
                                    // and that gets spammy as hell
                                };
                            }
                        }
                        None => {
                            println!("Failed to open FFMPEG stdin!");
                            return None;
                        }
                    };
                    // we now have a file named "{}.wav" where {} is a random UUID as a 128-bit integer.
                    // we should yield now to let other tasks proceed
                    task::yield_now().await;
                    let webhook = Arc::clone(&self.webhook);
                    let context = Arc::clone(&self.context);

                    task::spawn(async move {
                        match child.wait().await {
                            Ok(_) => {
                                match run_stt(file_path.clone()).await {
                                    Ok(r) => {
                                        if r.len() != 0 {
                                            match context.cache.user(uid).await {
                                                Some(u) => {
                                                    let profile_picture = match u.avatar {
                                                        Some(a) => {
                                                            format!("https://cdn.discordapp.com/avatars/{}/{}.png", u.id, a)
                                                        }
                                                        None => u.default_avatar_url(),
                                                    };
                                                    let name = u.name;

                                                    let _ = webhook
                                                        .execute(&context, false, |m| {
                                                            m.avatar_url(profile_picture)
                                                                .content(r)
                                                                .username(name)
                                                        })
                                                        .await;
                                                }
                                                None => {}
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        println!("Failed to run speech-to-text! {}", e);
                                    }
                                };
                            }
                            Err(e) => {
                                println!("FFMPEG failed! {}", e);
                            }
                        };
                        //if let Err(e) = tokio::fs::remove_file(&file_path).await {
                        //    println!("Failed to delete {}! {}", &file_path, e);
                        //};
                    });
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
                    let map = self.ssrc_map.read().await;
                    match map.get(&packet.ssrc) {
                        Some(u) => u.to_string().parse().unwrap(),
                        None => 0,
                    }
                };

                if !do_check(&UserId(uid), &self.active_users.read().await) {
                    return None;
                };

                match audio {
                    Some(audio) => {
                        let mut buf = self.audio_buffer.write().await;
                        let b = match buf.get_mut(&packet.ssrc) {
                            Some(b) => b,
                            None => {
                                return None;
                            }
                        };
                        b.extend(audio);
                    }
                    _ => {
                        /*
                        let audio_range: &usize = &(packet.payload.len() - payload_end_pad);
                        let range = std::ops::Range {
                            start: payload_offset,
                            end: audio_range,
                        };
                        let mut buf = self.encoded_audio_buffer.write().await;
                        let b = match buf.get_mut(&packet.ssrc) {
                            Some(b) => b,
                            None => {
                                return None;
                            }
                        };
                        let mut counter: i64 = -1;
                        for i in &packet.payload {
                            counter += 1;
                            if (counter <= *payload_offset as i64) | (counter > *audio_range as i64)
                            {
                                continue;
                            } else {
                                b.push(*i)
                            }
                        }
                        */
                        let mut audio = {
                            let mut decoders = self.decoders.write().await;
                            let decoder = match decoders
                                .get_mut(&packet.ssrc) {
                                Some(d) => d,
                                None => {return None;}
                            };
                            let mut v = Vec::new();
                            match decoder.opus_decoder.decode(&packet.payload, &mut v, false) {
                                Ok(s) => {
                                    println!("Decoded {} opus samples", s);
                                }
                                Err(e) => {
                                    println!("Failed to decode opus: {}", e);
                                    return None;
                                }
                            };
                            v
                        };
                        let mut buf = self.encoded_audio_buffer.write().await;
                        if let Some(b) = buf.get_mut(&packet.ssrc) {
                            b.append(&mut audio);
                        };

                    }
                }
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
                    let mut map = self.ssrc_map.write().await;
                    map.insert(*audio_ssrc, *user_id);
                }
                {
                    let mut decoders = self.decoders.write().await;
                    decoders.insert(*audio_ssrc, Decoder::new());
                }
                {
                    let mut active_users = self.active_users.write().await;
                    if active_users.len() > self.max_users as usize {
                        let mut next_users = self.next_users.write().await;
                        next_users.insert(*user_id);
                    } else {
                        active_users.insert(*user_id);
                    };
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
                if let Some(u) = {
                    let map = self.ssrc_map.read().await;
                    let mut id: Option<u32> = None;
                    for i in map.iter() {
                        // walk the map to find the UserId
                        if i.1 == user_id {
                            id = Some(*i.0);
                            break;
                        }
                    }
                    id
                } {
                    {
                        let mut audio_buf = self.encoded_audio_buffer.write().await;
                        audio_buf.remove(&u);
                    }
                    {
                        let mut audio_buf = self.audio_buffer.write().await;
                        audio_buf.remove(&u);
                    }
                    {
                        let mut decoders = self.decoders.write().await;
                        decoders.remove(&u);
                    }
                    {
                        let mut map = self.ssrc_map.write().await;
                        map.remove(&u);
                    }
                    {
                        let mut active_users = self.active_users.write().await;
                        active_users.remove(user_id);
                        let mut next_users = self.next_users.write().await;
                        if let Some(user) = next_users.pop_first() {
                            active_users.insert(user);
                        };
                    }
                };

                println!("Client disconnected: user {:?}", user_id);
            }
            _ => {}
        }

        None
    }
}

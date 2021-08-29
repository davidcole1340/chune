use std::{collections::VecDeque, sync::Arc};
use tokio::sync::Mutex;

use dashmap::{mapref::one::RefMut, DashMap};
use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::{
        id::{ChannelId, GuildId, UserId},
        interactions::{
            application_command::{
                ApplicationCommand, ApplicationCommandInteraction, ApplicationCommandOptionType,
            },
            Interaction,
        },
    },
};
use songbird::{tracks::TrackHandle, Event, TrackEvent};
use youtube_dl::{YoutubeDl, YoutubeDlOutput};

use crate::error::PlayError;

#[derive(Default)]
pub struct Handler {
    guilds: DashMap<GuildId, Arc<Mutex<Guild>>>,
}

#[derive(Debug)]
pub struct Guild {
    channel_id: ChannelId,
    handle: Option<TrackHandle>,
    now_playing: Option<Song>,
    queue: VecDeque<Song>,
}

impl Guild {
    pub fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            handle: None,
            now_playing: None,
            queue: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
pub struct Song {
    url: String,
}

#[async_trait]
impl EventHandler for Handler
where
    Handler: 'static,
{
    async fn ready(&self, ctx: serenity::client::Context, _: serenity::model::prelude::Ready) {
        log::info!("bot ready");

        let bot_id = ctx.http.get_current_application_info().await.unwrap();
        dbg!(bot_id);

        ApplicationCommand::set_global_application_commands(&ctx.http, |cmds| {
            cmds.create_application_command(|cmd| {
                cmd.name("play")
                    .description("Adds a track to the users voice channel song queue.")
                    .create_option(|opt| {
                        opt.name("song")
                            .description("URL to the song to play.")
                            .kind(ApplicationCommandOptionType::String)
                            .required(true)
                    })
            })
            .create_application_command(|cmd| {
                cmd.name("skip")
                    .description("Skips the currently playing song.")
            })
        })
        .await
        .expect("Failed to create bot commands");
    }

    async fn interaction_create(
        &self,
        ctx: serenity::client::Context,
        interaction: serenity::model::interactions::Interaction,
    ) {
        if let Interaction::ApplicationCommand(cmd) = interaction {
            let response = match cmd.data.name.as_str() {
                "play" => self.handle_play_cmd(ctx.clone(), &cmd),
                // "skip" => {}
                _ => return,
            }
            .await;

            if let Err(e) = response {
                match e {
                    PlayError::Unknown(_) => todo!(),
                    e => {
                        let _ = cmd
                            .create_interaction_response(&ctx.http, |resp| {
                                resp.interaction_response_data(|data| data.content(e.to_string()))
                            })
                            .await;
                    }
                };
            }
        }
    }
}

impl Handler
where
    Handler: 'static,
{
    pub async fn handle_play_cmd(
        &self,
        ctx: Context,
        cmd: &ApplicationCommandInteraction,
    ) -> Result<(), PlayError> {
        let guild_id = cmd.guild_id.ok_or(PlayError::NoGuildId)?;
        let url = cmd
            .data
            .options
            .first()
            .and_then(|opt| opt.value.as_ref())
            .and_then(|val| val.as_str())
            .ok_or(PlayError::NoUrl)?;
        let task_url = url.to_string();

        let yt_resp = tokio::spawn(async move {
            YoutubeDl::new(task_url)
                .socket_timeout("15")
                .format("bestaudio")
                .run()
        })
        .await
        .map_err(|e| PlayError::Unknown(Box::new(e)))?
        .map_err(|_| PlayError::Ytdl(url.to_string()))?;

        let channel_id = self
            .get_user_channel(&ctx, guild_id, cmd.user.id)
            .await
            .ok_or(PlayError::NoChannel)?;
        let guild_mutex = self.get_guild(guild_id, channel_id);

        {
            let mut guild = guild_mutex.lock().await;

            match yt_resp {
                YoutubeDlOutput::Playlist(playlist) => {
                    let entries = playlist
                        .entries
                        .as_ref()
                        .ok_or_else(|| PlayError::Ytdl(url.to_string()))?;

                    let start = guild.queue.len();
                    for entry in entries {
                        guild.queue.push_back(Song {
                            url: entry
                                .url
                                .clone()
                                .ok_or_else(|| PlayError::Ytdl(url.to_string()))?,
                        });
                    }

                    playlist.build_response(&ctx, cmd, start + 1).await?;
                }
                YoutubeDlOutput::SingleVideo(vid) => {
                    guild.queue.push_back(Song {
                        url: vid
                            .url
                            .clone()
                            .ok_or_else(|| PlayError::Ytdl(url.to_string()))?,
                    });
                    let pos = guild.queue.len();

                    vid.build_response(&ctx, cmd, pos).await?;
                }
            }
        }

        Guild::check_guild_queue(&guild_mutex, guild_id, &ctx).await?;
        Ok(())
    }

    async fn get_user_channel(
        &self,
        ctx: &Context,
        guild_id: GuildId,
        user_id: UserId,
    ) -> Option<ChannelId> {
        ctx.cache
            .guild(guild_id)
            .await?
            .voice_states
            .get(&user_id)?
            .channel_id
    }

    fn get_guild(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> RefMut<GuildId, Arc<Mutex<Guild>>> {
        match self.guilds.get_mut(&guild_id) {
            Some(guild) => guild,
            None => {
                let guild = Arc::new(Mutex::new(Guild::new(channel_id)));
                let result = self.guilds.insert(guild_id, guild);
                debug_assert!(result.is_none());
                self.guilds
                    .get_mut(&guild_id)
                    .expect("Inserted new guild and still failed to get mutable reference")
            }
        }
    }
}

impl Guild {
    async fn check_guild_queue(
        guild_mutex: &Arc<Mutex<Guild>>,
        guild_id: GuildId,
        ctx: &Context,
    ) -> Result<(), PlayError> {
        let mut guild = guild_mutex.lock().await;
        let songbird = songbird::get(ctx).await.unwrap();

        if guild.now_playing.is_none() {
            if let Some(new) = guild.queue.pop_front() {
                log::warn!("playing {:?}", &new);
                let (call, result) = songbird.join(guild_id, guild.channel_id).await;
                result.map_err(|_| PlayError::Join)?;

                let source = songbird::ffmpeg(&new.url)
                    .await
                    .map_err(|_| PlayError::Ffmpeg)?;
                let handle = call.lock().await.play_source(source);
                handle
                    .add_event(
                        Event::Track(TrackEvent::End),
                        SongEndHandler {
                            ctx: ctx.clone(),
                            guild_id,
                            guild: guild_mutex.clone(),
                        },
                    )
                    .map_err(|e| PlayError::Unknown(Box::new(e)))?;

                guild.handle.replace(handle);
                guild.now_playing.replace(new);
            } else {
                let _ = songbird.remove(guild_id).await;
            }
        }

        Ok(())
    }
}

struct SongEndHandler {
    ctx: Context,
    guild_id: GuildId,
    guild: Arc<Mutex<Guild>>,
}

#[async_trait]
impl songbird::EventHandler for SongEndHandler {
    async fn act(&self, _: &songbird::EventContext<'_>) -> Option<Event> {
        {
            let mut guild = self.guild.lock().await;
            guild.now_playing = None;
        }

        let _ = Guild::check_guild_queue(&self.guild, self.guild_id, &self.ctx).await;
        None
    }
}

#[async_trait]
trait IntoResponse {
    async fn build_response(
        &self,
        ctx: &Context,
        cmd: &ApplicationCommandInteraction,
        pos: usize,
    ) -> Result<(), PlayError>;
}

#[async_trait]
impl IntoResponse for youtube_dl::Playlist {
    async fn build_response(
        &self,
        ctx: &Context,
        cmd: &ApplicationCommandInteraction,
        pos: usize,
    ) -> Result<(), PlayError> {
        todo!()
    }
}

#[async_trait]
impl IntoResponse for youtube_dl::SingleVideo {
    async fn build_response(
        &self,
        ctx: &Context,
        cmd: &ApplicationCommandInteraction,
        pos: usize,
    ) -> Result<(), PlayError> {
        dbg!(&self);
        cmd.create_interaction_response(&ctx.http, |resp| {
            resp.interaction_response_data(|data| {
                data.create_embed(|embed| {
                    embed.author(|author| {
                        if let Some(avatar) = cmd.user.avatar_url() {
                            author.icon_url(avatar);
                        }

                        author.name("Added to queue")
                    });

                    if let Some(thumb) = self.thumbnail.as_ref() {
                        embed.thumbnail(thumb);
                    }

                    embed.title(self.title.as_str());

                    if let Some(url) = self.webpage_url.as_ref() {
                        embed.url(url);
                    }

                    embed.field("Position", pos.to_string(), false)
                })
            })
        })
        .await
        .unwrap();
        Ok(())
    }
}

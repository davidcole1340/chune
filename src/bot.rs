use std::{collections::VecDeque, sync::Arc};

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
            Interaction, InteractionResponseType,
        },
    },
};
use songbird::{tracks::TrackHandle, Event, TrackEvent};
use youtube_dl::{YoutubeDl, YoutubeDlOutput};

use crate::{config::Config, error::PlayError};

pub struct Handler {
    internal: Arc<InternalHandler>,
    config: Arc<Config>,
}

impl Handler {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            internal: Arc::default(),
            config,
        }
    }
}

#[derive(Default)]
pub struct InternalHandler {
    guilds: DashMap<GuildId, Guild>,
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
impl EventHandler for Handler {
    async fn ready(&self, ctx: serenity::client::Context, _: serenity::model::prelude::Ready) {
        log::info!("bot ready");

        if self.config.register {
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

        log::info!("set up commands");
    }

    async fn interaction_create(
        &self,
        ctx: serenity::client::Context,
        interaction: serenity::model::interactions::Interaction,
    ) {
        log::info!("interaction received");

        if let Interaction::ApplicationCommand(cmd) = interaction {
            let _ = cmd
                .create_interaction_response(&ctx.http, |resp| {
                    resp.kind(InteractionResponseType::DeferredChannelMessageWithSource)
                })
                .await;

            let response = match cmd.data.name.as_str() {
                "play" => self.handle_play(ctx.clone(), &cmd).await,
                "skip" => self.handle_skip(ctx.clone(), &cmd).await,
                _ => return,
            };

            if let Err(e) = response {
                match e {
                    PlayError::Unknown(e) => {
                        log::warn!("internal command error: {:?}", e);
                        let _ = cmd
                            .edit_original_interaction_response(&ctx.http, |resp| {
                                resp.content("Something went wrong. Give it another go?")
                            })
                            .await;
                    }
                    e => {
                        log::warn!("user command error: {:?}", &e);
                        let _ = cmd
                            .edit_original_interaction_response(&ctx.http, |resp| {
                                resp.content(e.to_string())
                            })
                            .await;
                    }
                };
            }
        }
    }
}

impl Handler {
    pub async fn handle_play(
        &self,
        ctx: Context,
        cmd: &ApplicationCommandInteraction,
    ) -> Result<(), PlayError> {
        log::info!("received play command");

        let guild_id = cmd.guild_id.ok_or(PlayError::NoGuildId)?;
        let url = cmd
            .data
            .options
            .first()
            .and_then(|opt| opt.value.as_ref())
            .and_then(|val| val.as_str())
            .ok_or(PlayError::NoUrl)?;
        let task_query = format!("ytsearch1:{}", url);

        let channel_id = self
            .get_user_channel(&ctx, guild_id, cmd.user.id)
            .await
            .ok_or(PlayError::NoChannel)?;

        log::info!(
            "running ytdl for url `{}`, guild {} channel {}",
            url,
            guild_id,
            channel_id
        );
        let yt_resp = tokio::spawn(async move {
            YoutubeDl::new(task_query)
                .socket_timeout("15")
                .format("bestaudio")
                .run()
        })
        .await
        .map_err(|e| PlayError::Unknown(Box::new(e)))?
        .map_err(|_| PlayError::Ytdl(url.to_string()))?;
        log::info!("ytdl success");

        {
            let mut guild = self.get_guild(guild_id, channel_id);

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

        self.internal.check_guild_queue(guild_id, &ctx).await?;
        Ok(())
    }

    pub async fn handle_skip(
        &self,
        ctx: Context,
        cmd: &ApplicationCommandInteraction,
    ) -> Result<(), PlayError> {
        log::info!("received skip command");

        let guild_id = cmd.guild_id.ok_or(PlayError::NoGuildId)?;
        let guild = self
            .internal
            .guilds
            .get_mut(&guild_id)
            .ok_or(PlayError::BotNotPlaying)?;

        if let Some(handle) = guild.handle.as_ref() {
            handle.stop().map_err(|e| PlayError::Unknown(Box::new(e)))?;
        }

        let _ = cmd
            .edit_original_interaction_response(&ctx.http, |resp| resp.content("✅"))
            .await;

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

    fn get_guild(&self, guild_id: GuildId, channel_id: ChannelId) -> RefMut<GuildId, Guild> {
        match self.internal.guilds.get_mut(&guild_id) {
            Some(mut guild) => {
                guild.channel_id = channel_id;
                guild
            }
            None => {
                let result = self
                    .internal
                    .guilds
                    .insert(guild_id, Guild::new(channel_id));
                debug_assert!(result.is_none());
                self.internal
                    .guilds
                    .get_mut(&guild_id)
                    .expect("Inserted new guild and still failed to get mutable reference")
            }
        }
    }
}

impl InternalHandler {
    async fn check_guild_queue(
        self: &Arc<Self>,
        guild_id: GuildId,
        ctx: &Context,
    ) -> Result<(), PlayError> {
        log::info!("guild {} checking queue", guild_id);

        let mut guild = self.guilds.get_mut(&guild_id).ok_or(PlayError::NoChannel)?;
        let songbird = songbird::get(ctx).await.unwrap();

        if guild.now_playing.is_none() {
            if let Some(new) = guild.queue.pop_front() {
                log::info!("guild {} playing {:?}", guild_id, &new);

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
                            handler: self.clone(),
                        },
                    )
                    .map_err(|e| PlayError::Unknown(Box::new(e)))?;

                guild.handle.replace(handle);
                guild.now_playing.replace(new);
            } else {
                log::info!("guild {} queue empty, leaving channel", guild_id);

                let _ = songbird.remove(guild_id).await;
                drop(guild);
                self.guilds.remove(&guild_id);
            }
        } else {
            log::info!("guild {} song already playing", guild_id);
        }

        Ok(())
    }
}

struct SongEndHandler {
    ctx: Context,
    guild_id: GuildId,
    handler: Arc<InternalHandler>,
}

#[async_trait]
impl songbird::EventHandler for SongEndHandler {
    async fn act(&self, _: &songbird::EventContext<'_>) -> Option<Event> {
        log::info!("guild {} song finished", self.guild_id);

        {
            let mut guild = self.handler.guilds.get_mut(&self.guild_id)?;
            guild.now_playing = None;
            guild.handle = None;
        }

        let _ = self
            .handler
            .check_guild_queue(self.guild_id, &self.ctx)
            .await;

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
        if let Some(entries) = &self.entries {
            if entries.len() == 1 {
                return entries.first().unwrap().build_response(ctx, cmd, pos).await;
            }
        }

        let _ = cmd
            .edit_original_interaction_response(&ctx.http, |resp| {
                resp.create_embed(|embed| {
                    embed
                        .author(|author| {
                            if let Some(avatar) = cmd.user.avatar_url() {
                                author.icon_url(avatar);
                            }

                            author.name("Added to queue")
                        })
                        .title(self.title.as_deref().or(self.id.as_deref()).unwrap_or("No playlist name"))
                        .field("Position", pos.to_string(), false)
                })
            })
            .await;

        Ok(())
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
        let _ = cmd
            .edit_original_interaction_response(&ctx.http, |resp| {
                resp.create_embed(|embed| {
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
            .await;
        Ok(())
    }
}

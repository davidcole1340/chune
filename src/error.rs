use thiserror::Error;

type DynError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Could not read file `{0}`: {1:?}")]
    InvalidPath(String, std::io::Error),
    #[error("Read invalid toml content from `{0}`: {1:?}")]
    InvalidContent(String, toml::de::Error),
}

#[derive(Debug, Error)]
pub enum PlayError {
    #[error("You must provide a URL to play.")]
    NoUrl,
    #[error("You can only use this command in a guild text channel.")]
    NoGuildId,
    #[error("Failed to retrieve information about `{0}`.")]
    Ytdl(String),
    #[error("Unable to join your voice channel.")]
    Join,
    #[error("Failed to start playing the given URL.")]
    Ffmpeg,
    #[error("Join a voice channel before trying to queue a song.")]
    NoChannel,

    #[error("Unknown play command error: {0:?}")]
    Unknown(DynError),
}

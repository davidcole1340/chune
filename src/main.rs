use std::sync::Arc;

use anyhow::Result;
use bot::Handler;
use config::Config;
use serenity::Client;
use songbird::SerenityInit;

mod bot;
mod config;
mod error;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let config = Arc::new(Config::from_path("config.toml")?);
    let handler = Handler::new(config.clone());

    Client::builder(config.token.clone())
        .event_handler(handler)
        .application_id(config.app_id)
        .register_songbird()
        .await?
        .start()
        .await?;

    Ok(())
}

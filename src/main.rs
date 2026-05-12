mod acp;
mod config;
mod discord;
mod format;
mod http;
mod reactions;

use serenity::prelude::*;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agent_broker=info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let cfg = config::load_config(&config_path)?;

    if cfg.discord.is_none() && !cfg.http.enabled {
        anyhow::bail!("no trigger configured — set [discord] bot_token or [http] enabled = true");
    }

    info!(
        agent_cmd = %cfg.agent.command,
        pool_max = cfg.pool.max_sessions,
        discord = cfg.discord.is_some(),
        http = cfg.http.enabled,
        "config loaded"
    );

    let pool = Arc::new(acp::SessionPool::new(cfg.agent, cfg.pool.max_sessions));
    let ttl_secs = cfg.pool.session_ttl_hours * 3600;

    // Cleanup task
    let cleanup_pool = pool.clone();
    let cleanup_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            cleanup_pool.cleanup_idle(ttl_secs).await;
        }
    });

    // Graceful shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("shutdown signal received");
        let _ = shutdown_tx.send(true);
    });

    // Spawn HTTP trigger if enabled
    let http_handle = if cfg.http.enabled {
        let pool = pool.clone();
        let http_cfg = cfg.http.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = http::run_server(pool, http_cfg).await {
                tracing::error!("http server error: {e}");
            }
        }))
    } else {
        None
    };

    // Start Discord bot or wait for shutdown signal
    if let Some(discord_cfg) = cfg.discord {
        let allowed_channels: HashSet<u64> = discord_cfg
            .allowed_channels
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();

        let handler = discord::Handler {
            pool: pool.clone(),
            allowed_channels,
            reactions_config: cfg.reactions,
        };

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILDS;

        let mut client = Client::builder(&discord_cfg.bot_token, intents)
            .event_handler(handler)
            .await?;

        let shard_manager = client.shard_manager.clone();
        let mut sr = shutdown_rx.clone();
        tokio::spawn(async move {
            sr.changed().await.ok();
            shard_manager.shutdown_all().await;
        });

        info!("starting discord bot");
        client.start().await?;
    } else {
        // HTTP-only mode: block until SIGINT
        let mut sr = shutdown_rx;
        sr.changed().await.ok();
    }

    cleanup_handle.abort();
    if let Some(h) = http_handle {
        h.abort();
    }
    pool.shutdown().await;
    info!("openab shut down");
    Ok(())
}

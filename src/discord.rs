use crate::acp::ContentBlock;
use crate::adapter::{AdapterRouter, ChatAdapter, ChannelRef, MessageRef, SenderContext};
use crate::config::SttConfig;
use crate::media;
use async_trait::async_trait;
use std::sync::LazyLock;
use serenity::builder::{CreateThread, EditMessage};
use serenity::http::Http;
use serenity::model::channel::{AutoArchiveDuration, Message, ReactionType};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId};
use serenity::prelude::*;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, error, info};

// --- DiscordAdapter: implements ChatAdapter for Discord via serenity ---

pub struct DiscordAdapter {
    http: Arc<Http>,
}

impl DiscordAdapter {
    pub fn new(http: Arc<Http>) -> Self {
        Self { http }
    }
}

#[async_trait]
impl ChatAdapter for DiscordAdapter {
    fn platform(&self) -> &'static str {
        "discord"
    }

    fn message_limit(&self) -> usize {
        2000
    }

    async fn send_message(&self, channel: &ChannelRef, content: &str) -> anyhow::Result<MessageRef> {
        let ch_id: u64 = channel.channel_id.parse()?;
        let msg = ChannelId::new(ch_id).say(&self.http, content).await?;
        Ok(MessageRef {
            channel: channel.clone(),
            message_id: msg.id.to_string(),
        })
    }

    async fn edit_message(&self, msg: &MessageRef, content: &str) -> anyhow::Result<()> {
        let ch_id: u64 = msg.channel.channel_id.parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        ChannelId::new(ch_id)
            .edit_message(
                &self.http,
                MessageId::new(msg_id),
                EditMessage::new().content(content),
            )
            .await?;
        Ok(())
    }

    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> anyhow::Result<ChannelRef> {
        let ch_id: u64 = channel.channel_id.parse()?;
        let msg_id: u64 = trigger_msg.message_id.parse()?;
        let thread = ChannelId::new(ch_id)
            .create_thread_from_message(
                &self.http,
                MessageId::new(msg_id),
                CreateThread::new(title).auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await?;
        Ok(ChannelRef {
            platform: "discord".into(),
            channel_id: thread.id.to_string(),
            thread_id: None,
            parent_id: Some(channel.channel_id.clone()),
        })
    }

    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = msg.channel.channel_id.parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .create_reaction(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }

    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = msg.channel.channel_id.parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .delete_reaction_me(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }
}

// --- Handler: serenity EventHandler that delegates to AdapterRouter ---

pub struct Handler {
    pub router: Arc<AdapterRouter>,
    pub allowed_channels: HashSet<u64>,
    pub allowed_users: HashSet<u64>,
    pub stt_config: SttConfig,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let adapter: Arc<dyn ChatAdapter> = Arc::new(DiscordAdapter::new(ctx.http.clone()));
        let bot_id = ctx.cache.current_user().id;

        let channel_id = msg.channel_id.get();
        let in_allowed_channel =
            self.allowed_channels.is_empty() || self.allowed_channels.contains(&channel_id);

        let is_mentioned = msg.mentions_user_id(bot_id)
            || msg.content.contains(&format!("<@{}>", bot_id))
            || msg
                .mention_roles
                .iter()
                .any(|r| msg.content.contains(&format!("<@&{}>", r)));

        let in_thread = if !in_allowed_channel {
            match msg.channel_id.to_channel(&ctx.http).await {
                Ok(serenity::model::channel::Channel::Guild(gc)) => {
                    let result = gc
                        .parent_id
                        .is_some_and(|pid| self.allowed_channels.contains(&pid.get()));
                    tracing::debug!(channel_id = %msg.channel_id, parent_id = ?gc.parent_id, result, "thread check");
                    result
                }
                Ok(other) => {
                    tracing::debug!(channel_id = %msg.channel_id, kind = ?other, "not a guild channel");
                    false
                }
                Err(e) => {
                    tracing::debug!(channel_id = %msg.channel_id, error = %e, "to_channel failed");
                    false
                }
            }
        } else {
            false
        };

        if !in_allowed_channel && !in_thread {
            return;
        }
        if !in_thread && !is_mentioned {
            return;
        }

        if !self.allowed_users.is_empty() && !self.allowed_users.contains(&msg.author.id.get()) {
            tracing::info!(user_id = %msg.author.id, "denied user, ignoring");
            let msg_ref = discord_msg_ref(&msg);
            let _ = adapter.add_reaction(&msg_ref, "🚫").await;
            return;
        }

        let prompt = if is_mentioned {
            strip_mention(&msg.content)
        } else {
            msg.content.trim().to_string()
        };

        // No text and no attachments → skip
        if prompt.is_empty() && msg.attachments.is_empty() {
            return;
        }

        let display_name = msg
            .member
            .as_ref()
            .and_then(|m| m.nick.as_ref())
            .unwrap_or(&msg.author.name);
        let sender = SenderContext {
            schema: "openab.sender.v1".into(),
            sender_id: msg.author.id.to_string(),
            sender_name: msg.author.name.clone(),
            display_name: display_name.to_string(),
            channel: "discord".into(),
            channel_id: msg.channel_id.to_string(),
            is_bot: msg.author.bot,
        };

        // Build extra content blocks from attachments (images, audio)
        let mut extra_blocks = Vec::new();
        for attachment in &msg.attachments {
            let mime = attachment.content_type.as_deref().unwrap_or("");
            if media::is_audio_mime(mime) {
                if self.stt_config.enabled {
                    let mime_clean = mime.split(';').next().unwrap_or(mime).trim();
                    if let Some(transcript) = media::download_and_transcribe(
                        &attachment.url,
                        &attachment.filename,
                        mime_clean,
                        u64::from(attachment.size),
                        &self.stt_config,
                        None, // Discord CDN is public
                    ).await {
                        debug!(filename = %attachment.filename, chars = transcript.len(), "voice transcript injected");
                        // Prepend transcript before the main text block
                        extra_blocks.insert(0, ContentBlock::Text {
                            text: format!("[Voice message transcript]: {transcript}"),
                        });
                    }
                } else {
                    debug!(filename = %attachment.filename, "skipping audio attachment (STT disabled)");
                }
            } else if let Some(block) = media::download_and_encode_image(
                &attachment.url,
                attachment.content_type.as_deref(),
                &attachment.filename,
                u64::from(attachment.size),
                None, // Discord CDN is public
            ).await {
                debug!(url = %attachment.url, filename = %attachment.filename, "adding image attachment");
                extra_blocks.push(block);
            }
        }

        tracing::debug!(
            num_extra_blocks = extra_blocks.len(),
            num_attachments = msg.attachments.len(),
            in_thread,
            "processing"
        );

        let thread_channel = if in_thread {
            ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: None,
            }
        } else {
            match get_or_create_thread(&ctx, &adapter, &msg, &prompt).await {
                Ok(ch) => ch,
                Err(e) => {
                    error!("failed to create thread: {e}");
                    return;
                }
            }
        };

        let trigger_msg = discord_msg_ref(&msg);

        if let Err(e) = self
            .router
            .handle_message(&adapter, &thread_channel, &sender, &prompt, extra_blocks, &trigger_msg)
            .await
        {
            error!("handle_message error: {e}");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "discord bot connected");
    }
}

// --- Discord-specific helpers ---

fn discord_msg_ref(msg: &Message) -> MessageRef {
    MessageRef {
        channel: ChannelRef {
            platform: "discord".into(),
            channel_id: msg.channel_id.get().to_string(),
            thread_id: None,
            parent_id: None,
        },
        message_id: msg.id.to_string(),
    }
}

async fn get_or_create_thread(
    ctx: &Context,
    adapter: &Arc<dyn ChatAdapter>,
    msg: &Message,
    prompt: &str,
) -> anyhow::Result<ChannelRef> {
    let channel = msg.channel_id.to_channel(&ctx.http).await?;
    if let serenity::model::channel::Channel::Guild(ref gc) = channel {
        if gc.thread_metadata.is_some() {
            return Ok(ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: None,
            });
        }
    }

    let thread_name = shorten_thread_name(prompt);
    let parent = ChannelRef {
        platform: "discord".into(),
        channel_id: msg.channel_id.get().to_string(),
        thread_id: None,
        parent_id: None,
    };
    let trigger_ref = discord_msg_ref(msg);
    adapter.create_thread(&parent, &trigger_ref, &thread_name).await
}

static MENTION_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"<@[!&]?\d+>").unwrap()
});

fn strip_mention(content: &str) -> String {
    MENTION_RE.replace_all(content, "").trim().to_string()
}

fn shorten_thread_name(prompt: &str) -> String {
    let re = regex::Regex::new(r"https?://github\.com/([^/]+/[^/]+)/(issues|pull)/(\d+)").unwrap();
    let shortened = re.replace_all(prompt, "$1#$3");
    let name: String = shortened.chars().take(40).collect();
    if name.len() < shortened.len() {
        format!("{name}...")
    } else {
        name
    }
}

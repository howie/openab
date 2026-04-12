use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::error;

use crate::acp::{classify_notification, AcpEvent, ContentBlock, SessionPool};
use crate::config::ReactionsConfig;
use crate::error_display::{format_coded_error, format_user_error};
use crate::format;
use crate::reactions::StatusReactionController;

// --- Platform-agnostic types ---

/// Identifies a channel or thread across platforms.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ChannelRef {
    pub platform: String,
    pub channel_id: String,
    /// Thread within a channel (e.g. Slack thread_ts, Telegram topic_id).
    /// For Discord, threads are separate channels so this is None.
    pub thread_id: Option<String>,
    /// Parent channel if this is a thread-as-channel (Discord).
    pub parent_id: Option<String>,
}

impl ChannelRef {
    pub fn is_thread(&self) -> bool {
        self.thread_id.is_some() || self.parent_id.is_some()
    }
}

/// Identifies a message across platforms.
#[derive(Clone, Debug)]
pub struct MessageRef {
    pub channel: ChannelRef,
    pub message_id: String,
}

/// Sender identity injected into prompts for downstream agent context.
#[derive(Clone, Debug, Serialize)]
pub struct SenderContext {
    pub schema: String,
    pub sender_id: String,
    pub sender_name: String,
    pub display_name: String,
    pub channel: String,
    pub channel_id: String,
    pub is_bot: bool,
}

// --- ChatAdapter trait ---

#[async_trait]
pub trait ChatAdapter: Send + Sync + 'static {
    /// Platform name for logging and session key namespacing.
    fn platform(&self) -> &'static str;

    /// Maximum message length for this platform (e.g. 2000 for Discord, 4000 for Slack).
    fn message_limit(&self) -> usize;

    /// Send a new message, returns a reference to the sent message.
    async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef>;

    /// Edit an existing message in-place.
    async fn edit_message(&self, msg: &MessageRef, content: &str) -> Result<()>;

    /// Create a thread from a trigger message, returns the thread channel ref.
    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> Result<ChannelRef>;

    /// Add a reaction/emoji to a message.
    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;

    /// Remove a reaction/emoji from a message.
    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;
}

// --- AdapterRouter ---

/// Shared logic for routing messages to ACP agents, managing sessions,
/// streaming edits, and controlling reactions. Platform-independent.
pub struct AdapterRouter {
    pool: Arc<SessionPool>,
    reactions_config: ReactionsConfig,
}

impl AdapterRouter {
    pub fn new(pool: Arc<SessionPool>, reactions_config: ReactionsConfig) -> Self {
        Self {
            pool,
            reactions_config,
        }
    }

    pub fn pool(&self) -> &SessionPool {
        &self.pool
    }

    /// Handle an incoming user message. The adapter is responsible for
    /// filtering, resolving the thread, and building the SenderContext.
    /// This method handles sender context injection, session management, and streaming.
    pub async fn handle_message(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        thread_channel: &ChannelRef,
        sender: &SenderContext,
        prompt: &str,
        extra_blocks: Vec<ContentBlock>,
        trigger_msg: &MessageRef,
    ) -> Result<()> {
        tracing::debug!(platform = adapter.platform(), "processing message");

        // Build content blocks: sender context + prompt text, then extra (images, transcripts)
        let sender_json = serde_json::to_string(sender).unwrap();
        let prompt_with_sender = format!(
            "<sender_context>\n{}\n</sender_context>\n\n{}",
            sender_json, prompt
        );

        let mut content_blocks = Vec::with_capacity(1 + extra_blocks.len());
        // Prepend any transcript blocks (they go before the text block)
        for block in &extra_blocks {
            if matches!(block, ContentBlock::Text { .. }) {
                content_blocks.push(block.clone());
            }
        }
        content_blocks.push(ContentBlock::Text {
            text: prompt_with_sender,
        });
        // Append non-text blocks (images)
        for block in extra_blocks {
            if !matches!(block, ContentBlock::Text { .. }) {
                content_blocks.push(block);
            }
        }

        let thinking_msg = adapter.send_message(thread_channel, "...").await?;

        let thread_key = format!(
            "{}:{}",
            adapter.platform(),
            thread_channel
                .thread_id
                .as_deref()
                .unwrap_or(&thread_channel.channel_id)
        );

        if let Err(e) = self.pool.get_or_create(&thread_key).await {
            let msg = format_user_error(&e.to_string());
            let _ = adapter
                .edit_message(&thinking_msg, &format!("⚠️ {msg}"))
                .await;
            error!("pool error: {e}");
            return Err(e);
        }

        let reactions = Arc::new(StatusReactionController::new(
            self.reactions_config.enabled,
            adapter.clone(),
            trigger_msg.clone(),
            self.reactions_config.emojis.clone(),
            self.reactions_config.timing.clone(),
        ));
        reactions.set_queued().await;

        let result = self
            .stream_prompt(
                adapter,
                &thread_key,
                content_blocks,
                thread_channel,
                &thinking_msg,
                reactions.clone(),
            )
            .await;

        match &result {
            Ok(()) => reactions.set_done().await,
            Err(_) => reactions.set_error().await,
        }

        let hold_ms = if result.is_ok() {
            self.reactions_config.timing.done_hold_ms
        } else {
            self.reactions_config.timing.error_hold_ms
        };
        if self.reactions_config.remove_after_reply {
            let reactions = reactions;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                reactions.clear().await;
            });
        }

        if let Err(ref e) = result {
            let _ = adapter
                .edit_message(&thinking_msg, &format!("⚠️ {e}"))
                .await;
        }

        result
    }

    async fn stream_prompt(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        thread_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        thinking_msg: &MessageRef,
        reactions: Arc<StatusReactionController>,
    ) -> Result<()> {
        let adapter = adapter.clone();
        let thread_channel = thread_channel.clone();
        let msg_ref = thinking_msg.clone();
        let message_limit = adapter.message_limit();

        self.pool
            .with_connection(thread_key, |conn| {
                let content_blocks = content_blocks.clone();
                Box::pin(async move {
                    let reset = conn.session_reset;
                    conn.session_reset = false;

                    let (mut rx, _) = conn.session_prompt(content_blocks).await?;
                    reactions.set_thinking().await;

                    let initial = if reset {
                        "⚠️ _Session expired, starting fresh..._\n\n...".to_string()
                    } else {
                        "...".to_string()
                    };
                    let (buf_tx, buf_rx) = watch::channel(initial);

                    let mut text_buf = String::new();
                    let mut tool_lines: Vec<ToolEntry> = Vec::new();

                    if reset {
                        text_buf.push_str("⚠️ _Session expired, starting fresh..._\n\n");
                    }

                    // Spawn edit-streaming task — only edits the single message, never sends new ones.
                    // Long content is truncated during streaming; final multi-message split happens after.
                    let streaming_limit = message_limit.saturating_sub(100);
                    let edit_handle = {
                        let adapter = adapter.clone();
                        let msg_ref = msg_ref.clone();
                        let mut buf_rx = buf_rx.clone();
                        tokio::spawn(async move {
                            let mut last_content = String::new();
                            loop {
                                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                if buf_rx.has_changed().unwrap_or(false) {
                                    let content = buf_rx.borrow_and_update().clone();
                                    if content != last_content {
                                        let display = if content.chars().count() > streaming_limit {
                                            let truncated = format::truncate_chars(&content, streaming_limit);
                                            format!("{truncated}…")
                                        } else {
                                            content.clone()
                                        };
                                        let _ = adapter.edit_message(&msg_ref, &display).await;
                                        last_content = content;
                                    }
                                }
                                if buf_rx.has_changed().is_err() {
                                    break;
                                }
                            }
                        })
                    };

                    // Process ACP notifications
                    let mut got_first_text = false;
                    let mut response_error: Option<String> = None;
                    while let Some(notification) = rx.recv().await {
                        if notification.id.is_some() {
                            if let Some(ref err) = notification.error {
                                response_error = Some(format_coded_error(err.code, &err.message));
                            }
                            break;
                        }

                        if let Some(event) = classify_notification(&notification) {
                            match event {
                                AcpEvent::Text(t) => {
                                    if !got_first_text {
                                        got_first_text = true;
                                    }
                                    text_buf.push_str(&t);
                                    let _ =
                                        buf_tx.send(compose_display(&tool_lines, &text_buf));
                                }
                                AcpEvent::Thinking => {
                                    reactions.set_thinking().await;
                                }
                                AcpEvent::ToolStart { id, title } if !title.is_empty() => {
                                    reactions.set_tool(&title).await;
                                    let title = sanitize_title(&title);
                                    if let Some(slot) = tool_lines.iter_mut().find(|e| e.id == id) {
                                        slot.title = title;
                                        slot.state = ToolState::Running;
                                    } else {
                                        tool_lines.push(ToolEntry {
                                            id,
                                            title,
                                            state: ToolState::Running,
                                        });
                                    }
                                    let _ =
                                        buf_tx.send(compose_display(&tool_lines, &text_buf));
                                }
                                AcpEvent::ToolDone { id, title, status } => {
                                    reactions.set_thinking().await;
                                    let new_state = if status == "completed" {
                                        ToolState::Completed
                                    } else {
                                        ToolState::Failed
                                    };
                                    if let Some(slot) = tool_lines.iter_mut().find(|e| e.id == id) {
                                        if !title.is_empty() {
                                            slot.title = sanitize_title(&title);
                                        }
                                        slot.state = new_state;
                                    } else if !title.is_empty() {
                                        tool_lines.push(ToolEntry {
                                            id,
                                            title: sanitize_title(&title),
                                            state: new_state,
                                        });
                                    }
                                    let _ =
                                        buf_tx.send(compose_display(&tool_lines, &text_buf));
                                }
                                _ => {}
                            }
                        }
                    }

                    conn.prompt_done().await;
                    drop(buf_tx);
                    let _ = edit_handle.await;

                    // Final edit with complete content
                    let final_content = compose_display(&tool_lines, &text_buf);
                    let final_content = if final_content.is_empty() {
                        if let Some(err) = response_error {
                            format!("⚠️ {err}")
                        } else {
                            "_(no response)_".to_string()
                        }
                    } else if let Some(err) = response_error {
                        format!("⚠️ {err}\n\n{final_content}")
                    } else {
                        final_content
                    };

                    let chunks = format::split_message(&final_content, message_limit);
                    let mut current_msg = msg_ref;
                    for (i, chunk) in chunks.iter().enumerate() {
                        if i == 0 {
                            let _ = adapter.edit_message(&current_msg, chunk).await;
                        } else if let Ok(new_msg) =
                            adapter.send_message(&thread_channel, chunk).await
                        {
                            current_msg = new_msg;
                        }
                    }

                    Ok(())
                })
            })
            .await
    }
}

/// Flatten a tool-call title into a single line safe for inline-code spans.
fn sanitize_title(title: &str) -> String {
    title.replace('\r', "").replace('\n', " ; ").replace('`', "'")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolState {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
struct ToolEntry {
    id: String,
    title: String,
    state: ToolState,
}

impl ToolEntry {
    fn render(&self) -> String {
        let icon = match self.state {
            ToolState::Running => "🔧",
            ToolState::Completed => "✅",
            ToolState::Failed => "❌",
        };
        let suffix = if self.state == ToolState::Running { "..." } else { "" };
        format!("{icon} `{}`{}", self.title, suffix)
    }
}

fn compose_display(tool_lines: &[ToolEntry], text: &str) -> String {
    let mut out = String::new();
    if !tool_lines.is_empty() {
        for entry in tool_lines {
            out.push_str(&entry.render());
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(text.trim_end());
    out
}

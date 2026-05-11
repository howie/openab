use crate::adapter::{ChannelRef, ChatAdapter, MessageRef};
use crate::config::{ReactionEmojis, ReactionTiming};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// Fallback content for `ReactionTiming.stall_text_template` when unset.
/// `{elapsed}` is substituted at fire time (e.g. `"11m"`).
const DEFAULT_HEARTBEAT_TEMPLATE: &str = "⏳ still working · elapsed {elapsed}";

const CODING_TOKENS: &[&str] = &["exec", "process", "read", "write", "edit", "bash", "shell"];
const WEB_TOKENS: &[&str] = &[
    "web_search",
    "web_fetch",
    "web-search",
    "web-fetch",
    "browser",
];

fn classify_tool<'a>(name: &str, emojis: &'a ReactionEmojis) -> &'a str {
    let n = name.to_lowercase();
    if WEB_TOKENS.iter().any(|t| n.contains(t)) {
        &emojis.web
    } else if CODING_TOKENS.iter().any(|t| n.contains(t)) {
        &emojis.coding
    } else {
        &emojis.tool
    }
}

struct Inner {
    adapter: Arc<dyn ChatAdapter>,
    message: MessageRef,
    thread_channel: ChannelRef,
    turn_start: Instant,
    emojis: ReactionEmojis,
    timing: ReactionTiming,
    current: String,
    finished: bool,
    debounce_handle: Option<tokio::task::JoinHandle<()>>,
    stall_soft_handle: Option<tokio::task::JoinHandle<()>>,
    stall_hard_handle: Option<tokio::task::JoinHandle<()>>,
    stall_text_handle: Option<tokio::task::JoinHandle<()>>,
}

pub struct StatusReactionController {
    inner: Arc<Mutex<Inner>>,
    enabled: bool,
}

impl StatusReactionController {
    pub fn new(
        enabled: bool,
        adapter: Arc<dyn ChatAdapter>,
        message: MessageRef,
        thread_channel: ChannelRef,
        emojis: ReactionEmojis,
        timing: ReactionTiming,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                adapter,
                message,
                thread_channel,
                turn_start: Instant::now(),
                emojis,
                timing,
                current: String::new(),
                finished: false,
                debounce_handle: None,
                stall_soft_handle: None,
                stall_hard_handle: None,
                stall_text_handle: None,
            })),
            enabled,
        }
    }

    pub async fn set_queued(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.queued.clone() };
        self.apply_immediate(&emoji).await;
    }

    pub async fn set_thinking(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.thinking.clone() };
        self.schedule_debounced(&emoji).await;
    }

    pub async fn set_tool(&self, tool_name: &str) {
        if !self.enabled {
            return;
        }
        let emoji = {
            let inner = self.inner.lock().await;
            classify_tool(tool_name, &inner.emojis).to_string()
        };
        self.schedule_debounced(&emoji).await;
    }

    pub async fn set_done(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.done.clone() };
        self.finish(&emoji).await;
        // Add a random mood face
        let faces = ["😊", "😎", "🫡", "🤓", "😏", "✌️", "💪", "🦾"];
        let face = faces[rand::random::<usize>() % faces.len()];
        let inner = self.inner.lock().await;
        let _ = inner.adapter.add_reaction(&inner.message, face).await;
    }

    pub async fn set_error(&self) {
        if !self.enabled {
            return;
        }
        let emoji = { self.inner.lock().await.emojis.error.clone() };
        self.finish(&emoji).await;
    }

    pub async fn clear(&self) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        cancel_timers(&mut inner);
        let current = inner.current.clone();
        if !current.is_empty() {
            let _ = inner
                .adapter
                .remove_reaction(&inner.message, &current)
                .await;
            inner.current.clear();
        }
    }

    async fn apply_immediate(&self, emoji: &str) {
        let mut inner = self.inner.lock().await;
        if inner.finished || emoji == inner.current {
            return;
        }
        cancel_debounce(&mut inner);
        let old = inner.current.clone();
        inner.current = emoji.to_string();
        let adapter = inner.adapter.clone();
        let msg = inner.message.clone();
        let new = emoji.to_string();
        drop(inner);

        let _ = adapter.add_reaction(&msg, &new).await;
        if !old.is_empty() && old != new {
            let _ = adapter.remove_reaction(&msg, &old).await;
        }
        self.reset_stall_timers().await;
    }

    async fn schedule_debounced(&self, emoji: &str) {
        let mut inner = self.inner.lock().await;
        if inner.finished || emoji == inner.current {
            self.reset_stall_timers_inner(&mut inner);
            return;
        }
        cancel_debounce(&mut inner);

        let emoji = emoji.to_string();
        let ctrl = self.inner.clone();
        let debounce_ms = inner.timing.debounce_ms;
        inner.debounce_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
            let mut inner = ctrl.lock().await;
            if inner.finished {
                return;
            }
            let old = inner.current.clone();
            inner.current = emoji.clone();
            let adapter = inner.adapter.clone();
            let msg = inner.message.clone();
            drop(inner);

            let _ = adapter.add_reaction(&msg, &emoji).await;
            if !old.is_empty() && old != emoji {
                let _ = adapter.remove_reaction(&msg, &old).await;
            }
        }));
        self.reset_stall_timers_inner(&mut inner);
    }

    async fn finish(&self, emoji: &str) {
        let mut inner = self.inner.lock().await;
        if inner.finished {
            return;
        }
        inner.finished = true;
        cancel_timers(&mut inner);

        let old = inner.current.clone();
        inner.current = emoji.to_string();
        let adapter = inner.adapter.clone();
        let msg = inner.message.clone();
        let new = emoji.to_string();
        drop(inner);

        let _ = adapter.add_reaction(&msg, &new).await;
        if !old.is_empty() && old != new {
            let _ = adapter.remove_reaction(&msg, &old).await;
        }
    }

    async fn reset_stall_timers(&self) {
        let mut inner = self.inner.lock().await;
        self.reset_stall_timers_inner(&mut inner);
    }

    fn reset_stall_timers_inner(&self, inner: &mut Inner) {
        if let Some(h) = inner.stall_soft_handle.take() {
            h.abort();
        }
        if let Some(h) = inner.stall_hard_handle.take() {
            h.abort();
        }
        if let Some(h) = inner.stall_text_handle.take() {
            h.abort();
        }

        let soft_ms = inner.timing.stall_soft_ms;
        let hard_ms = inner.timing.stall_hard_ms;
        let text_ms_opt = inner.timing.stall_text_ms;
        let text_repeat = inner.timing.stall_text_repeat;
        let text_template = inner
            .timing
            .stall_text_template
            .clone()
            .unwrap_or_else(|| DEFAULT_HEARTBEAT_TEMPLATE.to_string());
        let ctrl = self.inner.clone();

        inner.stall_soft_handle = Some(tokio::spawn({
            let ctrl = ctrl.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(soft_ms)).await;
                let mut inner = ctrl.lock().await;
                if inner.finished {
                    return;
                }
                let old = inner.current.clone();
                inner.current = "🥱".to_string();
                let adapter = inner.adapter.clone();
                let msg = inner.message.clone();
                drop(inner);
                let _ = adapter.add_reaction(&msg, "🥱").await;
                if !old.is_empty() && old != "🥱" {
                    let _ = adapter.remove_reaction(&msg, &old).await;
                }
            }
        }));

        inner.stall_hard_handle = Some(tokio::spawn({
            let ctrl = ctrl.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(hard_ms)).await;
                let mut inner = ctrl.lock().await;
                if inner.finished {
                    return;
                }
                let old = inner.current.clone();
                inner.current = "😨".to_string();
                let adapter = inner.adapter.clone();
                let msg = inner.message.clone();
                drop(inner);
                let _ = adapter.add_reaction(&msg, "😨").await;
                if !old.is_empty() && old != "😨" {
                    let _ = adapter.remove_reaction(&msg, &old).await;
                }
            }
        }));

        if let Some(text_ms) = text_ms_opt {
            inner.stall_text_handle = Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(text_ms)).await;
                    let (adapter, channel, content) = {
                        let inner = ctrl.lock().await;
                        if inner.finished {
                            return;
                        }
                        let elapsed = format_elapsed(inner.turn_start.elapsed());
                        let rendered = text_template.replace("{elapsed}", &elapsed);
                        (
                            inner.adapter.clone(),
                            inner.thread_channel.clone(),
                            rendered,
                        )
                    };
                    let _ = adapter.send_message(&channel, &content).await;
                    if !text_repeat {
                        return;
                    }
                }
            }));
        }
    }
}

/// Format a `Duration` as a compact human-readable elapsed string used in
/// heartbeat templates. Examples: `"45s"`, `"11m"`, `"1h12m"`.
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn cancel_debounce(inner: &mut Inner) {
    if let Some(h) = inner.debounce_handle.take() {
        h.abort();
    }
}

fn cancel_timers(inner: &mut Inner) {
    if let Some(h) = inner.debounce_handle.take() {
        h.abort();
    }
    if let Some(h) = inner.stall_soft_handle.take() {
        h.abort();
    }
    if let Some(h) = inner.stall_hard_handle.take() {
        h.abort();
    }
    if let Some(h) = inner.stall_text_handle.take() {
        h.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ChannelRef;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// Mock adapter that records `send_message` calls so heartbeat assertions
    /// can verify what was posted into the thread.
    #[derive(Default)]
    struct RecordingAdapter {
        sent: StdMutex<Vec<(ChannelRef, String)>>,
    }

    impl RecordingAdapter {
        fn sent_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
        fn sent_messages(&self) -> Vec<(ChannelRef, String)> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChatAdapter for RecordingAdapter {
        fn platform(&self) -> &'static str {
            "test"
        }
        fn message_limit(&self) -> usize {
            2000
        }
        async fn send_message(
            &self,
            channel: &ChannelRef,
            content: &str,
        ) -> Result<MessageRef> {
            self.sent
                .lock()
                .unwrap()
                .push((channel.clone(), content.to_string()));
            Ok(MessageRef {
                channel: channel.clone(),
                message_id: "sent".into(),
            })
        }
        async fn create_thread(
            &self,
            _: &ChannelRef,
            _: &MessageRef,
            _: &str,
        ) -> Result<ChannelRef> {
            unimplemented!()
        }
        async fn add_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
            Ok(())
        }
        fn use_streaming(&self, _: bool) -> bool {
            false
        }
    }

    fn test_channel() -> ChannelRef {
        ChannelRef {
            platform: "test".into(),
            channel_id: "C_test".into(),
            thread_id: Some("T_test".into()),
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn test_message() -> MessageRef {
        MessageRef {
            channel: test_channel(),
            message_id: "M_trigger".into(),
        }
    }

    /// Build a `ReactionTiming` with soft/hard stall pushed out of reach so
    /// they don't interfere with heartbeat assertions, then layer in the
    /// heartbeat config under test.
    fn timing_for_heartbeat(text_ms: u64, repeat: bool) -> ReactionTiming {
        ReactionTiming {
            stall_soft_ms: 60 * 60 * 1000,
            stall_hard_ms: 60 * 60 * 1000,
            stall_text_ms: Some(text_ms),
            stall_text_repeat: repeat,
            ..ReactionTiming::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_fires_after_stall_text_ms() {
        let adapter = Arc::new(RecordingAdapter::default());
        let ctrl = StatusReactionController::new(
            true,
            adapter.clone(),
            test_message(),
            test_channel(),
            ReactionEmojis::default(),
            timing_for_heartbeat(60_000, false),
        );
        ctrl.set_thinking().await;
        assert_eq!(adapter.sent_count(), 0);

        tokio::time::sleep(Duration::from_millis(60_500)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            adapter.sent_count(),
            1,
            "heartbeat should fire once after the configured window"
        );
        let (channel, content) = adapter.sent_messages().into_iter().next().unwrap();
        assert_eq!(channel.channel_id, "C_test");
        assert_eq!(channel.thread_id.as_deref(), Some("T_test"));
        assert!(
            content.contains("still working"),
            "expected default template substring, got: {content}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_disabled_when_stall_text_ms_is_none() {
        let adapter = Arc::new(RecordingAdapter::default());
        let timing = ReactionTiming {
            stall_soft_ms: 60 * 60 * 1000,
            stall_hard_ms: 60 * 60 * 1000,
            ..ReactionTiming::default()
        };
        assert!(timing.stall_text_ms.is_none(), "regression: default must keep heartbeat off");

        let ctrl = StatusReactionController::new(
            true,
            adapter.clone(),
            test_message(),
            test_channel(),
            ReactionEmojis::default(),
            timing,
        );
        ctrl.set_thinking().await;

        tokio::time::sleep(Duration::from_secs(600)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            adapter.sent_count(),
            0,
            "no heartbeat should fire when stall_text_ms is None"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_resets_on_acp_event() {
        let adapter = Arc::new(RecordingAdapter::default());
        let ctrl = StatusReactionController::new(
            true,
            adapter.clone(),
            test_message(),
            test_channel(),
            ReactionEmojis::default(),
            timing_for_heartbeat(60_000, false),
        );
        ctrl.set_thinking().await;

        tokio::time::sleep(Duration::from_millis(50_000)).await;
        ctrl.set_tool("read").await;
        tokio::time::sleep(Duration::from_millis(50_000)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            adapter.sent_count(),
            0,
            "event before threshold should reset the heartbeat timer"
        );

        tokio::time::sleep(Duration::from_millis(15_000)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            adapter.sent_count(),
            1,
            "heartbeat should fire 60s after the most recent event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_repeats_when_repeat_true() {
        let adapter = Arc::new(RecordingAdapter::default());
        let ctrl = StatusReactionController::new(
            true,
            adapter.clone(),
            test_message(),
            test_channel(),
            ReactionEmojis::default(),
            timing_for_heartbeat(60_000, true),
        );
        ctrl.set_thinking().await;

        tokio::time::sleep(Duration::from_millis(60_000 * 3 + 1_000)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            adapter.sent_count(),
            3,
            "expected 3 heartbeats over 3 windows when repeat=true"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_does_not_repeat_when_repeat_false() {
        let adapter = Arc::new(RecordingAdapter::default());
        let ctrl = StatusReactionController::new(
            true,
            adapter.clone(),
            test_message(),
            test_channel(),
            ReactionEmojis::default(),
            timing_for_heartbeat(60_000, false),
        );
        ctrl.set_thinking().await;

        tokio::time::sleep(Duration::from_millis(60_000 * 5)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            adapter.sent_count(),
            1,
            "should fire exactly once when repeat=false"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_template_substitutes_elapsed() {
        let adapter = Arc::new(RecordingAdapter::default());
        let timing = ReactionTiming {
            stall_text_template: Some("hi · {elapsed}".into()),
            ..timing_for_heartbeat(60_000, false)
        };
        let ctrl = StatusReactionController::new(
            true,
            adapter.clone(),
            test_message(),
            test_channel(),
            ReactionEmojis::default(),
            timing,
        );
        ctrl.set_thinking().await;

        tokio::time::sleep(Duration::from_millis(60_100)).await;
        tokio::task::yield_now().await;

        let messages = adapter.sent_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].1, "hi · 1m",
            "expected substituted elapsed in custom template"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_cancelled_after_set_done() {
        let adapter = Arc::new(RecordingAdapter::default());
        let ctrl = StatusReactionController::new(
            true,
            adapter.clone(),
            test_message(),
            test_channel(),
            ReactionEmojis::default(),
            timing_for_heartbeat(60_000, true),
        );
        ctrl.set_thinking().await;

        tokio::time::sleep(Duration::from_millis(30_000)).await;
        ctrl.set_done().await;

        tokio::time::sleep(Duration::from_millis(60_000 * 10)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            adapter.sent_count(),
            0,
            "no heartbeat should fire after set_done cancels timers"
        );
    }

    #[test]
    fn format_elapsed_seconds_range() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_elapsed_minutes_range() {
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m");
        assert_eq!(format_elapsed(Duration::from_secs(11 * 60)), "11m");
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59m");
    }

    #[test]
    fn format_elapsed_hours_range() {
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h0m");
        assert_eq!(format_elapsed(Duration::from_secs(3600 + 12 * 60)), "1h12m");
        assert_eq!(format_elapsed(Duration::from_secs(2 * 3600 + 30 * 60)), "2h30m");
    }
}

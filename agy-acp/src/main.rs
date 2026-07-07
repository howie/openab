mod adapter;
mod db;
mod protobuf;
mod streaming;
mod types;

use adapter::Adapter;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use types::*;

impl Adapter {
    /// Execute prompt subprocess without holding any adapter lock.
    pub async fn execute_prompt(
        id: Value,
        session_id: &str,
        args: Vec<String>,
        snapshot: Option<HashSet<String>>,
        initial_conv_id: Option<String>,
        initial_step_idx: i64,
        working_dir: String,
        conversations_dir: PathBuf,
        cancelled: Arc<AtomicBool>,
        out_tx: mpsc::UnboundedSender<Option<String>>,
    ) -> PromptOutput {
        // Snapshot agy's existing cli-*.log sizes before spawning so that, if the
        // turn produces no output, we can attribute only bytes written *during*
        // this turn (and any swallowed backend error they record) to it, without
        // picking up a stale error from an earlier turn or a concurrent session.
        // See `detect_swallowed_agy_error`.
        let log_pre_snapshot = snapshot_agy_logs(&conversations_dir);

        let spawn_result = Command::new(Adapter::agy_bin())
            .args(&args)
            .env("PATH", Adapter::augmented_path())
            .current_dir(&working_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let mut child = match spawn_result {
            Ok(child) => child,
            Err(e) => {
                return PromptOutput {
                    response_lines: vec![serde_json::to_string(&JsonRpcResponse {
                        jsonrpc: "2.0", id, result: None,
                        error: Some(json!({"code":-32000,"message":format!("failed to run agy: {e}")})),
                    }).unwrap()],
                    session_update: None,
                };
            }
        };

        let mut stdout_handle = child.stdout.take();
        let stdout_reader = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut stdout) = stdout_handle.take() { let _ = stdout.read_to_end(&mut buf).await; }
            buf
        });

        let mut stderr_handle = child.stderr.take();
        let stderr_reader = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut stderr) = stderr_handle.take() { let _ = stderr.read_to_end(&mut buf).await; }
            buf
        });

        let streaming_state = Arc::new(Mutex::new(StreamingState {
            conversation_id: initial_conv_id,
            base_step_idx: initial_step_idx,
            last_step_idx: initial_step_idx,
            emitted_len: HashMap::new(),
            emitted_tool_steps: HashSet::new(),
            had_updates: false,
        }));

        let stop_polling = Arc::new(AtomicBool::new(false));
        let poll_conversations_dir = conversations_dir.clone();
        let poll_snapshot = snapshot.clone();
        let poll_session_id = session_id.to_string();
        let poll_state = Arc::clone(&streaming_state);
        let poll_stop = Arc::clone(&stop_polling);
        let poll_tx = out_tx.clone();

        let poller = std::thread::spawn(move || {
            while !poll_stop.load(Ordering::SeqCst) {
                let lines = streaming::poll_streaming_delta(
                    &poll_conversations_dir, poll_snapshot.as_ref(), &poll_session_id, &poll_state,
                );
                for line in lines {
                    if poll_tx.send(Some(line)).is_err() { return; }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let _stop_guard = StopGuard(Arc::clone(&stop_polling));

        let mut was_cancelled = false;
        let result = tokio::select! {
            result = child.wait() => result,
            _ = async {
                while !cancelled.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => {
                was_cancelled = true;
                let _ = child.kill().await;
                child.wait().await
            }
        };

        let _ = stdout_reader.await;
        let stderr_bytes = stderr_reader.await.unwrap_or_default();

        stop_polling.store(true, Ordering::SeqCst);
        let _ = poller.join();

        // Final poll
        {
            let lines = streaming::poll_streaming_delta(
                &conversations_dir, snapshot.as_ref(), session_id, &streaming_state,
            );
            for line in lines { let _ = out_tx.send(Some(line)); }
        }

        let (bound_conv_id, new_step_idx, had_updates) = {
            let guard = streaming_state.lock().unwrap();
            (guard.conversation_id.clone(), guard.last_step_idx, guard.had_updates)
        };

        let session_update = Some((bound_conv_id.clone(), new_step_idx));

        let stop_reason = if was_cancelled { "cancelled" }
            else if result.as_ref().map(|s| !s.success()).unwrap_or(false) { "error" }
            else { "end_turn" };

        match result {
            Ok(status) => {
                let stderr_text = String::from_utf8_lossy(&stderr_bytes);
                if !stderr_text.is_empty() { eprintln!("[agy-acp] agy stderr: {}", stderr_text.trim_end()); }
                if !was_cancelled && !status.success() {
                    eprintln!("[agy-acp] WARN: agy exited with status: {}", status);
                    if !had_updates {
                        let msg = if stderr_text.is_empty() { format!("agy exited with status: {}", status) }
                            else { format!("agy failed: {}", stderr_text.trim_end()) };
                        return PromptOutput {
                            response_lines: vec![serde_json::to_string(&JsonRpcResponse {
                                jsonrpc: "2.0", id, result: None, error: Some(json!({"code":-32000,"message":msg})),
                            }).unwrap()],
                            session_update,
                        };
                    }
                } else if !was_cancelled && !had_updates {
                    // agy exited 0 but streamed nothing this turn. `agy --print`
                    // swallows backend failures (e.g. quota 429 / RESOURCE_EXHAUSTED)
                    // with a 0 exit code and empty stdout/stderr, recording the cause
                    // only in its own cli.log. An empty successful turn is therefore
                    // almost always a hidden error; surface it as a JSON-RPC error
                    // (mirroring claude-agent-acp) instead of a blank "(no response)".
                    if let Some(details) = detect_swallowed_agy_error(&conversations_dir, &log_pre_snapshot) {
                        eprintln!("[agy-acp] surfacing swallowed agy error: {}", details);
                        return PromptOutput {
                            response_lines: vec![serde_json::to_string(&JsonRpcResponse {
                                jsonrpc: "2.0", id, result: None, error: Some(json!({"code":-32603,"message":details})),
                            }).unwrap()],
                            session_update,
                        };
                    }
                }
            }
            Err(e) => {
                return PromptOutput {
                    response_lines: vec![serde_json::to_string(&JsonRpcResponse {
                        jsonrpc: "2.0", id, result: None,
                        error: Some(json!({"code":-32000,"message":format!("failed to wait for agy: {e}")})),
                    }).unwrap()],
                    session_update,
                };
            }
        }

        PromptOutput {
            response_lines: vec![serde_json::to_string(&JsonRpcResponse {
                jsonrpc: "2.0", id, result: Some(json!({ "stopReason": stop_reason })), error: None,
            }).unwrap()],
            session_update,
        }
    }
}

/// Snapshot agy's current `cli-*.log` files as `name -> byte length` under
/// `<conversations_dir>/../log`. Recording each file's pre-turn size (not just
/// its name) lets `detect_swallowed_agy_error` scan only the bytes appended
/// *during* this turn, so it never surfaces a stale error from an earlier turn
/// or from a concurrent session that shares this log directory.
fn snapshot_agy_logs(conversations_dir: &std::path::Path) -> HashMap<String, u64> {
    let Some(log_dir) = conversations_dir.parent().map(|p| p.join("log")) else {
        return HashMap::new();
    };
    let Ok(entries) = std::fs::read_dir(&log_dir) else {
        return HashMap::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !name.starts_with("cli-") || !name.ends_with(".log") {
                return None;
            }
            Some((name, e.metadata().ok()?.len()))
        })
        .collect()
}

/// Cap on how many bytes to scan from a single log's appended region. agy logs
/// errors at the tail, so if a turn wrote a very large burst (debug logging) we
/// scan only the newest tail slice, bounding the allocation instead of reading
/// the whole file with `read_to_string`.
const MAX_LOG_SCAN_BYTES: u64 = 256 * 1024;

/// Scan the `cli-*.log` files agy appended to during this turn for a backend
/// error it swallowed. `agy --print` exits 0 with empty stdout/stderr when the
/// model backend fails (e.g. quota 429 / RESOURCE_EXHAUSTED), recording the
/// cause only in its own cli.log. Only bytes written after each file's
/// `pre_snapshot` size are considered, so a stale error from an earlier turn or
/// a concurrent session is never attributed to this turn. Returns a cleaned,
/// human-readable message when a known signature is found.
fn detect_swallowed_agy_error(
    conversations_dir: &std::path::Path,
    pre_snapshot: &HashMap<String, u64>,
) -> Option<String> {
    let log_dir = conversations_dir.parent()?.join("log");
    let Ok(entries) = std::fs::read_dir(&log_dir) else {
        return None;
    };

    // Only logs that grew this turn (new file, or larger than the pre-turn
    // snapshot); `offset` is where this turn's bytes begin.
    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf, u64)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !name.starts_with("cli-") || !name.ends_with(".log") {
                return None;
            }
            let meta = e.metadata().ok()?;
            let offset = pre_snapshot.get(&name).copied().unwrap_or(0);
            if meta.len() <= offset {
                return None; // nothing appended this turn
            }
            Some((meta.modified().ok()?, e.path(), offset))
        })
        .collect();

    candidates.sort_by_key(|(mtime, _, _)| std::cmp::Reverse(*mtime)); // newest first

    candidates
        .iter()
        .take(3)
        .find_map(|(_, path, offset)| read_log_tail(path, *offset).and_then(|c| extract_agy_error_message(&c)))
}

/// Read the bytes a log accumulated after `offset`, capped at the last
/// `MAX_LOG_SCAN_BYTES` so an oversized debug log does not force a huge
/// allocation. Decoded lossily: a byte-range read can split a multi-byte char
/// at the start boundary, but the error anchors we match are pure ASCII.
fn read_log_tail(path: &std::path::Path, offset: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let end = file.metadata().ok()?.len();
    let start = offset.max(end.saturating_sub(MAX_LOG_SCAN_BYTES));
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_LOG_SCAN_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Extract a clean, single-line error message from agy's cli.log content. agy
/// logs errors via glog (`E0707 08:34:23.910604  84 log.go:398] <msg>`) and
/// self-wraps them (`<msg>.: <msg>`); this returns the most specific terminal
/// error, de-wrapped and byte-length-capped on a char boundary.
fn extract_agy_error_message(content: &str) -> Option<String> {
    // Most specific terminal error first.
    const ANCHORS: [&str; 3] = ["agent executor error:", "model unreachable:", "RESOURCE_EXHAUSTED"];
    for anchor in ANCHORS {
        // The last matching line is the terminal failure (retries log the same anchor).
        if let Some(line) = content.lines().rev().find(|l| l.contains(anchor)) {
            let start = line.find(anchor)?;
            let mut msg = line[start..].trim().to_string();
            // Drop glog's self-wrapped duplicate tail ("<msg>.: <msg>").
            if let Some((first, _)) = msg.split_once(".: ") {
                msg = format!("{}.", first);
            }
            if msg.len() > 500 {
                let mut end = 500;
                while !msg.is_char_boundary(end) {
                    end -= 1;
                }
                msg.truncate(end);
            }
            return Some(msg);
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let prefetch = tokio::task::spawn_blocking(Adapter::fetch_available_models);
    let adapter = Arc::new(tokio::sync::Mutex::new(Adapter::new()));

    if let Ok(models) = prefetch.await {
        let mut guard = adapter.lock().await;
        if !models.is_empty() {
            eprintln!("[agy-acp] fetched {} models from `agy models`, updating cache", models.len());
            guard.save_models_cache(&models);
            guard.available_models = Some(models);
        } else if let Some(cached) = guard.load_cached_models() {
            eprintln!("[agy-acp] `agy models` failed, using cached model list ({} models)", cached.len());
            guard.available_models = Some(cached);
        } else {
            eprintln!("[agy-acp] `agy models` failed and no cache found, using hardcoded fallback");
            guard.available_models = Some(Adapter::static_fallback_models());
        }
    }

    let active_cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Option<String>>();

    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) if !l.trim().is_empty() => { if tx.send(l).is_err() { break; } }
                Err(_) => break,
                _ => {}
            }
        }
    });

    let mut stdout = io::stdout();
    let mut stdin_open = true;
    let mut pending_prompts = 0usize;

    loop {
        if !stdin_open && pending_prompts == 0 { break; }

        let line = if stdin_open {
            tokio::select! {
                output = out_rx.recv() => {
                    match output {
                        Some(Some(line)) => { let _ = writeln!(stdout, "{}", line); let _ = stdout.flush(); }
                        Some(None) => pending_prompts = pending_prompts.saturating_sub(1),
                        None => {}
                    }
                    continue;
                }
                input = rx.recv() => {
                    match input {
                        Some(line) => line,
                        None => { stdin_open = false; continue; }
                    }
                }
            }
        } else {
            match out_rx.recv().await {
                Some(Some(line)) => { let _ = writeln!(stdout, "{}", line); let _ = stdout.flush(); }
                Some(None) => pending_prompts = pending_prompts.saturating_sub(1),
                None => break,
            }
            continue;
        };

        while let Ok(output) = out_rx.try_recv() {
            match output {
                Some(line) => { let _ = writeln!(stdout, "{}", line); let _ = stdout.flush(); }
                None => pending_prompts = pending_prompts.saturating_sub(1),
            }
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) { Ok(r) => r, Err(_) => continue };

        let id = match req.id {
            Some(id) => id,
            None => {
                if req.method.as_deref() == Some("session/cancel") {
                    let params = req.params.unwrap_or(json!({}));
                    if let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) {
                        if let Some(cancelled) = active_cancellations.lock().unwrap().get(session_id).cloned() {
                            cancelled.store(true, Ordering::SeqCst);
                        }
                    }
                }
                continue;
            }
        };

        let output = match req.method.as_deref() {
            Some("initialize") => {
                let adapter = Arc::clone(&adapter); let out_tx = out_tx.clone();
                pending_prompts += 1;
                tokio::spawn(async move {
                    let adapter = adapter.lock().await;
                    let _ = out_tx.send(Some(serde_json::to_string(&adapter.handle_initialize(id)).unwrap()));
                    let _ = out_tx.send(None);
                });
                Vec::new()
            }
            Some("session/new") => {
                let adapter = Arc::clone(&adapter); let out_tx = out_tx.clone();
                pending_prompts += 1;
                tokio::spawn(async move {
                    let mut adapter = adapter.lock().await;
                    let _ = out_tx.send(Some(serde_json::to_string(&adapter.handle_session_new(id)).unwrap()));
                    let _ = out_tx.send(None);
                });
                Vec::new()
            }
            Some("session/load") => {
                let params = req.params.unwrap_or(json!({}));
                let adapter = Arc::clone(&adapter); let out_tx = out_tx.clone();
                pending_prompts += 1;
                tokio::spawn(async move {
                    let mut adapter = adapter.lock().await;
                    let _ = out_tx.send(Some(serde_json::to_string(&adapter.handle_session_load(id, &params)).unwrap()));
                    let _ = out_tx.send(None);
                });
                Vec::new()
            }
            Some("session/prompt") => {
                let params = req.params.unwrap_or(json!({}));
                let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let cancelled = Arc::new(AtomicBool::new(false));
                if !session_id.is_empty() {
                    active_cancellations.lock().unwrap().insert(session_id.clone(), Arc::clone(&cancelled));
                }
                let adapter = Arc::clone(&adapter);
                let active_cancellations = Arc::clone(&active_cancellations);
                let out_tx = out_tx.clone();
                pending_prompts += 1;
                tokio::spawn(async move {
                    let (sid, args, snapshot, init_conv, init_idx, wd, cd) = {
                        let mut adapter = adapter.lock().await;
                        let (sid, _prompt, args, snapshot, init_conv, init_idx) = adapter.prepare_prompt_state(&params);
                        let wd = adapter.working_dir.clone();
                        let cd = adapter.conversations_dir.clone();
                        (sid, args, snapshot, init_conv, init_idx, wd, cd)
                    };
                    let output = Adapter::execute_prompt(
                        id, &sid, args, snapshot, init_conv, init_idx, wd, cd, cancelled, out_tx.clone(),
                    ).await;
                    if let Some((bound_conv_id, new_step_idx)) = output.session_update {
                        let mut adapter = adapter.lock().await;
                        if let Some(session) = adapter.sessions.get_mut(&sid) {
                            if session.conversation_id.is_none() { session.conversation_id = bound_conv_id.clone(); }
                            if bound_conv_id.is_some() { session.last_step_idx = new_step_idx; }
                        }
                        if bound_conv_id.is_some() {
                            let model_id = adapter.sessions.get(&sid).and_then(|s| s.model_id.clone());
                            adapter.persist_session(&sid, bound_conv_id.as_deref(), new_step_idx, model_id.as_deref());
                        }
                    }
                    if !session_id.is_empty() { active_cancellations.lock().unwrap().remove(&session_id); }
                    for line in output.response_lines { let _ = out_tx.send(Some(line)); }
                    let _ = out_tx.send(None);
                });
                Vec::new()
            }
            Some("session/setConfigOption") | Some("session/set_config_option") => {
                let params = req.params.unwrap_or(json!({}));
                let adapter = Arc::clone(&adapter); let out_tx = out_tx.clone();
                pending_prompts += 1;
                tokio::spawn(async move {
                    let mut adapter = adapter.lock().await;
                    let _ = out_tx.send(Some(serde_json::to_string(&adapter.handle_session_set_config_option(id, &params)).unwrap()));
                    let _ = out_tx.send(None);
                });
                Vec::new()
            }
            Some("session/cancel") => {
                let params = req.params.unwrap_or(json!({}));
                if let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) {
                    if let Some(cancelled) = active_cancellations.lock().unwrap().get(session_id).cloned() {
                        cancelled.store(true, Ordering::SeqCst);
                    }
                }
                vec![serde_json::to_string(&JsonRpcResponse { jsonrpc: "2.0", id, result: Some(json!({})), error: None }).unwrap()]
            }
            Some(method) => {
                vec![serde_json::to_string(&JsonRpcResponse {
                    jsonrpc: "2.0", id, result: None,
                    error: Some(json!({"code":-32601,"message":format!("method not found: {method}")})),
                }).unwrap()]
            }
            None => continue,
        };

        for line in output { let _ = writeln!(stdout, "{}", line); }
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn test_extract_text_from_step_payload_field20_field1() {
        let mut inner = Vec::new();
        inner.push(0x0A); inner.push(0x05);
        inner.extend_from_slice(b"hello");
        let mut blob = Vec::new();
        blob.push(0x08); blob.push(0x0F);
        blob.push(0xA2); blob.push(0x01);
        blob.push(inner.len() as u8);
        blob.extend_from_slice(&inner);
        assert_eq!(protobuf::extract_text_from_step_payload(&blob), Some("hello".to_string()));
    }

    #[test]
    fn test_extract_text_returns_none_without_field20() {
        let blob = vec![0x08, 0x03];
        assert_eq!(protobuf::extract_text_from_step_payload(&blob), None);
    }

    #[test]
    fn test_read_varint() {
        assert_eq!(protobuf::read_varint(&[0x05]), Some((5, 1)));
        assert_eq!(protobuf::read_varint(&[0xAC, 0x02]), Some((300, 2)));
        assert_eq!(protobuf::read_varint(&[]), None);
    }

    #[test]
    fn test_initialize_advertises_load_session_support() {
        let adapter = Adapter {
            sessions: HashMap::new(), working_dir: "/tmp".to_string(),
            conversations_dir: PathBuf::from("/tmp"), state_file: PathBuf::from("/tmp/sessions.json"),
            available_models: Some(vec![]),
        };
        let response = adapter.handle_initialize(json!(1));
        assert_eq!(response.result.as_ref().and_then(|r| r.get("agentCapabilities"))
            .and_then(|c| c.get("loadSession")).and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn test_is_narration_true() {
        assert!(db::is_narration("I will fetch the latest commits."));
        assert!(db::is_narration("I will fetch the latest commits.\nI will check the diff."));
    }

    #[test]
    fn test_is_narration_false() {
        assert!(!db::is_narration("Here is the result."));
        assert!(!db::is_narration("I will fetch the commits.\nHere is the result."));
        assert!(!db::is_narration(""));
    }

    #[test]
    fn test_filter_narration_drops_leading_narration() {
        std::env::remove_var("OPENAB_SHOW_NARRATION");
        let parts = vec![
            "I will fetch the latest commits.\nI will check the diff.".to_string(),
            "I will read the file.".to_string(),
            "The fix is confirmed! LGTM ✅".to_string(),
        ];
        assert_eq!(db::filter_narration(&parts), "The fix is confirmed! LGTM ✅");
    }

    #[test]
    fn test_filter_narration_single_part_unchanged() {
        let parts = vec!["I will do something.".to_string()];
        assert_eq!(db::filter_narration(&parts), "I will do something.");
    }

    #[test]
    fn test_json_rpc_id_as_string() {
        let req: JsonRpcRequest = serde_json::from_str(r#"{"jsonrpc":"2.0","id":"abc-123","method":"initialize"}"#).unwrap();
        assert_eq!(req.id, Some(json!("abc-123")));
    }

    #[test]
    fn test_json_rpc_id_as_number() {
        let req: JsonRpcRequest = serde_json::from_str(r#"{"jsonrpc":"2.0","id":42,"method":"initialize"}"#).unwrap();
        assert_eq!(req.id, Some(json!(42)));
    }

    const QUOTA_LOG: &str = "\
I0707 08:34:18.847769  84 http_helpers.go:208] URL: .../streamGenerateContent?alt=sse\n\
I0707 08:34:20.615268  84 log.go:398] RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 40h52m50s.: RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 40h52m50s.\n\
E0707 08:34:23.910604  84 log.go:398] agent executor error: model unreachable: RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 40h52m46s.: RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 40h52m46s.\n";

    #[test]
    fn test_extract_agy_error_message_dewraps_quota_error() {
        let msg = extract_agy_error_message(QUOTA_LOG).expect("should detect quota error");
        // Anchors on the most specific terminal error.
        assert!(msg.starts_with("agent executor error:"), "got: {msg}");
        // Human-readable cause is preserved.
        assert!(msg.contains("Individual quota reached"), "got: {msg}");
        assert!(msg.contains("Resets in 40h52m46s"), "got: {msg}");
        // glog's self-wrapped duplicate tail is removed.
        assert!(!msg.contains(".: "), "duplicate tail not stripped: {msg}");
    }

    #[test]
    fn test_extract_agy_error_message_none_for_clean_log() {
        let clean = "I0707 08:34:15.727406  84 printmode.go:225] Print mode: silent auth succeeded\n\
                     I0707 08:34:15.871543  84 server.go:825] Created conversation abc\n";
        assert_eq!(extract_agy_error_message(clean), None);
    }

    #[test]
    fn test_extract_agy_error_message_truncates_on_char_boundary() {
        // Anchor followed by >500 bytes of 2-byte chars so the 500th byte lands
        // mid-char; truncate must snap back to a boundary rather than panic.
        let line = format!(
            "E0707 08:34:23.910604  84 log.go:398] RESOURCE_EXHAUSTED {}",
            "é".repeat(400)
        );
        let msg = extract_agy_error_message(&line).expect("should detect anchored error");
        assert!(msg.starts_with("RESOURCE_EXHAUSTED"), "got: {msg}");
        assert!(msg.len() <= 500, "not capped: {} bytes", msg.len());
        // Result is valid UTF-8 (would have panicked in truncate otherwise).
        assert!(std::str::from_utf8(msg.as_bytes()).is_ok());
    }

    #[test]
    fn test_detect_swallowed_agy_error_reads_new_turn_log() {
        let root = std::env::temp_dir().join(format!("agy-acp-logscan-{}", Uuid::new_v4()));
        let conversations = root.join("conversations");
        let log_dir = root.join("log");
        fs::create_dir_all(&conversations).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(log_dir.join("cli-20260707_083407.log"), QUOTA_LOG).unwrap();

        let empty = HashMap::new();
        let detected = detect_swallowed_agy_error(&conversations, &empty);
        assert!(detected.is_some(), "should detect error in fresh log");
        assert!(detected.unwrap().contains("Individual quota reached"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_detect_swallowed_agy_error_none_when_no_logs() {
        let root = std::env::temp_dir().join(format!("agy-acp-logscan-empty-{}", Uuid::new_v4()));
        let conversations = root.join("conversations");
        fs::create_dir_all(&conversations).unwrap();
        let empty = HashMap::new();
        assert_eq!(detect_swallowed_agy_error(&conversations, &empty), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_detect_swallowed_agy_error_ignores_pre_existing_error() {
        // A log that already contained an error BEFORE this turn, then had only
        // benign lines appended during the turn, must not be surfaced (F1/F2:
        // stale-error / cross-session isolation).
        let root = std::env::temp_dir().join(format!("agy-acp-logscan-stale-{}", Uuid::new_v4()));
        let conversations = root.join("conversations");
        let log_dir = root.join("log");
        fs::create_dir_all(&conversations).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("cli-20260707_083407.log");
        fs::write(&log_path, QUOTA_LOG).unwrap();

        // Snapshot captures the size *after* the pre-existing error.
        let snapshot = snapshot_agy_logs(&conversations);

        // This turn appends only a benign line.
        let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        use std::io::Write as _;
        f.write_all(b"I0707 09:00:00.000000  84 server.go:825] turn ok\n").unwrap();
        drop(f);

        assert_eq!(
            detect_swallowed_agy_error(&conversations, &snapshot),
            None,
            "pre-existing error before the snapshot offset must not be surfaced"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_detect_swallowed_agy_error_reads_only_appended_bytes() {
        // Log started clean; this turn appended the quota error. Snapshot offset
        // skips the clean prefix, and detection surfaces the appended error.
        let root = std::env::temp_dir().join(format!("agy-acp-logscan-append-{}", Uuid::new_v4()));
        let conversations = root.join("conversations");
        let log_dir = root.join("log");
        fs::create_dir_all(&conversations).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("cli-20260707_083407.log");
        fs::write(&log_path, "I0707 08:00:00.000000  84 server.go:825] Created conversation abc\n").unwrap();

        let snapshot = snapshot_agy_logs(&conversations);

        let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        use std::io::Write as _;
        f.write_all(QUOTA_LOG.as_bytes()).unwrap();
        drop(f);

        let detected = detect_swallowed_agy_error(&conversations, &snapshot);
        assert!(detected.is_some(), "should detect error appended this turn");
        assert!(detected.unwrap().contains("Individual quota reached"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore]
    fn test_session_load_restores_persisted_session() {
        let root = std::env::temp_dir().join(format!("agy-acp-load-{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&root);
        let mut adapter = Adapter {
            sessions: HashMap::new(), working_dir: root.to_string_lossy().to_string(),
            conversations_dir: root.join("conversations"), state_file: root.join("sessions.json"),
            available_models: Some(vec![]),
        };
        adapter.persist_session("sess-1", Some("conv-abc"), 5, None);
        let response = adapter.handle_session_load(json!(7), &json!({"sessionId": "sess-1"}));
        assert!(response.error.is_none());
        assert_eq!(adapter.sessions.get("sess-1").and_then(|s| s.conversation_id.as_deref()), Some("conv-abc"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore]
    fn test_session_load_returns_config_options_for_models() {
        let root = std::env::temp_dir().join(format!("agy-acp-load-models-{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&root);
        let selected_model = "Gemini 3.1 Pro (High)";
        let mut adapter = Adapter {
            sessions: HashMap::new(), working_dir: root.to_string_lossy().to_string(),
            conversations_dir: root.join("conversations"), state_file: root.join("sessions.json"),
            available_models: Some(vec![
                "Gemini 3.5 Flash (Low)".to_string(),
                selected_model.to_string(),
            ]),
        };
        adapter.persist_session("sess-1", Some("conv-abc"), 5, Some(selected_model));
        let response = adapter.handle_session_load(json!(7), &json!({"sessionId": "sess-1"}));
        assert!(response.error.is_none());

        let result = response.result.expect("session/load should return a result");
        assert_eq!(result["sessionId"], json!("sess-1"));
        let config_options = result["configOptions"]
            .as_array()
            .expect("session/load should include configOptions");
        assert_eq!(config_options.len(), 1);
        assert_eq!(config_options[0]["id"], json!("model"));
        assert_eq!(config_options[0]["currentValue"], json!(selected_model));

        let options = config_options[0]["options"]
            .as_array()
            .expect("model config option should include options");
        assert_eq!(options.len(), 2);
        assert!(options.iter().any(|option| option["value"] == json!(selected_model)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore]
    fn test_persist_and_restore_session() {
        let root = std::env::temp_dir().join(format!("agy-acp-state-{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&root);
        let adapter = Adapter {
            sessions: HashMap::new(), working_dir: root.to_string_lossy().to_string(),
            conversations_dir: root.join("conversations"), state_file: root.join("sessions.json"),
            available_models: Some(vec![]),
        };
        adapter.persist_session("sess-1", Some("conv-abc"), 7, None);
        assert_eq!(adapter.restore_session("sess-1"), Some(("conv-abc".to_string(), 7, None)));
        assert_eq!(adapter.restore_session("sess-unknown"), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore]
    fn test_read_response_from_db() {
        let root = std::env::temp_dir().join(format!("agy-acp-sqlite-{}", Uuid::new_v4()));
        let conv_dir = root.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();
        let db_path = conv_dir.join("test-conv.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER NOT NULL DEFAULT 0, status INTEGER NOT NULL DEFAULT 0, has_subtrajectory NUMERIC NOT NULL DEFAULT 0, metadata BLOB, error_details BLOB, permissions BLOB, task_details BLOB, render_info BLOB, step_payload BLOB, step_format INTEGER NOT NULL DEFAULT 0)").unwrap();
        let mut inner = Vec::new();
        inner.push(0x0A); inner.push(11); inner.extend_from_slice(b"hello world");
        let mut payload = Vec::new();
        payload.push(0x08); payload.push(0x0F); payload.push(0xA2); payload.push(0x01);
        payload.push(inner.len() as u8); payload.extend_from_slice(&inner);
        conn.execute("INSERT INTO steps (idx, step_type, step_payload) VALUES (?1, 15, ?2)", rusqlite::params![1i64, payload]).unwrap();
        drop(conn);
        let result = db::read_response_from_db(&conv_dir, "test-conv", -1);
        assert_eq!(result, Some(("hello world".to_string(), 1)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore]
    fn test_streaming_poll_emits_delta() {
        let root = std::env::temp_dir().join(format!("agy-acp-stream-{}", Uuid::new_v4()));
        let conv_dir = root.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();
        let db_path = conv_dir.join("stream-conv.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER NOT NULL DEFAULT 0, status INTEGER NOT NULL DEFAULT 0, has_subtrajectory NUMERIC NOT NULL DEFAULT 0, metadata BLOB, error_details BLOB, permissions BLOB, task_details BLOB, render_info BLOB, step_payload BLOB, step_format INTEGER NOT NULL DEFAULT 0)").unwrap();
        fn make_payload(text: &str) -> Vec<u8> {
            let text_bytes = text.as_bytes();
            let mut inner = vec![0x0A];
            let mut len = text_bytes.len();
            loop { if len < 128 { inner.push(len as u8); break; } inner.push((len as u8 & 0x7F) | 0x80); len >>= 7; }
            inner.extend_from_slice(text_bytes);
            let mut outer = vec![0xA2, 0x01];
            let mut ilen = inner.len();
            loop { if ilen < 128 { outer.push(ilen as u8); break; } outer.push((ilen as u8 & 0x7F) | 0x80); ilen >>= 7; }
            outer.extend(inner);
            outer
        }
        conn.execute("INSERT INTO steps (idx, step_type, step_payload) VALUES (?1, 15, ?2)", rusqlite::params![1i64, make_payload("hello")]).unwrap();
        let state = Arc::new(Mutex::new(StreamingState {
            conversation_id: Some("stream-conv".to_string()), base_step_idx: -1, last_step_idx: -1,
            emitted_len: HashMap::new(), emitted_tool_steps: HashSet::new(), had_updates: false,
        }));
        let lines = streaming::poll_streaming_delta(&conv_dir, None, "sess-1", &state);
        assert_eq!(lines.len(), 1);
        let msg: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(msg["params"]["update"]["content"]["text"], "hello");
        let lines = streaming::poll_streaming_delta(&conv_dir, None, "sess-1", &state);
        assert!(lines.is_empty());
        conn.execute("UPDATE steps SET step_payload = ?1 WHERE idx = 1", rusqlite::params![make_payload("hello world")]).unwrap();
        let lines = streaming::poll_streaming_delta(&conv_dir, None, "sess-1", &state);
        assert_eq!(lines.len(), 1);
        let msg: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(msg["params"]["update"]["content"]["text"], " world");
        drop(conn);
        let _ = fs::remove_dir_all(root);
    }

    fn prepare_auth() -> bool {
        if std::env::var("GEMINI_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) { return true; }
        let home = std::env::var("HOME").unwrap_or_default();
        if std::path::Path::new(&format!("{}/.gemini/antigravity-cli/settings.json", home)).exists() { return true; }
        eprintln!("SKIP: No auth found"); false
    }

    #[test]
    #[ignore]
    fn test_e2e_agy_acp_full_round_trip() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        if !prepare_auth() { return; }
        if std::process::Command::new("agy").arg("--help").output().map(|o| !o.status.success()).unwrap_or(true) { return; }
        let binary = std::env::current_dir().unwrap().join("target/release/agy-acp");
        if !binary.exists() { panic!("Run `cargo build --release` first"); }
        let mut child = Command::new(&binary).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut send_recv = |msg: &str| -> String { writeln!(stdin, "{}", msg).unwrap(); stdin.flush().unwrap(); let mut l = String::new(); reader.read_line(&mut l).unwrap(); l };
        let resp = send_recv(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        let init: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(init["result"]["protocolVersion"], 1);
        let resp = send_recv(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#);
        let session: Value = serde_json::from_str(&resp).unwrap();
        let sid = session["result"]["sessionId"].as_str().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{}","prompt":[{{"type":"text","text":"Reply with exactly one word: PONG"}}]}}}}"#, sid).unwrap();
        stdin.flush().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let mut got_notif = false; let mut text = String::new();
        loop {
            if std::time::Instant::now() > deadline { panic!("Timed out"); }
            let mut line = String::new(); reader.read_line(&mut line).unwrap();
            if line.is_empty() { std::thread::sleep(Duration::from_millis(100)); continue; }
            let msg: Value = serde_json::from_str(line.trim()).unwrap();
            if msg.get("method") == Some(&json!("session/update")) { got_notif = true; if let Some(t) = msg["params"]["update"]["content"]["text"].as_str() { text.push_str(t); } }
            if msg.get("id") == Some(&json!(3)) { break; }
        }
        drop(stdin); let _ = child.wait();
        assert!(got_notif); assert!(text.to_lowercase().contains("pong"));
    }
}

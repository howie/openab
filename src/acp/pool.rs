use crate::acp::connection::AcpConnection;
use crate::config::AgentConfig;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tracing::{info, warn};

/// Combined state protected by a single lock to prevent deadlocks.
/// Lock ordering: always acquire `state` before any operation on either map.
///
/// Each connection is wrapped in `Arc<Mutex<_>>` so that the pool-level
/// `RwLock` is only held briefly for HashMap lookups while the per-connection
/// `Mutex` is held during actual use. This allows multiple connections to be
/// used concurrently without blocking each other.
struct PoolState {
    /// Active connections: thread_key → AcpConnection (per-connection lock).
    active: HashMap<String, Arc<Mutex<AcpConnection>>>,
    /// Suspended sessions: thread_key → ACP sessionId.
    /// Saved on eviction so sessions can be resumed via `session/load`.
    suspended: HashMap<String, String>,
}

pub struct SessionPool {
    state: RwLock<PoolState>,
    config: AgentConfig,
    max_sessions: usize,
}

impl SessionPool {
    pub fn new(config: AgentConfig, max_sessions: usize) -> Self {
        Self {
            state: RwLock::new(PoolState {
                active: HashMap::new(),
                suspended: HashMap::new(),
            }),
            config,
            max_sessions,
        }
    }

    pub async fn get_or_create(&self, thread_id: &str) -> Result<()> {
        // Check if alive connection exists (brief read lock + per-connection try_lock)
        {
            let state = self.state.read().await;
            if let Some(conn_arc) = state.active.get(thread_id) {
                if let Ok(conn) = conn_arc.try_lock() {
                    if conn.alive() {
                        return Ok(());
                    }
                } else {
                    // Connection is currently in use — therefore alive
                    return Ok(());
                }
            }
        }

        // Need to create or rebuild
        let mut state = self.state.write().await;

        // Double-check after acquiring write lock
        if let Some(conn_arc) = state.active.get(thread_id) {
            if let Ok(conn) = conn_arc.try_lock() {
                if conn.alive() {
                    return Ok(());
                }
            } else {
                // In use — alive
                return Ok(());
            }
            warn!(thread_id, "stale connection, rebuilding");
            suspend_entry(&mut state, thread_id);
        }

        if state.active.len() >= self.max_sessions {
            // LRU evict: suspend the oldest idle session to make room.
            // Skip connections that are currently locked (in use).
            let oldest = state.active
                .iter()
                .filter_map(|(k, arc)| {
                    arc.try_lock().ok().map(|c| (k.clone(), c.last_active))
                })
                .min_by_key(|(_, ts)| *ts)
                .map(|(k, _)| k);
            if let Some(key) = oldest {
                info!(evicted = %key, "pool full, suspending oldest idle session");
                suspend_entry(&mut state, &key);
            } else {
                return Err(anyhow!("pool exhausted ({} sessions)", self.max_sessions));
            }
        }

        let mut conn = AcpConnection::spawn(
            &self.config.command,
            &self.config.args,
            &self.config.working_dir,
            &self.config.env,
        )
        .await?;

        conn.initialize().await?;

        // Try to resume a suspended session via session/load
        let saved_session_id = state.suspended.remove(thread_id);
        let mut resumed = false;
        if let Some(ref sid) = saved_session_id {
            if conn.supports_load_session {
                match conn.session_load(sid, &self.config.working_dir).await {
                    Ok(()) => {
                        info!(thread_id, session_id = %sid, "session resumed via session/load");
                        resumed = true;
                    }
                    Err(e) => {
                        warn!(thread_id, session_id = %sid, error = %e, "session/load failed, creating new session");
                    }
                }
            }
        }

        if !resumed {
            conn.session_new(&self.config.working_dir).await?;
            if saved_session_id.is_some() {
                conn.session_reset = true;
            }
        }

        state.active.insert(thread_id.to_string(), Arc::new(Mutex::new(conn)));
        Ok(())
    }

    /// Get mutable access to a connection. Caller must have called get_or_create first.
    ///
    /// Only the per-connection `Mutex` is held during `f`; the pool-level
    /// `RwLock` is acquired briefly (read-only) to look up the `Arc` and then
    /// released, so other connections can be used concurrently.
    pub async fn with_connection<F, R>(&self, thread_id: &str, f: F) -> Result<R>
    where
        F: FnOnce(&mut AcpConnection) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<R>> + Send + '_>>,
    {
        let conn_arc = {
            let state = self.state.read().await;
            state.active
                .get(thread_id)
                .cloned()
                .ok_or_else(|| anyhow!("no connection for thread {thread_id}"))?
        };
        let mut conn = conn_arc.lock().await;
        f(&mut conn).await
    }

    pub async fn cleanup_idle(&self, ttl_secs: u64) {
        let cutoff = Instant::now() - std::time::Duration::from_secs(ttl_secs);
        let mut state = self.state.write().await;
        // Only consider connections we can lock (unlocked = idle).
        // In-use connections are by definition not idle.
        let stale: Vec<String> = state.active
            .iter()
            .filter(|(_, arc)| {
                match arc.try_lock() {
                    Ok(c) => c.last_active < cutoff || !c.alive(),
                    Err(_) => false, // in use, skip
                }
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            info!(thread_id = %key, "cleaning up idle session");
            suspend_entry(&mut state, &key);
        }
    }

    pub async fn shutdown(&self) {
        let mut state = self.state.write().await;
        let count = state.active.len();
        state.active.clear(); // Drop impl kills process groups
        info!(count, "pool shutdown complete");
    }
}

/// Suspend a connection: save its sessionId to the suspended map and remove
/// from active. When the last `Arc` reference is dropped the `Drop` impl
/// kills the process group.
fn suspend_entry(state: &mut PoolState, thread_id: &str) {
    if let Some(conn_arc) = state.active.remove(thread_id) {
        // try_lock is safe here: callers only suspend idle (unlocked) connections.
        if let Ok(conn) = conn_arc.try_lock() {
            if let Some(sid) = &conn.acp_session_id {
                info!(thread_id, session_id = %sid, "suspending session");
                state.suspended.insert(thread_id.to_string(), sid.clone());
            }
        }
        // conn_arc dropped here → when Arc refcount reaches 0, Drop kills process group
    }
}

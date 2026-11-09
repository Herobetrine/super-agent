//! Ticket-private MCP sessions. This deliberately does not advertise a shared
//! connection extension: stateful/legacy servers are reused only by one ticket.

use crate::mcp::{self, CANCELLED, Client, ServerConfig};
use anyhow::{Result, anyhow, ensure};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

const IDLE_TTL: Duration = Duration::from_secs(60);
const MAX_CACHED: usize = 64;

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    ticket: i64,
    server: String,
    revision: String,
    fingerprint: String,
}

struct Slot {
    client: Arc<AsyncMutex<Option<Client>>>,
    cancel: CancellationToken,
    touched: Mutex<Instant>,
}

impl Slot {
    fn new() -> Self {
        Self {
            client: Arc::new(AsyncMutex::new(None)),
            cancel: CancellationToken::new(),
            touched: Mutex::new(Instant::now()),
        }
    }
}

#[derive(Default)]
pub(crate) struct Pool {
    slots: Arc<Mutex<HashMap<Key, Arc<Slot>>>>,
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.clear();
    }
}

impl Pool {
    pub fn clear(&self) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        for (_, slot) in slots.drain() {
            slot.cancel.cancel();
        }
    }

    /// Called synchronously after a configuration/revocation transaction.
    /// Active calls select on this token and become uncertain; idle sessions close.
    pub fn revoke(&self, server: &str) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|key, slot| {
            if key.server != server {
                return true;
            }
            slot.cancel.cancel();
            false
        });
    }

    pub async fn acquire(
        &self,
        ticket: i64,
        server: &str,
        revision: &str,
        cwd: &str,
        config: &ServerConfig,
        cancel: &CancellationToken,
    ) -> Result<Lease> {
        ensure!(!cancel.is_cancelled(), CANCELLED);
        let prepared = mcp::prepare(config, cwd)?;
        let key = Key {
            ticket,
            server: server.into(),
            revision: revision.into(),
            fingerprint: prepared.fingerprint.clone(),
        };
        let (slot, fresh, cached) = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            // A new credential/configuration invalidates old sessions of this
            // ticket. Never let a previous credential continue an active call.
            slots.retain(|old, slot| {
                let stale = slot.cancel.is_cancelled()
                    || (old.ticket == ticket && old.server == server && old != &key);
                if stale {
                    slot.cancel.cancel();
                }
                !stale
            });
            if let Some(slot) = slots.get(&key) {
                (slot.clone(), false, true)
            } else {
                let slot = Arc::new(Slot::new());
                // At capacity use a one-operation private connection. Never
                // evict another ticket's in-flight operation to save a slot.
                let cached = slots.len() < MAX_CACHED;
                if cached {
                    slots.insert(key.clone(), slot.clone());
                }
                (slot, true, cached)
            }
        };
        if fresh {
            reap(Arc::downgrade(&self.slots), key, Arc::downgrade(&slot));
        }
        let mut guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(anyhow!(CANCELLED)),
            () = slot.cancel.cancelled() => return Err(anyhow!("MCP 连接授权已撤销")),
            result = tokio::time::timeout(mcp::CALL_TIMEOUT, slot.client.clone().lock_owned()) =>
                result.map_err(|_| anyhow!("等待 MCP 连接超时"))?,
        };
        // Disconnected cached sessions are replaced before issuing a tool call.
        // A call that itself fails is never silently replayed.
        if guard.as_ref().is_some_and(|c| c.is_closed()) {
            guard.take();
        }
        if guard.is_none() {
            *guard = Some(tokio::select! {
                biased;
                () = slot.cancel.cancelled() => return Err(anyhow!("MCP 连接授权已撤销")),
                result = mcp::connect_prepared(config, &prepared, cancel) => result?,
            });
        }
        Ok(Lease {
            guard,
            slot,
            reusable: false,
            cached,
        })
    }
}

/// Holding the mutex serializes calls within one ticket and provides a clear
/// ownership boundary. An aborted/dropped invocation always destroys its client.
pub(crate) struct Lease {
    guard: OwnedMutexGuard<Option<Client>>,
    slot: Arc<Slot>,
    reusable: bool,
    cached: bool,
}

impl Lease {
    pub fn client(&self) -> &Client {
        self.guard.as_ref().expect("connected lease")
    }
    pub fn revoked(&self) -> CancellationToken {
        self.slot.cancel.clone()
    }
    pub fn keep(&mut self) {
        self.reusable = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.reusable || !self.cached || self.slot.cancel.is_cancelled() {
            self.guard.take();
            self.slot.cancel.cancel();
        }
        *self.slot.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
}

fn reap(slots: Weak<Mutex<HashMap<Key, Arc<Slot>>>>, key: Key, slot: Weak<Slot>) {
    tokio::spawn(async move {
        loop {
            let Some(current) = slot.upgrade() else {
                return;
            };
            let token = current.cancel.clone();
            drop(current);
            tokio::select! { () = token.cancelled() => {}, () = tokio::time::sleep(IDLE_TTL) => {} }
            let Some(current) = slot.upgrade() else {
                return;
            };
            let expired = current
                .touched
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .elapsed()
                >= IDLE_TTL;
            let mut client = if token.is_cancelled() {
                current.client.lock().await
            } else {
                let Ok(client) = current.client.try_lock() else {
                    continue;
                };
                client
            };
            if !token.is_cancelled() && !expired {
                continue;
            }
            if let Some(slots) = slots.upgrade() {
                let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
                if slots.get(&key).is_some_and(|s| Arc::ptr_eq(s, &current)) {
                    slots.remove(&key);
                }
            }
            token.cancel();
            if let Some(client) = client.take() {
                mcp::close(client).await;
            }
            return;
        }
    });
}

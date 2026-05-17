//! In-memory routing table for federation (#463).
//!
//! Maps subscription patterns to the set of peers that issued them. The hub
//! consults this table on every locally-published frame to decide where to
//! fan out. The peer keeps a *local* mirror — the patterns it has subscribed
//! itself to — used to know which locally-published topics should be sent
//! upstream.
//!
//! ## Lifecycle
//!
//! - `subscribe(peer, pattern)`: add `(pattern, peer)`.
//! - `unsubscribe(peer, pattern)`: remove `(pattern, peer)`.
//! - `drop_peer(peer)`: remove every pattern → peer mapping. Called on
//!   disconnect; subscriptions are re-issued on reconnect (peer-side
//!   responsibility, not the hub's).
//! - `match_topic(topic)`: return the deduplicated set of peers whose
//!   subscriptions match.
//!
//! Inbox subscriptions live in their own bucket — they don't pattern-match
//! the topic, they match the literal `inbox/<name>` target shape preserved by
//! the existing deskd inbox routing. See [`RoutingTable::subscribe_inbox`].

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use super::glob;

/// Routing table — `pattern → set<peer_name>` plus an inbox bucket.
#[derive(Default)]
pub struct RoutingTable {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Topic-glob subscriptions: pattern → set of peers.
    by_pattern: HashMap<String, HashSet<String>>,
    /// Inbox subscriptions: inbox name → set of peers.
    inboxes: HashMap<String, HashSet<String>>,
}

impl RoutingTable {
    /// Create an empty routing table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `pattern → peer`. Idempotent.
    pub fn subscribe(&self, peer: &str, pattern: &str) {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        inner
            .by_pattern
            .entry(pattern.to_string())
            .or_default()
            .insert(peer.to_string());
    }

    /// Remove `pattern → peer`. Empty entries collapse.
    pub fn unsubscribe(&self, peer: &str, pattern: &str) {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        if let Some(set) = inner.by_pattern.get_mut(pattern) {
            set.remove(peer);
            if set.is_empty() {
                inner.by_pattern.remove(pattern);
            }
        }
    }

    /// Add an inbox-name subscription. Inbox routing preserves the existing
    /// deskd `inbox/<name>` target shape — the hub forwards a frame to this
    /// peer when its target matches `inbox/<name>` exactly or
    /// `inbox/<name>@<peer>`.
    pub fn subscribe_inbox(&self, peer: &str, inbox: &str) {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        inner
            .inboxes
            .entry(inbox.to_string())
            .or_default()
            .insert(peer.to_string());
    }

    /// Remove an inbox subscription.
    pub fn unsubscribe_inbox(&self, peer: &str, inbox: &str) {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        if let Some(set) = inner.inboxes.get_mut(inbox) {
            set.remove(peer);
            if set.is_empty() {
                inner.inboxes.remove(inbox);
            }
        }
    }

    /// Drop all subscriptions for `peer`. Called on disconnect.
    pub fn drop_peer(&self, peer: &str) {
        let mut inner = self.inner.lock().expect("routing mutex poisoned");
        inner.by_pattern.retain(|_, set| {
            set.remove(peer);
            !set.is_empty()
        });
        inner.inboxes.retain(|_, set| {
            set.remove(peer);
            !set.is_empty()
        });
    }

    /// Return the deduplicated set of peers whose pattern subscriptions match
    /// the given topic.
    pub fn match_topic(&self, topic: &str) -> HashSet<String> {
        let inner = self.inner.lock().expect("routing mutex poisoned");
        let mut out = HashSet::new();
        for (pattern, peers) in inner.by_pattern.iter() {
            if glob::matches(pattern, topic) {
                for p in peers {
                    out.insert(p.clone());
                }
            }
        }
        out
    }

    /// Return peers subscribed to a literal inbox target `inbox/<name>` or
    /// `inbox/<name>@<peer>`. Returns the empty set if `target` is not in the
    /// inbox-target shape.
    pub fn match_inbox(&self, target: &str) -> HashSet<String> {
        let name = match target.strip_prefix("inbox/") {
            Some(rest) => rest.split('@').next().unwrap_or(rest),
            None => return HashSet::new(),
        };
        let inner = self.inner.lock().expect("routing mutex poisoned");
        inner.inboxes.get(name).cloned().unwrap_or_default()
    }

    /// Snapshot the current pattern subscriptions. Used by integration tests
    /// and admin endpoints.
    pub fn snapshot_patterns(&self) -> Vec<(String, Vec<String>)> {
        let inner = self.inner.lock().expect("routing mutex poisoned");
        let mut out: Vec<(String, Vec<String>)> = inner
            .by_pattern
            .iter()
            .map(|(p, set)| {
                let mut peers: Vec<String> = set.iter().cloned().collect();
                peers.sort();
                (p.clone(), peers)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Number of distinct (pattern, peer) pairs. Cheap consistency probe.
    pub fn pattern_subscription_count(&self) -> usize {
        let inner = self.inner.lock().expect("routing mutex poisoned");
        inner.by_pattern.values().map(|s| s.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_and_match() {
        let t = RoutingTable::new();
        t.subscribe("mac", "voice.*");
        let peers = t.match_topic("voice.recorded");
        assert!(peers.contains("mac"));
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn match_dedupes_overlapping_patterns() {
        let t = RoutingTable::new();
        t.subscribe("mac", "voice.*");
        t.subscribe("mac", "voice.recorded");
        let peers = t.match_topic("voice.recorded");
        assert_eq!(peers.len(), 1);
        assert!(peers.contains("mac"));
    }

    #[test]
    fn multiple_peers_on_same_pattern() {
        let t = RoutingTable::new();
        t.subscribe("mac", "voice.*");
        t.subscribe("phone", "voice.*");
        let peers = t.match_topic("voice.recorded");
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn unsubscribe_removes_mapping() {
        let t = RoutingTable::new();
        t.subscribe("mac", "voice.*");
        t.unsubscribe("mac", "voice.*");
        assert!(t.match_topic("voice.recorded").is_empty());
        assert_eq!(t.pattern_subscription_count(), 0);
    }

    #[test]
    fn drop_peer_removes_all_subscriptions() {
        let t = RoutingTable::new();
        t.subscribe("mac", "voice.*");
        t.subscribe("mac", "agent.dev.completed");
        t.subscribe("phone", "voice.*");
        t.drop_peer("mac");
        let peers = t.match_topic("voice.recorded");
        assert_eq!(peers.len(), 1);
        assert!(peers.contains("phone"));
        assert!(t.match_topic("agent.dev.completed").is_empty());
    }

    #[test]
    fn inbox_routing() {
        let t = RoutingTable::new();
        t.subscribe_inbox("mac", "personal");
        let peers = t.match_inbox("inbox/personal");
        assert_eq!(peers.len(), 1);
        assert!(peers.contains("mac"));
        // Cross-device addressing `inbox/<name>@<device>`.
        let peers = t.match_inbox("inbox/personal@mac");
        assert!(peers.contains("mac"));
        // Non-inbox target → empty.
        assert!(t.match_inbox("voice.recorded").is_empty());
    }

    #[test]
    fn drop_peer_clears_inboxes_too() {
        let t = RoutingTable::new();
        t.subscribe_inbox("mac", "personal");
        t.drop_peer("mac");
        assert!(t.match_inbox("inbox/personal").is_empty());
    }

    #[test]
    fn snapshot_is_stable_sorted() {
        let t = RoutingTable::new();
        t.subscribe("b", "y.*");
        t.subscribe("a", "x.*");
        t.subscribe("c", "x.*");
        let snap = t.snapshot_patterns();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].0, "x.*");
        assert_eq!(snap[0].1, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(snap[1].0, "y.*");
    }
}

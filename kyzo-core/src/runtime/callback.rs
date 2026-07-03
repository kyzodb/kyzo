/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`runtime/callback.rs`, MPL-2.0):
 *
 * - The registry tuple `(BTreeMap<u32, decl>, BTreeMap<name, ids>)` is a
 *   named struct ([`EventCallbackRegistry`]) — the two halves' coherence
 *   (every id in `by_relation` exists in `by_id`) was a convention across
 *   field-`0`/field-`1` accesses; now it is at least nameable and locally
 *   audited (register/unregister/prune are the only mutators).
 * - Channels are std `mpsc` instead of crossbeam (the kernel dropped that
 *   dependency; `fixed_rule/mod.rs` made the same substitution). The
 *   original's optional bounded capacity is gone: a bounded channel made
 *   `send_callbacks` — which runs on the session thread after commit —
 *   block on a slow consumer. Unbounded + lossy-by-disconnect is the whole
 *   contract now, stated below.
 * - `unregister_callback`'s two `unwrap`s on the directory are gone: a
 *   missing directory entry is treated as already-unregistered (law 5).
 * - The retry law (story #3): the collector is built fresh per commit
 *   attempt and delivered only after a successful commit, so a conflicted
 *   attempt can never leak phantom events. The reset lives at the retry
 *   sites in `runtime/db.rs`; this file's contribution is that
 *   [`CallbackCollector`] is plain data with no channel side effects until
 *   [`Db::send_callbacks`].
 */

//! Post-commit event callbacks: the universe telling its observers what
//! changed.
//!
//! A callback is a channel registered against a relation name. During a
//! mutating query the session *collects* (relation, op, new rows, old
//! rows) into a [`CallbackCollector`] — pure data, no delivery — and only
//! after the transaction commits does [`Db::send_callbacks`] fan the
//! collected events out to the registered channels. Ordering guarantees:
//! events for one commit are delivered after that commit is durable in the
//! process-crash sense, in relation order, in mutation order within a
//! relation.
//!
//! **Lossy by disconnect, documented**: delivery is `send` on an unbounded
//! channel; if the receiver has been dropped the callback is pruned and
//! the event is gone. Callbacks are a notification surface, not a
//! replication log — an observer that must not miss events should read the
//! relation, not trust the channel.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{Receiver, Sender, channel};

use smartstring::{LazyCompact, SmartString};

use crate::fixed_rule::NamedRows;
use crate::runtime::db::Db;
use crate::storage::Storage;

/// Represents the kind of operation that triggered the callback.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CallbackOp {
    /// Triggered by Put operations.
    Put,
    /// Triggered by Rm operations.
    Rm,
}

impl Display for CallbackOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl CallbackOp {
    /// Get the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            CallbackOp::Put => "Put",
            CallbackOp::Rm => "Rm",
        }
    }
}

/// One delivered event: what happened, the new rows, the old rows.
pub type CallbackEvent = (CallbackOp, NamedRows, NamedRows);

/// One registered callback: the relation it watches and the channel it is
/// delivered on.
pub struct CallbackDeclaration {
    pub(crate) dependent: SmartString<LazyCompact>,
    pub(crate) sender: Sender<CallbackEvent>,
}

/// The events one commit attempt collected, keyed by relation, in mutation
/// order. Plain data: building one has no side effects, so the retry loop
/// discards a conflicted attempt's collector wholesale and no observer
/// ever sees an aborted transaction's events.
pub(crate) type CallbackCollector = BTreeMap<SmartString<LazyCompact>, Vec<CallbackEvent>>;

/// The callback registry: every registered callback by id, and the index
/// from relation name to the ids watching it. The two maps agree by
/// construction — `register`, `unregister`, and the disconnect pruning in
/// [`Db::send_callbacks`] are the only mutators, and each maintains both.
#[derive(Default)]
pub(crate) struct EventCallbackRegistry {
    pub(crate) by_id: BTreeMap<u32, CallbackDeclaration>,
    pub(crate) by_relation: BTreeMap<SmartString<LazyCompact>, BTreeSet<u32>>,
}

impl EventCallbackRegistry {
    fn register(&mut self, id: u32, decl: CallbackDeclaration) {
        self.by_relation
            .entry(decl.dependent.clone())
            .or_default()
            .insert(id);
        self.by_id.insert(id, decl);
    }

    fn unregister(&mut self, id: u32) -> bool {
        match self.by_id.remove(&id) {
            None => false,
            Some(decl) => {
                if let Some(ids) = self.by_relation.get_mut(&decl.dependent) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        self.by_relation.remove(&decl.dependent);
                    }
                }
                true
            }
        }
    }
}

impl<S: Storage> Db<S> {
    /// Register a callback channel to receive changes when the requested
    /// relation is successfully committed. The returned id unregisters it.
    ///
    /// (The CozoDB original took an optional bounded capacity; see the
    /// header — delivery is unbounded and lossy-by-disconnect.)
    pub fn register_callback(&self, relation: &str) -> (u32, Receiver<CallbackEvent>) {
        let (sender, receiver) = channel();
        let decl = CallbackDeclaration {
            dependent: SmartString::from(relation),
            sender,
        };
        let mut registry = self
            .event_callbacks
            .write()
            .expect("registry lock poisoned");
        let new_id = self.next_callback_id();
        registry.register(new_id, decl);
        (new_id, receiver)
    }

    /// Unregister a callback; true if it existed.
    pub fn unregister_callback(&self, id: u32) -> bool {
        self.event_callbacks
            .write()
            .expect("registry lock poisoned")
            .unregister(id)
    }

    /// The relations any callback currently watches: mutation collects
    /// old/new rows only for these (snapshotted once per transaction, so a
    /// registration racing a commit either sees all of it or none of it).
    pub(crate) fn current_callback_targets(&self) -> BTreeSet<SmartString<LazyCompact>> {
        self.event_callbacks
            .read()
            .expect("registry lock poisoned")
            .by_relation
            .keys()
            .cloned()
            .collect()
    }

    /// Deliver a committed transaction's collected events. Post-commit
    /// only. A send failing means the receiver is gone; the callback is
    /// pruned and the event dropped (lossy by disconnect — the documented
    /// contract).
    pub(crate) fn send_callbacks(&self, collector: CallbackCollector) {
        let mut to_remove = vec![];
        {
            let registry = self.event_callbacks.read().expect("registry lock poisoned");
            for (relation, events) in collector {
                let Some(ids) = registry.by_relation.get(&relation) else {
                    continue;
                };
                for (op, new, old) in events {
                    for id in ids {
                        if let Some(decl) = registry.by_id.get(id)
                            && decl.sender.send((op, new.clone(), old.clone())).is_err()
                        {
                            to_remove.push(*id);
                        }
                    }
                }
            }
        }
        if !to_remove.is_empty() {
            let mut registry = self
                .event_callbacks
                .write()
                .expect("registry lock poisoned");
            for id in to_remove {
                registry.unregister(id);
            }
        }
    }
}

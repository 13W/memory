//! The ref-counted shard LRU manager (spec 02 §5 L3, 05 §2/§8) — T09-02.
//!
//! `ShardManager` is the L3 primitive named in spec 02 §5's lock table
//! (`shard-manager map | mutex | open-shard LRU map only (handle
//! lookup/insert/evict)`), realizing `LockLevel::L3` (T09-01 shipped the
//! variant, undbacked, naming this task). It sits in front of
//! [`ProjectionStore::open`], bounding how many shards stay open at once
//! (`max_open_shards`) and making every *physical* open — a cold miss or a
//! reopen after eviction — go through [`open_and_validate`], so a corrupt
//! shard self-heals the moment something asks for it again.
//!
//! ## Design
//!
//! - **Ref-counted handles are plain `Arc<dyn ShardHandle>`.** Every
//!   [`ShardHandle`] method except `destroy` takes `&self` and the trait is
//!   already `Send + Sync`, so sharing through an `Arc` is sound. The manager
//!   never calls `destroy` itself — spec 05 §8 is explicit that eviction
//!   "closes cold shards (files remain; only handles are evicted)"; destroying
//!   on-disk state remains [`crate::rebuild::rebuild`]'s job alone (it already
//!   calls `destroy` on its own throwaway handle before recreating a shard).
//!   This is also why the manager can afford to hand out `Arc` clones freely:
//!   `Box<dyn ShardHandle>` cannot be reconstructed from an `Arc` once shared
//!   (`Arc::try_unwrap`/`into_inner` need `Sized`), but since the manager never
//!   needs that owned `Box` back, the one-way `Box` → `Arc` conversion
//!   ([`Arc::from`]) is all it ever needs.
//! - **"In use" is race-free by construction.** The map holds exactly one
//!   `Arc<dyn ShardHandle>` per cached entry; every external caller's copy is
//!   an additional clone of that same `Arc`. "Not in use" = `Arc::strong_count
//!   == 1` (only the map holds it), read while holding the L3 mutex. Any
//!   concurrent `acquire` for the same key must also take L3 to find/clone the
//!   entry, so whichever operation takes the mutex first wins deterministically
//!   — and a caller who already holds a clone has already bumped the count
//!   before they could ever drop it, so a live external reference can never be
//!   observed as a transient `1`.
//! - **Single-flight via [`tokio::sync::OnceCell`].** L3 is held only for the
//!   get-or-insert-empty-entry step (`lookup_or_insert`) — instant, no
//!   `.await` — matching spec 02 §5's read-path rule verbatim: "L3 held only
//!   for the map lookup, released before the query." The actual slow fill
//!   (`open_and_validate` + a follow-up `open`) runs through
//!   `OnceCell::get_or_try_init` *outside* L3; concurrent `acquire`s for the
//!   same worktree converge on the same cell, so exactly one physical open
//!   happens. `get_or_try_init` never caches an `Err` — if the driving fill is
//!   cancelled or fails, the cell stays empty and the next caller (or the next
//!   `acquire`) retries from scratch, which is exactly the self-healing
//!   behavior [`remove`](ShardManager::remove) relies on.
//! - **LRU uses a monotonic logical tick, not wall-clock.** `next_tick` is an
//!   `AtomicU64` bumped on every access; eviction walks the (small,
//!   `max_open_shards`-bounded) map oldest-first, evicting each entry with no
//!   external holder (`Arc::strong_count == 1`) — but *stops* at the first
//!   entry still in use or still filling, rather than skipping past it to
//!   evict a more-recently-used one instead ("never evict a live handle"
//!   defers that eviction; it does not license substituting a different
//!   victim). No fancier O(1) structure is warranted at this size.
//! - **Background rebuild + cancellation.** Each fill runs as its own
//!   `tokio::spawn`ed task, throttled to one at a time store-wide by
//!   `rebuild_semaphore` (spec 05 §8: "one rebuild at a time per store by
//!   default"), tracked in `inflight` by [`tokio::task::AbortHandle`].
//!   [`remove`](ShardManager::remove) aborts it. Abort is cooperative — it
//!   takes effect at the task's next `.await` point — and this is exactly
//!   what makes it safe: `state.sqlite` writes physically run on
//!   `StateWriter`'s dedicated OS thread (T09-01), so a job already enqueued
//!   runs to completion regardless of the awaiting task's cancellation; only
//!   the *next* step of [`crate::rebuild::rebuild`]'s three-transaction
//!   sequence (`mark_dirty` → `begin_rebuild` → `finish_rebuild`, each
//!   independently committed) is ever skipped. That leaves
//!   `worktree_projection_state` exactly where a crash between the same two
//!   transactions would — already proven self-healing by group 07's fault
//!   matrix (F11-class recovery), so no new recovery logic is needed here.
//!
//! ## Scope
//!
//! [`remove`](ShardManager::remove) is a forced, manager-level API distinct
//! from the LRU's own passive eviction — it is *not* wired to the worktree
//! registry's removal lifecycle (that needs a `removed_at` migration that
//! does not exist yet, deferred by D-004 to "group 07/09 shard lifecycle" in
//! general, not this specific task). Also explicitly deferred, "seam in
//! place, not silently closed": the dormant-worktree model migration (spec 05
//! §8 `[FIXED]` — needs the real model-space registry, T11-01); and adopting
//! this manager into [`crate::switch::switch`] (T11-05, group 11) or the
//! reconcile driver (no dedicated task yet in the current plan) — until then,
//! those direct `store.open()` call sites still race with this manager's own
//! cache exactly as they did before this task (the fake backend's documented
//! "two concurrent opens of one directory can clobber each other" hazard is
//! closed only *within* the manager's cache, not store-wide). The search
//! executor *is* adopted, by
//! T09-03: `local_rag_search::SearchEngine::run_locked`
//! (`crates/search/src/pipeline.rs`) calls [`acquire`](ShardManager::acquire)
//! once per search, inside the caller's held `L2.read`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_store::LockLevel;
use local_rag_store::StateDb;
use local_rag_store::lock::checked_scope_sync;

use crate::contract::{ProjectionError, ProjectionStore, ShardHandle, ShardParams};
use crate::rebuild::{RebuildError, open_and_validate};
use crate::switch::VectorSource;

/// Why [`ShardManager::acquire`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum AcquireError {
    /// Validate-on-open or its rebuild failed (spec 05 §6/§7).
    Rebuild(RebuildError),
    /// The manager's own follow-up [`ProjectionStore::open`] — after a
    /// successful validate/rebuild — failed.
    Open(ProjectionError),
    /// The fill was cancelled by [`ShardManager::remove`]; the cache entry no
    /// longer reflects it. Retry with a fresh `acquire`.
    Removed,
    /// The fill task panicked.
    Panicked(String),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::Rebuild(e) => write!(f, "shard acquire: validate/rebuild failed: {e}"),
            AcquireError::Open(e) => write!(f, "shard acquire: open failed: {e}"),
            AcquireError::Removed => {
                write!(f, "shard acquire: cancelled by a concurrent remove()")
            }
            AcquireError::Panicked(why) => write!(f, "shard acquire: fill task panicked: {why}"),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcquireError::Rebuild(e) => Some(e),
            AcquireError::Open(e) => Some(e),
            AcquireError::Removed | AcquireError::Panicked(_) => None,
        }
    }
}

/// One cached slot. `once` is the single-flight cell for the handle itself;
/// `last_used` is the logical LRU tick as of the most recent `acquire`.
struct Entry {
    once: Arc<tokio::sync::OnceCell<Arc<dyn ShardHandle>>>,
    last_used: u64,
}

/// The L3 shard-manager map (spec 02 §5): bounded, ref-counted, LRU-evicted
/// [`ShardHandle`] cache. See the module docs for the full design.
pub struct ShardManager {
    db: Arc<StateDb>,
    store: Arc<dyn ProjectionStore>,
    layout: StoreLayout,
    shard_params: ShardParams,
    vectors: Arc<dyn VectorSource + Send + Sync>,
    uuids: Arc<dyn UuidSource + Send + Sync>,
    max_open_shards: usize,
    entries: Mutex<HashMap<Uuid, Entry>>,
    next_tick: AtomicU64,
    inflight: Mutex<HashMap<Uuid, tokio::task::AbortHandle>>,
    rebuild_semaphore: Arc<tokio::sync::Semaphore>,
}

impl ShardManager {
    /// A fresh, empty manager over `store`, capped at `max_open_shards`
    /// concurrently open shards (clamped to at least 1).
    pub fn new(
        db: Arc<StateDb>,
        store: Arc<dyn ProjectionStore>,
        layout: StoreLayout,
        shard_params: ShardParams,
        vectors: Arc<dyn VectorSource + Send + Sync>,
        uuids: Arc<dyn UuidSource + Send + Sync>,
        max_open_shards: u32,
    ) -> Self {
        Self {
            db,
            store,
            layout,
            shard_params,
            vectors,
            uuids,
            max_open_shards: (max_open_shards as usize).max(1),
            entries: Mutex::new(HashMap::new()),
            next_tick: AtomicU64::new(0),
            inflight: Mutex::new(HashMap::new()),
            rebuild_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// Get-or-open a ref-counted handle for `worktree_id`. A cache hit is
    /// instant; a miss (cold start, prior eviction, or a just-forced
    /// [`remove`](Self::remove)) validates-on-open via [`open_and_validate`]
    /// — self-healing any divergence — before handing back a handle.
    pub async fn acquire(
        &self,
        worktree_id: Uuid,
        now_ms: i64,
    ) -> Result<Arc<dyn ShardHandle>, AcquireError> {
        let once = self.lookup_or_insert(worktree_id);
        let handle = once
            .get_or_try_init(|| self.spawn_fill(worktree_id, now_ms))
            .await?
            .clone();
        self.evict_if_over_capacity();
        Ok(handle)
    }

    /// Forced removal: cancel any in-flight fill for `worktree_id` and drop
    /// the manager's cache entry, ignoring the "in use" deferral that
    /// governs passive LRU eviction — an external `Arc` clone someone else
    /// already holds keeps working, this only stops the map from handing out
    /// *new* clones and stops a fill that hasn't finished yet. See the module
    /// docs for why this is deliberately not wired to the worktree registry's
    /// own removal lifecycle.
    pub fn remove(&self, worktree_id: Uuid) {
        // Abort before dropping the map entry: a fill still holds its
        // `AbortHandle` registration until it observes the abort, so aborting
        // first means a late-arriving concurrent `acquire` either sees no
        // entry (fresh start) or sees the entry but the fill it joins is
        // already doomed to return `Removed` and retry on its own.
        if let Some(abort) = self
            .inflight
            .lock()
            .expect("shard manager inflight mutex poisoned")
            .remove(&worktree_id)
        {
            abort.abort();
        }
        checked_scope_sync(LockLevel::L3, || {
            self.entries
                .lock()
                .expect("shard manager entries mutex poisoned")
                .remove(&worktree_id);
        });
    }

    /// Whether `worktree_id` currently has a resolved (not merely in-flight)
    /// cached handle. Test/observability seam.
    pub fn is_cached(&self, worktree_id: Uuid) -> bool {
        checked_scope_sync(LockLevel::L3, || {
            self.entries
                .lock()
                .expect("shard manager entries mutex poisoned")
                .get(&worktree_id)
                .is_some_and(|e| e.once.initialized())
        })
    }

    /// The number of entries currently in the map (resolved or still
    /// filling). Test/observability seam.
    pub fn open_count(&self) -> usize {
        checked_scope_sync(LockLevel::L3, || {
            self.entries
                .lock()
                .expect("shard manager entries mutex poisoned")
                .len()
        })
    }

    /// Whether a background fill is currently registered for `worktree_id`
    /// (i.e. [`remove`](Self::remove) would find something to abort right
    /// now). Test/observability seam.
    pub fn is_inflight(&self, worktree_id: Uuid) -> bool {
        self.inflight
            .lock()
            .expect("shard manager inflight mutex poisoned")
            .contains_key(&worktree_id)
    }

    /// L3 critical section: get-or-insert `worktree_id`'s entry, bump its
    /// recency tick, and clone out the (possibly still-unfilled) single-flight
    /// cell. Synchronous, no `.await` — released before any I/O.
    fn lookup_or_insert(
        &self,
        worktree_id: Uuid,
    ) -> Arc<tokio::sync::OnceCell<Arc<dyn ShardHandle>>> {
        checked_scope_sync(LockLevel::L3, || {
            let mut entries = self
                .entries
                .lock()
                .expect("shard manager entries mutex poisoned");
            let tick = self.next_tick.fetch_add(1, Ordering::Relaxed);
            let entry = entries.entry(worktree_id).or_insert_with(|| Entry {
                once: Arc::new(tokio::sync::OnceCell::new()),
                last_used: tick,
            });
            entry.last_used = tick;
            entry.once.clone()
        })
    }

    /// L3 critical section: evict least-recently-used entries, oldest first,
    /// until at or under `max_open_shards` — but stop at the first entry that
    /// is still in use (`Arc::strong_count != 1`) or still filling, rather
    /// than skipping past it to evict a more-recently-used victim instead.
    /// "Never evict a live handle" defers the eviction that would remove it;
    /// it does not license substituting a different one.
    fn evict_if_over_capacity(&self) {
        checked_scope_sync(LockLevel::L3, || {
            let mut entries = self
                .entries
                .lock()
                .expect("shard manager entries mutex poisoned");
            if entries.len() <= self.max_open_shards {
                return;
            }
            let mut order: Vec<(Uuid, u64)> =
                entries.iter().map(|(id, e)| (*id, e.last_used)).collect();
            order.sort_by_key(|(_, last_used)| *last_used);
            for (id, _) in order {
                if entries.len() <= self.max_open_shards {
                    break;
                }
                let evictable = entries
                    .get(&id)
                    .and_then(|e| e.once.get())
                    .is_some_and(|handle| Arc::strong_count(handle) == 1);
                if !evictable {
                    break;
                }
                entries.remove(&id);
            }
        })
    }

    /// Spawn the fill as an independent, abortable task, register it in
    /// `inflight` for [`remove`](Self::remove) to find, and forward its
    /// result. Never called directly — only as the closure driving
    /// [`tokio::sync::OnceCell::get_or_try_init`].
    async fn spawn_fill(
        &self,
        worktree_id: Uuid,
        now_ms: i64,
    ) -> Result<Arc<dyn ShardHandle>, AcquireError> {
        let shard_dir = self.layout.projection_shard(&worktree_id.to_string());
        let quarantine_dir = self.layout.quarantine_dir();
        let join = tokio::spawn(fill(
            self.db.clone(),
            self.store.clone(),
            shard_dir,
            quarantine_dir,
            self.shard_params,
            worktree_id,
            self.vectors.clone(),
            self.uuids.clone(),
            self.rebuild_semaphore.clone(),
            now_ms,
        ));

        self.inflight
            .lock()
            .expect("shard manager inflight mutex poisoned")
            .insert(worktree_id, join.abort_handle());

        let result = join.await;

        self.inflight
            .lock()
            .expect("shard manager inflight mutex poisoned")
            .remove(&worktree_id);

        match result {
            Ok(inner) => inner,
            Err(join_err) if join_err.is_cancelled() => Err(AcquireError::Removed),
            Err(join_err) => Err(AcquireError::Panicked(join_err.to_string())),
        }
    }
}

/// The actual physical fill: validate-on-open (repairing via
/// [`crate::rebuild::rebuild`] on any divergence), then the manager's own
/// long-lived [`ProjectionStore::open`]. Runs on its own spawned task so
/// [`ShardManager::remove`] can abort it independently of whichever caller's
/// `acquire` is (or isn't) currently awaiting it. Throttled to one at a time
/// store-wide by `semaphore` (spec 05 §8).
#[allow(clippy::too_many_arguments)]
async fn fill(
    db: Arc<StateDb>,
    store: Arc<dyn ProjectionStore>,
    shard_dir: std::path::PathBuf,
    quarantine_dir: std::path::PathBuf,
    shard_params: ShardParams,
    worktree_id: Uuid,
    vectors: Arc<dyn VectorSource + Send + Sync>,
    uuids: Arc<dyn UuidSource + Send + Sync>,
    semaphore: Arc<tokio::sync::Semaphore>,
    now_ms: i64,
) -> Result<Arc<dyn ShardHandle>, AcquireError> {
    let _permit = semaphore
        .acquire()
        .await
        .expect("rebuild semaphore is never closed");

    open_and_validate(
        &db,
        &*store,
        &shard_dir,
        &quarantine_dir,
        shard_params,
        worktree_id,
        &*vectors,
        &*uuids,
        now_ms,
    )
    .await
    .map_err(AcquireError::Rebuild)?;

    let handle = store
        .open(&shard_dir, shard_params)
        .map_err(AcquireError::Open)?;
    Ok(Arc::from(handle))
}

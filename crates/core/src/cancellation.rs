//! Cancellation tokens. Std-only (no tokio) so core stays testable in plain
//! threads. Parent cancellation cascades to children; a cancel races with
//! `wait` and never loses (wait observes cancellation even if it started
//! before `cancel`). [`CancellationToken::cancelled`] is a wake-driven async
//! wait built on std waker registration — `cancel()` wakes every registered
//! waker, so async waiters surface cancellation immediately without polling
//! (audit round 14: the guarded-line transport used to poll on a timer).
//!
//! # Registration lifetime (no waiter leak, no recursive cancel)
//!
//! Every `wait()`/`attach()` registration is keyed by a process-unique id in
//! a `slab`-style map and is removed by its owner:
//!
//! - a sync wait holds an RAII [`Registration`] that removes the entry when
//!   the wait completes (woken or cancelled) or unwinds;
//! - a cascade registration is removed when the last clone of the child
//!   token drops ([`Inner::drop`] follows its parent back-links), so a
//!   long-lived parent accumulates only *live* registrations, never one
//!   entry per historical child;
//! - async waits already unregister on future drop.
//!
//! Cancellation propagation is an explicit breadth-first walk
//! (`VecDeque<Arc<Inner>>`): child refs are collected under each token's
//! registration lock, the lock is released, and the queue is drained
//! iteratively — a 100k-deep child tree cannot recurse the stack. All
//! cancellation locks recover from poisoning (a panic elsewhere must never
//! turn cancellation into a panic).

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};

/// Process-unique registration ids (one map per token, ids never reused).
static NEXT_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

fn next_registration_id() -> u64 {
    NEXT_REGISTRATION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Lock without ever panicking on an unrelated poison: cancellation
/// infrastructure must stay available when some other thread panicked.
fn lock_ignore_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Inner {
    cancelled: AtomicBool,
    /// Live registrations (sync waits and cascade children) by id. See the
    /// module docs for the removal rules.
    waiters: Mutex<BTreeMap<u64, Arc<Waiter>>>,
    /// Registrations of live [`CancellationToken::cancelled`] futures. One
    /// entry per awaiting future, removed when the future drops; `cancel()`
    /// wakes every entry.
    async_waiters: Mutex<Vec<Waker>>,
    /// Back-links `(parent, registration id)` for every cascade `attach()`
    /// this token is registered under. `Weak` so parent and child never
    /// keep each other alive; [`Inner::drop`] unregisters every link.
    parents: Mutex<Vec<(Weak<Inner>, u64)>>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            waiters: Mutex::new(BTreeMap::new()),
            async_waiters: Mutex::new(Vec::new()),
            parents: Mutex::new(Vec::new()),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last clone of this token is going away: every parent
        // registration it holds is no longer live. Taking the links is
        // panic-free (poison recovery); a parent already gone needs no
        // cleanup.
        let links = std::mem::take(&mut *lock_ignore_poison(&self.parents));
        for (parent, id) in links {
            if let Some(parent) = parent.upgrade() {
                lock_ignore_poison(&parent.waiters).remove(&id);
            }
        }
    }
}

/// What one registration does when its token is cancelled.
enum WaiterKind {
    /// A sync `wait()` registration: notified through the condvar.
    Wait,
    /// A cascade registration: the referenced child token is cancelled.
    /// `Weak` — the parent never keeps a dropped child alive.
    Cascade(Weak<Inner>),
}

/// A registration on a parent token.
struct Waiter {
    cond: Condvar,
    notified: Mutex<bool>,
    kind: WaiterKind,
}

/// RAII handle for one sync-wait registration: removes the map entry when
/// the wait completes or unwinds.
struct Registration {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock_ignore_poison(&self.inner.waiters).remove(&self.id);
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<Inner>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Resolve when this token is cancelled. Wake-driven and
    /// executor-agnostic (std waker registration — core stays
    /// dependency-free): the first poll registers the caller's waker with
    /// the token and `cancel()` wakes every registered waker, so a waiter
    /// surfaces cancellation immediately instead of polling. The cancel /
    /// register race never loses: the flag is checked before AND under the
    /// registration lock. Already-cancelled tokens resolve on the first
    /// poll. A dropped future unregisters itself (see
    /// [`CancelledAwait::drop`]).
    pub fn cancelled(&self) -> CancelledAwait<'_> {
        CancelledAwait {
            token: self,
            registered: None,
        }
    }

    /// Cancel. Returns true if this call performed the cancellation
    /// (first caller wins; subsequent calls return false). Propagates to
    /// every attached child with an explicit worklist — no recursion.
    pub fn cancel(&self) -> bool {
        if self.inner.cancelled.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut queue: VecDeque<Arc<Inner>> = VecDeque::new();
        self.propagate(&mut queue);
        while let Some(inner) = queue.pop_front() {
            // A child already cancelled independently has already
            // propagated through its own subtree.
            if inner.cancelled.swap(true, Ordering::AcqRel) {
                continue;
            }
            propagate_inner(&inner, &mut queue);
        }
        true
    }

    /// Notify this token's live registrations and enqueue cascade children.
    fn propagate(&self, queue: &mut VecDeque<Arc<Inner>>) {
        propagate_inner(&self.inner, queue);
    }

    /// A child token: cancelling the parent cancels the child. Cancelling the
    /// child does not cancel the parent.
    pub fn child(&self) -> CancellationToken {
        let child = CancellationToken::new();
        self.attach(child.clone());
        child
    }

    /// Register a token that must be cancelled when this one is. Used by
    /// `child()` and by structured concurrency to fan cancellation out.
    ///
    /// The registration lives until the last clone of `other` is dropped
    /// (then [`Inner::drop`] removes it) or until this token is cancelled,
    /// whichever comes first — a long-lived parent never accumulates dead
    /// child registrations.
    pub fn attach(&self, other: CancellationToken) {
        let id = next_registration_id();
        let waiter = Arc::new(Waiter {
            cond: Condvar::new(),
            notified: Mutex::new(false),
            kind: WaiterKind::Cascade(Arc::downgrade(&other.inner)),
        });
        // Lock order is child back-links then parent registrations — the
        // same order `Inner::drop` uses, and `other` is held here so its
        // inner cannot be dropping.
        let mut parents = lock_ignore_poison(&other.inner.parents);
        let mut waiters = lock_ignore_poison(&self.inner.waiters);
        if self.inner.cancelled.load(Ordering::Acquire) {
            drop(waiters);
            drop(parents);
            other.cancel();
            return;
        }
        waiters.insert(id, waiter);
        parents.push((Arc::downgrade(&self.inner), id));
        drop(waiters);
        drop(parents);
        // Re-check under the same critical section was already done: a
        // `cancel()` that swapped the flag before we acquired the lock is
        // seen above; one that swaps after we release the lock sees the
        // fully published registration (entry + back-link).
        if self.inner.cancelled.load(Ordering::Acquire) {
            other.cancel();
        }
    }

    /// Block until cancelled (or immediately if already cancelled).
    pub fn wait(&self) {
        if self.inner.cancelled.load(Ordering::Acquire) {
            return;
        }
        let waiter = Arc::new(Waiter {
            cond: Condvar::new(),
            notified: Mutex::new(false),
            kind: WaiterKind::Wait,
        });
        let id = next_registration_id();
        {
            let mut waiters = lock_ignore_poison(&self.inner.waiters);
            if self.inner.cancelled.load(Ordering::Acquire) {
                return;
            }
            waiters.insert(id, waiter.clone());
        }
        // The guard lives exactly as long as this wait: completion, early
        // return, or unwind all remove the registration.
        let _registration = Registration {
            inner: self.inner.clone(),
            id,
        };
        if self.inner.cancelled.load(Ordering::Acquire) {
            return;
        }
        let mut notified = lock_ignore_poison(&waiter.notified);
        while !*notified && !self.inner.cancelled.load(Ordering::Acquire) {
            notified = waiter
                .cond
                .wait(notified)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Notify `inner`'s sync waiters (condvar), enqueue its live cascade
/// children, and wake its async waiters. Locks are released before any
/// child is touched or waker woken (a wake may re-poll inline).
fn propagate_inner(inner: &Arc<Inner>, queue: &mut VecDeque<Arc<Inner>>) {
    {
        let waiters = lock_ignore_poison(&inner.waiters);
        for waiter in waiters.values() {
            {
                let mut notified = lock_ignore_poison(&waiter.notified);
                *notified = true;
                waiter.cond.notify_all();
            }
            if let WaiterKind::Cascade(child) = &waiter.kind {
                if let Some(child) = child.upgrade() {
                    queue.push_back(child);
                }
            }
        }
    }
    // Async waiters: drain under the lock, wake outside it — a wake may
    // re-poll the waiting task inline, and the poll would take the same
    // lock (std Mutex is not reentrant).
    let to_wake = {
        let mut async_waiters = lock_ignore_poison(&inner.async_waiters);
        std::mem::take(&mut *async_waiters)
    };
    for waker in to_wake {
        waker.wake();
    }
}

/// Future returned by [`CancellationToken::cancelled`]. Registers the task
/// waker with the token on the first poll and holds it so the registration
/// can be released when this future is dropped.
pub struct CancelledAwait<'a> {
    token: &'a CancellationToken,
    /// The waker this future registered (cleared on completion so drop
    /// never removes someone else's entry after `cancel()` drained the list).
    registered: Option<Waker>,
}

impl Future for CancelledAwait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.token.is_cancelled() {
            this.registered = None;
            return Poll::Ready(());
        }
        let mut async_waiters = lock_ignore_poison(&this.token.inner.async_waiters);
        // Re-check under the lock: cancel() cannot interleave between this
        // check and the registration below, so no wake can be missed.
        if this.token.is_cancelled() {
            this.registered = None;
            return Poll::Ready(());
        }
        match this.registered.as_ref() {
            // Still registered with the executor's current waker: nothing to
            // do (a spurious re-poll must not duplicate the registration).
            Some(existing) if existing.will_wake(cx.waker()) => {}
            Some(existing) => {
                // The executor switched wakers: replace our slot.
                if let Some(pos) = async_waiters.iter().position(|w| w.will_wake(existing)) {
                    async_waiters.remove(pos);
                }
                async_waiters.push(cx.waker().clone());
                this.registered = Some(cx.waker().clone());
            }
            None => {
                async_waiters.push(cx.waker().clone());
                this.registered = Some(cx.waker().clone());
            }
        }
        Poll::Pending
    }
}

impl Drop for CancelledAwait<'_> {
    fn drop(&mut self) {
        let Some(registered) = self.registered.take() else {
            return;
        };
        let mut async_waiters = lock_ignore_poison(&self.token.inner.async_waiters);
        // Entries are per-future clones of the same waker; removing any one
        // matching entry is safe — at most one registration per live future
        // exists and wake semantics only need one survivor per task.
        if let Some(pos) = async_waiters.iter().position(|w| w.will_wake(&registered)) {
            async_waiters.remove(pos);
        }
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    // ------------------------------------------------------------ async wait

    /// The safe wake channel: `std::task::Wake` over the mpsc sender — no
    /// hand-rolled `RawWaker` vtable, so this test executor contains zero
    /// unsafe code (the workspace denies `unsafe_code` outside the platform
    /// authority crates).
    struct ChannelWake(mpsc::Sender<()>);

    impl std::task::Wake for ChannelWake {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    /// Minimal wake-driven executor (no tokio in core): polls the future;
    /// a wake sends `()` on the channel and the executor re-polls. Proves
    /// the async wait is woken by `cancel()` from another thread.
    fn thread_waker(tx: mpsc::Sender<()>) -> Waker {
        use std::task::Waker;
        Waker::from(Arc::new(ChannelWake(tx)))
    }

    /// Block until `fut` resolves or 5s pass without a wake (returns the
    /// outcome). Requires at least one wake per re-poll.
    fn block_on_wake_driven(fut: CancelledAwait<'_>) -> bool {
        use std::task::{Context, Poll};
        let mut fut = Box::pin(fut);
        let (tx, rx) = mpsc::channel::<()>();
        let waker = thread_waker(tx);
        let mut cx = Context::from_waker(&waker);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(()) => return true,
                Poll::Pending => match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(()) => continue,
                    Err(_) => return false,
                },
            }
        }
    }

    /// Poll once; true when the future resolved immediately.
    fn poll_once(fut: &mut Pin<Box<CancelledAwait<'_>>>) -> bool {
        let (tx, _rx) = mpsc::channel::<()>();
        let waker = thread_waker(tx);
        let mut cx = std::task::Context::from_waker(&waker);
        fut.as_mut().poll(&mut cx).is_ready()
    }

    #[test]
    fn cancelled_async_resolves_immediately_when_already_cancelled() {
        let t = CancellationToken::new();
        let mut fut = Box::pin(t.cancelled());
        assert!(!poll_once(&mut fut), "uncancelled token must park");
        drop(fut);
        t.cancel();
        let mut fut = Box::pin(t.cancelled());
        assert!(poll_once(&mut fut));
    }

    #[test]
    fn cancel_wakes_registered_async_waiter() {
        let t = Arc::new(CancellationToken::new());
        let t2 = t.clone();
        let h = thread::spawn(move || block_on_wake_driven(t2.cancelled()));
        thread::sleep(Duration::from_millis(30));
        assert!(
            !h.is_finished(),
            "waiter must still be parked before cancel"
        );
        t.cancel();
        assert!(h.join().unwrap(), "cancel must wake the registered waiter");
    }

    #[test]
    fn many_async_waiters_all_wake() {
        let t = Arc::new(CancellationToken::new());
        let mut handles = vec![];
        for _ in 0..16 {
            let t = t.clone();
            handles.push(thread::spawn(move || block_on_wake_driven(t.cancelled())));
        }
        thread::sleep(Duration::from_millis(20));
        t.cancel();
        for h in handles {
            assert!(h.join().unwrap());
        }
    }

    #[test]
    fn dropped_async_waiter_unregisters() {
        let t = CancellationToken::new();
        let fut = t.cancelled();
        let mut fut = Box::pin(fut);
        {
            let (tx, _rx) = mpsc::channel::<()>();
            let waker = thread_waker(tx);
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(fut.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(t.inner.async_waiters.lock().unwrap().len(), 1);
        drop(fut); // no cancel ever: the registration must be released
        assert!(
            t.inner.async_waiters.lock().unwrap().is_empty(),
            "a dropped waiter must not leave a stale registration behind"
        );
        // Re-polling registers again and cancel still wakes it.
        let t2 = t.clone();
        let h = thread::spawn(move || block_on_wake_driven(t2.cancelled()));
        thread::sleep(Duration::from_millis(10));
        t.cancel();
        assert!(h.join().unwrap());
    }

    #[test]
    fn parent_cancel_wakes_child_async_waiter() {
        let parent = CancellationToken::new();
        let child = parent.child();
        let h = thread::spawn(move || block_on_wake_driven(child.cancelled()));
        thread::sleep(Duration::from_millis(20));
        parent.cancel();
        assert!(
            h.join().unwrap(),
            "cancelling the parent must cascade to the child's async waiters"
        );
    }

    #[test]
    fn async_cancel_race_never_loses() {
        // Hammer: async waiters race a cancel from another thread. Every
        // waiter must complete.
        let t = Arc::new(CancellationToken::new());
        let mut handles = vec![];
        for _ in 0..8 {
            let t = t.clone();
            handles.push(thread::spawn(move || {
                thread::sleep(Duration::from_micros(50));
                block_on_wake_driven(t.cancelled())
            }));
        }
        thread::sleep(Duration::from_millis(5));
        t.cancel();
        for h in handles {
            assert!(h.join().unwrap(), "a racing waiter must never hang");
        }
    }

    // -------------------------------------------------------------- sync API

    #[test]
    fn cancel_before_wait_returns_immediately() {
        let t = CancellationToken::new();
        assert!(!t.is_cancelled());
        assert!(t.cancel());
        assert!(!t.cancel(), "second cancel loses");
        assert!(t.is_cancelled());
        t.wait(); // must return instantly
    }

    #[test]
    fn cancel_wakes_blocked_waiters() {
        let t = Arc::new(CancellationToken::new());
        let t2 = t.clone();
        let h = thread::spawn(move || {
            t2.wait();
            true
        });
        thread::sleep(Duration::from_millis(30));
        t.cancel();
        assert!(h.join().unwrap());
    }

    #[test]
    fn many_waiters_all_wake() {
        let t = Arc::new(CancellationToken::new());
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..32 {
            let t = t.clone();
            let woke = woke.clone();
            handles.push(thread::spawn(move || {
                t.wait();
                woke.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        thread::sleep(Duration::from_millis(20));
        assert!(t.cancel(), "the first cancel wins");
        for h in handles {
            h.join().unwrap();
        }
        assert!(t.is_cancelled());
        assert_eq!(
            woke.load(std::sync::atomic::Ordering::SeqCst),
            32,
            "every blocked waiter must observe the cancellation"
        );
    }

    #[test]
    fn child_cancel_does_not_cancel_parent() {
        let parent = CancellationToken::new();
        let child = parent.child();
        assert!(child.cancel());
        assert!(!parent.is_cancelled());
        assert!(child.is_cancelled());
    }

    #[test]
    fn parent_cancel_cascades_to_children() {
        let parent = CancellationToken::new();
        let c1 = parent.child();
        let c2 = parent.child();
        let c3 = c1.child(); // grandchild
        parent.cancel();
        assert!(c1.is_cancelled());
        assert!(c2.is_cancelled());
        assert!(c3.is_cancelled());
    }

    #[test]
    fn attach_before_cancel_propagates() {
        let a = CancellationToken::new();
        let b = CancellationToken::new();
        a.attach(b.clone());
        a.cancel();
        assert!(b.is_cancelled());
    }

    #[test]
    fn attach_after_cancel_immediately_cancels() {
        let a = CancellationToken::new();
        a.cancel();
        let b = CancellationToken::new();
        a.attach(b.clone());
        assert!(b.is_cancelled(), "late attach must not leak a live token");
    }

    #[test]
    fn cancel_while_attaching_is_race_safe() {
        // Hammer: spawn threads that attach while another cancels; afterwards
        // every attached token must be cancelled.
        let parent = Arc::new(CancellationToken::new());
        let children: Vec<Arc<CancellationToken>> = (0..64)
            .map(|_| Arc::new(CancellationToken::new()))
            .collect();
        let mut handles = vec![];
        for (i, c) in children.iter().enumerate() {
            let parent = parent.clone();
            let c = c.clone();
            handles.push(thread::spawn(move || {
                if i % 2 == 0 {
                    thread::sleep(Duration::from_micros(5));
                }
                parent.attach((*c).clone());
            }));
        }
        thread::sleep(Duration::from_millis(3));
        parent.cancel();
        for h in handles {
            h.join().unwrap();
        }
        for c in &children {
            assert!(c.is_cancelled());
        }
    }

    #[test]
    fn wait_never_hangs_after_cancel_under_stress() {
        let parent = Arc::new(CancellationToken::new());
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut waiters = vec![];
        for _ in 0..16 {
            let p = parent.clone();
            let woke = woke.clone();
            waiters.push(thread::spawn(move || {
                p.wait();
                woke.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        parent.cancel();
        for w in waiters {
            w.join().unwrap();
        }
        assert!(parent.is_cancelled());
        assert_eq!(
            woke.load(std::sync::atomic::Ordering::SeqCst),
            16,
            "no waiter may hang or be skipped"
        );
    }

    // ------------------------------------------- registration lifetime limits

    fn waiter_count(token: &CancellationToken) -> usize {
        lock_ignore_poison(&token.inner.waiters).len()
    }

    fn await_waiter_count(token: &CancellationToken, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while waiter_count(token) != expected {
            assert!(
                Instant::now() < deadline,
                "waiter count never reached {expected} (currently {})",
                waiter_count(token)
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn sync_wait_registrations_are_released_on_completion() {
        // A parked wait registers exactly one live entry; waking removes it.
        let t = Arc::new(CancellationToken::new());
        let h = thread::spawn({
            let t = t.clone();
            move || t.wait()
        });
        await_waiter_count(&t, 1);
        assert_eq!(waiter_count(&t), 1);
        t.cancel();
        h.join().unwrap();
        assert_eq!(
            waiter_count(&t),
            0,
            "a completed wait must leave no registration behind"
        );
        // Sequential waits against the now-cancelled token register nothing.
        for _ in 0..1_000 {
            t.wait();
        }
        assert_eq!(waiter_count(&t), 0);
    }

    #[test]
    fn many_blocked_waiters_are_bounded_and_released() {
        let t = Arc::new(CancellationToken::new());
        let mut handles = vec![];
        for _ in 0..64 {
            let t = t.clone();
            handles.push(thread::spawn(move || t.wait()));
        }
        await_waiter_count(&t, 64);
        assert_eq!(waiter_count(&t), 64, "one entry per live waiter, no more");
        t.cancel();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(waiter_count(&t), 0, "every waiter must unregister");
    }

    #[test]
    fn dropped_children_unregister_from_every_parent() {
        let parent = CancellationToken::new();
        for _ in 0..1_000 {
            let child = parent.child();
            assert_eq!(waiter_count(&parent), 1);
            drop(child);
            assert_eq!(
                waiter_count(&parent),
                0,
                "dropping the last child clone must remove its registration"
            );
        }
        // A live clone keeps the one registration alive; the last drop
        // removes it.
        let child = parent.child();
        let clone = child.clone();
        drop(child);
        assert_eq!(
            waiter_count(&parent),
            1,
            "a live clone keeps the child alive"
        );
        drop(clone);
        assert_eq!(waiter_count(&parent), 0);
        // One child attached to two parents unregisters from both.
        let other = CancellationToken::new();
        let child = parent.child();
        other.attach(child.clone());
        assert_eq!(waiter_count(&parent), 1);
        assert_eq!(waiter_count(&other), 1);
        drop(child);
        assert_eq!(waiter_count(&parent), 0);
        assert_eq!(waiter_count(&other), 0);
    }

    #[test]
    fn hundred_thousand_node_trees_cancel_iteratively() {
        // Deep chain: a recursive cascade would overflow the stack here.
        let root = CancellationToken::new();
        let mut tokens = Vec::with_capacity(100_001);
        tokens.push(root.clone());
        for i in 0..100_000 {
            let child = tokens[i].child();
            tokens.push(child);
        }
        assert_eq!(waiter_count(&root), 1, "the chain is one live child link");
        assert!(root.cancel());
        for (i, token) in tokens.iter().enumerate() {
            assert!(token.is_cancelled(), "deep node {i} must be cancelled");
        }
        drop(tokens);
        assert_eq!(
            waiter_count(&root),
            0,
            "no dead registration may survive the tree"
        );

        // Wide tree: 100k live siblings under one root.
        let root = CancellationToken::new();
        let children: Vec<CancellationToken> = (0..100_000).map(|_| root.child()).collect();
        assert_eq!(waiter_count(&root), 100_000);
        assert!(root.cancel());
        assert!(children.iter().all(CancellationToken::is_cancelled));
        drop(children);
        assert_eq!(waiter_count(&root), 0);
    }

    #[test]
    fn poisoned_cancellation_locks_never_panic() {
        fn poison<T>(mutex: &Mutex<T>) {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = mutex.lock().unwrap();
                panic!("poison injection");
            }));
            assert!(mutex.lock().is_err(), "the mutex must really be poisoned");
        }

        // Poisoned registration map: cancel/attach/child/drop all recover.
        let a = CancellationToken::new();
        poison(&a.inner.waiters);
        let b = CancellationToken::new();
        a.attach(b.clone());
        let c = a.child();
        assert!(a.cancel(), "cancel must not panic on a poisoned map");
        assert!(b.is_cancelled());
        assert!(c.is_cancelled());
        drop(c);
        drop(b);
        a.wait();

        // Poisoned `notified` of a parked waiter: cancel still wakes it and
        // the waiter still returns (no panic, no hang).
        let d = Arc::new(CancellationToken::new());
        let h = thread::spawn({
            let d = d.clone();
            move || d.wait()
        });
        await_waiter_count(&d, 1);
        let waiter = {
            let waiters = lock_ignore_poison(&d.inner.waiters);
            waiters.values().next().expect("registered waiter").clone()
        };
        poison(&waiter.notified);
        d.cancel();
        h.join().unwrap();

        // Poisoned async waker list: poll, drop and cancel all recover.
        let e = CancellationToken::new();
        poison(&e.inner.async_waiters);
        let mut fut = Box::pin(e.cancelled());
        assert!(!poll_once(&mut fut));
        drop(fut);
        assert!(e.cancel());

        // Poisoned back-links: dropping a child unregisters through recovery.
        let f = CancellationToken::new();
        let child = f.child();
        poison(&child.inner.parents);
        drop(child);
        assert_eq!(waiter_count(&f), 0);
    }
}

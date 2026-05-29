//! Central lock manager coordinating table- and row-level locks across
//! connections.
//!
//! Locks in a thread-per-connection server must coordinate across real OS
//! threads, so the manager lives behind a single `Mutex` in the server's
//! `Shared` state with a companion `Condvar` that blocked waiters park on. A
//! request that conflicts with an existing holder either waits (parking on the
//! condvar until the conflict clears) or, with `NOWAIT`, fails immediately with
//! SQLSTATE `55P03`.
//!
//! # Lock modes and conflict matrix
//!
//! The eight PostgreSQL table-lock modes are modelled by [`LockMode`]. Two
//! requests conflict exactly per PostgreSQL's documented matrix (see
//! [`LockMode::conflicts_with`]). Row locks reuse a two-mode subset: `FOR
//! SHARE` maps to [`LockMode::Share`] and `FOR UPDATE` to
//! [`LockMode::Exclusive`] over a per-row lock object.
//!
//! # Lock objects and granularity
//!
//! A [`LockObject`] is either a whole table (by name) or a single row
//! (`table` + an opaque row-key string). Table locks are the primary
//! granularity. Row locks are *coarser than PostgreSQL's physical-tuple
//! locks*: the row key is a fingerprint of the projected row values produced by
//! the locking `SELECT`, so it identifies the logical tuple as returned rather
//! than a heap TID. This is sufficient for `FOR UPDATE`/`FOR SHARE` /
//! `NOWAIT` / `SKIP LOCKED` semantics in this engine and is documented as a
//! deliberate simplification.
//!
//! # Deadlock detection
//!
//! Every blocked request records a wait-for edge (waiter pid → each holder pid
//! it is blocked on). Before parking, [`LockManager`] builds the current
//! wait-for graph and runs the pure [`has_cycle`] function; if granting the
//! wait would close a cycle, the requesting transaction is aborted with
//! SQLSTATE `40P01` instead of blocking forever. The cycle check is a pure
//! function over an adjacency map so it can be unit-tested directly.

use std::collections::HashMap;

/// The PostgreSQL table-level lock modes, ordered weakest to strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockMode {
    AccessShare,
    RowShare,
    RowExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
}

impl LockMode {
    /// A small index (0..8) used to key the static conflict matrix.
    fn index(self) -> usize {
        match self {
            LockMode::AccessShare => 0,
            LockMode::RowShare => 1,
            LockMode::RowExclusive => 2,
            LockMode::ShareUpdateExclusive => 3,
            LockMode::Share => 4,
            LockMode::ShareRowExclusive => 5,
            LockMode::Exclusive => 6,
            LockMode::AccessExclusive => 7,
        }
    }

    /// The canonical PostgreSQL spelling, used in command tags / errors.
    pub fn as_str(self) -> &'static str {
        match self {
            LockMode::AccessShare => "ACCESS SHARE",
            LockMode::RowShare => "ROW SHARE",
            LockMode::RowExclusive => "ROW EXCLUSIVE",
            LockMode::ShareUpdateExclusive => "SHARE UPDATE EXCLUSIVE",
            LockMode::Share => "SHARE",
            LockMode::ShareRowExclusive => "SHARE ROW EXCLUSIVE",
            LockMode::Exclusive => "EXCLUSIVE",
            LockMode::AccessExclusive => "ACCESS EXCLUSIVE",
        }
    }

    /// Parse a `LOCK TABLE ... IN <mode> MODE` clause (case-insensitive). The
    /// default when no clause is given is `ACCESS EXCLUSIVE`, matching
    /// PostgreSQL.
    pub fn parse(s: &str) -> Option<LockMode> {
        let norm = s.trim().to_ascii_uppercase();
        Some(match norm.as_str() {
            "ACCESS SHARE" => LockMode::AccessShare,
            "ROW SHARE" => LockMode::RowShare,
            "ROW EXCLUSIVE" => LockMode::RowExclusive,
            "SHARE UPDATE EXCLUSIVE" => LockMode::ShareUpdateExclusive,
            "SHARE" => LockMode::Share,
            "SHARE ROW EXCLUSIVE" => LockMode::ShareRowExclusive,
            "EXCLUSIVE" => LockMode::Exclusive,
            "ACCESS EXCLUSIVE" => LockMode::AccessExclusive,
            _ => return None,
        })
    }

    /// Whether holding `self` conflicts with a concurrent request for `other`,
    /// per PostgreSQL's table-level lock conflict matrix. The relation is
    /// symmetric.
    pub fn conflicts_with(self, other: LockMode) -> bool {
        CONFLICTS[self.index()][other.index()]
    }
}

/// PostgreSQL's lock-mode conflict matrix. `CONFLICTS[a][b]` is true iff mode
/// `a` conflicts with mode `b`. Row order matches [`LockMode::index`]:
/// ACCESS SHARE, ROW SHARE, ROW EXCLUSIVE, SHARE UPDATE EXCLUSIVE, SHARE,
/// SHARE ROW EXCLUSIVE, EXCLUSIVE, ACCESS EXCLUSIVE.
#[rustfmt::skip]
const CONFLICTS: [[bool; 8]; 8] = [
    // AccS   RowS   RowX   SUExc  Share  SRowX  Excl   AccX
    [ false, false, false, false, false, false, false, true ], // AccessShare
    [ false, false, false, false, false, false, true,  true ], // RowShare
    [ false, false, false, false, true,  true,  true,  true ], // RowExclusive
    [ false, false, false, true,  true,  true,  true,  true ], // ShareUpdateExclusive
    [ false, false, true,  true,  false, true,  true,  true ], // Share
    [ false, false, true,  true,  true,  true,  true,  true ], // ShareRowExclusive
    [ false, true,  true,  true,  true,  true,  true,  true ], // Exclusive
    [ true,  true,  true,  true,  true,  true,  true,  true ], // AccessExclusive
];

/// A lockable object: a whole table or a single (logical) row within one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockObject {
    Table(String),
    Row { table: String, key: String },
}

/// One held lock: which backend holds it and in what mode.
#[derive(Debug, Clone)]
struct Holder {
    pid: i32,
    mode: LockMode,
}

/// One request that is currently parked waiting for an object.
#[derive(Debug, Clone)]
struct Waiter {
    pid: i32,
    mode: LockMode,
}

/// The state of a single lock object: who holds it and who is queued.
#[derive(Debug, Default)]
struct ObjectState {
    holders: Vec<Holder>,
    waiters: Vec<Waiter>,
}

impl ObjectState {
    fn is_empty(&self) -> bool {
        self.holders.is_empty() && self.waiters.is_empty()
    }
}

/// Outcome of attempting to acquire a lock without blocking.
#[derive(Debug, PartialEq, Eq)]
pub enum TryAcquire {
    /// The lock was granted (or was already held in a covering mode).
    Granted,
    /// The lock conflicts with an existing holder; the caller may wait.
    /// Carries the set of holder pids the requester is blocked on.
    Conflict(Vec<i32>),
    /// Granting (or waiting for) the lock would create a deadlock.
    Deadlock,
}

/// The central lock table. Lives behind a `Mutex` in the server's shared
/// state, paired with a `Condvar` that waiters park on. All methods take
/// `&mut self` (the caller holds the mutex); none of them block — blocking is
/// the caller's responsibility via the condvar, so the manager's own lock is
/// never held across a wait.
#[derive(Default)]
pub struct LockManager {
    objects: HashMap<LockObject, ObjectState>,
}

impl LockManager {
    pub fn new() -> Self {
        LockManager::default()
    }

    /// Attempt to grant `pid` a lock on `obj` in `mode` without blocking.
    ///
    /// On success the holder is recorded and any prior waiter entry for this
    /// pid+object is cleared. On conflict the pid is recorded as a waiter (so
    /// the wait-for graph reflects it) unless doing so would deadlock, in which
    /// case nothing is recorded and [`TryAcquire::Deadlock`] is returned.
    pub fn try_acquire(&mut self, pid: i32, obj: &LockObject, mode: LockMode) -> TryAcquire {
        // Identify conflicting holders other than ourselves.
        let conflicting: Vec<i32> = self
            .objects
            .get(obj)
            .map(|st| {
                st.holders
                    .iter()
                    .filter(|h| h.pid != pid && h.mode.conflicts_with(mode))
                    .map(|h| h.pid)
                    .collect()
            })
            .unwrap_or_default();

        if conflicting.is_empty() {
            self.grant(pid, obj, mode);
            return TryAcquire::Granted;
        }

        // Would waiting close a cycle in the wait-for graph? Tentatively add
        // our edges, test, and roll back if so.
        self.add_waiter(pid, obj, mode);
        let graph = self.wait_for_graph();
        if has_cycle(&graph) {
            self.remove_waiter(pid, obj);
            return TryAcquire::Deadlock;
        }
        TryAcquire::Conflict(conflicting)
    }

    /// Record that `pid` now holds `obj` in `mode`, clearing any waiter entry.
    fn grant(&mut self, pid: i32, obj: &LockObject, mode: LockMode) {
        let st = self.objects.entry(obj.clone()).or_default();
        st.waiters.retain(|w| w.pid != pid);
        // Upgrade in place if we already hold a weaker mode on this object.
        if let Some(h) = st.holders.iter_mut().find(|h| h.pid == pid) {
            if mode.index() > h.mode.index() {
                h.mode = mode;
            }
        } else {
            st.holders.push(Holder { pid, mode });
        }
    }

    fn add_waiter(&mut self, pid: i32, obj: &LockObject, mode: LockMode) {
        let st = self.objects.entry(obj.clone()).or_default();
        if !st.waiters.iter().any(|w| w.pid == pid) {
            st.waiters.push(Waiter { pid, mode });
        }
    }

    fn remove_waiter(&mut self, pid: i32, obj: &LockObject) {
        if let Some(st) = self.objects.get_mut(obj) {
            st.waiters.retain(|w| w.pid != pid);
            if st.is_empty() {
                self.objects.remove(obj);
            }
        }
    }

    /// Release every lock and waiter belonging to `pid` (called at end of
    /// transaction and on disconnect). Returns true if anything changed, so the
    /// caller knows to wake parked waiters.
    pub fn release_all(&mut self, pid: i32) -> bool {
        let mut changed = false;
        self.objects.retain(|_, st| {
            let before = st.holders.len() + st.waiters.len();
            st.holders.retain(|h| h.pid != pid);
            st.waiters.retain(|w| w.pid != pid);
            if st.holders.len() + st.waiters.len() != before {
                changed = true;
            }
            !st.is_empty()
        });
        changed
    }

    /// Whether `obj` is currently held by some backend other than `pid` in a
    /// mode conflicting with `mode`. Used to implement `SKIP LOCKED` (omit such
    /// rows) without parking.
    pub fn is_locked_by_other(&self, pid: i32, obj: &LockObject, mode: LockMode) -> bool {
        self.objects.get(obj).is_some_and(|st| {
            st.holders
                .iter()
                .any(|h| h.pid != pid && h.mode.conflicts_with(mode))
        })
    }

    /// Build the current wait-for graph: waiter pid → set of holder pids whose
    /// (conflicting) locks it is blocked on. Exposed for testing.
    pub fn wait_for_graph(&self) -> HashMap<i32, Vec<i32>> {
        let mut graph: HashMap<i32, Vec<i32>> = HashMap::new();
        for st in self.objects.values() {
            for w in &st.waiters {
                for h in &st.holders {
                    if h.pid != w.pid && h.mode.conflicts_with(w.mode) {
                        let edges = graph.entry(w.pid).or_default();
                        if !edges.contains(&h.pid) {
                            edges.push(h.pid);
                        }
                    }
                }
            }
        }
        graph
    }
}

/// Detect a cycle in a directed graph given as an adjacency map (node → its
/// out-neighbours). Pure and side-effect free so it can be unit-tested in
/// isolation from the lock manager. Used for deadlock detection over the
/// wait-for graph: a cycle means a set of transactions are mutually blocked.
pub fn has_cycle(graph: &HashMap<i32, Vec<i32>>) -> bool {
    // Standard DFS with white/gray/black coloring: a back-edge to a gray node
    // is a cycle.
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: HashMap<i32, Color> = HashMap::new();
    for &node in graph.keys() {
        color.entry(node).or_insert(Color::White);
    }
    // Iterative DFS to avoid stack overflow on long chains.
    for &start in graph.keys() {
        if color[&start] != Color::White {
            continue;
        }
        // Stack of (node, whether we've finished its children).
        let mut stack: Vec<(i32, bool)> = vec![(start, false)];
        while let Some((node, processed)) = stack.pop() {
            if processed {
                color.insert(node, Color::Black);
                continue;
            }
            color.insert(node, Color::Gray);
            stack.push((node, true));
            if let Some(neighbours) = graph.get(&node) {
                for &next in neighbours {
                    match color.get(&next).copied().unwrap_or(Color::White) {
                        Color::Gray => return true, // back-edge → cycle
                        Color::White => stack.push((next, false)),
                        Color::Black => {}
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_matrix_is_symmetric() {
        use LockMode::*;
        let modes = [
            AccessShare,
            RowShare,
            RowExclusive,
            ShareUpdateExclusive,
            Share,
            ShareRowExclusive,
            Exclusive,
            AccessExclusive,
        ];
        for a in modes {
            for b in modes {
                assert_eq!(
                    a.conflicts_with(b),
                    b.conflicts_with(a),
                    "matrix asymmetric at {a:?}/{b:?}"
                );
            }
        }
    }

    #[test]
    fn conflict_matrix_known_cases() {
        use LockMode::*;
        // ACCESS SHARE only conflicts with ACCESS EXCLUSIVE.
        assert!(!AccessShare.conflicts_with(AccessShare));
        assert!(!AccessShare.conflicts_with(RowExclusive));
        assert!(!AccessShare.conflicts_with(Exclusive));
        assert!(AccessShare.conflicts_with(AccessExclusive));
        // ACCESS EXCLUSIVE conflicts with everything.
        for m in [AccessShare, RowShare, RowExclusive, Share, Exclusive, AccessExclusive] {
            assert!(AccessExclusive.conflicts_with(m));
        }
        // Two ROW EXCLUSIVE (concurrent writers) do not conflict.
        assert!(!RowExclusive.conflicts_with(RowExclusive));
        // SHARE conflicts with ROW EXCLUSIVE but not with itself.
        assert!(Share.conflicts_with(RowExclusive));
        assert!(!Share.conflicts_with(Share));
        // EXCLUSIVE allows ACCESS SHARE readers but blocks ROW SHARE.
        assert!(!Exclusive.conflicts_with(AccessShare));
        assert!(Exclusive.conflicts_with(RowShare));
    }

    #[test]
    fn parse_round_trips() {
        for m in [
            LockMode::AccessShare,
            LockMode::RowShare,
            LockMode::RowExclusive,
            LockMode::ShareUpdateExclusive,
            LockMode::Share,
            LockMode::ShareRowExclusive,
            LockMode::Exclusive,
            LockMode::AccessExclusive,
        ] {
            assert_eq!(LockMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(LockMode::parse("access exclusive"), Some(LockMode::AccessExclusive));
        assert_eq!(LockMode::parse("bogus"), None);
    }

    #[test]
    fn cycle_detection_acyclic() {
        // 1 -> 2 -> 3, no cycle.
        let mut g = HashMap::new();
        g.insert(1, vec![2]);
        g.insert(2, vec![3]);
        assert!(!has_cycle(&g));
    }

    #[test]
    fn cycle_detection_simple_cycle() {
        // 1 -> 2 -> 1.
        let mut g = HashMap::new();
        g.insert(1, vec![2]);
        g.insert(2, vec![1]);
        assert!(has_cycle(&g));
    }

    #[test]
    fn cycle_detection_self_loop() {
        let mut g = HashMap::new();
        g.insert(1, vec![1]);
        assert!(has_cycle(&g));
    }

    #[test]
    fn cycle_detection_three_cycle() {
        // 1 -> 2 -> 3 -> 1.
        let mut g = HashMap::new();
        g.insert(1, vec![2]);
        g.insert(2, vec![3]);
        g.insert(3, vec![1]);
        assert!(has_cycle(&g));
    }

    #[test]
    fn cycle_detection_diamond_no_cycle() {
        // 1 -> 2, 1 -> 3, 2 -> 4, 3 -> 4 (diamond, acyclic).
        let mut g = HashMap::new();
        g.insert(1, vec![2, 3]);
        g.insert(2, vec![4]);
        g.insert(3, vec![4]);
        assert!(!has_cycle(&g));
    }

    #[test]
    fn try_acquire_grants_when_free() {
        let mut lm = LockManager::new();
        let obj = LockObject::Table("t".into());
        assert_eq!(lm.try_acquire(1, &obj, LockMode::AccessExclusive), TryAcquire::Granted);
    }

    #[test]
    fn try_acquire_conflicts_then_releases() {
        let mut lm = LockManager::new();
        let obj = LockObject::Table("t".into());
        assert_eq!(lm.try_acquire(1, &obj, LockMode::Exclusive), TryAcquire::Granted);
        // pid 2 wants a conflicting lock → conflict on holder 1.
        match lm.try_acquire(2, &obj, LockMode::Exclusive) {
            TryAcquire::Conflict(on) => assert_eq!(on, vec![1]),
            other => panic!("expected conflict, got {other:?}"),
        }
        // After pid 1 releases, pid 2 can take it.
        assert!(lm.release_all(1));
        assert_eq!(lm.try_acquire(2, &obj, LockMode::Exclusive), TryAcquire::Granted);
    }

    #[test]
    fn try_acquire_detects_deadlock() {
        let mut lm = LockManager::new();
        let a = LockObject::Table("a".into());
        let b = LockObject::Table("b".into());
        // pid1 holds a, pid2 holds b.
        assert_eq!(lm.try_acquire(1, &a, LockMode::Exclusive), TryAcquire::Granted);
        assert_eq!(lm.try_acquire(2, &b, LockMode::Exclusive), TryAcquire::Granted);
        // pid1 waits for b (held by 2): conflict, no cycle yet.
        assert!(matches!(lm.try_acquire(1, &b, LockMode::Exclusive), TryAcquire::Conflict(_)));
        // pid2 waits for a (held by 1): would close cycle 1->2->1 → deadlock.
        assert_eq!(lm.try_acquire(2, &a, LockMode::Exclusive), TryAcquire::Deadlock);
    }

    #[test]
    fn shared_locks_coexist() {
        let mut lm = LockManager::new();
        let obj = LockObject::Row { table: "t".into(), key: "1".into() };
        assert_eq!(lm.try_acquire(1, &obj, LockMode::Share), TryAcquire::Granted);
        assert_eq!(lm.try_acquire(2, &obj, LockMode::Share), TryAcquire::Granted);
        // An exclusive request now conflicts with both shared holders.
        match lm.try_acquire(3, &obj, LockMode::Exclusive) {
            TryAcquire::Conflict(mut on) => {
                on.sort();
                assert_eq!(on, vec![1, 2]);
            }
            other => panic!("expected conflict, got {other:?}"),
        }
    }
}

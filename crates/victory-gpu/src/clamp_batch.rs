//! Indirect clamps batched ahead per segment from what the same segment read last time (`VICTORY_CLAMP_BATCH=1`, needs `VICTORY_WINDOWS_MANY=1`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

/// Whether segments' clamps are batched.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLAMP_BATCH").is_some_and(|v| v == "1"))
}

/// Since the frame line: batches clamped, windows clamped in them past their head, plans
/// kept, windows of plans that no draw read.
pub(crate) static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// A window of a plan: its first record's offset from the head's, the step between its
/// records and how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    pub(crate) delta: i64,
    pub(crate) step: u32,
    pub(crate) records: u32,
}

impl Planned {
    /// Whether it holds the record at `delta` from the head.
    fn holds(&self, delta: i64) -> bool {
        delta >= self.delta
            && (delta - self.delta) % i64::from(self.step.max(1)) == 0
            && (delta - self.delta) / i64::from(self.step.max(1)) < i64::from(self.records)
    }
}

/// A segment's head: pipeline, kind, stride, and the how-manieth head of those in the list.
pub(crate) type Key = (usize, u32, u32, u32);

/// The segment a list is in, and how many heads of each kind it saw.
#[derive(Default)]
pub(crate) struct Segment {
    /// The head's key, its argument copy and its record's offset there.
    head: Option<(Key, u64, u64)>,
    /// Windows clamped after the head, and whether a draw read each.
    seen: Vec<(Planned, bool)>,
    nth: HashMap<(usize, u32, u32), u32>,
}

impl Segment {
    /// A clamp with no segment open begins one: its key.
    pub(crate) fn begin(&mut self, (pipeline, kind, stride): (usize, u32, u32), copy: u64, offset: u64) -> Key {
        let n = self.nth.entry((pipeline, kind, stride)).or_insert(0);
        *n += 1;
        let key = (pipeline, kind, stride, *n);
        self.head = Some((key, copy, offset));
        self.seen.clear();
        key
    }

    pub(crate) fn open(&self) -> bool {
        self.head.is_some()
    }

    /// The offset from the head's record of the record at `offset` in `copy`, if in its copy.
    fn delta(&self, copy: u64, offset: u64) -> Option<i64> {
        let &(_, head_copy, head) = self.head.as_ref()?;
        (copy == head_copy).then(|| offset as i64 - head as i64)
    }

    /// A window clamped in the segment past its head: `read`, whether a draw reads it now.
    pub(crate) fn note(&mut self, copy: u64, offset: u64, step: u32, records: u32, read: bool) {
        if let Some(delta) = self.delta(copy, offset)
            && delta != 0
        {
            self.seen.push((Planned { delta, step, records }, read));
        }
    }

    /// A draw read the record at `offset` of `copy` from a window.
    pub(crate) fn read(&mut self, copy: u64, offset: u64) {
        if let Some(delta) = self.delta(copy, offset)
            && let Some(w) = self.seen.iter_mut().rev().find(|(p, _)| p.holds(delta))
        {
            w.1 = true;
        }
    }

    /// The segment ends: its head and its plan (the windows a draw read), if it has one;
    /// and how many windows of it no draw read.
    pub(crate) fn end(&mut self) -> Option<(Key, Vec<Planned>, usize)> {
        let (key, _, _) = self.head.take()?;
        let unread = self.seen.iter().filter(|(_, read)| !read).count();
        let plan: Vec<Planned> = self.seen.drain(..).filter(|(_, read)| *read).map(|(p, _)| p).collect();
        (!plan.is_empty() || unread > 0).then_some((key, plan, unread))
    }

    /// Another host list: heads are counted afresh.
    pub(crate) fn new_list(&mut self) {
        self.head = None;
        self.seen.clear();
        self.nth.clear();
    }
}

/// Most plans kept; past it they are all forgotten.
const PLANS_MOST: usize = 8192;

static PLANS: Mutex<Option<HashMap<Key, Vec<Planned>>>> = Mutex::new(None);

/// The plan kept for a head.
pub(crate) fn plan(key: Key) -> Option<Vec<Planned>> {
    PLANS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(&key).cloned()
}

/// Keeps a head's plan; an empty one forgets it.
pub(crate) fn keep(key: Key, plan: Vec<Planned>) {
    let mut plans = PLANS.lock().unwrap_or_else(|e| e.into_inner());
    let map = plans.get_or_insert_with(HashMap::new);
    if plan.is_empty() {
        map.remove(&key);
        return;
    }
    if map.len() >= PLANS_MOST && !map.contains_key(&key) {
        map.clear();
    }
    map.insert(key, plan);
}

#[cfg(test)]
mod tests {
    use super::{Planned, Segment};

    #[test]
    fn a_segment_keeps_the_windows_its_draws_read_as_its_plan() {
        let mut s = Segment::default();
        let key = s.begin((7, 10, 12), 1, 0x100);
        assert_eq!(key, (7, 10, 12, 1));
        // two windows after the head, one read, one not; one in another copy
        s.note(1, 0x900, 32, 16, true);
        s.note(1, 0x2000, 32, 16, false);
        s.note(2, 0x100, 32, 16, true);
        // the head itself is not part of its plan
        s.note(1, 0x100, 32, 16, true);
        let (k, plan, unread) = s.end().expect("a plan");
        assert_eq!(k, key);
        assert_eq!(plan, [Planned { delta: 0x800, step: 32, records: 16 }]);
        assert_eq!(unread, 1);
        assert!(!s.open());
        // the same head again in the list is the second of its kind
        assert_eq!(s.begin((7, 10, 12), 1, 0x100).3, 2);
        s.new_list();
        assert_eq!(s.begin((7, 10, 12), 1, 0x100).3, 1);
    }

    #[test]
    fn a_planned_window_is_marked_read_by_a_draw_of_any_of_its_records() {
        let mut s = Segment::default();
        s.begin((7, 10, 12), 1, 0x100);
        s.note(1, 0x1100, 32, 4, false);
        // not on the step, past the end
        s.read(1, 0x1110);
        s.read(1, 0x1100 + 4 * 32);
        assert_eq!(s.end().map(|e| e.1.len()), Some(0));
        s.begin((7, 10, 12), 1, 0x100);
        s.note(1, 0x1100, 32, 4, false);
        s.read(1, 0x1100 + 3 * 32);
        assert_eq!(s.end().map(|e| e.1), Some(vec![Planned { delta: 0x1000, step: 32, records: 4 }]));
    }

    #[test]
    fn plans_are_kept_and_forgotten() {
        let key = (0xdead, 10, 12, 1);
        super::keep(key, vec![Planned { delta: 64, step: 32, records: 2 }]);
        assert_eq!(super::plan(key).map(|p| p.len()), Some(1));
        super::keep(key, Vec::new());
        assert_eq!(super::plan(key), None);
    }
}

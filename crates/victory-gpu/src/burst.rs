//! burst refresh (VICTORY_BURST_REFRESH=1, off by default), on fxceless's refresh-once

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, Weak};

use crate::shadow::Shadow;

// by copy (its address): the copy and the title range that is up to date
pub(crate) type Current<T> = HashMap<usize, (Arc<T>, usize, usize)>;

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_BURST_REFRESH").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                check = check_asked(),
                "copies stay up to date across the submissions of a burst (VICTORY_BURST_REFRESH=1)"
            );
        }
        on
    })
}

fn check_asked() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BURST_REFRESH_CHECK").is_some_and(|v| v == "1"))
}

// the old check runs as well and says what a skip would have missed (VICTORY_BURST_REFRESH_CHECK=1)
pub(crate) fn check() -> bool {
    on() && check_asked()
}

// submissions put in a line so far
static ENQUEUED: AtomicU64 = AtomicU64::new(0);

// at the title's submit call: the submission's place in the order of enqueues
pub(crate) fn stamp() -> u64 {
    ENQUEUED.fetch_add(1, SeqCst) + 1
}

thread_local! {
    // the stamp of the submission this thread is preparing; 0 is none
    static STAMP: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) struct Entered(u64);

// while the returned value lives, this thread prepares the submission stamped `stamp`
pub(crate) fn enter(stamp: u64) -> Entered {
    Entered(STAMP.replace(stamp))
}

impl Drop for Entered {
    fn drop(&mut self) {
        STAMP.set(self.0);
    }
}

// a submission that was in no line counts as enqueued now
fn stamp_here() -> u64 {
    match STAMP.get() {
        0 => u64::MAX,
        stamp => stamp,
    }
}

// what stays between submissions: each range with the enqueue count it is good up to
struct Kept<T> {
    ranges: HashMap<usize, (Weak<T>, usize, usize, u64)>,
    // the enqueue count when the last heap sync began
    heap: u64,
}

impl<T> Kept<T> {
    fn new() -> Self {
        Kept { ranges: HashMap::new(), heap: 0 }
    }

    // the ranges good for a submission stamped `stamp`, and what each was good up to
    fn seed(&mut self, stamp: u64, current: &mut Current<T>) -> HashMap<usize, u64> {
        let mut seqs = HashMap::new();
        for (key, (copy, from, to, seq)) in self.ranges.drain() {
            if seq >= stamp
                && let Some(copy) = copy.upgrade()
            {
                current.insert(key, (copy, from, to));
                seqs.insert(key, seq);
            }
        }
        seqs
    }

    // after a submission whose checks began at enqueue count `seq`
    fn keep(&mut self, current: &Current<T>, seqs: &HashMap<usize, u64>, seq: u64) {
        self.ranges.clear();
        for (&key, (copy, from, to)) in current {
            let good = seqs.get(&key).map_or(seq, |&old| old.min(seq));
            self.ranges.insert(key, (Arc::downgrade(copy), *from, *to, good));
        }
    }

    // whether a submission stamped `stamp` needs a heap sync; one that does begins at `now`
    fn heap_due(&mut self, stamp: u64, now: u64) -> bool {
        if self.heap >= stamp {
            return false;
        }
        self.heap = now;
        true
    }
}

static KEPT: std::sync::LazyLock<Mutex<Kept<Shadow>>> = std::sync::LazyLock::new(|| Mutex::new(Kept::new()));

// title memory written or remapped by us or the runtime since the last look
struct Log {
    writes: Vec<(usize, usize)>,
    // too many to list: everything counts as written
    all: bool,
}

const LOG_MOST: usize = 1 << 15;

impl Log {
    fn note(&mut self, at: usize, end: usize) {
        if self.all {
            return;
        }
        if self.writes.len() >= LOG_MOST {
            self.writes.clear();
            self.all = true;
        } else {
            self.writes.push((at, end));
        }
    }

    // takes what was noted out of `current`
    fn apply<T>(&mut self, current: &mut Current<T>) {
        if std::mem::take(&mut self.all) {
            current.clear();
        }
        for (at, end) in self.writes.drain(..) {
            trim(current, at, end);
        }
    }
}

static LOG: Mutex<Log> = Mutex::new(Log { writes: Vec::new(), all: false });
// counts the notes, so a look with nothing new takes no lock
static WROTE: AtomicU64 = AtomicU64::new(0);

// title memory `[at, at + len)` was written or remapped other than by the title's own code
pub(crate) fn wrote(at: usize, len: usize) {
    if len == 0 || !on() {
        return;
    }
    LOG.lock().unwrap_or_else(|e| e.into_inner()).note(at, at.saturating_add(len));
    WROTE.fetch_add(1, SeqCst);
}

// of a range over `[at, end)` the larger part beside it stays, as Batch::title_wrote has it
pub(crate) fn trim<T>(current: &mut Current<T>, at: usize, end: usize) {
    current.retain(|_, entry| {
        let (from, to) = (entry.1, entry.2);
        if end <= from || to <= at {
            return true;
        }
        let (left, right) = (at.saturating_sub(from), to.saturating_sub(end));
        if left >= right {
            entry.2 = from + left;
        } else {
            entry.1 = end;
        }
        entry.1 < entry.2
    });
}

// skips, their bytes, skips across submissions, heap syncs left out, missed ranges, their bytes, submissions
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

fn count(i: usize, n: u64) {
    STATS[i].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

// one submission's side of the burst
#[derive(Default)]
pub(crate) struct Pass {
    stamp: u64,
    // the enqueue count before this submission's first check
    seq: u64,
    seqs: HashMap<usize, u64>,
    wrote: u64,
}

// when a submission's preparing begins: what is still good for it goes into `current`
pub(crate) fn begin(current: &mut Current<Shadow>) -> Option<Pass> {
    if !on() {
        return None;
    }
    let stamp = stamp_here();
    let seq = ENQUEUED.load(SeqCst);
    let seqs = KEPT.lock().unwrap_or_else(|e| e.into_inner()).seed(stamp, current);
    let mut pass = Pass { stamp, seq, seqs, wrote: 0 };
    pass.written(current);
    Some(pass)
}

impl Pass {
    // takes what was written since the last look out of `current`
    pub(crate) fn written(&mut self, current: &mut Current<Shadow>) {
        let now = WROTE.load(SeqCst);
        if now == self.wrote {
            return;
        }
        self.wrote = now;
        LOG.lock().unwrap_or_else(|e| e.into_inner()).apply(current);
    }

    // a refresh of `[address, address + len)` of `copy` was left out
    pub(crate) fn skipped(&self, copy: &Arc<Shadow>, address: usize, len: usize) {
        let across = self.seqs.contains_key(&(Arc::as_ptr(copy) as usize));
        count(0, 1);
        count(1, len as u64);
        count(2, across as u64);
        if !check() {
            return;
        }
        let bytes = copy.update(address, len);
        if bytes == 0 {
            return;
        }
        count(4, 1);
        count(5, bytes as u64);
        let said = STATS[4].load(std::sync::atomic::Ordering::Relaxed);
        if said <= 256 || said.is_power_of_two() {
            tracing::warn!(
                said,
                copy = copy.id(),
                range = format_args!("{address:#x}+{len:#x}"),
                bytes,
                across_submissions = across,
                stamp = self.stamp,
                good_up_to = self.seqs.get(&(Arc::as_ptr(copy) as usize)).copied().unwrap_or(self.seq),
                "burst refresh would have left written pages uncopied here (VICTORY_BURST_REFRESH_CHECK=1 copied them)"
            );
        }
    }

    // when the submission's refreshes are done
    pub(crate) fn end(self, current: &Current<Shadow>) {
        KEPT.lock().unwrap_or_else(|e| e.into_inner()).keep(current, &self.seqs, self.seq);
        count(6, 1);
        line();
    }
}

// whether the submission this thread prepares needs the heaps synced
pub(crate) fn heap_due() -> bool {
    if !on() {
        return true;
    }
    let now = ENQUEUED.load(SeqCst);
    let due = KEPT.lock().unwrap_or_else(|e| e.into_inner()).heap_due(stamp_here(), now);
    if !due {
        count(3, 1);
    }
    due || check()
}

fn line() {
    use std::sync::atomic::Ordering::Relaxed;
    const EVERY: u64 = 2048;
    if STATS[6].load(Relaxed) < EVERY {
        return;
    }
    let [skips, bytes, across, heaps, missed, missed_bytes, submissions] = [0, 1, 2, 3, 4, 5, 6].map(|i| STATS[i].swap(0, Relaxed));
    let per = submissions.max(1) as f64;
    tracing::info!(
        "burst refresh per submission (VICTORY_BURST_REFRESH=1): {:.1} refreshes left out ({:.1} of them from an earlier \
         submission), {:.1} MB; {:.2} heap syncs left out; the check found {missed} skips over written pages, {:.2} MB, in {submissions} submissions",
        skips as f64 / per,
        across as f64 / per,
        bytes as f64 / per / 1048576.0,
        heaps as f64 / per,
        missed_bytes as f64 / 1048576.0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(kept: &mut Kept<u8>, log: &mut Log, stamp: u64, enqueued: u64, copy: &Arc<u8>, range: (usize, usize)) -> bool {
        let mut current = Current::new();
        let seqs = kept.seed(stamp, &mut current);
        log.apply(&mut current);
        let key = Arc::as_ptr(copy) as usize;
        let skipped = current.get(&key).is_some_and(|&(_, from, to)| from <= range.0 && range.1 <= to);
        if !skipped {
            current.insert(key, (copy.clone(), range.0, range.1));
        }
        kept.keep(&current, &seqs, enqueued);
        skipped
    }

    #[test]
    fn a_submission_enqueued_before_the_check_skips_and_one_after_does_not() {
        let (mut kept, mut log) = (Kept::<u8>::new(), Log { writes: Vec::new(), all: false });
        let copy = Arc::new(0u8);
        // a and b are in the line (stamps 1, 2) when a is prepared
        assert!(!prepared(&mut kept, &mut log, 1, 2, &copy, (0x1000, 0x9000)));
        assert!(prepared(&mut kept, &mut log, 2, 2, &copy, (0x2000, 0x3000)));
        // the title wrote, then enqueued c (stamp 3): checked again
        assert!(!prepared(&mut kept, &mut log, 3, 3, &copy, (0x2000, 0x3000)));
    }

    #[test]
    fn a_write_between_enqueue_and_prep_is_taken() {
        let (mut kept, mut log) = (Kept::<u8>::new(), Log { writes: Vec::new(), all: false });
        let copy = Arc::new(0u8);
        assert!(!prepared(&mut kept, &mut log, 1, 2, &copy, (0x1000, 0x9000)));
        // a copy-back or a title buffer copy of ours lands in the range before b is prepared
        log.note(0x1800, 0x1900);
        assert!(!prepared(&mut kept, &mut log, 2, 2, &copy, (0x1000, 0x2000)));
        // b's refresh stands for what it covered, still good up to enqueue count 2
        assert!(prepared(&mut kept, &mut log, 2, 2, &copy, (0x1000, 0x2000)));
    }

    #[test]
    fn the_larger_part_beside_a_write_stays_up_to_date() {
        let mut current: Current<u8> = Current::new();
        let copy = Arc::new(0u8);
        current.insert(1, (copy.clone(), 0x1000, 0x9000));
        trim(&mut current, 0x1800, 0x1900);
        assert_eq!((current[&1].1, current[&1].2), (0x1900, 0x9000));
        trim(&mut current, 0, 0x10000);
        assert!(current.is_empty());
    }

    #[test]
    fn a_remap_mid_burst_ends_what_it_covers() {
        let (mut kept, mut log) = (Kept::<u8>::new(), Log { writes: Vec::new(), all: false });
        let (a, b) = (Arc::new(0u8), Arc::new(0u8));
        assert!(!prepared(&mut kept, &mut log, 1, 3, &a, (0x10000, 0x20000)));
        assert!(!prepared(&mut kept, &mut log, 1, 3, &b, (0x40000, 0x50000)));
        // the runtime maps other pages over a's range
        log.note(0x10000, 0x20000);
        assert!(!prepared(&mut kept, &mut log, 2, 3, &a, (0x10000, 0x20000)));
        assert!(prepared(&mut kept, &mut log, 3, 3, &b, (0x40000, 0x50000)));
    }

    #[test]
    fn too_many_writes_end_everything() {
        let (mut kept, mut log) = (Kept::<u8>::new(), Log { writes: Vec::new(), all: false });
        let copy = Arc::new(0u8);
        assert!(!prepared(&mut kept, &mut log, 1, 2, &copy, (0x1000, 0x9000)));
        for i in 0..=LOG_MOST {
            log.note(0x100000 + i, 0x100001 + i);
        }
        assert!(!prepared(&mut kept, &mut log, 2, 2, &copy, (0x1000, 0x9000)));
    }

    #[test]
    fn a_copy_let_go_is_not_kept() {
        let (mut kept, mut log) = (Kept::<u8>::new(), Log { writes: Vec::new(), all: false });
        let copy = Arc::new(0u8);
        assert!(!prepared(&mut kept, &mut log, 1, 2, &copy, (0x1000, 0x9000)));
        drop(copy);
        let mut current = Current::new();
        assert!(kept.seed(2, &mut current).is_empty() && current.is_empty());
    }

    #[test]
    fn the_heaps_are_synced_once_for_what_was_enqueued() {
        let mut kept = Kept::<u8>::new();
        assert!(kept.heap_due(1, 3));
        assert!(!kept.heap_due(2, 3));
        assert!(!kept.heap_due(3, 3));
        assert!(kept.heap_due(4, 5));
        // a submission that was in no line
        assert!(kept.heap_due(u64::MAX, 5));
    }

    #[test]
    fn a_thread_names_its_submission_only_while_it_prepares_it() {
        assert_eq!(stamp_here(), u64::MAX);
        {
            let _in = enter(7);
            assert_eq!(stamp_here(), 7);
        }
        assert_eq!(stamp_here(), u64::MAX);
    }
}

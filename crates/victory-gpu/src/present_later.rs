//! A present that the queues' lines run in order (`VICTORY_PRESENT_LATER=1`,
//! with the submitting thread of ours, `crate::queue::behind`, and presents
//! sent ahead, `crate::host::late_free_present`).
//!
//! Why: with `behind` the title's present still runs the lines itself
//! (`release_held_now`), and that takes the releasers' lock our thread holds
//! while it prepares the whole frame: in vf-all-3 the presenting thread
//! stood 236 of its 242 ms a frame, so the title only starts the next
//! frame (65 ms before its first list call) once ours has sent the last.
//! With the switch the present is a mark at the end of every line; our
//! thread shows it when every line has reached its mark, and the title goes
//! on with the next frame meanwhile, one frame ahead at most.

use std::sync::{Condvar, Mutex, OnceLock};

/// Whether presents go behind the lines.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_PRESENT_LATER").is_some_and(|v| v == "1");
        if on {
            tracing::info!("a present is shown by our thread once the lines before it have run (VICTORY_PRESENT_LATER=1)");
        }
        on
    })
}

/// A present waiting for the lines: the title's resource and the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Due {
    pub(crate) resource: usize,
    pub(crate) frame: u64,
}

/// The present the lines are running towards; the title waits on the
/// condvar while there is one (one frame ahead at most).
static DUE: (Mutex<Option<Due>>, Condvar) = (Mutex::new(None), Condvar::new());

fn lock() -> std::sync::MutexGuard<'static, Option<Due>> {
    DUE.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// The title's side: waits until the last present was shown, then `mark`
/// puts the marks in the lines (under the lock, so our thread never sees
/// marks without the present they belong to).
pub(crate) fn give(due: Due, mark: impl FnOnce()) {
    let mut now = lock();
    while now.is_some() {
        now = DUE.1.wait(now).unwrap_or_else(|e| e.into_inner());
    }
    mark();
    *now = Some(due);
}

/// The present the lines run towards, if any.
pub(crate) fn due() -> Option<Due> {
    *lock()
}

/// Our side, when every line has reached its mark: `unmark` takes the
/// marks off, then the present is handed back to be shown and the title
/// may give the next one.
pub(crate) fn take(unmark: impl FnOnce()) -> Option<Due> {
    let mut now = lock();
    let due = now.take()?;
    unmark();
    DUE.1.notify_all();
    Some(due)
}

/// Tests that use the present due take turns.
#[cfg(test)]
pub(crate) static TEST_ONE: Mutex<()> = Mutex::new(());

/// Whether a line stands at its mark, as far as the present goes: at the
/// mark, or empty (nothing before the present).
pub(crate) fn line_ready(head_is_mark: Option<bool>) -> bool {
    head_is_mark.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_present_waits_until_the_first_is_shown() {
        let _one = TEST_ONE.lock().unwrap_or_else(|e| e.into_inner());
        let first = Due {
            resource: 1,
            frame: 1,
        };
        let second = Due {
            resource: 1,
            frame: 2,
        };
        give(first, || {});
        assert_eq!(due(), Some(first));
        let title = std::thread::spawn(move || give(second, || {}));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(due(), Some(first), "the title went two frames ahead");
        assert_eq!(take(|| {}), Some(first));
        title.join().unwrap();
        assert_eq!(due(), Some(second));
        assert_eq!(take(|| {}), Some(second));
        assert_eq!(take(|| {}), None);
    }

    #[test]
    fn an_empty_line_does_not_hold_a_present() {
        assert!(line_ready(None));
        assert!(line_ready(Some(true)));
        assert!(!line_ready(Some(false)));
    }
}

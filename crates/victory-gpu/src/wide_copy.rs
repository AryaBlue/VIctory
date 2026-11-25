//! A flush's byte moves on the wide threads (`VICTORY_WIDE_COPIES=1`; off
//! by default).
//!
//! Why: in the s2-prep-1 profile the submitting thread spent about 100 ms a
//! frame in `Shadow::update`, 33 of them moving title bytes into host copies
//! (`copy_mapped`), while the four wide threads stood 98 % idle.
//!
//! With the switch a flush's refreshes are decided on the submitting thread
//! as before (the write-watch journal, the guards, the copies' stamps and
//! notes stay single-writer, in order); the byte moves they decide are only
//! noted, and made after the last refresh of the flush, on the wide threads
//! and this one, before the flush returns. Nothing reads a copy's bytes in
//! between: the flush's refreshes only look at stamps and notes.

use std::cell::RefCell;

/// Whether a flush's moves go wide (not with `VICTORY_REFRESH_CHECK=1`,
/// which reads a copy right after its refresh).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_WIDE_COPIES").is_some_and(|v| v == "1")
            && !std::env::var_os("VICTORY_REFRESH_CHECK").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                "a flush's moves of title bytes run on the wide threads (VICTORY_WIDE_COPIES=1)"
            );
        }
        on
    })
}

/// A move noted: `len` bytes from title address `src` into host memory at `dst`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Noted {
    pub(crate) dst: usize,
    pub(crate) src: usize,
    pub(crate) len: usize,
}

thread_local! {
    static NOTING: RefCell<Option<Vec<Noted>>> = const { RefCell::new(None) };
}

/// Whether this thread notes its moves instead of making them.
pub(crate) fn noting() -> bool {
    NOTING.with(|n| n.borrow().is_some())
}

/// Notes a move (with [`noting`]).
pub(crate) fn note(dst: usize, src: usize, len: usize) {
    NOTING.with(|n| {
        if let Some(list) = n.borrow_mut().as_mut()
            && len > 0
        {
            list.push(Noted { dst, src, len });
        }
    });
}

/// Runs `f` with this thread's moves noted, and returns them.
pub(crate) fn noted_in(f: impl FnOnce()) -> Vec<Noted> {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            NOTING.with(|n| *n.borrow_mut() = None);
        }
    }
    NOTING.with(|n| *n.borrow_mut() = Some(Vec::new()));
    let stop = Stop;
    f();
    let list = NOTING.with(|n| n.borrow_mut().take()).unwrap_or_default();
    drop(stop);
    list
}

/// About this many bytes to a piece of work.
const PIECE: usize = 1 << 20;
/// Below this in all, the moves stay on this thread: waking the others
/// costs more.
const ALONE: usize = 256 << 10;

/// `noted` as pieces of about [`PIECE`] bytes: long moves cut, short ones
/// together, in order.
pub(crate) fn pieces(noted: &[Noted]) -> Vec<Vec<Noted>> {
    let mut out: Vec<Vec<Noted>> = Vec::new();
    let mut group: Vec<Noted> = Vec::new();
    let mut bytes = 0;
    for m in noted {
        let mut at = 0;
        while at < m.len {
            let n = (PIECE - bytes).min(m.len - at);
            group.push(Noted {
                dst: m.dst + at,
                src: m.src + at,
                len: n,
            });
            at += n;
            bytes += n;
            if bytes >= PIECE {
                out.push(std::mem::take(&mut group));
                bytes = 0;
            }
        }
    }
    if !group.is_empty() {
        out.push(group);
    }
    out
}

/// Makes the moves with `mv` (destination, source, bytes), on the wide
/// threads and this one; returns when all are made.
pub(crate) fn make(noted: Vec<Noted>, mv: fn(usize, usize, usize)) {
    if noted.iter().map(|m| m.len).sum::<usize>() < ALONE {
        for m in noted {
            mv(m.dst, m.src, m.len);
        }
        return;
    }
    let jobs: Vec<Box<dyn FnOnce() + Send>> = pieces(&noted)
        .into_iter()
        .map(|group| {
            Box::new(move || {
                for m in group {
                    mv(m.dst, m.src, m.len);
                }
            }) as Box<dyn FnOnce() + Send>
        })
        .collect();
    crate::shadow::wide_run(jobs);
}

/// A move of a host copy's refresh: title memory through the kernel, as
/// `crate::host::copy_mapped` does it.
pub(crate) fn title_move(dst: usize, src: usize, len: usize) {
    crate::host::copy_mapped(dst as *mut u8, src as *const u8, len, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_are_noted_only_inside() {
        note(1, 2, 3);
        let noted = noted_in(|| {
            assert!(noting());
            note(10, 20, 30);
            note(11, 21, 0);
        });
        assert!(!noting());
        assert_eq!(
            noted,
            vec![Noted {
                dst: 10,
                src: 20,
                len: 30
            }]
        );
    }

    #[test]
    fn pieces_cover_every_byte_once_in_order() {
        let noted = vec![
            Noted {
                dst: 0,
                src: 1 << 30,
                len: 3 * PIECE + 5,
            },
            Noted {
                dst: 10 << 20,
                src: 2 << 30,
                len: 7,
            },
            Noted {
                dst: 20 << 20,
                src: 3 << 30,
                len: PIECE - 1,
            },
        ];
        let pieces = pieces(&noted);
        assert!(
            pieces
                .iter()
                .all(|g| g.iter().map(|m| m.len).sum::<usize>() <= PIECE)
        );
        let flat: Vec<Noted> = pieces.into_iter().flatten().collect();
        let mut want = Vec::new();
        for m in &noted {
            for b in 0..m.len {
                want.push((m.dst + b, m.src + b));
            }
        }
        let got: Vec<(usize, usize)> = flat
            .iter()
            .flat_map(|m| (0..m.len).map(move |b| (m.dst + b, m.src + b)))
            .collect();
        assert_eq!(got, want);
    }

    /// Title memory moved wide lands as it would have one move at a time.
    #[test]
    fn wide_moves_copy_every_byte() {
        let src: Vec<u8> = (0..(5 * PIECE + 1234))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let mut dst = vec![0u8; src.len()];
        let (s, d) = (src.as_ptr() as usize, dst.as_mut_ptr() as usize);
        let mut noted = Vec::new();
        let mut at = 0;
        let mut k = 0;
        while at < src.len() {
            let n = [3, 4096, 70_000, PIECE + 17, 1][k % 5].min(src.len() - at);
            noted.push(Noted {
                dst: d + at,
                src: s + at,
                len: n,
            });
            at += n;
            k += 1;
        }
        make(noted, title_move);
        assert!(dst == src);
    }
}

//! heap records mirrored ahead of the submit thread (VICTORY_HEAP_SYNC_AHEAD=1, off by default)

use super::{SHADER_HEAPS, SYNCING, heap_parts, sync_range};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

// whether a helper thread mirrors changed heap pages ahead of submissions
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SYNC_AHEAD").is_some_and(|v| v == "1"))
}

// pages taken from the write watch and not mirrored yet: heap, its start, title range; only touched under SYNCING
static QUEUE: Mutex<Vec<(usize, u64, usize, usize)>> = Mutex::new(Vec::new());

// submit passes waiting for or holding SYNCING: the helper stays off it meanwhile
static WANTED: AtomicUsize = AtomicUsize::new(0);

// pages the helper mirrors under one taking of SYNCING
const CHUNK: usize = 2;

// since the last line: helper pages, records mirrored, helper us, submit passes, us they waited for SYNCING, pages they drained
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

// a submit pass is about to take SYNCING: starts the helper on first use
pub(super) fn submit_begin() -> Option<std::time::Instant> {
    if !on() {
        return None;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("victory-heap-ahead".into()).spawn(helper);
    });
    WANTED.fetch_add(1, Relaxed);
    Some(std::time::Instant::now())
}

// the submit pass holds SYNCING now
pub(super) fn submit_waited(asking: Option<std::time::Instant>) {
    if let Some(at) = asking {
        STATS[3].fetch_add(1, Relaxed);
        STATS[4].fetch_add(at.elapsed().as_micros() as u64, Relaxed);
    }
}

// the submit pass is done
pub(super) fn submit_end() {
    if on() {
        WANTED.fetch_sub(1, Relaxed);
    }
}

// under SYNCING: mirrors every page still queued (heaps gone since are skipped)
pub(super) fn drain(heaps: &[(u64, u64, usize)], page_bytes: &mut Vec<u8>, counts: &mut [u64; 3]) {
    let queued = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    if queued.is_empty() {
        return;
    }
    STATS[5].fetch_add(queued.len() as u64, Relaxed);
    run(heaps, &queued, page_bytes, counts);
}

fn run(heaps: &[(u64, u64, usize)], pages: &[(usize, u64, usize, usize)], page_bytes: &mut Vec<u8>, counts: &mut [u64; 3]) {
    for &(at, start, from, to) in pages {
        if !heaps.iter().any(|&(s, _, h)| h == at && s == start) {
            continue;
        }
        if let Some((parts, _)) = heap_parts(at, start) {
            sync_range(&parts, (from, to), page_bytes, counts);
        }
    }
}

// one step of the helper: with nothing queued takes every heap's changed pages; then mirrors up to `chunk` of them. Whether it mirrored any
pub(super) fn step(page_bytes: &mut Vec<u8>, chunk: usize) -> bool {
    let started = std::time::Instant::now();
    let _one = SYNCING.lock().unwrap_or_else(|e| e.into_inner());
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if queue.is_empty() {
        for &(start, end, at) in heaps.iter() {
            let Some((_, written)) = heap_parts(at, start) else {
                continue;
            };
            for (from, to) in written.take(start as usize, end as usize) {
                // page by page, so a step can stop between them
                let mut page = from & !(crate::watch::PAGE - 1);
                while page < to {
                    queue.push((at, start, page.max(from), (page + crate::watch::PAGE).min(to)));
                    page += crate::watch::PAGE;
                }
            }
        }
        // done from the front
        queue.reverse();
    }
    let n = queue.len().min(chunk);
    let keep = queue.len() - n;
    let mut now: Vec<(usize, u64, usize, usize)> = queue.split_off(keep);
    now.reverse();
    drop(queue);
    let mut counts = [0u64; 3];
    run(&heaps, &now, page_bytes, &mut counts);
    STATS[0].fetch_add(counts[0], Relaxed);
    STATS[1].fetch_add(counts[2], Relaxed);
    STATS[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    n > 0
}

fn helper() {
    let mut page_bytes = Vec::new();
    loop {
        if WANTED.load(Relaxed) > 0 {
            std::thread::sleep(std::time::Duration::from_micros(200));
            continue;
        }
        if !step(&mut page_bytes, CHUNK) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

// with the frame line
pub(super) fn line(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64 / frames).collect();
    tracing::info!(
        "heap records mirrored ahead per frame (VICTORY_HEAP_SYNC_AHEAD=1): the helper did {:.1} pages, {:.1} records mirrored, \
         {:.2} ms; {:.1} submit passes waited {:.2} ms for it and mirrored {:.1} pages it had taken",
        s[0],
        s[1],
        s[2] / 1000.0,
        s[3],
        s[4] / 1000.0,
        s[5],
    );
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::object::{S_OK, slot};
    use super::step;
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[repr(C)]
    struct Extended(HeapDesc, Placement);

    fn create(desc: HeapDesc, placement: Placement, out: &mut This) -> crate::object::HRESULT {
        let extended = Extended(desc, placement);
        create_heap(std::ptr::null(), &extended.0, &HEAP_IIDS[0], out)
    }

    // a placed shader-visible sampler heap of 64 slots in write-watched memory
    fn sampler_heap() -> (*mut std::ffi::c_void, This) {
        // SAFETY: a fresh allocation, released by the test
        let memory = unsafe { VirtualAlloc(std::ptr::null(), crate::watch::PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) };
        assert!(!memory.is_null());
        let desc = HeapDesc { kind: SAMPLERS, count: 64, flags: SHADER_VISIBLE, nodes: 1 };
        let placement = Placement { address: memory as u64, size: 64 * 16 };
        let mut h: This = std::ptr::null();
        assert_eq!(create(desc, placement, &mut h), S_OK);
        (memory, h)
    }

    fn release(memory: *mut std::ffi::c_void, h: This) {
        slot::<extern "system" fn(This) -> u32>(h, 0x10)(h);
        // SAFETY: the test's allocation
        unsafe { VirtualFree(memory, 0, MEM_RELEASE) };
    }

    // a sampler record made elsewhere, as the title makes one before copying it
    fn sampler(filter: u32) -> [u64; 2] {
        let mut record = [0u64; 2];
        let words = [filter, 3, 3, 3, 0, 1, 0, 0, 0, 0, 0, 0x7f7f_ffff, 0];
        create_sampler(std::ptr::null(), &words, record.as_mut_ptr() as u64);
        record
    }

    // the title copies a record into slot `index` as bytes
    fn copy_in(memory: *mut std::ffi::c_void, index: usize, record: &[u64; 2]) {
        // SAFETY: the slot and the record both hold 16 bytes
        unsafe { std::ptr::copy_nonoverlapping(record.as_ptr().cast::<u8>(), memory.cast::<u8>().add(index * 16), 16) };
    }

    // a record written after the helper's pass is mirrored by the submit pass
    #[test]
    fn a_record_written_after_the_helper_is_mirrored_at_submit() {
        let (memory, h) = sampler_heap();
        if let Some(host) = heap(h).host() {
            let (a, b) = (sampler(21), sampler(5));
            copy_in(memory, 5, &a);
            let mut bytes = Vec::new();
            while step(&mut bytes, 2) {}
            assert!(host.made(5), "the helper mirrored nothing");
            copy_in(memory, 9, &b);
            assert!(!host.made(9));
            sync_heaps();
            assert!(host.made(9), "the submit pass missed a record written after the helper");
        }
        release(memory, h);
    }

    // pages the helper took and had not mirrored yet are mirrored by the submit pass
    #[test]
    fn pages_the_helper_took_are_mirrored_at_submit() {
        let (memory, h) = sampler_heap();
        if let Some(host) = heap(h).host() {
            copy_in(memory, 7, &sampler(21));
            let mut bytes = Vec::new();
            // taken from the write watch, none mirrored
            assert!(!step(&mut bytes, 0));
            assert!(!host.made(7));
            sync_heaps();
            assert!(host.made(7), "a page the helper took was lost");
        }
        release(memory, h);
    }

    // the same writes with and without the helper's passes leave the same host slots
    #[test]
    fn host_heaps_match_with_and_without_the_helper() {
        let (a, b, c) = (sampler(21), sampler(5), sampler(1));
        let run = |helper: bool| -> Option<Vec<u64>> {
            let (memory, h) = sampler_heap();
            let mut keys = None;
            if let Some(host) = heap(h).host() {
                let mut bytes = Vec::new();
                copy_in(memory, 1, &a);
                copy_in(memory, 2, &b);
                if helper {
                    while step(&mut bytes, 2) {}
                }
                copy_in(memory, 3, &c);
                copy_in(memory, 1, &c);
                if helper {
                    // taken, left for the submit pass
                    step(&mut bytes, 0);
                }
                copy_in(memory, 2, &[0, 0]);
                copy_in(memory, 4, &a);
                sync_heaps();
                keys = Some((0..64).map(|i| host.made_key(i)).collect());
            }
            release(memory, h);
            keys
        };
        let (Some(off), Some(on)) = (run(false), run(true)) else {
            return;
        };
        assert!(off.iter().filter(|&&k| k != 0).count() >= 3, "nothing made");
        assert_eq!(off, on);
    }
}

//! Byte patches written into a loaded module after relocation, before TLS
//! redirection and import binding. `VICTORY_PATCH=<module>:<rva>:<hex>,...`
//! writes anything; named switches (`VICTORY_NO_WATCHDOG=1`) write known
//! patches only where the bytes they replace match. Every write is logged.
//! The raw knob follows Halfect's upstream PR #1059.

use std::env;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Patch {
    // file name, case-insensitive; `None` tries every module
    module: Option<String>,
    rva: usize,
    // bytes that must be there first; `None` writes blind
    expect: Option<Vec<u8>>,
    bytes: Vec<u8>,
}

// the title's hang watchdog thread (+0x2215a60) kills the process when the
// main loop's heartbeat (+0x71b6338) is 30 s old; frames that slow are normal
// here. `jne` on its quit flag becomes `jmp`, so the thread exits at once
// through its own epilogue. image b99f876b (930.048.020).
const WATCHDOG_RVA: usize = 0x2215ab8;
const WATCHDOG_OLD: [u8; 14] = [
    0x8a, 0x05, 0x82, 0x08, 0xfa, 0x04, 0xa8, 0x01, 0x0f, 0x85, 0x08, 0x03, 0x00, 0x00,
];
const WATCHDOG_NEW: [u8; 14] = [
    0x8a, 0x05, 0x82, 0x08, 0xfa, 0x04, 0xa8, 0x01, 0x90, 0xe9, 0x08, 0x03, 0x00, 0x00,
];

fn watchdog() -> Patch {
    Patch {
        module: None,
        rva: WATCHDOG_RVA,
        expect: Some(WATCHDOG_OLD.to_vec()),
        bytes: WATCHDOG_NEW.to_vec(),
    }
}

fn hex(s: &str) -> Option<usize> {
    usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().trim_start_matches("0x");
    if s.is_empty() || !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn on(name: &str) -> bool {
    env::var(name).is_ok_and(|v| v == "1")
}

pub(crate) fn from_env() -> Vec<Patch> {
    let mut patches = parse(&env::var("VICTORY_PATCH").unwrap_or_default());
    if on("VICTORY_NO_WATCHDOG") {
        patches.push(watchdog());
    }
    patches
}

// malformed entries are skipped, like the other knobs
fn parse(spec: &str) -> Vec<Patch> {
    spec.split(',')
        .filter_map(|entry| {
            let mut parts = entry.splitn(3, ':');
            let module = parts.next()?.trim();
            let rva = hex(parts.next()?)?;
            let bytes = hex_bytes(parts.next()?)?;
            (!module.is_empty()).then(|| Patch {
                module: Some(module.into()),
                rva,
                expect: None,
                bytes,
            })
        })
        .collect()
}

fn planned<'a>(patches: &'a [Patch], module: &str, size: usize) -> Vec<&'a Patch> {
    patches
        .iter()
        .filter(|p| {
            p.module
                .as_ref()
                .is_none_or(|m| m.eq_ignore_ascii_case(module))
        })
        .filter(|p| {
            p.rva
                .checked_add(p.bytes.len())
                .is_some_and(|end| end <= size)
        })
        .collect()
}

// the replaced bytes, or what was found when it isn't what the patch expects
fn write(image: &mut [u8], patch: &Patch) -> Result<Vec<u8>, Vec<u8>> {
    let range = patch.rva..patch.rva + patch.bytes.len();
    let old = image[range.clone()].to_vec();
    if patch.expect.as_ref().is_some_and(|e| *e != old) {
        return Err(old);
    }
    image[range].copy_from_slice(&patch.bytes);
    Ok(old)
}

#[cfg(windows)]
pub(crate) fn apply(base: usize, size: usize, module: &str) {
    use std::ffi::c_void;

    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};

    let patches = from_env();
    let named = patches
        .iter()
        .filter(|p| {
            p.module
                .as_ref()
                .is_some_and(|m| m.eq_ignore_ascii_case(module))
        })
        .count();
    let planned = planned(&patches, module, size);
    let fitting = planned.iter().filter(|p| p.module.is_some()).count();
    if named > fitting {
        tracing::warn!(
            module,
            skipped = named - fitting,
            "VICTORY_PATCH entries outside the image, skipped"
        );
    }
    for patch in planned {
        let at = base + patch.rva;
        let len = patch.bytes.len();
        let mut protection = 0;
        // SAFETY: inside the mapped image (checked by `planned`); .text is
        // read-only, so lift the protection for the write and restore it.
        if unsafe {
            VirtualProtect(
                at as *const c_void,
                len,
                PAGE_EXECUTE_READWRITE,
                &mut protection,
            )
        } == 0
        {
            tracing::warn!(module, rva = format_args!("{:#x}", patch.rva), error = %std::io::Error::last_os_error(), "VirtualProtect failed; patch skipped");
            continue;
        }
        // SAFETY: in bounds of the mapped image, writable above.
        let result = write(
            unsafe { std::slice::from_raw_parts_mut(at as *mut u8, len) },
            patch,
        );
        let mut ignored = 0;
        // SAFETY: restores the range's own protection.
        unsafe { VirtualProtect(at as *const c_void, len, protection, &mut ignored) };
        match result {
            Ok(old) => tracing::info!(
                module,
                rva = format_args!("{:#x}", patch.rva),
                old = format_args!("{old:02x?}"),
                new = format_args!("{:02x?}", patch.bytes),
                "patch applied"
            ),
            // a known patch on another module or build: quiet unless named
            Err(found) if patch.module.is_some() => tracing::warn!(
                module,
                rva = format_args!("{:#x}", patch.rva),
                found = format_args!("{found:02x?}"),
                "patch skipped, bytes differ"
            ),
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries_like_the_other_knobs() {
        let patches = parse("game.exe:247af2d:90, Game.DLL : 0x1c8 : 90cc ");
        assert_eq!(
            patches,
            vec![
                Patch {
                    module: Some("game.exe".into()),
                    rva: 0x247af2d,
                    expect: None,
                    bytes: vec![0x90],
                },
                Patch {
                    module: Some("Game.DLL".into()),
                    rva: 0x1c8,
                    expect: None,
                    bytes: vec![0x90, 0xcc],
                },
            ]
        );
    }

    #[test]
    fn skips_malformed_entries() {
        let patches = parse(
            "game.exe:10, game.exe:zz:90, game.exe:10:9, game.exe:10:0x, game.exe:10:9g, :10:90",
        );
        assert!(patches.is_empty(), "{patches:?}");
        let patches = parse("game.exe:zz:90,game.exe:10:9091");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].bytes, vec![0x90, 0x91]);
        assert!(parse("").is_empty());
    }

    #[test]
    fn planned_matches_the_module_and_the_image_bounds() {
        let patches = parse("game.exe:0:90, GAME.EXE:e:9091, game.exe:f:9090, other.exe:0:90");
        let picked = planned(&patches, "game.exe", 0x10);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].rva, 0);
        assert_eq!(picked[1].rva, 0xe);
        assert_eq!(planned(&patches, "Game.Exe", 0x10).len(), 2);
        assert!(planned(&patches, "unrelated.exe", 0x10).is_empty());
    }

    #[test]
    fn planned_cannot_overflow_the_image_end() {
        let patches = parse(&format!("game.exe:{:x}:9090", usize::MAX - 1));
        assert!(planned(&patches, "game.exe", 0x10).is_empty());
    }

    #[test]
    fn write_replaces_and_reports_the_old_bytes() {
        let patches = parse("game.exe:4:010203");
        let planned = planned(&patches, "game.exe", 16);
        let mut image = [0xccu8; 16];
        assert_eq!(write(&mut image, planned[0]), Ok(vec![0xcc; 3]));
        assert_eq!(&image[4..7], &[1, 2, 3]);
        assert_eq!(image[7], 0xcc);
    }

    #[test]
    fn watchdog_patch_turns_the_quit_check_into_a_jump_to_the_same_exit() {
        let mut image = vec![0u8; WATCHDOG_RVA + 0x40];
        image[WATCHDOG_RVA..WATCHDOG_RVA + 14].copy_from_slice(&WATCHDOG_OLD);
        let patch = watchdog();
        let picked = planned(
            std::slice::from_ref(&patch),
            "victory-image-anything.dll",
            image.len(),
        );
        assert_eq!(picked.len(), 1);
        assert_eq!(write(&mut image, picked[0]), Ok(WATCHDOG_OLD.to_vec()));
        let at = WATCHDOG_RVA + 8;
        // jne rel32 (0f 85, ends at +6) -> nop; jmp rel32 (e9, ends at +6)
        assert_eq!(&image[at..at + 2], &[0x90, 0xe9]);
        let rel = i32::from_le_bytes(image[at + 2..at + 6].try_into().unwrap());
        assert_eq!(at as i64 + 6 + rel as i64, 0x2215dce);
    }

    #[test]
    fn watchdog_patch_leaves_other_bytes_alone() {
        let mut image = vec![0x90u8; WATCHDOG_RVA + 0x40];
        let before = image.clone();
        assert!(write(&mut image, &watchdog()).is_err());
        assert_eq!(image, before);
        assert!(planned(&[watchdog()], "small.dll", 0x1000).is_empty());
    }
}

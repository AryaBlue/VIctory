//! fault-safe copy of our own memory without the kernel (VICTORY_FAST_READ)

use std::sync::OnceLock;

// leaf function, volatile registers only, no stack use: needs no unwind data
std::arch::global_asm!(
    ".text",
    ".p2align 4",
    ".globl victory_safe_copy",
    "victory_safe_copy:",
    "    cmp r8, 64",
    "    jb victory_safe_copy_16",
    "victory_safe_copy_64:",
    "    movdqu xmm0, xmmword ptr [rdx]",
    "    movdqu xmm1, xmmword ptr [rdx + 16]",
    "    movdqu xmm2, xmmword ptr [rdx + 32]",
    "    movdqu xmm3, xmmword ptr [rdx + 48]",
    "    movdqu xmmword ptr [rcx], xmm0",
    "    movdqu xmmword ptr [rcx + 16], xmm1",
    "    movdqu xmmword ptr [rcx + 32], xmm2",
    "    movdqu xmmword ptr [rcx + 48], xmm3",
    "    add rdx, 64",
    "    add rcx, 64",
    "    sub r8, 64",
    "    cmp r8, 64",
    "    jae victory_safe_copy_64",
    "victory_safe_copy_16:",
    "    cmp r8, 16",
    "    jb victory_safe_copy_1",
    "    movdqu xmm0, xmmword ptr [rdx]",
    "    movdqu xmmword ptr [rcx], xmm0",
    "    add rdx, 16",
    "    add rcx, 16",
    "    sub r8, 16",
    "    jmp victory_safe_copy_16",
    "victory_safe_copy_1:",
    "    test r8, r8",
    "    jz victory_safe_copy_done",
    "    movzx eax, byte ptr [rdx]",
    "    mov byte ptr [rcx], al",
    "    inc rdx",
    "    inc rcx",
    "    dec r8",
    "    jmp victory_safe_copy_1",
    "victory_safe_copy_done:",
    "    mov eax, 1",
    "    ret",
    ".globl victory_safe_copy_end",
    "victory_safe_copy_end:",
    ".globl victory_safe_copy_fault",
    "victory_safe_copy_fault:",
    "    xor eax, eax",
    "    ret",
    // real addresses, not incremental-link thunks
    ".data",
    ".p2align 3",
    ".globl victory_safe_copy_bounds",
    "victory_safe_copy_bounds:",
    "    .quad victory_safe_copy",
    "    .quad victory_safe_copy_end",
    "    .quad victory_safe_copy_fault",
    ".text",
);

unsafe extern "system" {
    fn victory_safe_copy(dst: *mut u8, src: *const u8, len: usize) -> u32;
    static victory_safe_copy_bounds: [u64; 3];
}

/// title reads skip ReadProcessMemory (VICTORY_FAST_READ=1; off by default)
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FAST_READ").is_some_and(|v| v == "1"))
}

/// false if a page faulted; dst may hold the bytes before it, as with rpm
pub(crate) fn copy(dst: *mut u8, src: *const u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    install();
    // SAFETY: a fault inside the loop resumes at its failure exit.
    unsafe { victory_safe_copy(dst, src, len) != 0 }
}

fn install() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        // access violation, in-page error, guard page
        const CODES: [u32; 3] = [0xC000_0005, 0xC000_0006, 0x8000_0001];
        // SAFETY: the system passes the exception being dispatched.
        let (r, c) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
        // SAFETY: written once by the loader's relocations.
        let [start, end, fault] = unsafe { victory_safe_copy_bounds };
        if !CODES.contains(&(r.ExceptionCode as u32)) || c.Rip < start || c.Rip >= end {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        c.Rip = fault;
        EXCEPTION_CONTINUE_EXECUTION
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only redirects faults raised inside our copy loop.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_fault));
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn copies_and_reports_unmapped_sources() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let src: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        for len in [0, 1, 15, 16, 63, 64, 65, 200, 1000] {
            let mut dst = vec![0u8; len];
            assert!(super::copy(dst.as_mut_ptr(), src.as_ptr(), len));
            assert_eq!(dst, src[..len]);
        }
        // SAFETY: a fresh reservation, freed below.
        let base = unsafe { VirtualAlloc(std::ptr::null(), 2 << 16, MEM_RESERVE, PAGE_NOACCESS) } as *mut u8;
        assert!(!base.is_null());
        // SAFETY: commits the first half of it.
        assert!(!unsafe { VirtualAlloc(base.cast_const().cast(), 1 << 16, MEM_COMMIT, PAGE_READWRITE) }.is_null());
        let mut dst = vec![0u8; 2 << 16];
        assert!(super::copy(dst.as_mut_ptr(), base, 1 << 16));
        assert!(!super::copy(dst.as_mut_ptr(), base, 2 << 16));
        // SAFETY: frees the reservation made above.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
    }
}

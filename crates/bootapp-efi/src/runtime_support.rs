//! Freestanding UTF-16 runtime support.
//!
//! Rust 1.99 lowers uefi's CStr16 pointer scan to the C `wcslen` symbol.
//! UEFI has no C runtime, so each executable must provide this leaf function.

/// Length of a null-terminated UEFI UTF-16 string, excluding the terminator.
///
/// # Safety
/// `text` must point to readable, aligned UTF-16 code units terminated by zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcslen(text: *const u16) -> usize {
    let mut len = 0;
    // Volatile loads prevent LLVM from replacing this implementation with a
    // call to the same unavailable C function (or recursively calling itself).
    // SAFETY: the caller guarantees readable units through the terminator.
    while unsafe { text.add(len).read_volatile() } != 0 {
        len += 1;
    }
    len
}

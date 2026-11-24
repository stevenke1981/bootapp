#![no_std]
#![no_main]

use core::arch::asm;
mod timer;

unsafe extern "C" {
    fn bios_read(drive: u32, lba: u32) -> u32;
    fn bios_ticks() -> u32;
    fn bios_key() -> u32;
    fn bios_chainload(drive: u32, lba: u32) -> u32;
    fn bios_boot_drive() -> u32;
    /// Chainloads the sector already in 0x8000 (no disk read); never returns.
    fn bios_chainload_buffer(drive: u32) -> u32;
    /// Protected-mode stack top chosen by stage2 (EBDA / INT 12h bounded).
    static bios_stack_top: u32;
}

/// Thunk sector buffer (EDD reads land here; chainload copies it to 0x7c00).
const SECTOR_BUFFER: usize = 0x8000;

/// Where an entry came from; decides its title and chainload target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// MBR (LBA 0) of another BIOS disk.
    Disk,
    /// Primary partition 1..=4 of BootApp's own disk; chainloads its VBR.
    Partition(u8),
    /// LBA 121 of BootApp's own disk: the MBR saved by the installer.
    SavedMbr,
}

#[derive(Clone, Copy)]
struct Entry {
    drive: u8,
    lba: u32,
    kind: u8,
    source: Source,
    partition: [u8; 16],
}

const EMPTY: Entry = Entry {
    drive: 0,
    lba: 0,
    kind: 0,
    source: Source::Disk,
    partition: [0; 16],
};

/// Own disk: four partitions plus the saved MBR; other disks: at most eight.
/// Rows 5..=20 hold the list, keeping the whole screen within 25 rows.
const MAX_ENTRIES: usize = 16;
/// Install layout: LBA 1..=120 stage2, LBA 121 saved original MBR.
const SAVED_MBR_LBA: u32 = 121;
/// Installer contract: every partition starts at or after this LBA.
const FIRST_PARTITION_LBA: u32 = 128;
/// Real-mode DS:SI partition entry handed to the chainloaded sector.
const PARTITION_ENTRY_ADDR: usize = 0x620;

struct EntryList {
    entries: [Entry; MAX_ENTRIES],
    len: usize,
}

impl EntryList {
    fn push(&mut self, entry: Entry) -> bool {
        if self.len >= MAX_ENTRIES {
            trace("BOOTAPP_BIOS_ENTRY_LIMIT\r\n");
            return false;
        }
        self.entries[self.len] = entry;
        self.len += 1;
        true
    }
}

unsafe fn out(port: u16, value: u8) {
    // SAFETY: this no_std binary only runs at BIOS ring 0 on x86.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

unsafe fn input(port: u16) -> u8 {
    let value;
    // SAFETY: same ring-0 I/O boundary as out().
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn serial_init() {
    // SAFETY: COM1 standard legacy UART; no physical memory pointers involved.
    unsafe {
        out(0x3f9, 0);
        out(0x3fb, 0x80);
        out(0x3f8, 1);
        out(0x3f9, 0);
        out(0x3fb, 3);
        out(0x3fa, 0xc7);
        out(0x3fc, 0x0b);
    }
}

fn serial_byte(byte: u8) {
    // Bound the wait so a missing UART cannot prevent boot.
    for _ in 0..100_000 {
        // SAFETY: COM1 line-status/data ports; ring-0 port I/O only.
        if unsafe { input(0x3fd) } & 0x20 != 0 {
            // SAFETY: transmitter holding register is empty (LSR bit 5).
            unsafe {
                out(0x3f8, byte);
            }
            return;
        }
    }
}

fn trace(s: &str) {
    for byte in s.bytes() {
        serial_byte(byte);
    }
}
fn hex(value: u32) {
    for shift in (0..8).rev() {
        serial_byte(b"0123456789ABCDEF"[((value >> (shift * 4)) & 15) as usize]);
    }
}

fn write_at(row: usize, column: usize, text: &str, selected: bool) {
    let color = if selected { 0x70u16 } else { 0x07 };
    for (i, byte) in text
        .bytes()
        .take(80usize.saturating_sub(column))
        .enumerate()
    {
        // SAFETY: BIOS text mode 3 is initialized by stage2; rows are bounded.
        if row < 25 {
            unsafe {
                core::ptr::write_volatile(
                    (0xb8000 + 2 * (row * 80 + column + i)) as *mut u16,
                    (color << 8) | u16::from(byte),
                );
            }
        }
    }
}

fn clear() {
    for row in 0..25 {
        write_at(
            row,
            0,
            "                                                                                ",
            false,
        );
    }
}
fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "Windows loader",
        2 => "Linux GRUB loader",
        _ => "Unknown bootable disk",
    }
}
fn sector_kind_name(kind: u8) -> &'static str {
    match kind {
        1 | 2 => kind_name(kind),
        _ => "Unknown",
    }
}
fn contains(data: &[u8], needle: &[u8]) -> bool {
    data.windows(needle.len()).any(|w| w == needle)
}

fn classify(data: &[u8; 512]) -> u8 {
    if contains(&data[..440], b"GRUB\0") || contains(&data[..440], b"GRUB ") {
        2
    } else if &data[3..11] == b"NTFS    "
        && (contains(data, b"BOOTMGR") || contains(data, b"NTLDR"))
    {
        1
    } else {
        0
    }
}

fn read(drive: u8, lba: u32, sector: &mut [u8; 512]) -> bool {
    // SAFETY: assembly thunk uses dedicated 0x8000 sector scratch and restores
    // protected-mode stack/segments. The output copy is bounded to 512 bytes.
    if unsafe { bios_read(u32::from(drive), lba) } != 0 {
        return false;
    }
    for (i, byte) in sector.iter_mut().enumerate() {
        // SAFETY: 0x8000..0x81ff is the thunk's identity-mapped sector buffer.
        *byte = unsafe { core::ptr::read_volatile((SECTOR_BUFFER + i) as *const u8) };
    }
    true
}

fn signed(sector: &[u8; 512]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xaa
}

fn partition_entry(mbr: &[u8; 512], index: usize) -> [u8; 16] {
    let mut entry = [0; 16];
    let offset = 446 + index * 16;
    entry.copy_from_slice(&mbr[offset..offset + 16]);
    entry
}

fn partition_lba(entry: &[u8; 16]) -> u32 {
    u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]])
}

/// Extended containers hold EBRs, not boot code; 0xEE marks a GPT disk.
fn chainloadable_type(kind: u8) -> bool {
    !matches!(kind, 0x00 | 0x05 | 0x0f | 0x85 | 0xee)
}

/// Bounded byte comparison written out so no libc memcmp/bcmp is required.
/// Boot code 0..440 that is all 0x00 or all 0xFF cannot be an MBR loader
/// (e.g. a disk that had a partition table but never boot code).
fn blank_boot_code(sector: &[u8; 512]) -> bool {
    let first = sector[0];
    if first != 0x00 && first != 0xff {
        return false;
    }
    let mut i = 1;
    while i < 440 {
        // SAFETY: i < 440 < 512 and `sector` is a valid array. Volatile keeps
        // the loop from being lowered into a libc call.
        if unsafe { core::ptr::read_volatile(sector.as_ptr().add(i)) } != first {
            return false;
        }
        i += 1;
    }
    true
}

/// Saved original MBR (LBA 121) is offered only when signed, not blank and
/// different from the current boot code (which is BootApp itself).
fn saved_mbr_bootable(saved: &[u8; 512], current: &[u8; 512]) -> bool {
    signed(saved) && !blank_boot_code(saved) && !same_boot_code(saved, current)
}

/// Boot the saved original MBR with the *current* disk signature and
/// partition table: LBA0[440..510] is copied over the saved sector in the
/// 0x8000 buffer, so partitions changed after installation are not booted
/// from a stale table. Returns only on failure.
fn chainload_saved_mbr(entry: &Entry) -> u32 {
    let mut current = [0; 512];
    if !read(entry.drive, 0, &mut current) || !signed(&current) {
        return 0x100;
    }
    // SAFETY: the thunk writes only the 0x8000 sector buffer.
    let status = unsafe { bios_read(u32::from(entry.drive), SAVED_MBR_LBA) };
    if status != 0 {
        return status;
    }
    let mut saved = [0; 512];
    for (i, byte) in saved.iter_mut().enumerate() {
        // SAFETY: 0x8000..0x81ff is the thunk's sector buffer, just filled.
        *byte = unsafe { core::ptr::read_volatile((SECTOR_BUFFER + i) as *const u8) };
    }
    if !saved_mbr_bootable(&saved, &current) {
        return 0x101;
    }
    for (i, byte) in current.iter().enumerate().take(510).skip(440) {
        // SAFETY: 0x8000+440..0x8000+510 lies inside the 512-byte buffer.
        unsafe {
            core::ptr::write_volatile((SECTOR_BUFFER + i) as *mut u8, *byte);
        }
    }
    trace("BOOTAPP_BIOS_CHAINLOAD drive=");
    hex(u32::from(entry.drive));
    trace(" lba=");
    hex(SAVED_MBR_LBA);
    trace(" table=current\r\n");
    // SAFETY: jumps to the patched 0x8000 sector via 0000:7c00; never returns.
    unsafe { bios_chainload_buffer(u32::from(entry.drive)) }
}

fn same_boot_code(a: &[u8; 512], b: &[u8; 512]) -> bool {
    let mut i = 0;
    while i < 440 {
        // SAFETY: i < 440 < 512 and both references are valid arrays. Volatile
        // reads keep LLVM from lowering this loop into a bcmp call.
        let (x, y) = unsafe {
            (
                core::ptr::read_volatile(a.as_ptr().add(i)),
                core::ptr::read_volatile(b.as_ptr().add(i)),
            )
        };
        if x != y {
            return false;
        }
        i += 1;
    }
    true
}

fn trace_detected(entry: &Entry) {
    trace("BOOTAPP_BIOS_DETECTED drive=");
    hex(u32::from(entry.drive));
    match entry.source {
        Source::Disk => {}
        Source::Partition(n) => {
            trace(" partition=");
            serial_byte(b'0' + n);
        }
        Source::SavedMbr => trace(" saved_mbr"),
    }
    trace(" lba=");
    hex(entry.lba);
    trace(" kind=");
    trace(match entry.source {
        Source::Disk => kind_name(entry.kind),
        _ => sector_kind_name(entry.kind),
    });
    trace("\r\n");
}

/// BootApp's own disk (install layout): signed primary-partition VBRs, then
/// the saved original MBR when it differs from the current boot code.
/// LBA 0 itself is never offered: it is BootApp stage1 and would recurse.
fn scan_own(own: u8, list: &mut EntryList) {
    let mut mbr = [0; 512];
    if !read(own, 0, &mut mbr) || !signed(&mbr) {
        trace("BOOTAPP_BIOS_OWN_MBR unreadable\r\n");
        return;
    }
    let mut sector = [0; 512];
    let mut active = [0; 16];
    for index in 0..4 {
        let entry = partition_entry(&mbr, index);
        let lba = partition_lba(&entry);
        if entry[0] == 0x80 {
            active = entry;
        }
        // lba >= 128 also rules out LBA 0 (recursion) and the stage2 area.
        if !chainloadable_type(entry[4]) || lba < FIRST_PARTITION_LBA {
            continue;
        }
        if read(own, lba, &mut sector) && signed(&sector) {
            let found = Entry {
                drive: own,
                lba,
                kind: classify(&sector),
                source: Source::Partition(index as u8 + 1),
                partition: entry,
            };
            if list.push(found) {
                trace_detected(&found);
            }
        }
    }
    if read(own, SAVED_MBR_LBA, &mut sector) && saved_mbr_bootable(&sector, &mbr) {
        let found = Entry {
            drive: own,
            lba: SAVED_MBR_LBA,
            kind: classify(&sector),
            source: Source::SavedMbr,
            partition: active,
        };
        if list.push(found) {
            trace_detected(&found);
        }
    }
}

fn scan(own: u8, list: &mut EntryList) {
    if own >= 0x80 {
        scan_own(own, list);
    }
    // SAFETY: BIOS Data Area hard-disk count; bounded even with faulty data.
    let count = unsafe { core::ptr::read_volatile(0x475 as *const u8) }.min(8);
    let mut sector = [0; 512];
    let mut vbr = [0; 512];
    for n in 0..count {
        let drive = 0x80 + n;
        if drive == own || !read(drive, 0, &mut sector) || !signed(&sector) {
            continue;
        }
        let mut kind = classify(&sector);
        let mut active_partition = [0; 16];
        for p in 0..4 {
            let part = partition_entry(&sector, p);
            let lba = partition_lba(&part);
            if part[4] == 0 || lba == 0 {
                continue;
            }
            if part[0] == 0x80 {
                active_partition = part;
            }
            if kind == 0 && read(drive, lba, &mut vbr) && signed(&vbr) {
                kind = classify(&vbr);
            }
        }
        let found = Entry {
            drive,
            lba: 0,
            kind,
            source: Source::Disk,
            partition: active_partition,
        };
        if list.push(found) {
            trace_detected(&found);
        }
    }
}

fn digits(value: u32, buffer: &mut [u8; 10]) -> &str {
    let mut start = buffer.len();
    let mut value = value;
    loop {
        start -= 1;
        buffer[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 || start == 0 {
            break;
        }
    }
    core::str::from_utf8(&buffer[start..]).unwrap_or("?")
}

const COUNTDOWN_ROW: usize = 22;
const BLANK: &str =
    "                                                                              ";

fn render_countdown(countdown: Option<u32>) {
    write_at(COUNTDOWN_ROW, 2, BLANK, false);
    match countdown {
        Some(seconds) => {
            let mut buffer = [0; 10];
            let number = digits(seconds, &mut buffer);
            write_at(COUNTDOWN_ROW, 2, "Automatic boot in ", false);
            write_at(COUNTDOWN_ROW, 20, number, false);
            write_at(
                COUNTDOWN_ROW,
                20 + number.len(),
                if seconds == 1 {
                    " second; any key cancels."
                } else {
                    " seconds; any key cancels."
                },
                false,
            );
        }
        None => write_at(COUNTDOWN_ROW, 2, "Automatic boot paused.", false),
    }
}

fn render_entry(row: usize, entry: &Entry, own: u8, selected: bool) {
    write_at(row, 2, if selected { ">" } else { " " }, selected);
    match entry.source {
        Source::Disk => write_at(row, 4, kind_name(entry.kind), selected),
        Source::Partition(n) => {
            let label = [b'0' + n];
            write_at(row, 4, "Partition ", selected);
            write_at(
                row,
                14,
                core::str::from_utf8(&label).unwrap_or("?"),
                selected,
            );
            write_at(row, 15, ": ", selected);
            write_at(row, 17, sector_kind_name(entry.kind), selected);
        }
        Source::SavedMbr => {
            write_at(row, 4, "Saved original MBR: ", selected);
            write_at(row, 24, sector_kind_name(entry.kind), selected);
        }
    }
    let index = entry.drive.wrapping_sub(0x80) & 0x7f;
    let drive = [b'0' + index / 10 % 10, b'0' + index % 10];
    write_at(row, 46, "BIOS disk #", selected);
    write_at(
        row,
        57,
        core::str::from_utf8(&drive).unwrap_or("??"),
        selected,
    );
    if entry.drive == own {
        write_at(row, 60, "(this disk)", selected);
    }
}

fn render(entries: &[Entry], own: u8, selected: usize, countdown: Option<u32>, failure: bool) {
    clear();
    write_at(1, 2, "BootApp 0.3 | Rust Legacy BIOS | x86", false);
    write_at(
        3,
        2,
        "Up/Down select  Enter boot  Esc cancel countdown",
        false,
    );
    for (index, entry) in entries.iter().enumerate().take(MAX_ENTRIES) {
        render_entry(5 + index, entry, own, index == selected);
    }
    if entries.is_empty() {
        write_at(
            6,
            2,
            "No signed BIOS boot disk or partition found. Attach an OS disk.",
            false,
        );
    }
    render_countdown(countdown);
    if failure {
        write_at(23, 2, "Disk read failed; select another entry.", false);
    }
}

fn trace_countdown(seconds: u32) {
    let mut buffer = [0; 10];
    trace("BOOTAPP_BIOS_COUNTDOWN ");
    trace(digits(seconds, &mut buffer));
    trace("\r\n");
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_main() -> ! {
    serial_init();
    trace("BOOTAPP_BIOS_READY\r\n");
    trace("BOOTAPP_BIOS_STACK top=");
    // SAFETY: stage2 writes this word once before calling rust_main.
    hex(unsafe { core::ptr::read_volatile(&raw const bios_stack_top) });
    trace("\r\n");
    // SAFETY: reads the byte stage1 stored at 0x600; no other side effect.
    let own = unsafe { bios_boot_drive() } as u8;
    let mut list = EntryList {
        entries: [EMPTY; MAX_ENTRIES],
        len: 0,
    };
    scan(own, &mut list);
    let len = list.len;
    let entries = &list.entries[..len];
    let mut selected = 0;
    let mut automatic = len != 0;
    // SAFETY: firmware thunk; INT 1Ah has no memory side effects for Rust.
    let started = unsafe { bios_ticks() };
    let mut failure = false;
    let mut shown = timer::remaining_seconds(0, timer::AUTO_BOOT_TICKS);
    render(entries, own, selected, automatic.then_some(shown), failure);
    if automatic {
        trace_countdown(shown);
    }
    loop {
        // SAFETY: firmware thunk; INT 16h polling only.
        let key = unsafe { bios_key() };
        let mut boot = false;
        if key != 0 {
            automatic = false;
            let ascii = (key & 0xff) as u8;
            let scan = ((key >> 8) & 0xff) as u8;
            if !entries.is_empty() {
                if scan == 0x48 {
                    selected = if selected == 0 { len - 1 } else { selected - 1 };
                }
                if scan == 0x50 {
                    selected = (selected + 1) % len;
                }
                boot = ascii == b'\r';
            }
            render(entries, own, selected, None, failure);
            trace("BOOTAPP_BIOS_KEY timeout=cancelled\r\n");
        } else if automatic {
            // SAFETY: firmware thunk; INT 1Ah only.
            let elapsed = timer::elapsed_ticks(started, unsafe { bios_ticks() });
            if elapsed >= timer::AUTO_BOOT_TICKS {
                boot = true;
                automatic = false;
            } else {
                let seconds = timer::remaining_seconds(elapsed, timer::AUTO_BOOT_TICKS);
                if seconds != shown {
                    shown = seconds;
                    render_countdown(Some(seconds));
                    trace_countdown(seconds);
                }
            }
        }
        if boot {
            let entry = entries[selected];
            // Conventional DS:SI partition pointer for VBR/MBR consumers. The
            // thunk then reads the boot sector to 0x8000..0x81ff, which this
            // 16-byte copy at 0x620..0x62f cannot overlap.
            for (i, byte) in entry.partition.iter().enumerate() {
                // SAFETY: 0x620..0x62f is reserved for this ABI in low memory.
                unsafe {
                    core::ptr::write_volatile((PARTITION_ENTRY_ADDR + i) as *mut u8, *byte);
                }
            }
            let status = if entry.source == Source::SavedMbr {
                chainload_saved_mbr(&entry)
            } else {
                trace("BOOTAPP_BIOS_CHAINLOAD drive=");
                hex(u32::from(entry.drive));
                trace(" lba=");
                hex(entry.lba);
                trace("\r\n");
                // SAFETY: on success the thunk never returns (real-mode jump to
                // 0000:7c00); on failure it restores protected mode and returns.
                unsafe { bios_chainload(u32::from(entry.drive), entry.lba) }
            };
            trace("BOOTAPP_BIOS_CHAINLOAD_ERROR status=");
            hex(status);
            trace("\r\n");
            failure = true;
            render(entries, own, selected, None, failure);
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    trace("BOOTAPP_BIOS_PANIC\r\n");
    loop {
        // SAFETY: ring 0; halting with interrupts disabled is the panic policy.
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}

// LLVM may lower bounded slice initialization/copies to these freestanding
// intrinsics. Volatile accesses prevent recursive lowering back into themselves.
/// # Safety
/// `dst` must be valid for `len` writable bytes, following C memset semantics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(
    dst: *mut core::ffi::c_void,
    value: i32,
    len: usize,
) -> *mut core::ffi::c_void {
    for i in 0..len {
        // SAFETY: i < len and the caller guarantees `dst` covers `len` bytes.
        unsafe {
            core::ptr::write_volatile(dst.cast::<u8>().add(i), value as u8);
        }
    }
    dst
}
/// # Safety
/// `dst` and `src` must be valid, nonoverlapping ranges of `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    len: usize,
) -> *mut core::ffi::c_void {
    for i in 0..len {
        // SAFETY: i < len and the caller guarantees both ranges cover `len` bytes.
        unsafe {
            core::ptr::write_volatile(
                dst.cast::<u8>().add(i),
                core::ptr::read_volatile(src.cast::<u8>().add(i)),
            );
        }
    }
    dst
}

// The precompiled Linux core archive retains a personality reference although
// this freestanding binary uses panic=abort and never unwinds.
#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}

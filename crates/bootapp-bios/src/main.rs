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
}

#[derive(Clone, Copy)]
struct Entry {
    drive: u8,
    lba: u32,
    kind: u8,
    partition: [u8; 16],
}

const EMPTY: Entry = Entry {
    drive: 0,
    lba: 0,
    kind: 0,
    partition: [0; 16],
};

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
        if unsafe { input(0x3fd) } & 0x20 != 0 {
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
        *byte = unsafe { core::ptr::read_volatile((0x8000 + i) as *const u8) };
    }
    true
}

fn signed(sector: &[u8; 512]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xaa
}

fn scan(entries: &mut [Entry; 32]) -> usize {
    let own = unsafe { bios_boot_drive() } as u8;
    // BIOS Data Area hard-disk count; bounded even with faulty firmware data.
    let count = unsafe { core::ptr::read_volatile(0x475 as *const u8) }.min(8);
    let mut len = 0;
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
            let offset = 446 + p * 16;
            let part = &sector[offset..offset + 16];
            let lba = u32::from_le_bytes([part[8], part[9], part[10], part[11]]);
            if part[4] == 0 || lba == 0 {
                continue;
            }
            if part[0] == 0x80 {
                active_partition.copy_from_slice(part);
            }
            if kind == 0 && read(drive, lba, &mut vbr) && signed(&vbr) {
                kind = classify(&vbr);
            }
        }
        entries[len] = Entry {
            drive,
            lba: 0,
            kind,
            partition: active_partition,
        };
        trace("BOOTAPP_BIOS_DETECTED drive=");
        hex(u32::from(drive));
        trace(" kind=");
        trace(kind_name(kind));
        trace("\r\n");
        len += 1;
    }
    len
}

fn render(entries: &[Entry], selected: usize, timer: bool, failure: bool) {
    clear();
    write_at(1, 2, "BootApp 0.2 | Rust Legacy BIOS | x86", false);
    write_at(
        3,
        2,
        "Up/Down select  Enter boot  Esc cancel countdown",
        false,
    );
    for (index, entry) in entries.iter().enumerate().take(16) {
        let row = 5 + index;
        write_at(
            row,
            2,
            if index == selected { ">" } else { " " },
            index == selected,
        );
        write_at(row, 4, kind_name(entry.kind), index == selected);
        let drive = [
            b'0' + ((entry.drive - 0x80) / 10),
            b'0' + ((entry.drive - 0x80) % 10),
        ];
        write_at(row, 29, "BIOS disk #", index == selected);
        write_at(
            row,
            41,
            core::str::from_utf8(&drive).unwrap_or("??"),
            index == selected,
        );
    }
    if entries.is_empty() {
        write_at(
            6,
            2,
            "No other signed BIOS boot disk found. Attach an OS disk.",
            false,
        );
    }
    write_at(
        22,
        2,
        if timer {
            "Automatic boot in 5 seconds; any key cancels."
        } else {
            "Automatic boot paused."
        },
        false,
    );
    if failure {
        write_at(23, 2, "Disk read failed; select another entry.", false);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_main() -> ! {
    serial_init();
    trace("BOOTAPP_BIOS_READY\r\n");
    let mut entries = [EMPTY; 32];
    let len = scan(&mut entries);
    let entries = &entries[..len];
    let mut selected = 0;
    let mut automatic = len != 0;
    let started = unsafe { bios_ticks() };
    let mut failure = false;
    render(entries, selected, automatic, failure);
    loop {
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
            render(entries, selected, automatic, failure);
            trace("BOOTAPP_BIOS_KEY timeout=cancelled\r\n");
        } else if automatic && timer::elapsed_ticks(started, unsafe { bios_ticks() }) >= 91 {
            boot = true;
            automatic = false;
        }
        if boot {
            let entry = entries[selected];
            // Conventional DS:SI active partition pointer for VBR/MBR consumers.
            for (i, byte) in entry.partition.iter().enumerate() {
                unsafe {
                    core::ptr::write_volatile((0x620 + i) as *mut u8, *byte);
                }
            }
            trace("BOOTAPP_BIOS_CHAINLOAD drive=");
            hex(u32::from(entry.drive));
            trace(" lba=");
            hex(entry.lba);
            trace("\r\n");
            let status = unsafe { bios_chainload(u32::from(entry.drive), entry.lba) };
            trace("BOOTAPP_BIOS_CHAINLOAD_ERROR status=");
            hex(status);
            trace("\r\n");
            failure = true;
            render(entries, selected, automatic, failure);
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    trace("BOOTAPP_BIOS_PANIC\r\n");
    loop {
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

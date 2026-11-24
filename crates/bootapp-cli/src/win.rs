//! Hand-written Win32 declarations (no external crates). Windows only.
//! Every call site documents its safety argument with a `// SAFETY:` comment.

use std::{
    ffi::{OsStr, c_void},
    fs::{File, OpenOptions},
    io,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
};

pub type Handle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Luid {
    pub low_part: u32,
    pub high_part: i32,
}

#[repr(C)]
pub struct TokenPrivileges {
    pub privilege_count: u32,
    pub luid: Luid,
    pub attributes: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn DeviceIoControl(
        device: Handle,
        code: u32,
        in_buffer: *const c_void,
        in_size: u32,
        out_buffer: *mut c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    pub fn GetLastError() -> u32;
    pub fn GetCurrentProcess() -> Handle;
    pub fn CloseHandle(handle: Handle) -> i32;
    pub fn GetFirmwareEnvironmentVariableExW(
        name: *const u16,
        guid: *const u16,
        buffer: *mut c_void,
        size: u32,
        attributes: *mut u32,
    ) -> u32;
    pub fn SetFirmwareEnvironmentVariableExW(
        name: *const u16,
        guid: *const u16,
        value: *const c_void,
        size: u32,
        attributes: u32,
    ) -> i32;
    pub fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    pub fn GetSystemWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn RegGetValueW(
        key: Handle,
        sub_key: *const u16,
        value: *const u16,
        flags: u32,
        value_type: *mut u32,
        data: *mut c_void,
        data_size: *mut u32,
    ) -> i32;
    pub fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    pub fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut Luid) -> i32;
    pub fn AdjustTokenPrivileges(
        token: Handle,
        disable_all: i32,
        new_state: *const TokenPrivileges,
        buffer_length: u32,
        previous: *mut TokenPrivileges,
        return_length: *mut u32,
    ) -> i32;
}

pub const ERROR_INVALID_FUNCTION: u32 = 1;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_ENVVAR_NOT_FOUND: u32 = 203;
pub const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

pub const IOCTL_DISK_GET_DRIVE_GEOMETRY: u32 = 0x0007_0000;
pub const IOCTL_DISK_GET_PARTITION_INFO_EX: u32 = 0x0007_0048;
pub const IOCTL_DISK_GET_DRIVE_LAYOUT_EX: u32 = 0x0007_0050;
pub const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405c;
pub const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;

pub fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

/// Win32 error code rendered with the system message text.
pub fn error_text(code: u32) -> String {
    format!(
        "Win32 error {code}: {}",
        io::Error::from_raw_os_error(code as i32)
    )
}

/// Open a device for IOCTL queries only (no read/write access requested).
pub fn open_query(path: &str) -> Result<File, String> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ_WRITE)
        .open(path)
        .map_err(|e| format!("cannot open {path} for query: {e}"))
}

/// Open a raw device for sector I/O.
pub fn open_device(path: &str, write: bool) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(write)
        .share_mode(FILE_SHARE_READ_WRITE)
        .open(path)
        .map_err(|e| {
            format!("cannot open {path} (administrator rights are required for raw disks): {e}")
        })
}

/// Run an output-only IOCTL; returns the bytes the driver reported.
pub fn ioctl_out(file: &File, code: u32, out_len: usize) -> Result<Vec<u8>, String> {
    // u64 storage keeps the buffer 8-byte aligned for LARGE_INTEGER fields.
    let mut storage = vec![0u64; out_len.div_ceil(8)];
    let size = u32::try_from(storage.len() * 8).map_err(|_| "IOCTL buffer too large")?;
    let mut returned = 0u32;
    // SAFETY: the handle is owned by `file` and open for the duration of the call;
    // the output pointer/size describe `storage`, which outlives the call; no input
    // buffer and no OVERLAPPED structure are passed (synchronous call).
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as Handle,
            code,
            std::ptr::null(),
            0,
            storage.as_mut_ptr().cast(),
            size,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError has no preconditions.
        let code_err = unsafe { GetLastError() };
        return Err(format!(
            "DeviceIoControl(0x{code:08X}) failed: {}",
            error_text(code_err)
        ));
    }
    let bytes: Vec<u8> = storage.iter().flat_map(|w| w.to_le_bytes()).collect();
    Ok(bytes[..(returned as usize).min(bytes.len())].to_vec())
}

pub fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| "IOCTL returned a truncated structure".to_string())
}

pub fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
        .ok_or_else(|| "IOCTL returned a truncated structure".to_string())
}

/// Physical disk numbers backing a volume (`\\.\C:`).
pub fn volume_disk_numbers(volume: &File) -> Result<Vec<(u32, u64, u64)>, String> {
    let out = ioctl_out(volume, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, 8 + 24 * 32)?;
    let count = u32_at(&out, 0)? as usize;
    if count == 0 || count > 32 {
        return Err(format!("volume reports {count} disk extents"));
    }
    (0..count)
        .map(|i| {
            let base = 8 + 24 * i;
            Ok((
                u32_at(&out, base)?,
                u64_at(&out, base + 8)?,
                u64_at(&out, base + 16)?,
            ))
        })
        .collect()
}

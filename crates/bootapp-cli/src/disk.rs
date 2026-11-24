//! Sector-level access to BIOS install targets and ESP volume identification.
//!
//! Targets are regular disk-image files (all platforms) or, on Windows only,
//! `\\.\PhysicalDriveN`. All I/O is whole 512-byte sectors at sector offsets.

use bootapp_core::loadopt::HardDriveNode;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub const SECTOR: usize = 512;

/// Whole-sector block I/O used by the BIOS installer (in-memory in tests).
pub trait SectorIo {
    fn sector_count(&self) -> u64;
    fn read_sectors(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), String>;
    fn write_sectors(&mut self, lba: u64, data: &[u8]) -> Result<(), String>;
    fn flush(&mut self) -> Result<(), String>;
}

fn check_span(lba: u64, len: usize, sectors: u64) -> Result<u64, String> {
    if len == 0 || !len.is_multiple_of(SECTOR) {
        return Err(format!(
            "I/O length {len} is not a positive multiple of {SECTOR}"
        ));
    }
    let count = (len / SECTOR) as u64;
    if lba.checked_add(count).is_none_or(|end| end > sectors) {
        return Err(format!(
            "I/O at LBA {lba} (+{count}) exceeds the target size of {sectors} sectors"
        ));
    }
    lba.checked_mul(SECTOR as u64)
        .ok_or_else(|| "sector offset overflow".to_string())
}

/// `\\.\PhysicalDriveN` (case-insensitive) → N.
pub fn physical_drive_number(path: &str) -> Option<u32> {
    let prefix = r"\\.\physicaldrive";
    if path.len() <= prefix.len() || !path.is_char_boundary(prefix.len()) {
        return None;
    }
    let (head, digits) = path.split_at(prefix.len());
    if !head.eq_ignore_ascii_case(prefix)
        || digits.len() > 3
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

pub struct FileDisk {
    file: File,
    sectors: u64,
    pub description: String,
}

impl SectorIo for FileDisk {
    fn sector_count(&self) -> u64 {
        self.sectors
    }
    fn read_sectors(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), String> {
        let offset = check_span(lba, buffer.len(), self.sectors)?;
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(buffer))
            .map_err(|e| format!("read at LBA {lba} failed: {e}"))
    }
    fn write_sectors(&mut self, lba: u64, data: &[u8]) -> Result<(), String> {
        let offset = check_span(lba, data.len(), self.sectors)?;
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(data))
            .map_err(|e| format!("write at LBA {lba} failed: {e}"))
    }
    fn flush(&mut self) -> Result<(), String> {
        self.file
            .flush()
            .and_then(|_| self.file.sync_all())
            .map_err(|e| format!("flush failed: {e}"))
    }
}

/// Open a BIOS install target. `write` is false for dry runs.
pub fn open_target(path: &str, write: bool, allow_system_disk: bool) -> Result<FileDisk, String> {
    if let Some(number) = physical_drive_number(path) {
        return open_physical(number, path, write, allow_system_disk);
    }
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("cannot inspect target {path}: {e}"))?;
    if !metadata.is_file() {
        return Err(format!(
            "target {path} is not a regular disk-image file (raw devices are accepted only as \\\\.\\PhysicalDriveN on Windows)"
        ));
    }
    let len = metadata.len();
    if !len.is_multiple_of(SECTOR as u64) {
        return Err(format!(
            "target image size {len} is not a multiple of {SECTOR} bytes"
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .open(path)
        .map_err(|e| format!("cannot open target {path}: {e}"))?;
    Ok(FileDisk {
        file,
        sectors: len / SECTOR as u64,
        description: format!("disk image file, {len} bytes"),
    })
}

#[cfg(not(windows))]
fn open_physical(_: u32, path: &str, _: bool, _: bool) -> Result<FileDisk, String> {
    Err(format!(
        "{path}: physical drives are supported only on Windows"
    ))
}

#[cfg(windows)]
fn open_physical(
    number: u32,
    path: &str,
    write: bool,
    allow_system_disk: bool,
) -> Result<FileDisk, String> {
    use crate::win;
    let protected = protected_disks()
        .map_err(|e| format!("cannot determine the system disk (refusing to continue): {e}"))?;
    if let Some((_, what)) = protected.iter().find(|(disk, _)| *disk == number) {
        if !allow_system_disk {
            return Err(format!(
                "{path} holds the {what}; refusing (pass --allow-system-disk only if you really intend this)"
            ));
        }
        eprintln!("WARNING: {path} holds the {what} (--allow-system-disk given).");
    }
    let file = win::open_device(path, write)?;
    let geometry = win::ioctl_out(&file, win::IOCTL_DISK_GET_DRIVE_GEOMETRY, 24)?;
    let bytes_per_sector = win::u32_at(&geometry, 20)?;
    if bytes_per_sector as usize != SECTOR {
        return Err(format!(
            "{path} uses {bytes_per_sector}-byte sectors; only 512-byte sectors are supported for BIOS MBR install"
        ));
    }
    let length = win::u64_at(
        &win::ioctl_out(&file, win::IOCTL_DISK_GET_LENGTH_INFO, 8)?,
        0,
    )?;
    Ok(FileDisk {
        file,
        sectors: length / SECTOR as u64,
        description: format!("physical drive {number}, {length} bytes"),
    })
}

/// Disks that must not be written without `--allow-system-disk`:
/// * every disk backing the running Windows volume (from
///   GetSystemWindowsDirectoryW, not the spoofable %SystemDrive%) — required,
///   failure refuses;
/// * every disk backing the firmware boot/system partition, from
///   `HKLM\SYSTEM\Setup\SystemPartition` (`\Device\HarddiskVolumeN`, opened via
///   `\\?\GLOBALROOT`). If that value is absent the limitation is reported as a
///   warning (no other documented non-admin API names the boot partition); if it
///   is present but cannot be resolved, the guard fails closed.
#[cfg(windows)]
pub fn protected_disks() -> Result<Vec<(u32, &'static str)>, String> {
    use crate::win;
    let mut buffer = [0u16; 512];
    // SAFETY: `buffer` is writable for its full length, which is passed as the size.
    let len = unsafe { win::GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 || len as usize >= buffer.len() {
        // SAFETY: no preconditions.
        let code = unsafe { win::GetLastError() };
        return Err(format!(
            "GetSystemWindowsDirectoryW: {}",
            win::error_text(code)
        ));
    }
    let windows_dir = String::from_utf16_lossy(&buffer[..len as usize]);
    let drive = windows_dir.get(..2).filter(|d| {
        let b = d.as_bytes();
        b[0].is_ascii_alphabetic() && b[1] == b':'
    });
    let drive = drive.ok_or_else(|| format!("unexpected Windows directory {windows_dir:?}"))?;
    let mut disks: Vec<(u32, &'static str)> =
        win::volume_disk_numbers(&win::open_query(&format!(r"\\.\{drive}"))?)?
            .into_iter()
            .map(|(disk, _, _)| (disk, "running Windows volume"))
            .collect();
    match system_partition_device()? {
        None => eprintln!(
            "WARNING: HKLM\\SYSTEM\\Setup\\SystemPartition is not set; only the Windows volume's disk is protected."
        ),
        Some(device) => {
            let path = format!(r"\\?\GLOBALROOT{device}");
            let volume = win::open_query(&path)
                .map_err(|e| format!("boot/system partition {device}: {e}"))?;
            for (disk, _, _) in win::volume_disk_numbers(&volume)? {
                disks.push((disk, "firmware boot/system partition"));
            }
        }
    }
    Ok(disks)
}

/// `\Device\HarddiskVolumeN` of the boot/system partition, if Windows recorded it.
#[cfg(windows)]
fn system_partition_device() -> Result<Option<String>, String> {
    use crate::win;
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    let key = win::wide(r"SYSTEM\Setup");
    let value = win::wide("SystemPartition");
    let mut data = [0u16; 260];
    let mut size = (data.len() * 2) as u32;
    // SAFETY: key/value are NUL-terminated UTF-16; `data` is writable for `size`
    // bytes; HKEY_LOCAL_MACHINE is a predefined pseudo-handle.
    let status = unsafe {
        win::RegGetValueW(
            HKEY_LOCAL_MACHINE as win::Handle,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != 0 {
        return Err(format!(
            "reading HKLM\\SYSTEM\\Setup\\SystemPartition: {}",
            win::error_text(status as u32)
        ));
    }
    let chars = (size as usize / 2).min(data.len());
    let text = String::from_utf16_lossy(&data[..chars]);
    let text = text.trim_end_matches('\0');
    if !text.starts_with(r"\Device\") || text.contains(['/', ':']) {
        return Err(format!("unexpected SystemPartition value {text:?}"));
    }
    Ok(Some(text.to_string()))
}

/// `S:` / `S:\` → 'S'. `--register` needs a volume root so the ESP-relative
/// file path matches the device path we register.
#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows --register path
pub fn volume_root_letter(dir: &Path) -> Option<char> {
    let text = dir.to_str()?;
    let trimmed = text.strip_suffix(['\\', '/']).unwrap_or(text);
    let mut chars = trimmed.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => {
            Some(letter.to_ascii_uppercase())
        }
        _ => None,
    }
}

#[cfg(not(windows))]
pub fn esp_hard_drive_node(_: &Path) -> Result<HardDriveNode, String> {
    Err("--register needs Windows firmware-variable and volume APIs; not supported on this platform".into())
}

/// Build the UEFI Hard Drive node for the partition holding volume `dir`.
#[cfg(windows)]
pub fn esp_hard_drive_node(dir: &Path) -> Result<HardDriveNode, String> {
    use crate::win;
    let letter = volume_root_letter(dir).ok_or_else(|| {
        format!(
            "--register requires --esp to be a mounted volume root such as S:\\ (got {})",
            dir.display()
        )
    })?;
    let volume = win::open_query(&format!(r"\\.\{letter}:"))?;
    let info = win::ioctl_out(&volume, win::IOCTL_DISK_GET_PARTITION_INFO_EX, 144)?;
    let style = win::u32_at(&info, 0)?;
    let start = win::u64_at(&info, 8)?;
    let length = win::u64_at(&info, 16)?;
    // LIMITATION: UEFI wants the partition-table entry index (GPT entry index,
    // 1-based; MBR 1..4). Windows' PartitionNumber is an ordinal of the
    // partitions it enumerates, which matches the GPT entry index on typical
    // disks (no unused entries before the ESP) but is not guaranteed to. No
    // non-admin API returns the raw entry index; firmware matches HD nodes
    // primarily by partition signature (GPT GUID), so a mismatch here is
    // normally tolerated, and `loadopt::same_boot_target` compares whole nodes.
    let partition_number = win::u32_at(&info, 24)?;
    let extents = win::volume_disk_numbers(&volume)?;
    if extents.len() != 1 {
        return Err(format!(
            "{letter}: spans {} disk extents; an ESP must be a single partition",
            extents.len()
        ));
    }
    let disk_number = extents[0].0;
    let disk = win::open_query(&format!(r"\\.\PhysicalDrive{disk_number}"))?;
    let geometry = win::ioctl_out(&disk, win::IOCTL_DISK_GET_DRIVE_GEOMETRY, 24)?;
    let sector = u64::from(win::u32_at(&geometry, 20)?);
    if sector == 0 || !start.is_multiple_of(sector) || !length.is_multiple_of(sector) {
        return Err(format!(
            "{letter}: partition is not aligned to the {sector}-byte sector size"
        ));
    }
    let mut signature = [0u8; 16];
    let (partition_format, signature_type) = match style {
        0 => {
            // PARTITION_INFORMATION_MBR.PartitionType at union offset 32.
            if info.get(32) != Some(&0xEF) {
                eprintln!("WARNING: {letter}: MBR partition type is not 0xEF (EFI System).");
            }
            let layout =
                win::ioctl_out(&disk, win::IOCTL_DISK_GET_DRIVE_LAYOUT_EX, 48 + 144 * 128)?;
            signature[..4].copy_from_slice(&win::u32_at(&layout, 8)?.to_le_bytes());
            (0x01, 0x01)
        }
        1 => {
            const ESP_TYPE: [u8; 16] = [
                0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
                0xc9, 0x3b,
            ];
            if info.get(32..48) != Some(&ESP_TYPE[..]) {
                eprintln!(
                    "WARNING: {letter}: GPT partition type is not the EFI System Partition GUID."
                );
            }
            signature.copy_from_slice(info.get(48..64).ok_or("truncated GPT partition info")?);
            (0x02, 0x02)
        }
        other => {
            return Err(format!(
                "{letter}: unsupported partition style {other} (RAW)"
            ));
        }
    };
    Ok(HardDriveNode {
        partition_number,
        partition_start_lba: start / sector,
        partition_size_lba: length / sector,
        signature,
        partition_format,
        signature_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_physical_drive_names() {
        assert_eq!(physical_drive_number(r"\\.\PhysicalDrive0"), Some(0));
        assert_eq!(physical_drive_number(r"\\.\physicaldrive12"), Some(12));
        assert_eq!(physical_drive_number(r"\\.\PhysicalDrive"), None);
        assert_eq!(physical_drive_number(r"\\.\PhysicalDrive1x"), None);
        assert_eq!(physical_drive_number(r"C:\disk.img"), None);
        assert_eq!(physical_drive_number(r"\\.\PhysicalDrive12345"), None);
    }

    #[test]
    fn volume_root_letters() {
        assert_eq!(volume_root_letter(Path::new("s:")), Some('S'));
        assert_eq!(volume_root_letter(Path::new("S:\\")), Some('S'));
        assert_eq!(volume_root_letter(Path::new("S:\\EFI")), None);
        assert_eq!(volume_root_letter(Path::new("dir")), None);
    }

    #[test]
    fn image_target_rejects_unaligned_size_and_out_of_range_io() {
        let dir = std::env::temp_dir().join(format!("bootapp-disk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let odd = dir.join("odd.img");
        std::fs::write(&odd, vec![0u8; 1000]).unwrap();
        assert!(open_target(odd.to_str().unwrap(), false, false).is_err());
        let good = dir.join("good.img");
        std::fs::write(&good, vec![0u8; 4 * SECTOR]).unwrap();
        let mut disk = open_target(good.to_str().unwrap(), false, false).unwrap();
        assert_eq!(disk.sector_count(), 4);
        let mut buf = vec![0u8; SECTOR];
        assert!(disk.read_sectors(3, &mut buf).is_ok());
        assert!(disk.read_sectors(4, &mut buf).is_err());
        assert!(disk.read_sectors(0, &mut buf[..100]).is_err());
        assert!(open_target(dir.to_str().unwrap(), false, false).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

//! Firmware filesystem volumes: enumeration order, identity and GRUB
//! `search` selector resolution.
//!
//! Protocol open modes (review finding v0.2):
//! * DevicePath and BlockIO are opened GET_PROTOCOL (non-exclusive). Partition,
//!   disk-I/O and filesystem drivers hold them BY_DRIVER; an EXCLUSIVE open
//!   makes the firmware disconnect those drivers, which can tear down the very
//!   filesystem being scanned. Our uses are read-only and short-lived.
//! * SimpleFileSystem stays EXCLUSIVE while a volume's files are read: it is a
//!   leaf protocol that firmware drivers do not consume BY_DRIVER, so nothing
//!   is disconnected, and exclusivity keeps another agent from taking it over
//!   while our file handles are live.

use alloc::{string::String, string::ToString, vec, vec::Vec};
use bootapp_core::VolumeSelector;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, SearchType};
use uefi::proto::ProtocolPointer;
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::file::{
    Directory, File, FileAttribute, FileMode, FileSystemInfo, FileSystemVolumeLabel, FileType,
};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CString16, Handle, Identify, Status};

use crate::volume_policy::{fat_volume_serial, fs_uuid_matches, label_matches};

pub const MAX_VOLUMES: usize = 128;
/// Largest logical block / I/O alignment accepted when reading a boot sector.
const MAX_BLOCK_BYTES: usize = 4096;
/// Bounded label info buffer (fixed FileSystemInfo header + UTF-16 label).
const LABEL_INFO_BYTES: usize = 1024;

/// A filesystem volume and its device path bytes (the sort key, so that volume
/// indices are stable for identical hardware across boots).
pub struct Volume {
    pub handle: Handle,
    pub device_path: Vec<u8>,
}

/// Open a protocol for read-only, short-lived use without disconnecting
/// drivers that hold it BY_DRIVER.
fn get_protocol<P: ProtocolPointer + ?Sized>(handle: Handle) -> Result<ScopedProtocol<P>, Status> {
    // SAFETY: GET_PROTOCOL requires that the interface is not uninstalled while
    // used and that access does not conflict. `handle` was just returned by the
    // firmware; this image never uninstalls or reinstalls protocols; every
    // caller copies data out (or issues a single ReadBlocks) and drops the
    // ScopedProtocol, which closes the open, before returning. UEFI boot
    // services run single-threaded, so no concurrent use from this image exists.
    unsafe {
        boot::open_protocol::<P>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .map_err(|e| e.status())
}

/// Copy of the handle's device path bytes (including the End node).
pub fn device_path_bytes(handle: Handle) -> Result<Vec<u8>, Status> {
    let path = get_protocol::<DevicePath>(handle)?;
    Ok(path.as_bytes().to_vec())
}

/// SimpleFileSystem volumes with a device path, sorted by device path bytes.
/// This is THE volume order: discovery's `volume=N` and selector resolution's
/// "first volume" both index this list.
pub fn sorted_volumes() -> Result<Vec<Volume>, Status> {
    let handles = boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleFileSystem::GUID))
        .map_err(|e| e.status())?;
    let mut volumes: Vec<Volume> = handles
        .iter()
        .copied()
        .take(MAX_VOLUMES)
        .filter_map(|handle| {
            device_path_bytes(handle).ok().map(|device_path| Volume {
                handle,
                device_path,
            })
        })
        .collect();
    volumes.sort_by(|left, right| left.device_path.cmp(&right.device_path));
    Ok(volumes)
}

pub fn index_of(volumes: &[Volume], handle: Handle) -> Option<usize> {
    volumes.iter().position(|volume| volume.handle == handle)
}

/// Run `body` on the volume's root directory while SimpleFileSystem is held.
pub fn with_root<T>(
    handle: Handle,
    body: impl FnOnce(&mut Directory) -> Result<T, Status>,
) -> Result<T, Status> {
    let mut fs =
        boot::open_protocol_exclusive::<SimpleFileSystem>(handle).map_err(|e| e.status())?;
    let mut root = fs.open_volume().map_err(|e| e.status())?;
    body(&mut root)
}

fn is_regular_file(root: &mut Directory, path: &str) -> bool {
    let Ok(name) = CString16::try_from(path) else {
        return false;
    };
    root.open(&name, FileMode::Read, FileAttribute::empty())
        .ok()
        .and_then(|file| file.into_type().ok())
        .is_some_and(|kind| matches!(kind, FileType::Regular(_)))
}

/// GRUB-style FAT serial of the partition behind `handle`, from LBA 0.
pub fn fat_serial(handle: Handle) -> Result<String, Status> {
    let mut block = get_protocol::<BlockIO>(handle)?;
    let media = block.media();
    if !media.is_media_present() {
        return Err(Status::NO_MEDIA);
    }
    let media_id = media.media_id();
    let size = usize::try_from(media.block_size()).map_err(|_| Status::UNSUPPORTED)?;
    let align = usize::try_from(media.io_align().max(1)).map_err(|_| Status::UNSUPPORTED)?;
    if !(512..=MAX_BLOCK_BYTES).contains(&size)
        || !size.is_power_of_two()
        || align > MAX_BLOCK_BYTES
        || !align.is_power_of_two()
    {
        return Err(Status::UNSUPPORTED);
    }
    // Over-allocate and slice so the buffer satisfies IoAlign without unsafe.
    let mut raw = vec![0u8; size + align];
    let offset = raw.as_ptr().align_offset(align);
    let buffer = raw
        .get_mut(offset..offset + size)
        .ok_or(Status::UNSUPPORTED)?;
    block
        .read_blocks(media_id, 0, buffer)
        .map_err(|e| e.status())?;
    fat_volume_serial(buffer).ok_or(Status::UNSUPPORTED)
}

/// Filesystem volume label (FileSystemVolumeLabel, else FileSystemInfo).
pub fn volume_label(handle: Handle) -> Result<String, Status> {
    with_root(handle, |root| {
        // Fixed bounded storage; slicing to an 8-byte boundary satisfies both
        // info types' alignment without unsafe and without trusting sizes.
        let mut raw = [0u8; LABEL_INFO_BYTES + 8];
        let offset = raw.as_ptr().align_offset(8);
        let buffer = &mut raw[offset..offset + LABEL_INFO_BYTES];
        if let Ok(info) = root.get_info::<FileSystemVolumeLabel>(buffer) {
            return Ok(info.volume_label().to_string());
        }
        root.get_info::<FileSystemInfo>(buffer)
            .map(|info| info.volume_label().to_string())
            .map_err(|e| e.status())
    })
}

/// Resolve a configured selector to (volume handle, index in `volumes`).
/// `Own` is the manager's volume even if it lacks a device path.
/// Volumes that fail to open/read are skipped, never fatal.
pub fn resolve(
    selector: &VolumeSelector,
    volumes: &[Volume],
    own: Handle,
) -> Option<(Handle, Option<usize>)> {
    let found = match selector {
        VolumeSelector::Own => return Some((own, index_of(volumes, own))),
        VolumeSelector::File(path) => volumes.iter().position(|volume| {
            with_root(volume.handle, |root| Ok(is_regular_file(root, path))).unwrap_or(false)
        }),
        VolumeSelector::FsUuid(uuid) => volumes.iter().position(|volume| {
            fat_serial(volume.handle).is_ok_and(|serial| fs_uuid_matches(&serial, uuid))
        }),
        VolumeSelector::Label(label) => volumes.iter().position(|volume| {
            volume_label(volume.handle).is_ok_and(|found| label_matches(&found, label))
        }),
    }?;
    Some((volumes[found].handle, Some(found)))
}

/// Short text for traces, e.g. `fs-uuid=ABCD-1234`.
pub fn describe(selector: &VolumeSelector) -> String {
    match selector {
        VolumeSelector::Own => String::from("own"),
        VolumeSelector::File(path) => alloc::format!("file={path}"),
        VolumeSelector::FsUuid(uuid) => alloc::format!("fs-uuid={uuid}"),
        VolumeSelector::Label(label) => alloc::format!("label={label}"),
    }
}

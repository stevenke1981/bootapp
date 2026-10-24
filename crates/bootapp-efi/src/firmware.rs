use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use bootapp_core::detection::{OsKind, classify_loader_path, loader_candidates, pe_architecture};
use bootapp_core::{Architecture, Config, Entry, MenuAction, MenuEvent, MenuState};
use uefi::boot::{EventType, LoadImageSource, TimerTrigger, Tpl};
use uefi::proto::BootPolicy;
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::proto::device_path::{DevicePath, build, media};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{Directory, File, FileAttribute, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CString16, Handle, Identify, Status, boot, cstr16};

#[path = "trace.rs"]
mod trace;

const MAX_DISCOVERED: usize = 128;
const MAX_VOLUMES: usize = 128;
const MAX_DIRECTORY_ENTRIES: usize = 256;
const MAX_SCAN_DIRECTORIES: usize = 64;

struct BootTarget {
    entry: Entry,
    volume: Handle,
}

fn own_volume() -> Result<Handle, Status> {
    let image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|e| e.status())?;
    image.device().ok_or(Status::NOT_FOUND)
}

/// File reads are bounded independently of untrusted file metadata.
fn read_prefix(root: &mut Directory, path: &str, limit: usize) -> Result<Vec<u8>, Status> {
    let name = CString16::try_from(path).map_err(|_| Status::INVALID_PARAMETER)?;
    let file = root
        .open(&name, FileMode::Read, FileAttribute::empty())
        .map_err(|e| e.status())?;
    let mut file = file.into_regular_file().ok_or(Status::INVALID_PARAMETER)?;
    let mut bytes = vec![0; limit];
    let mut size = 0;
    while size < bytes.len() {
        let count = file.read(&mut bytes[size..]).map_err(|e| e.status())?;
        if count == 0 {
            break;
        }
        size += count;
    }
    bytes.truncate(size);
    Ok(bytes)
}

/// Return bounded sorted children. Fixed aligned storage avoids directory entries
/// tricking an allocator into accepting unbounded firmware-reported sizes.
fn children(root: &mut Directory, path: &str) -> Vec<(String, bool)> {
    let Ok(name) = CString16::try_from(path) else {
        return Vec::new();
    };
    let Ok(file) = root.open(&name, FileMode::Read, FileAttribute::empty()) else {
        return Vec::new();
    };
    let Some(mut dir) = file.into_directory() else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut aligned = [0u64; 512];
    for _ in 0..MAX_DIRECTORY_ENTRIES {
        // SAFETY: this byte view covers the initialized u64 storage, retains its
        // alignment (FileInfo requires 8), and no other reference aliases it.
        let storage = unsafe {
            core::slice::from_raw_parts_mut(
                aligned.as_mut_ptr().cast::<u8>(),
                core::mem::size_of_val(&aligned),
            )
        };
        let Ok(Some(info)) = dir.read_entry(storage) else {
            break;
        };
        let component = info.file_name().to_string();
        if component == "."
            || component == ".."
            || component.is_empty()
            || component
                .chars()
                .any(|c| c.is_control() || matches!(c, '\\' | '/' | ':'))
        {
            continue;
        }
        result.push((component, info.is_directory()));
    }
    result.sort_by_cached_key(|(name, _)| name.to_ascii_lowercase());
    result
}

fn enumerate_candidates(root: &mut Directory) -> Vec<(String, OsKind)> {
    let mut paths: Vec<(String, OsKind)> = loader_candidates(arch())
        .into_iter()
        .map(|(path, kind)| (path.to_string(), kind))
        .collect();
    // APFS/HFS are visible only when firmware already provides a filesystem
    // driver. Probe exact recovery and canonical Preboot UUID layouts; scanning
    // these paths does not install a filesystem driver or prove macOS bootability.
    let recovery = String::from("\\com.apple.recovery.boot\\boot.efi");
    if let Some(kind) = classify_loader_path(&recovery) {
        paths.push((recovery, kind));
    }
    for (directory, is_dir) in children(root, "\\") {
        if !is_dir {
            continue;
        }
        let path = format!("\\{directory}\\System\\Library\\CoreServices\\boot.efi");
        if let Some(OsKind::MacOs) = classify_loader_path(&path)
            && !paths.iter().any(|(old, _)| old.eq_ignore_ascii_case(&path))
        {
            paths.push((path, OsKind::MacOs));
        }
    }
    let mut pending = vec![(String::from("\\EFI"), 0usize)];
    let mut directories = 0;
    while let Some((directory, depth)) = pending.pop() {
        if directories >= MAX_SCAN_DIRECTORIES {
            break;
        }
        directories += 1;
        for (name, is_dir) in children(root, &directory) {
            let path = format!("{directory}\\{name}");
            if is_dir {
                // Enumerate vendor folders and Linux UKIs. Apple loaders can be
                // nested; other arbitrary vendor trees are not recursively read.
                if depth == 0
                    || (path.to_ascii_lowercase().starts_with("\\efi\\apple\\") && depth < 4)
                {
                    pending.push((path, depth + 1));
                }
            } else if let Some(kind) = classify_loader_path(&path)
                && !paths.iter().any(|(old, _)| old.eq_ignore_ascii_case(&path))
            {
                paths.push((path, kind));
            }
            if paths.len() >= 512 {
                return paths;
            }
        }
    }
    paths
}

fn discover(
    targets: &mut Vec<BootTarget>,
    own: Handle,
    own_path: Option<&str>,
) -> Result<(), Status> {
    let handles = boot::locate_handle_buffer(boot::SearchType::ByProtocol(&SimpleFileSystem::GUID))
        .map_err(|e| e.status())?;
    let mut volumes: Vec<(Vec<u8>, Handle)> = handles
        .iter()
        .copied()
        .take(MAX_VOLUMES)
        .filter_map(|handle| {
            boot::open_protocol_exclusive::<DevicePath>(handle)
                .ok()
                .map(|path| (path.as_bytes().to_vec(), handle))
        })
        .collect();
    volumes.sort_by(|left, right| left.0.cmp(&right.0));
    for (index, (_, volume)) in volumes.into_iter().enumerate() {
        let Ok(mut fs) = boot::open_protocol_exclusive::<SimpleFileSystem>(volume) else {
            continue;
        };
        let Ok(mut root) = fs.open_volume() else {
            continue;
        };
        for (path, kind) in enumerate_candidates(&mut root) {
            if targets.len() >= MAX_DISCOVERED {
                return Ok(());
            }
            if crate::discovery_policy::skip_self(&path, own_path, volume == own)
                || targets
                    .iter()
                    .any(|t| t.volume == volume && t.entry.path.eq_ignore_ascii_case(&path))
            {
                continue;
            }
            let Ok(header) = read_prefix(&mut root, &path, 65_536) else {
                continue;
            };
            if pe_architecture(&header) != Some(arch()) {
                continue;
            }
            trace::trace(&format!(
                "BOOTAPP_DETECTED {} volume={index} handle={volume:?} path={path}",
                kind.label()
            ));
            targets.push(BootTarget {
                entry: Entry {
                    id: format!("auto-{}", targets.len()),
                    title: format!("{} [volume {}] {}", kind.label(), index, path),
                    path,
                    arch: Some(arch()),
                    options: String::new(),
                },
                volume,
            });
        }
    }
    Ok(())
}

fn arch() -> Architecture {
    #[cfg(target_arch = "x86")]
    {
        Architecture::X86
    }
    #[cfg(target_arch = "x86_64")]
    {
        Architecture::X64
    }
    #[cfg(target_arch = "aarch64")]
    {
        Architecture::Aarch64
    }
}

/// Read at most 64 KiB plus one sentinel byte, including short-read handling.
fn read_config() -> Result<Config, Status> {
    let mut fs = boot::get_image_file_system(boot::image_handle()).map_err(|e| e.status())?;
    let mut root = fs.open_volume().map_err(|e| e.status())?;
    let handle = root
        .open(
            cstr16!("\\EFI\\BOOTAPP\\bootapp.cfg"),
            FileMode::Read,
            FileAttribute::empty(),
        )
        .map_err(|e| e.status())?;
    let mut file = match handle.into_type().map_err(|e| e.status())? {
        FileType::Regular(file) => file,
        FileType::Dir(_) => return Err(Status::INVALID_PARAMETER),
    };
    let mut data = vec![0; 65_537];
    let mut len = 0;
    while len < data.len() {
        let count = file.read(&mut data[len..]).map_err(|e| e.status())?;
        if count == 0 {
            break;
        }
        len += count;
    }
    if len > 65_536 {
        return Err(Status::BAD_BUFFER_SIZE);
    }
    let text = core::str::from_utf8(&data[..len]).map_err(|_| Status::INVALID_PARAMETER)?;
    Config::parse(text).map_err(|error| {
        uefi::println!("Invalid bootapp.cfg: {}", error);
        trace::trace(&format!("BOOTAPP_CONFIG_ERROR {error}"));
        Status::INVALID_PARAMETER
    })
}

fn own_file_path() -> Result<String, Status> {
    let image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|e| e.status())?;
    let path = image.file_path().ok_or(Status::NOT_FOUND)?;
    let mut text = String::new();
    for node in path.node_iter() {
        if let Ok(file) = <&media::FilePath>::try_from(node) {
            let chars: Vec<u16> = file.path_name().iter().take_while(|v| *v != 0).collect();
            text.push_str(&String::from_utf16(&chars).map_err(|_| Status::INVALID_PARAMETER)?);
        }
    }
    if text.is_empty() {
        return Err(Status::NOT_FOUND);
    }
    Ok(text)
}

fn chainload(target: &BootTarget, own: Handle, own_path: Option<&str>) -> Result<(), Status> {
    let entry = &target.entry;
    if crate::discovery_policy::skip_self(&entry.path, own_path, target.volume == own) {
        return Err(Status::ACCESS_DENIED);
    }
    // Preserve discovery's device identity even when another volume has the same
    // pathname. Configured targets are explicitly bound to the manager's volume.
    let volume =
        boot::open_protocol_exclusive::<DevicePath>(target.volume).map_err(|e| e.status())?;
    let path = CString16::try_from(entry.path.as_str()).map_err(|_| Status::INVALID_PARAMETER)?;
    let mut storage = Vec::new();
    let mut builder = build::DevicePathBuilder::with_vec(&mut storage);
    for node in volume.node_iter() {
        builder = builder.push(&node).map_err(|_| Status::INVALID_PARAMETER)?;
    }
    let device_path = builder
        .push(&build::media::FilePath { path_name: &path })
        .map_err(|_| Status::INVALID_PARAMETER)?
        .finalize()
        .map_err(|_| Status::INVALID_PARAMETER)?;
    let child = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromDevicePath {
            device_path,
            boot_policy: BootPolicy::ExactMatch,
        },
    )
    .map_err(|e| e.status())?;
    drop(volume);
    let options: Vec<u16> = entry
        .options
        .encode_utf16()
        .chain(core::iter::once(0))
        .collect();
    let setup = (|| {
        let mut image =
            boot::open_protocol_exclusive::<LoadedImage>(child).map_err(|e| e.status())?;
        let size = u32::try_from(options.len() * 2).map_err(|_| Status::BAD_BUFFER_SIZE)?;
        // SAFETY: `options` is aligned UTF-16 storage, remains alive through
        // StartImage and until UnloadImage below, and is never mutated meanwhile.
        unsafe {
            image.set_load_options(options.as_ptr().cast(), size);
        }
        Ok(())
    })();
    if let Err(status) = setup {
        let _ = boot::unload_image(child);
        return Err(status);
    }
    trace::trace(&format!(
        "BOOTAPP_START {} volume={:?}",
        entry.path, target.volume
    ));
    let result = boot::start_image(child).map_err(|e| e.status());
    // UEFI automatically unloads applications that exit; UnloadImage is a
    // best-effort cleanup for an image whose StartImage failed before dispatch.
    let cleared = if let Ok(mut image) = boot::open_protocol_exclusive::<LoadedImage>(child) {
        // SAFETY: a null pointer with zero size removes the borrowed load options.
        unsafe {
            image.set_load_options(core::ptr::null(), 0);
        }
        true
    } else {
        false
    };
    let unload = boot::unload_image(child);
    if !cleared
        && unload
            .as_ref()
            .is_err_and(|e| e.status() != Status::INVALID_PARAMETER)
    {
        // A nonconforming child may leave an inaccessible resident image behind.
        // Retain its options rather than leave a dangling firmware-visible pointer.
        core::mem::forget(options);
    }
    trace::trace(&format!("BOOTAPP_CHILD_RETURN {:?}", result));
    result
}

fn render(entries: &[BootTarget], state: &MenuState, message: &str) {
    uefi::system::with_stdout(|out| {
        let _ = out.set_color(Color::LightGray, Color::Black);
        let _ = out.clear();
    });
    uefi::println!("BootApp 0.2 | Rust UEFI | {}", arch().as_str());
    uefi::println!("Up/Down: select  Enter: boot  Esc: stop timer  R: reboot  Q: exit\n");
    let start = state.selected().saturating_sub(5);
    for (index, entry) in entries.iter().enumerate().skip(start).take(10) {
        if index == state.selected() {
            uefi::system::with_stdout(|out| {
                let _ = out.set_color(Color::Black, Color::LightGray);
            });
        }
        // Bound rendered width; keep long user titles from flooding the screen.
        let title: String = entry.entry.title.chars().take(65).collect();
        uefi::println!(
            " {} {}",
            if index == state.selected() { ">" } else { " " },
            title
        );
        uefi::system::with_stdout(|out| {
            let _ = out.set_color(Color::LightGray, Color::Black);
        });
    }
    if let Some(seconds) = state.remaining() {
        uefi::println!(
            "\nAutomatic boot in {} seconds. Any key stops timer.",
            seconds
        );
    } else {
        uefi::println!("\nAutomatic boot paused.");
    }
    uefi::println!("{}", message);
}

pub fn run() -> Result<(), Status> {
    boot::set_watchdog_timer(0, 0, None).map_err(|e| e.status())?;
    trace::trace("BOOTAPP_MANAGER_READY");
    let own = own_volume()?;
    let own_path = own_file_path().ok();
    if own_path.is_none() {
        trace::trace("BOOTAPP_SELF_PATH_UNAVAILABLE: own-volume fallback loaders suppressed");
    }
    let config = match read_config() {
        Ok(config) => Some(config),
        Err(Status::NOT_FOUND) => {
            trace::trace("BOOTAPP_AUTO_MODE: no config; discovering filesystem volumes");
            None
        }
        Err(status) => {
            trace::trace(&format!("BOOTAPP_CONFIG_READ_ERROR {status:?}"));
            return Err(status);
        }
    };
    let mut entries = Vec::new();
    let mut selected = 0;
    let mut timeout = 5;
    if let Some(config) = config {
        selected = config.default_index(arch()).unwrap_or(0);
        timeout = config.timeout;
        for entry in config.entries_for(arch()) {
            entries.push(BootTarget {
                entry: entry.clone(),
                volume: own,
            });
        }
    }
    if let Err(status) = discover(&mut entries, own, own_path.as_deref()) {
        // A broken discovery protocol must not discard a usable explicit config.
        trace::trace(&format!("BOOTAPP_DISCOVERY_ERROR {status:?}"));
        if entries.is_empty() {
            return Err(status);
        }
    }
    if entries.is_empty() {
        uefi::println!("No compatible EFI application found on firmware-visible filesystems.");
        return Err(Status::NOT_FOUND);
    }
    trace::trace(&format!(
        "BOOTAPP_MENU entries={} default={} timeout={}",
        entries.len(),
        selected,
        timeout
    ));
    let mut state = MenuState::new(entries.len(), selected, timeout);
    // SAFETY: a plain timer has no callback/context and is closed before return;
    // the application does not exit boot services while waiting on this event.
    let timer = unsafe { boot::create_event(EventType::TIMER, Tpl::APPLICATION, None, None) }
        .map_err(|e| e.status())?;
    if let Err(error) = boot::set_timer(
        &timer,
        TimerTrigger::Periodic(core::time::Duration::from_secs(1)),
    ) {
        let _ = boot::close_event(timer);
        return Err(error.status());
    }
    let result = menu_loop(&entries, &mut state, own, own_path.as_deref(), &timer);
    let _ = boot::set_timer(&timer, TimerTrigger::Cancel);
    let _ = boot::close_event(timer);
    result
}

fn menu_loop(
    entries: &[BootTarget],
    state: &mut MenuState,
    own: Handle,
    own_path: Option<&str>,
    timer: &uefi::Event,
) -> Result<(), Status> {
    let mut message = String::new();
    loop {
        render(entries, state, &message);
        let key_event =
            uefi::system::with_stdin(|input| input.wait_for_key_event()).map_err(|e| e.status())?;
        // SAFETY: neither alias closes the timer. The original remains live until
        // this loop exits; wait_for_event only borrows the aliased handle.
        let events = [key_event, unsafe { timer.unsafe_clone() }];
        let which = boot::wait_for_event(&events).map_err(|e| e.status())?;
        let action = if which == 1 {
            state.handle(MenuEvent::Tick)
        } else {
            let key = uefi::system::with_stdin(|input| input.read_key()).map_err(|e| e.status())?;
            let Some(key) = key else {
                continue;
            };
            state.handle(MenuEvent::CancelTimeout);
            match key {
                Key::Special(ScanCode::UP) => state.handle(MenuEvent::Up),
                Key::Special(ScanCode::DOWN) => state.handle(MenuEvent::Down),
                Key::Printable(c) => match char::from(c) {
                    '\r' | '\n' => state.handle(MenuEvent::Enter),
                    'q' | 'Q' => return Ok(()),
                    'r' | 'R' => {
                        uefi::runtime::reset(uefi::runtime::ResetType::COLD, Status::SUCCESS, None)
                    }
                    _ => MenuAction::None,
                },
                _ => MenuAction::None,
            }
        };
        if let MenuAction::Boot(index) = action {
            state.handle(MenuEvent::CancelTimeout);
            message = match chainload(&entries[index], own, own_path) {
                Ok(()) => {
                    String::from("Selected EFI application returned. Select an entry or Q to exit.")
                }
                Err(status) => {
                    format!("Boot failed: {status:?}. Select another entry or Q to exit.")
                }
            };
            trace::trace(&message);
            // Child firmware applications can change console modes or watchdogs.
            let _ = boot::set_watchdog_timer(0, 0, None);
        }
    }
}

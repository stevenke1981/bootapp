use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use bootapp_core::detection::{OsKind, classify_loader_path, loader_candidates, pe_architecture};
use bootapp_core::{
    Architecture, Config, Entry, MenuAction, MenuEvent, MenuState, TIMEOUT_DISABLED,
    VolumeSelector, grub,
};
use uefi::boot::{EventType, LoadImageSource, TimerTrigger, Tpl};
use uefi::proto::BootPolicy;
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::proto::device_path::{DevicePath, build, media};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{Directory, File, FileAttribute, FileMode, FileType};
use uefi::{CString16, Handle, Status, boot};

use crate::trace::trace;
use crate::volumes::{self, Volume};

const MAX_DISCOVERED: usize = 128;
const MAX_DIRECTORY_ENTRIES: usize = 256;
const MAX_SCAN_DIRECTORIES: usize = 64;
const MAX_TRACED_WARNINGS: usize = 32;
const BOOTAPP_CONFIG_PATH: &str = "\\EFI\\BOOTAPP\\bootapp.cfg";

/// `volume: None` means "resolve `entry.volume` when the entry is booted".
///
/// Selector resolution (GRUB `search`) is lazy: volumes are probed only when
/// the user (or the timer) boots that entry. This keeps startup independent of
/// slow or failing media, reflects the media present at boot time, and lets an
/// unresolvable entry stay listed and fail with "volume not found".
struct BootTarget {
    entry: Entry,
    volume: Option<Handle>,
}

enum BootError {
    VolumeNotFound,
    Firmware(Status),
}

impl From<Status> for BootError {
    fn from(status: Status) -> Self {
        Self::Firmware(status)
    }
}

/// LoadedImage of our own image handle: no driver binds it BY_DRIVER, so the
/// EXCLUSIVE (safe uefi-rs) open disconnects nothing and is closed on return.
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

/// Append auto-discovered loaders. `volumes` is already in device-path order,
/// so `volume=N` in traces/titles is the index used by selector resolution.
fn discover(
    targets: &mut Vec<BootTarget>,
    volumes: &[Volume],
    own: Handle,
    own_path: Option<&str>,
) {
    for (index, volume) in volumes.iter().map(|volume| volume.handle).enumerate() {
        // An unreadable volume is skipped; it must not hide the others.
        let _ = volumes::with_root(volume, |root| {
            for (path, kind) in enumerate_candidates(root) {
                if targets.len() >= MAX_DISCOVERED {
                    return Ok(());
                }
                if crate::discovery_policy::skip_self(&path, own_path, volume == own)
                    || targets.iter().any(|t| {
                        t.volume == Some(volume) && t.entry.path.eq_ignore_ascii_case(&path)
                    })
                {
                    continue;
                }
                let Ok(header) = read_prefix(root, &path, 65_536) else {
                    continue;
                };
                if pe_architecture(&header) != Some(arch()) {
                    continue;
                }
                trace(&format!(
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
                        volume: VolumeSelector::Own,
                    },
                    volume: Some(volume),
                });
            }
            Ok(())
        });
    }
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

/// Read a whole config file: at most 64 KiB plus one sentinel byte, with
/// short-read handling. NOT_FOUND is returned unchanged for precedence.
fn read_config_text(root: &mut Directory, path: &str) -> Result<String, Status> {
    let name = CString16::try_from(path).map_err(|_| Status::INVALID_PARAMETER)?;
    let handle = root
        .open(&name, FileMode::Read, FileAttribute::empty())
        .map_err(|e| e.status())?;
    let mut file = match handle.into_type().map_err(|e| e.status())? {
        FileType::Regular(file) => file,
        FileType::Dir(_) => return Err(Status::INVALID_PARAMETER),
    };
    let mut data = vec![0; bootapp_core::MAX_CONFIG_BYTES + 1];
    let mut len = 0;
    while len < data.len() {
        let count = file.read(&mut data[len..]).map_err(|e| e.status())?;
        if count == 0 {
            break;
        }
        len += count;
    }
    if len > bootapp_core::MAX_CONFIG_BYTES {
        uefi::println!("Invalid {}: larger than 64 KiB", path);
        trace(&format!("BOOTAPP_CONFIG_ERROR {path}: larger than 64 KiB"));
        return Err(Status::BAD_BUFFER_SIZE);
    }
    data.truncate(len);
    String::from_utf8(data).map_err(|_| {
        uefi::println!("Invalid {}: not UTF-8", path);
        trace(&format!("BOOTAPP_CONFIG_ERROR {path}: not UTF-8"));
        Status::INVALID_PARAMETER
    })
}

/// Config precedence on the manager's own volume: bootapp.cfg, else the GRUB
/// subset in grub.cfg, else `Ok(None)` (auto-discovery only). A present but
/// invalid file is an error (never silently ignored). Returns the config and
/// the initial on-screen status message.
fn load_config(own: Handle) -> Result<(Option<Config>, String), Status> {
    volumes::with_root(own, |root| {
        match read_config_text(root, BOOTAPP_CONFIG_PATH) {
            Ok(text) => {
                trace("BOOTAPP_CONFIG_SOURCE bootapp.cfg");
                return Config::parse(&text)
                    .map(|config| (Some(config), String::new()))
                    .map_err(|error| {
                        uefi::println!("Invalid bootapp.cfg: {}", error);
                        trace(&format!("BOOTAPP_CONFIG_ERROR {error}"));
                        Status::INVALID_PARAMETER
                    });
            }
            Err(Status::NOT_FOUND) => {}
            Err(status) => return Err(status),
        }
        let text = match read_config_text(root, grub::GRUB_CONFIG_PATH) {
            Ok(text) => text,
            Err(Status::NOT_FOUND) => return Ok((None, String::new())),
            Err(status) => return Err(status),
        };
        trace("BOOTAPP_CONFIG_SOURCE grub.cfg");
        let parsed = grub::parse(&text).map_err(|error| {
            uefi::println!("Invalid {}: {}", grub::GRUB_CONFIG_PATH, error);
            trace(&format!("BOOTAPP_CONFIG_ERROR {error}"));
            Status::INVALID_PARAMETER
        })?;
        for warning in parsed.warnings.iter().take(MAX_TRACED_WARNINGS) {
            trace(&format!(
                "BOOTAPP_GRUB_WARNING line={} {}",
                warning.line, warning.message
            ));
        }
        let message = if parsed.warnings.is_empty() {
            String::new()
        } else {
            format!(
                "grub.cfg: {} construct(s) accepted without effect; see serial trace.",
                parsed.warnings.len()
            )
        };
        Ok((Some(parsed.config), message))
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

fn resolve_volume(
    target: &BootTarget,
    volumes: &[Volume],
    own: Handle,
) -> Result<Handle, BootError> {
    if let Some(volume) = target.volume {
        return Ok(volume);
    }
    let entry = &target.entry;
    let selector = volumes::describe(&entry.volume);
    match volumes::resolve(&entry.volume, volumes, own) {
        Some((handle, index)) => {
            let index = index.map_or_else(|| String::from("unlisted"), |index| index.to_string());
            trace(&format!(
                "BOOTAPP_VOLUME_RESOLVED id={} volume={index} {selector}",
                entry.id
            ));
            Ok(handle)
        }
        None => {
            trace(&format!(
                "BOOTAPP_VOLUME_NOT_FOUND id={} {selector}",
                entry.id
            ));
            Err(BootError::VolumeNotFound)
        }
    }
}

fn chainload(
    target: &BootTarget,
    volumes: &[Volume],
    own: Handle,
    own_path: Option<&str>,
) -> Result<(), BootError> {
    let entry = &target.entry;
    let volume = resolve_volume(target, volumes, own)?;
    // Self-recursion protection applies after resolution, so a GRUB `search`
    // that lands on the manager's own volume is still caught.
    if crate::discovery_policy::skip_self(&entry.path, own_path, volume == own) {
        return Err(Status::ACCESS_DENIED.into());
    }
    // Preserve the volume's device identity even when another volume has the
    // same pathname. The bytes are copied out of a non-exclusive open.
    let device = volumes::device_path_bytes(volume)?;
    let device =
        <&DevicePath>::try_from(device.as_slice()).map_err(|_| Status::INVALID_PARAMETER)?;
    // EFI-stub kernels need no .efi suffix; LoadImage itself enforces PE format.
    let path = CString16::try_from(entry.path.as_str()).map_err(|_| Status::INVALID_PARAMETER)?;
    let mut storage = Vec::new();
    let mut builder = build::DevicePathBuilder::with_vec(&mut storage);
    for node in device.node_iter() {
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
    let options: Vec<u16> = entry
        .options
        .encode_utf16()
        .chain(core::iter::once(0))
        .collect();
    // The child's LoadedImage is mutated (load options), so it is opened
    // EXCLUSIVE: no driver binds a not-yet-started image, and the open is
    // closed before StartImage so the child can open it itself.
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
        return Err(BootError::Firmware(status));
    }
    trace(&format!("BOOTAPP_START {} volume={:?}", entry.path, volume));
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
    trace(&format!("BOOTAPP_CHILD_RETURN {:?}", result));
    result.map_err(BootError::Firmware)
}

fn render(entries: &[BootTarget], state: &MenuState, auto_disabled: bool, message: &str) {
    uefi::system::with_stdout(|out| {
        let _ = out.set_color(Color::LightGray, Color::Black);
        let _ = out.clear();
    });
    uefi::println!("BootApp 0.3 | Rust UEFI | {}", arch().as_str());
    uefi::println!("Up/Down: select  Enter: boot  Esc: stop timer  R: reboot  Q: exit");
    uefi::println!("F: make BootApp first in firmware BootOrder\n");
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
    } else if auto_disabled {
        uefi::println!("\nAutomatic boot disabled.");
    } else {
        uefi::println!("\nAutomatic boot paused.");
    }
    uefi::println!("{}", message);
}

pub fn run() -> Result<(), Status> {
    boot::set_watchdog_timer(0, 0, None).map_err(|e| e.status())?;
    trace("BOOTAPP_MANAGER_READY");
    if let Some(current) = crate::bootorder::boot_current() {
        trace(&format!("BOOTAPP_BOOT_CURRENT {current:04X}"));
    }
    // Startup BootOrder, so tests (and serial users) can compare it with the
    // order written by key F. Read-only; failure is reported, never fatal.
    match crate::bootorder::read_boot_order() {
        Ok(order) => trace(&format!(
            "BOOTAPP_BOOT_ORDER {}",
            crate::bootorder::order_text(&order)
        )),
        Err(status) => trace(&format!("BOOTAPP_BOOT_ORDER_ERROR {status:?}")),
    }
    let own = own_volume()?;
    let own_path = own_file_path().ok();
    if own_path.is_none() {
        trace("BOOTAPP_SELF_PATH_UNAVAILABLE: own-volume fallback loaders suppressed");
    }
    let (config, mut message) = match load_config(own) {
        Ok(loaded) => loaded,
        Err(status) => {
            trace(&format!("BOOTAPP_CONFIG_READ_ERROR {status:?}"));
            return Err(status);
        }
    };
    // Discovery always runs; auto entries follow configured ones. This marker
    // only records that no bootapp.cfg / grub.cfg was found.
    if config.is_none() {
        trace("BOOTAPP_AUTO_MODE: no config file; menu lists discovered loaders only");
    }
    let volumes = match volumes::sorted_volumes() {
        Ok(volumes) => volumes,
        Err(status) => {
            // A broken discovery protocol must not discard a usable explicit config.
            trace(&format!("BOOTAPP_DISCOVERY_ERROR {status:?}"));
            Vec::new()
        }
    };
    if let Some(index) = volumes::index_of(&volumes, own) {
        trace(&format!("BOOTAPP_OWN_VOLUME volume={index}"));
    }
    let mut entries = Vec::new();
    let mut selected = 0;
    let mut timeout = 5;
    if let Some(config) = config {
        selected = config.default_index(arch()).unwrap_or(0);
        timeout = config.timeout;
        for entry in config.entries_for(arch()) {
            entries.push(BootTarget {
                volume: (entry.volume == VolumeSelector::Own).then_some(own),
                entry: entry.clone(),
            });
        }
    }
    discover(&mut entries, &volumes, own, own_path.as_deref());
    if entries.is_empty() {
        uefi::println!("No compatible EFI application found on firmware-visible filesystems.");
        return Err(Status::NOT_FOUND);
    }
    let auto_disabled = timeout == TIMEOUT_DISABLED;
    let timeout_text = if auto_disabled {
        String::from("disabled")
    } else {
        timeout.to_string()
    };
    trace(&format!(
        "BOOTAPP_MENU entries={} default={} timeout={timeout_text}",
        entries.len(),
        selected,
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
    let menu = Menu {
        entries: &entries,
        volumes: &volumes,
        own,
        own_path: own_path.as_deref(),
        auto_disabled,
    };
    let result = menu.run(&mut state, &timer, &mut message);
    let _ = boot::set_timer(&timer, TimerTrigger::Cancel);
    let _ = boot::close_event(timer);
    result
}

struct Menu<'a> {
    entries: &'a [BootTarget],
    volumes: &'a [Volume],
    own: Handle,
    own_path: Option<&'a str>,
    auto_disabled: bool,
}

impl Menu<'_> {
    fn run(
        &self,
        state: &mut MenuState,
        timer: &uefi::Event,
        message: &mut String,
    ) -> Result<(), Status> {
        loop {
            render(self.entries, state, self.auto_disabled, message);
            let key_event = uefi::system::with_stdin(|input| input.wait_for_key_event())
                .map_err(|e| e.status())?;
            // SAFETY: neither alias closes the timer. The original remains live until
            // this loop exits; wait_for_event only borrows the aliased handle.
            let events = [key_event, unsafe { timer.unsafe_clone() }];
            let which = boot::wait_for_event(&events).map_err(|e| e.status())?;
            let action = if which == 1 {
                state.handle(MenuEvent::Tick)
            } else {
                let key =
                    uefi::system::with_stdin(|input| input.read_key()).map_err(|e| e.status())?;
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
                        'r' | 'R' => uefi::runtime::reset(
                            uefi::runtime::ResetType::COLD,
                            Status::SUCCESS,
                            None,
                        ),
                        'f' | 'F' => {
                            *message = self.make_first();
                            MenuAction::None
                        }
                        _ => MenuAction::None,
                    },
                    _ => MenuAction::None,
                }
            };
            if let MenuAction::Boot(index) = action {
                state.handle(MenuEvent::CancelTimeout);
                let target = &self.entries[index];
                *message = match chainload(target, self.volumes, self.own, self.own_path) {
                    Ok(()) => String::from(
                        "Selected EFI application returned. Select an entry or Q to exit.",
                    ),
                    Err(BootError::VolumeNotFound) => String::from(
                        "Boot failed: volume not found. Select another entry or Q to exit.",
                    ),
                    Err(BootError::Firmware(status)) => {
                        format!("Boot failed: {status:?}. Select another entry or Q to exit.")
                    }
                };
                trace(message);
                // Child firmware applications can change console modes or watchdogs.
                let _ = boot::set_watchdog_timer(0, 0, None);
            }
        }
    }

    /// Menu key `F`: register this image as the first firmware boot option.
    fn make_first(&self) -> String {
        match crate::bootorder::make_first(self.own, self.own_path) {
            Ok(done) => {
                let order = crate::bootorder::order_text(&done.order);
                trace(&format!(
                    "BOOTAPP_BOOTORDER_SET Boot{:04X} order={order}",
                    done.number
                ));
                let shown: String = order.chars().take(40).collect();
                format!(
                    "BootOrder updated: Boot{:04X} ({}) is first. Order: {shown}",
                    done.number,
                    if done.reused {
                        "existing entry"
                    } else {
                        "new entry"
                    },
                )
            }
            Err(status) => {
                trace(&format!("BOOTAPP_BOOTORDER_ERROR {status:?}"));
                format!("BootOrder update failed: {status:?}. No variable was deleted.")
            }
        }
    }
}

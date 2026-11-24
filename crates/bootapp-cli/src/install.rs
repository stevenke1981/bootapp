//! UEFI install into a mounted ESP directory (or any directory, for tests).
//!
//! Rules: dry-run unless `apply`; never overwrite a file without a backup;
//! BootApp-owned files that already exist must be byte-identical (they are then
//! kept and not removed by uninstall); an existing manifest means "installed":
//! an identical re-install is a no-op, anything else is refused until
//! `bootapp uninstall uefi`. The manifest is written FIRST (state=pending) as a
//! journal so an interrupted install can be undone by `uninstall`; it is
//! rewritten with state=complete at the end. Any failure during apply rolls
//! back via the uninstall logic and reports what was done.
//!
//! File safety: every file is written to a unique temp name in the same
//! directory, fsynced, then renamed into place. New files (and backups) use a
//! no-replace rename, so a file that appears after planning is never
//! overwritten; replacements re-check the planned original just before the
//! rename. No path component under `--esp` may be a symlink/junction/reparse
//! point. On Windows a bare `S:` is normalised to the volume root `S:\`.
//!
//! Manifest (`EFI\BOOTAPP\install-manifest.txt`, `key=value`, fields split by `|`):
//! ```text
//! manifest_version=1
//! bootapp_version=0.3
//! arch=x64
//! state=pending|complete
//! dir=<rel>                                  directory created by install
//! file=<rel>|<size>|<sha256>                 file created by install
//! kept=<rel>|<size>|<sha256>                 identical file that already existed
//! replaced=<rel>|<size>|<sha256>|<backup rel>|<orig size>|<orig sha256>
//! boot=<XXXX>|created|<file path>|<device path hex>|-|-
//! boot=<XXXX>|reused|<file path>|<device path hex>|<prior payload hex>|<prior BootOrder index or ->
//! ```
//! Uninstall accepts ONLY the paths install itself can produce (allow-list,
//! case-insensitive); any other path rejects the whole manifest.

use crate::{
    nvram::{self, BootRecord},
    sha256,
};
use bootapp_core::{Architecture, loadopt};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

pub const MANIFEST: &str = r"EFI\BOOTAPP\install-manifest.txt";
const MANIFEST_VERSION: &str = "1";
const MAX_MANAGER_BYTES: u64 = 64 * 1024 * 1024;
const ARCH_NAMES: [&str; 3] = ["IA32", "X64", "AA64"];

pub struct UefiInstall {
    pub esp: PathBuf,
    pub arch: Architecture,
    pub manager: PathBuf,
    pub config: Option<PathBuf>,
    pub fallback: bool,
    pub register: bool,
    pub apply: bool,
}

pub fn manager_file(arch: Architecture) -> &'static str {
    match arch {
        Architecture::X86 => "BOOTAPPIA32.EFI",
        Architecture::X64 => "BOOTAPPX64.EFI",
        Architecture::Aarch64 => "BOOTAPPAA64.EFI",
    }
}

/// Windows: `S:` is drive-relative (current directory of S:); use the root `S:\`.
pub fn normalize_esp(esp: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(text) = esp.to_str() {
        let bytes = text.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return PathBuf::from(format!("{text}\\"));
        }
    }
    esp.to_path_buf()
}

/// Files install can create, replace or back up (case-insensitive).
fn allowed_file(rel: &str) -> bool {
    let rel = rel.to_ascii_uppercase();
    ARCH_NAMES.iter().any(|a| {
        rel == format!(r"EFI\BOOTAPP\BOOTAPP{a}.EFI")
            || rel == format!(r"EFI\BOOT\BOOT{a}.EFI")
            || rel == format!(r"EFI\BOOTAPP\BACKUP\BOOT{a}.EFI")
    }) || rel == r"EFI\BOOTAPP\BOOTAPP.CFG"
        || rel == r"EFI\BOOTAPP\GRUB.CFG"
}

fn allowed_dir(rel: &str) -> bool {
    [r"EFI", r"EFI\BOOTAPP", r"EFI\BOOTAPP\backup", r"EFI\BOOT"]
        .iter()
        .any(|d| d.eq_ignore_ascii_case(rel))
}

/// `replaced=` may only pair `EFI\BOOT\X` with `EFI\BOOTAPP\backup\X`.
fn allowed_replacement(rel: &str, backup: &str) -> bool {
    ARCH_NAMES.iter().any(|a| {
        rel.eq_ignore_ascii_case(&format!(r"EFI\BOOT\BOOT{a}.EFI"))
            && backup.eq_ignore_ascii_case(&format!(r"EFI\BOOTAPP\backup\BOOT{a}.EFI"))
    })
}

/// `EFI\BOOTAPP\x` relative to `esp`, built component-wise (portable).
fn at(esp: &Path, rel: &str) -> PathBuf {
    rel.split('\\').fold(esp.to_path_buf(), |p, c| p.join(c))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

/// Refuse if any existing component of `rel` under `esp` is a link/reparse point.
fn check_no_links(esp: &Path, rel: &str) -> Result<(), String> {
    let mut path = esp.to_path_buf();
    let mut shown = String::new();
    for component in rel.split('\\') {
        path.push(component);
        if !shown.is_empty() {
            shown.push('\\');
        }
        shown.push_str(component);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{shown}: {e}")),
            Ok(m) if is_link(&m) => {
                return Err(format!(
                    "{shown} is a symlink, junction or reparse point; refusing to follow it"
                ));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

fn read_existing(esp: &Path, rel: &str) -> Result<Option<Vec<u8>>, String> {
    check_no_links(esp, rel)?;
    let path = at(esp, rel);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{rel}: {e}")),
        Ok(m) if m.is_file() => crate::read_bounded(&path, MAX_MANAGER_BYTES).map(Some),
        Ok(_) => Err(format!("{rel} exists and is not a regular file")),
    }
}

fn fingerprint(data: &[u8]) -> String {
    format!("{}|{}", data.len(), sha256::hex_digest(data))
}

/// Unique sibling temp name: `<name>.bootapp-<pid>-<n>.tmp`.
fn temp_sibling(path: &Path) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".bootapp-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (from_w, to_w) = (wide(from), wide(to));
    // SAFETY: both arguments are NUL-terminated UTF-16 buffers alive for the call;
    // without MOVEFILE_REPLACE_EXISTING the call fails if `to` exists.
    if unsafe { crate::win::MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_WRITE_THROUGH) }
        == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    // hard_link fails if `to` exists; filesystems without hard links (FAT) fall
    // back to check-then-rename (a narrow race, documented).
    match fs::hard_link(from, to) {
        Ok(()) => fs::remove_file(from),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) if fs::symlink_metadata(to).is_ok() => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "destination exists",
        )),
        Err(_) => fs::rename(from, to),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Put {
    /// Fails if the destination exists.
    New,
    /// Replaces the destination only if it still holds `expected`.
    Replace,
}

/// Temp file + fsync + rename, then read-back verification. The temp file is
/// removed on any failure; only files this process created are ever removed.
fn put_file(path: &Path, data: &[u8], mode: Put, expected: Option<&[u8]>) -> Result<(), String> {
    let temp = temp_sibling(path);
    let written = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("cannot create {}: {e}", temp.display()))?;
        file.write_all(data)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("writing {} failed: {e}", temp.display()))?;
        drop(file);
        if mode == Put::Replace
            && let Some(expected) = expected
            && fs::read(path).ok().as_deref() != Some(expected)
        {
            return Err(format!(
                "{} changed since planning; refusing to replace it",
                path.display()
            ));
        }
        match mode {
            Put::New => rename_no_replace(&temp, path),
            Put::Replace => fs::rename(&temp, path),
        }
        .map_err(|e| format!("cannot move {} into place: {e}", path.display()))
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    if fs::read(path).map_err(|e| e.to_string())? != data {
        return Err(format!("{} failed read-back verification", path.display()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Create,
    Keep,
    Replace { backup: String, original: Vec<u8> },
}

struct Planned {
    rel: String,
    data: Vec<u8>,
    action: Action,
}

/// Load and validate a config by file name: `grub.cfg` → GRUB subset, else bootapp.cfg.
pub fn load_config(path: &Path, arch: Architecture) -> Result<(&'static str, Vec<u8>), String> {
    let crate::LoadedConfig {
        kind,
        text,
        config,
        warnings,
    } = crate::load_any_config(path)?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    if config.entries_for(arch).is_empty() {
        eprintln!(
            "WARNING: {} has no entries for {}; only auto-discovered loaders will be listed.",
            path.display(),
            arch.as_str()
        );
    }
    let dest = if kind == crate::ConfigKind::Grub {
        r"EFI\BOOTAPP\grub.cfg"
    } else {
        r"EFI\BOOTAPP\bootapp.cfg"
    };
    Ok((dest, text.into_bytes()))
}

fn plan_file(
    esp: &Path,
    rel: &str,
    data: Vec<u8>,
    backup: Option<&str>,
) -> Result<Planned, String> {
    let action = match read_existing(esp, rel)? {
        None => Action::Create,
        Some(existing) if existing == data => Action::Keep,
        Some(existing) => match backup {
            None => {
                return Err(format!(
                    "{rel} already exists with different content; refusing to overwrite a BootApp file (run `bootapp uninstall uefi` or remove it)"
                ));
            }
            Some(backup) => {
                check_no_links(esp, backup)?;
                if fs::symlink_metadata(at(esp, backup)).is_ok() {
                    return Err(format!(
                        "{rel} must be backed up but {backup} already exists; refusing (resolve the old backup first)"
                    ));
                }
                Action::Replace {
                    backup: backup.into(),
                    original: existing,
                }
            }
        },
    };
    Ok(Planned {
        rel: rel.into(),
        data,
        action,
    })
}

fn manifest_text(
    arch: Architecture,
    state: &str,
    dirs: &[String],
    files: &[Planned],
    boot: Option<&str>,
) -> String {
    let mut out = String::from(
        "# BootApp install manifest; read by `bootapp uninstall uefi`. Do not edit.\n",
    );
    out += &format!(
        "manifest_version={MANIFEST_VERSION}\nbootapp_version=0.3\narch={}\nstate={state}\n",
        arch.as_str()
    );
    for d in dirs {
        out += &format!("dir={d}\n");
    }
    for f in files {
        match &f.action {
            Action::Create => out += &format!("file={}|{}\n", f.rel, fingerprint(&f.data)),
            Action::Keep => out += &format!("kept={}|{}\n", f.rel, fingerprint(&f.data)),
            Action::Replace { backup, original } => {
                out += &format!(
                    "replaced={}|{}|{backup}|{}\n",
                    f.rel,
                    fingerprint(&f.data),
                    fingerprint(original)
                )
            }
        }
    }
    if let Some(boot) = boot {
        out += &format!("boot={boot}\n");
    }
    out
}

fn boot_line(file_path: &str, device_path: &[u8], r: &nvram::Registration) -> String {
    let number = loadopt::boot_variable_name(r.number);
    let (how, prior, position) = if r.reused {
        (
            "reused",
            r.prior_payload
                .as_deref()
                .map_or("-".to_string(), crate::hex),
            r.prior_position.map_or("-".to_string(), |p| p.to_string()),
        )
    } else {
        ("created", "-".to_string(), "-".to_string())
    };
    format!(
        "{}|{how}|{file_path}|{}|{prior}|{position}",
        number.trim_start_matches("Boot"),
        crate::hex(device_path)
    )
}

pub fn install(args: &UefiInstall) -> Result<(), String> {
    let esp = &normalize_esp(&args.esp);
    if !fs::metadata(esp).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!(
            "--esp {} is not an existing directory",
            esp.display()
        ));
    }
    for dir in [r"EFI\BOOTAPP\backup", r"EFI\BOOT"] {
        check_no_links(esp, dir)?;
    }
    let manager = crate::read_bounded(&args.manager, MAX_MANAGER_BYTES)?;
    crate::validate_pe(&manager, args.arch)
        .map_err(|e| format!("{}: {e} for {}", args.manager.display(), args.arch.as_str()))?;
    let manager_rel = format!(r"EFI\BOOTAPP\{}", manager_file(args.arch));
    let planned = (|| -> Result<Vec<Planned>, String> {
        let mut files = vec![plan_file(esp, &manager_rel, manager.clone(), None)?];
        if let Some(config) = &args.config {
            let (dest, data) = load_config(config, args.arch)?;
            if dest.ends_with("grub.cfg") && at(esp, r"EFI\BOOTAPP\bootapp.cfg").exists() {
                eprintln!(
                    r"WARNING: EFI\BOOTAPP\bootapp.cfg exists and takes precedence over grub.cfg."
                );
            }
            files.push(plan_file(esp, dest, data, None)?);
        }
        if args.fallback {
            let name = crate::efi_name(args.arch);
            let rel = format!(r"EFI\BOOT\{name}");
            let backup = format!(r"EFI\BOOTAPP\backup\{name}");
            files.push(plan_file(esp, &rel, manager.clone(), Some(&backup))?);
        }
        Ok(files)
    })();
    if read_existing(esp, MANIFEST)?.is_some() {
        let manifest = Manifest::read(esp)?;
        let identical = planned
            .as_ref()
            .is_ok_and(|files| files.iter().all(|f| f.action == Action::Keep));
        if identical && manifest.state == "complete" && (!args.register || manifest.boot.is_some())
        {
            println!(
                "BootApp is already installed with identical files (idempotent); nothing to do."
            );
            return Ok(());
        }
        return Err(format!(
            "an existing BootApp installation was found ({MANIFEST}, state={}); run `bootapp uninstall uefi --esp {} --apply` first",
            manifest.state,
            esp.display()
        ));
    }
    let files = planned?;
    let mut dirs = vec![r"EFI".to_string(), r"EFI\BOOTAPP".to_string()];
    if files
        .iter()
        .any(|f| matches!(f.action, Action::Replace { .. }))
    {
        dirs.push(r"EFI\BOOTAPP\backup".into());
    }
    if args.fallback {
        dirs.push(r"EFI\BOOT".into());
    }
    let created_dirs: Vec<String> = dirs.into_iter().filter(|d| !at(esp, d).exists()).collect();
    let file_path = format!("\\{manager_rel}");

    // NVRAM plan (Windows + admin only); computed before any write.
    let registration = if args.register {
        let hd = crate::disk::esp_hard_drive_node(esp)?;
        let device_path = loadopt::hard_drive_file_device_path(&hd, &file_path);
        let vars = nvram::system_vars()?;
        Some((
            nvram::plan_register(vars.as_ref(), &device_path)?,
            device_path,
        ))
    } else {
        None
    };
    let boot_line = registration
        .as_ref()
        .map(|(r, device_path)| boot_line(&file_path, device_path, r));

    println!("Plan (UEFI install to {}):", esp.display());
    for d in &created_dirs {
        println!("  mkdir {d}");
    }
    println!("  create {MANIFEST} (journal, state=pending)");
    for f in &files {
        match &f.action {
            Action::Create => println!(
                "  create {} ({} bytes, sha256 {})",
                f.rel,
                f.data.len(),
                sha256::hex_digest(&f.data)
            ),
            Action::Keep => println!("  keep {} (already present, byte-identical)", f.rel),
            Action::Replace { backup, original } => println!(
                "  backup {} -> {backup} ({} bytes), then replace {} ({} bytes, sha256 {})",
                f.rel,
                original.len(),
                f.rel,
                f.data.len(),
                sha256::hex_digest(&f.data)
            ),
        }
    }
    if let Some((r, device_path)) = &registration {
        println!(
            "  register {} ({}) device path {}",
            loadopt::boot_variable_name(r.number),
            if r.reused {
                "reusing existing BootApp entry; its prior payload and BootOrder position are recorded"
            } else {
                "new entry"
            },
            loadopt::describe_device_path(device_path)
        );
        nvram::print_changes(&r.changes);
    }
    println!("  rewrite {MANIFEST} (state=complete)");
    if !args.apply {
        println!("DRY RUN: nothing was written. Re-run with --apply to perform this plan.");
        return Ok(());
    }

    // Directories first; on failure remove the ones we made.
    let mut made: Vec<String> = Vec::new();
    for d in &created_dirs {
        if let Err(e) = fs::create_dir(at(esp, d)) {
            for m in made.iter().rev() {
                let _ = fs::remove_dir(at(esp, m));
            }
            return Err(format!(
                "cannot create {d}: {e}; created directories were removed"
            ));
        }
        made.push(d.clone());
    }
    let pending = manifest_text(
        args.arch,
        "pending",
        &created_dirs,
        &files,
        boot_line.as_deref(),
    );
    if let Err(e) = put_file(&at(esp, MANIFEST), pending.as_bytes(), Put::New, None) {
        for m in made.iter().rev() {
            let _ = fs::remove_dir(at(esp, m));
        }
        return Err(format!("{e}; created directories were removed"));
    }
    println!("done: wrote {MANIFEST} (pending)");
    let result = (|| -> Result<(), String> {
        for f in &files {
            if let Action::Replace { backup, original } = &f.action {
                put_file(&at(esp, backup), original, Put::New, None)?;
                println!("done: backup {} -> {backup}", f.rel);
            }
        }
        for f in &files {
            match &f.action {
                Action::Keep => continue,
                Action::Create => put_file(&at(esp, &f.rel), &f.data, Put::New, None)?,
                Action::Replace { original, .. } => {
                    put_file(&at(esp, &f.rel), &f.data, Put::Replace, Some(original))?
                }
            }
            println!("done: wrote {}", f.rel);
        }
        if let Some((r, _)) = &registration {
            let mut vars = nvram::system_vars()?;
            nvram::apply_changes(vars.as_mut(), &r.changes)?;
        }
        let complete = manifest_text(
            args.arch,
            "complete",
            &created_dirs,
            &files,
            boot_line.as_deref(),
        );
        put_file(
            &at(esp, MANIFEST),
            complete.as_bytes(),
            Put::Replace,
            Some(pending.as_bytes()),
        )?;
        println!("done: wrote {MANIFEST} (complete)");
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("BootApp: install failed: {e}");
        eprintln!("Rolling back using the manifest journal...");
        return match uninstall(esp, true) {
            Ok(()) => Err(format!(
                "install failed ({e}); rollback completed, ESP restored"
            )),
            Err(r) => Err(format!(
                "install failed ({e}); ROLLBACK INCOMPLETE ({r}). The journal {MANIFEST} is kept; fix the cause and run `bootapp uninstall uefi --esp {} --apply`",
                esp.display()
            )),
        };
    }
    println!(
        "BootApp {} installed in {}{}.",
        args.arch.as_str(),
        esp.display(),
        if args.register { " and registered" } else { "" }
    );
    Ok(())
}

#[derive(Debug, Default)]
struct Manifest {
    state: String,
    dirs: Vec<String>,
    /// rel, size|sha
    files: Vec<(String, String)>,
    kept: Vec<String>,
    /// rel, new fingerprint, backup rel, original fingerprint
    replaced: Vec<(String, String, String, String)>,
    /// file path, record
    boot: Option<(String, BootRecord)>,
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty()
        || !text.len().is_multiple_of(2)
        || text.len() > 2 * loadopt::MAX_LOAD_OPTION_BYTES
    {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

impl Manifest {
    fn read(esp: &Path) -> Result<Self, String> {
        check_no_links(esp, MANIFEST)?;
        let path = at(esp, MANIFEST);
        let data = crate::read_bounded(&path, 64 * 1024)
            .map_err(|e| format!("cannot read install manifest: {e}"))?;
        let text =
            String::from_utf8(data).map_err(|_| "install manifest is not UTF-8".to_string())?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut m = Manifest::default();
        let mut version = None;
        for (index, line) in text.lines().enumerate() {
            let bad = |what: &str| format!("install manifest line {}: {what}", index + 1);
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| bad("expected key=value"))?;
            let fields: Vec<&str> = value.split('|').collect();
            let check_fp = |size: &str, sha: &str| {
                size.parse::<u64>().is_ok()
                    && sha.len() == 64
                    && sha.bytes().all(|b| b.is_ascii_hexdigit())
            };
            match (key, fields.as_slice()) {
                ("manifest_version", [v]) => version = Some(v.to_string()),
                ("bootapp_version" | "arch", [_]) => {}
                ("state", [s]) if *s == "pending" || *s == "complete" => m.state = s.to_string(),
                ("dir", [d]) if allowed_dir(d) => m.dirs.push(d.to_string()),
                ("file", [rel, size, sha]) if allowed_file(rel) && check_fp(size, sha) => {
                    m.files.push((rel.to_string(), format!("{size}|{sha}")))
                }
                ("kept", [rel, size, sha]) if allowed_file(rel) && check_fp(size, sha) => {
                    m.kept.push(rel.to_string())
                }
                ("replaced", [rel, size, sha, backup, osize, osha])
                    if allowed_replacement(rel, backup)
                        && check_fp(size, sha)
                        && check_fp(osize, osha) =>
                {
                    m.replaced.push((
                        rel.to_string(),
                        format!("{size}|{sha}"),
                        backup.to_string(),
                        format!("{osize}|{osha}"),
                    ))
                }
                ("boot", [number, how, file, path_hex, prior, position]) if m.boot.is_none() => {
                    let number = loadopt::parse_boot_number(number)
                        .ok_or_else(|| bad("invalid boot number"))?;
                    let created = match *how {
                        "created" => true,
                        "reused" => false,
                        _ => return Err(bad("boot must be created or reused")),
                    };
                    if !allowed_file(file.trim_start_matches('\\'))
                        || !file.starts_with('\\')
                        || !bootapp_core::validate_efi_path(file)
                    {
                        return Err(bad("invalid boot file path"));
                    }
                    let device_path = unhex(path_hex)
                        .filter(|p| {
                            loadopt::device_path_file_name(p)
                                .is_some_and(|name| name.eq_ignore_ascii_case(file))
                        })
                        .ok_or_else(|| bad("invalid boot device path"))?;
                    let prior_payload = match (*prior, created) {
                        ("-", _) => None,
                        (_, true) => return Err(bad("created entry cannot have a prior payload")),
                        (hex, false) => Some(
                            unhex(hex)
                                .filter(|p| loadopt::LoadOption::decode(p).is_ok())
                                .ok_or_else(|| bad("invalid prior payload"))?,
                        ),
                    };
                    let prior_position = match (*position, created) {
                        ("-", _) => None,
                        (_, true) => return Err(bad("created entry cannot have a prior position")),
                        (text, false) => Some(
                            text.parse::<usize>()
                                .ok()
                                .filter(|&p| p < loadopt::MAX_BOOT_ORDER_ENTRIES)
                                .ok_or_else(|| bad("invalid prior BootOrder position"))?,
                        ),
                    };
                    m.boot = Some((
                        file.to_string(),
                        BootRecord {
                            number,
                            created,
                            device_path,
                            prior_payload,
                            prior_position,
                        },
                    ));
                }
                _ => return Err(bad("unrecognized, malformed or non-BootApp entry")),
            }
        }
        if version.as_deref() != Some(MANIFEST_VERSION) {
            return Err(format!(
                "unsupported install manifest version (expected {MANIFEST_VERSION})"
            ));
        }
        if m.state.is_empty() {
            return Err("install manifest has no state".into());
        }
        Ok(m)
    }
}

enum Step {
    Remove(String),
    Restore {
        rel: String,
        backup: String,
        data: Vec<u8>,
        ours: String,
    },
    RemoveBackup(String),
}

/// Uninstall using the manifest (dry-run unless `apply`). Tolerates a partially
/// applied (pending) install: absent files are skipped. Files modified after
/// install are left in place (noted) while everything else is still undone; the
/// command then fails with a summary and keeps the manifest for a later re-run.
pub fn uninstall(esp: &Path, apply: bool) -> Result<(), String> {
    let esp = &normalize_esp(esp);
    let manifest = Manifest::read(esp)?;
    let mut steps = Vec::new();
    let mut notes = Vec::new();
    let mut skipped = Vec::new();
    for (rel, fp) in &manifest.files {
        match read_existing(esp, rel) {
            Err(e) => skipped.push(e),
            Ok(None) => notes.push(format!("{rel} is already absent")),
            Ok(Some(d)) if fingerprint(&d) == *fp => steps.push(Step::Remove(rel.clone())),
            Ok(Some(_)) => skipped.push(format!("{rel} was modified after install; left in place")),
        }
    }
    for (rel, fp, backup, ofp) in &manifest.replaced {
        let (current, saved) = match (read_existing(esp, rel), read_existing(esp, backup)) {
            (Ok(c), Ok(s)) => (c, s),
            (Err(e), _) | (_, Err(e)) => {
                skipped.push(e);
                continue;
            }
        };
        let saved_ok = saved.as_ref().is_some_and(|d| fingerprint(d) == *ofp);
        let current_fp = current.as_deref().map(fingerprint);
        match current_fp {
            Some(c) if c == *ofp => {
                if saved_ok {
                    steps.push(Step::RemoveBackup(backup.clone()));
                } else if saved.is_some() {
                    skipped.push(format!(
                        "{backup} does not match the manifest; left in place"
                    ));
                }
            }
            Some(c) if c != *fp => skipped.push(format!(
                "{rel} was modified after install (e.g. by OS servicing); left in place, backup {backup} kept"
            )),
            _ if saved_ok => steps.push(Step::Restore {
                rel: rel.clone(),
                backup: backup.clone(),
                data: saved.unwrap_or_default(),
                ours: fp.clone(),
            }),
            Some(_) => skipped.push(format!(
                "backup {backup} for {rel} is missing or modified; BootApp's {rel} left in place"
            )),
            None => notes.push(format!("{rel} and its backup are absent")),
        }
    }
    let mut nvram_plan = None;
    if let Some((_, record)) = &manifest.boot {
        let name = loadopt::boot_variable_name(record.number);
        match nvram::system_vars().and_then(|vars| nvram::plan_unregister(vars.as_ref(), record)) {
            Ok((changes, n)) => {
                notes.extend(n);
                nvram_plan = Some(changes);
            }
            Err(e) => skipped.push(format!("{name} could not be unregistered: {e}")),
        }
    }

    println!(
        "Plan (UEFI uninstall from {}, manifest state={}):",
        esp.display(),
        manifest.state
    );
    if let Some(changes) = &nvram_plan {
        nvram::print_changes(changes);
    }
    for step in &steps {
        match step {
            Step::Remove(rel) => println!("  remove {rel}"),
            Step::Restore { rel, backup, .. } => {
                println!("  restore {rel} from {backup}, then remove {backup}")
            }
            Step::RemoveBackup(backup) => println!("  remove unused backup {backup}"),
        }
    }
    for rel in &manifest.kept {
        println!("  keep {rel} (existed before install)");
    }
    if skipped.is_empty() {
        println!("  remove {MANIFEST}");
        for d in manifest.dirs.iter().rev() {
            println!("  rmdir {d} (if empty)");
        }
    } else {
        println!("  keep {MANIFEST} (some items are skipped)");
    }
    for n in &notes {
        println!("  note: {n}");
    }
    for s in &skipped {
        println!("  SKIP: {s}");
    }
    if !apply {
        println!("DRY RUN: nothing was changed. Re-run with --apply to perform this plan.");
        return Ok(());
    }
    let mut failed = Vec::new();
    if let Some(changes) = &nvram_plan
        && let Err(e) =
            nvram::system_vars().and_then(|mut vars| nvram::apply_changes(vars.as_mut(), changes))
    {
        failed.push(e);
    }
    for step in &steps {
        let outcome = match step {
            Step::Remove(rel) => fs::remove_file(at(esp, rel))
                .map(|_| format!("removed {rel}"))
                .map_err(|e| format!("remove {rel}: {e}")),
            Step::Restore {
                rel,
                backup,
                data,
                ours,
            } => {
                let current = read_existing(esp, rel).ok().flatten();
                if current.is_some() && current.as_deref().map(fingerprint).as_ref() != Some(ours) {
                    Err(format!("{rel} changed during uninstall; left in place"))
                } else {
                    let mode = if current.is_some() {
                        Put::Replace
                    } else {
                        Put::New
                    };
                    put_file(&at(esp, rel), data, mode, current.as_deref())
                        .and_then(|_| {
                            fs::remove_file(at(esp, backup))
                                .map_err(|e| format!("remove {backup}: {e}"))
                        })
                        .map(|_| format!("restored {rel}"))
                }
            }
            Step::RemoveBackup(backup) => fs::remove_file(at(esp, backup))
                .map(|_| format!("removed {backup}"))
                .map_err(|e| format!("remove {backup}: {e}")),
        };
        match outcome {
            Ok(done) => println!("done: {done}"),
            Err(e) => failed.push(e),
        }
    }
    if !skipped.is_empty() || !failed.is_empty() {
        let mut summary = Vec::new();
        summary.extend(skipped.iter().map(|s| format!("skipped: {s}")));
        summary.extend(failed.iter().map(|f| format!("failed: {f}")));
        return Err(format!(
            "uninstall incomplete; everything else was undone. {}. {MANIFEST} is kept: resolve the items above and re-run `bootapp uninstall uefi`, or, after reviewing them, delete the manifest and leftover backups manually",
            summary.join("; ")
        ));
    }
    fs::remove_file(at(esp, MANIFEST)).map_err(|e| format!("remove {MANIFEST}: {e}"))?;
    println!("done: removed {MANIFEST}");
    for d in manifest.dirs.iter().rev() {
        match fs::remove_dir(at(esp, d)) {
            Ok(()) => println!("done: removed directory {d}"),
            Err(_) => println!("kept directory {d} (not empty)"),
        }
    }
    println!("BootApp uninstalled from {}.", esp.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::pe;

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("bootapp-uefi-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("esp")).unwrap();
            fs::write(root.join("manager.efi"), pe(Architecture::X64)).unwrap();
            fs::write(
                root.join("bootapp.cfg"),
                "[entry t]\ntitle=T\npath=\\EFI\\BOOTAPP\\payload.efi\n",
            )
            .unwrap();
            Self { root }
        }
        fn args(&self, apply: bool) -> UefiInstall {
            UefiInstall {
                esp: self.root.join("esp"),
                arch: Architecture::X64,
                manager: self.root.join("manager.efi"),
                config: Some(self.root.join("bootapp.cfg")),
                fallback: true,
                register: false,
                apply,
            }
        }
        fn snapshot(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
            fn walk(p: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
                let mut entries: Vec<_> = fs::read_dir(p)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .collect();
                entries.sort();
                for e in entries {
                    if e.is_dir() {
                        out.push((e.clone(), None));
                        walk(&e, out);
                    } else {
                        out.push((e.clone(), Some(fs::read(&e).unwrap())));
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.root.join("esp"), &mut out);
            out
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn install_uninstall_round_trip_with_fallback_backup() {
        let f = Fixture::new("roundtrip");
        let esp = f.root.join("esp");
        fs::create_dir_all(esp.join("EFI").join("BOOT")).unwrap();
        fs::write(
            esp.join("EFI").join("BOOT").join("BOOTX64.EFI"),
            b"original fallback",
        )
        .unwrap();
        fs::write(esp.join("EFI").join("vendor.txt"), b"other").unwrap();
        let before = f.snapshot();
        install(&f.args(false)).unwrap();
        assert_eq!(f.snapshot(), before, "dry run must not write");
        install(&f.args(true)).unwrap();
        let bootapp = esp.join("EFI").join("BOOTAPP");
        assert_eq!(
            fs::read(bootapp.join("BOOTAPPX64.EFI")).unwrap(),
            pe(Architecture::X64)
        );
        assert_eq!(
            fs::read(esp.join("EFI").join("BOOT").join("BOOTX64.EFI")).unwrap(),
            pe(Architecture::X64)
        );
        assert_eq!(
            fs::read(bootapp.join("backup").join("BOOTX64.EFI")).unwrap(),
            b"original fallback"
        );
        assert!(bootapp.join("bootapp.cfg").exists());
        let manifest = fs::read_to_string(bootapp.join("install-manifest.txt")).unwrap();
        assert!(manifest.contains("state=complete"), "{manifest}");
        assert!(
            manifest.contains(r"replaced=EFI\BOOT\BOOTX64.EFI|"),
            "{manifest}"
        );
        // Identical re-install is a no-op; changed manager is refused.
        install(&f.args(true)).unwrap();
        let mut changed = pe(Architecture::X64);
        changed[200] = 1;
        fs::write(f.root.join("manager.efi"), changed).unwrap();
        assert!(
            install(&f.args(true))
                .unwrap_err()
                .contains("existing BootApp installation")
        );
        fs::write(f.root.join("manager.efi"), pe(Architecture::X64)).unwrap();
        // Uninstall: dry run changes nothing, apply restores the exact prior tree.
        let installed = f.snapshot();
        uninstall(&esp, false).unwrap();
        assert_eq!(f.snapshot(), installed);
        uninstall(&esp, true).unwrap();
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn empty_esp_round_trip_and_modified_file_refusal() {
        let f = Fixture::new("empty");
        let esp = f.root.join("esp");
        install(&f.args(true)).unwrap();
        let cfg = esp.join("EFI").join("BOOTAPP").join("bootapp.cfg");
        fs::write(&cfg, "[entry t]\ntitle=Edited\npath=\\EFI\\x.efi\n").unwrap();
        let err = uninstall(&esp, true).unwrap_err();
        assert!(err.contains("modified"), "{err}");
        assert!(
            esp.join("EFI")
                .join("BOOTAPP")
                .join("install-manifest.txt")
                .exists()
        );
        fs::write(&cfg, fs::read(f.root.join("bootapp.cfg")).unwrap()).unwrap();
        uninstall(&esp, true).unwrap();
        assert!(
            fs::read_dir(&esp).unwrap().next().is_none(),
            "ESP must be empty again"
        );
    }

    #[test]
    fn refuses_to_overwrite_without_backup() {
        let f = Fixture::new("refuse");
        let esp = f.root.join("esp");
        let bootapp = esp.join("EFI").join("BOOTAPP");
        fs::create_dir_all(bootapp.join("backup")).unwrap();
        fs::write(bootapp.join("BOOTAPPX64.EFI"), b"foreign").unwrap();
        assert!(
            install(&f.args(true))
                .unwrap_err()
                .contains("already exists with different content")
        );
        fs::remove_file(bootapp.join("BOOTAPPX64.EFI")).unwrap();
        fs::create_dir_all(esp.join("EFI").join("BOOT")).unwrap();
        fs::write(esp.join("EFI").join("BOOT").join("BOOTX64.EFI"), b"vendor").unwrap();
        fs::write(bootapp.join("backup").join("BOOTX64.EFI"), b"old backup").unwrap();
        let before = f.snapshot();
        assert!(
            install(&f.args(true))
                .unwrap_err()
                .contains("already exists; refusing")
        );
        assert_eq!(f.snapshot(), before);
        // Wrong architecture manager.
        let mut args = f.args(true);
        args.arch = Architecture::Aarch64;
        assert!(install(&args).is_err());
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn kept_identical_files_survive_uninstall() {
        let f = Fixture::new("kept");
        let esp = f.root.join("esp");
        let bootapp = esp.join("EFI").join("BOOTAPP");
        fs::create_dir_all(&bootapp).unwrap();
        fs::write(
            bootapp.join("bootapp.cfg"),
            fs::read(f.root.join("bootapp.cfg")).unwrap(),
        )
        .unwrap();
        let before = f.snapshot();
        install(&f.args(true)).unwrap();
        uninstall(&esp, true).unwrap();
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn grub_config_goes_to_grub_cfg() {
        let f = Fixture::new("grub");
        let esp = f.root.join("esp");
        fs::write(
            f.root.join("grub.cfg"),
            "menuentry 'Payload' {\n  chainloader /EFI/BOOTAPP/payload.efi\n}\n",
        )
        .unwrap();
        let mut args = f.args(true);
        args.config = Some(f.root.join("grub.cfg"));
        args.fallback = false;
        install(&args).unwrap();
        assert!(esp.join("EFI").join("BOOTAPP").join("grub.cfg").exists());
        assert!(!esp.join("EFI").join("BOOTAPP").join("bootapp.cfg").exists());
        uninstall(&esp, true).unwrap();
    }

    #[test]
    fn pending_journal_can_be_uninstalled() {
        let f = Fixture::new("pending");
        let esp = f.root.join("esp");
        install(&f.args(true)).unwrap();
        // Simulate a crash after the journal but before files were written.
        let bootapp = esp.join("EFI").join("BOOTAPP");
        let manifest = fs::read_to_string(bootapp.join("install-manifest.txt")).unwrap();
        fs::write(
            bootapp.join("install-manifest.txt"),
            manifest.replace("state=complete", "state=pending"),
        )
        .unwrap();
        fs::remove_file(bootapp.join("BOOTAPPX64.EFI")).unwrap();
        fs::remove_file(esp.join("EFI").join("BOOT").join("BOOTX64.EFI")).unwrap();
        uninstall(&esp, true).unwrap();
        assert!(fs::read_dir(&esp).unwrap().next().is_none());
    }

    #[test]
    fn manifest_parser_rejects_tampering() {
        let fp = format!("1|{}", "0".repeat(64));
        let head = "manifest_version=1\nstate=complete\n";
        assert!(Manifest::parse("manifest_version=2\nstate=complete\n").is_err());
        for line in [
            format!("file=..\\x|{fp}"),
            format!("file=EFI\\..\\x|{fp}"),
            format!(r"file=EFI\Microsoft\Boot\bootmgfw.efi|{fp}"),
            format!(r"kept=EFI\vendor\x.efi|{fp}"),
            r"dir=EFI\Microsoft\Boot".to_string(),
            r"dir=EFI\Microsoft".to_string(),
            format!(r"replaced=EFI\BOOT\BOOTX64.EFI|{fp}|EFI\BOOTAPP\backup\BOOTIA32.EFI|{fp}"),
            format!(
                r"replaced=EFI\Microsoft\Boot\bootmgfw.efi|{fp}|EFI\BOOTAPP\backup\BOOTX64.EFI|{fp}"
            ),
            format!(r"replaced=EFI\BOOT\BOOTX64.EFI|{fp}|EFI\BOOT\BOOTX64.EFI|{fp}"),
            "unknown=1".to_string(),
        ] {
            assert!(
                Manifest::parse(&format!("{head}{line}\n")).is_err(),
                "{line}"
            );
        }
        // Every path install can write is accepted, case-insensitively.
        let ok_lines = [
            format!(r"file=EFI\BOOTAPP\BOOTAPPX64.EFI|{fp}"),
            format!(r"file=efi\bootapp\bootappaa64.efi|{fp}"),
            format!(r"file=EFI\BOOTAPP\grub.cfg|{fp}"),
            format!(r"kept=EFI\BOOTAPP\bootapp.cfg|{fp}"),
            format!(r"replaced=EFI\BOOT\BOOTIA32.EFI|{fp}|EFI\BOOTAPP\backup\BOOTIA32.EFI|{fp}"),
            r"dir=EFI\BOOTAPP\backup".to_string(),
        ];
        let text = format!("{head}{}\n", ok_lines.join("\n"));
        Manifest::parse(&text).unwrap();

        let hd = loadopt::HardDriveNode {
            partition_number: 1,
            partition_start_lba: 2048,
            partition_size_lba: 4096,
            signature: [3; 16],
            partition_format: 2,
            signature_type: 2,
        };
        let file = r"\EFI\BOOTAPP\BOOTAPPX64.EFI";
        let path = loadopt::hard_drive_file_device_path(&hd, file);
        let hex = crate::hex(&path);
        let ok = format!("{head}boot=0004|created|{file}|{hex}|-|-\n");
        let m = Manifest::parse(&ok).unwrap();
        let (f, record) = m.boot.unwrap();
        assert_eq!(f, file);
        assert_eq!((record.number, record.created), (4, true));
        assert_eq!(record.device_path, path);
        let prior = loadopt::LoadOption {
            attributes: 0,
            description: "BootApp".into(),
            device_path: path.clone(),
            optional_data: Vec::new(),
        }
        .encode();
        let reused = format!(
            "{head}boot=0004|reused|{file}|{hex}|{}|3\n",
            crate::hex(&prior)
        );
        let (_, record) = Manifest::parse(&reused).unwrap().boot.unwrap();
        assert_eq!(record.prior_payload, Some(prior.clone()));
        assert_eq!(record.prior_position, Some(3));
        let other = crate::hex(&loadopt::hard_drive_file_device_path(
            &hd,
            r"\EFI\other.efi",
        ));
        for line in [
            format!("boot=0004|created|{file}|zz|-|-"),
            format!("boot=0004|created|{file}|{other}|-|-"),
            format!(r"boot=0004|created|\EFI\other.efi|{other}|-|-"),
            format!("boot=0004|created|{file}|{hex}"),
            format!("boot=0004|created|{file}|{hex}|{}|-", crate::hex(&prior)),
            format!("boot=0004|created|{file}|{hex}|-|2"),
            format!("boot=0004|reused|{file}|{hex}|00|-"),
            format!("boot=0004|reused|{file}|{hex}|-|x"),
        ] {
            assert!(
                Manifest::parse(&format!("{head}{line}\n")).is_err(),
                "{line}"
            );
        }
    }

    #[test]
    fn tampered_manifest_cannot_delete_foreign_files() {
        let f = Fixture::new("tamper");
        let esp = f.root.join("esp");
        install(&f.args(true)).unwrap();
        let ms = esp.join("EFI").join("Microsoft").join("Boot");
        fs::create_dir_all(&ms).unwrap();
        let foreign = ms.join("bootmgfw.efi");
        fs::write(&foreign, b"windows boot manager").unwrap();
        let manifest = esp.join("EFI").join("BOOTAPP").join("install-manifest.txt");
        let original = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            format!(
                "{original}file=EFI\\Microsoft\\Boot\\bootmgfw.efi|{}\ndir=EFI\\Microsoft\\Boot\n",
                fingerprint(b"windows boot manager")
            ),
        )
        .unwrap();
        let before = f.snapshot();
        assert!(uninstall(&esp, true).is_err());
        assert_eq!(
            f.snapshot(),
            before,
            "a rejected manifest must change nothing"
        );
        assert_eq!(fs::read(&foreign).unwrap(), b"windows boot manager");
    }

    #[test]
    fn modified_files_are_skipped_and_the_rest_is_undone() {
        let f = Fixture::new("skip");
        let esp = f.root.join("esp");
        let boot = esp.join("EFI").join("BOOT");
        fs::create_dir_all(&boot).unwrap();
        fs::write(boot.join("BOOTX64.EFI"), b"vendor fallback").unwrap();
        install(&f.args(true)).unwrap();
        // OS servicing rewrites the fallback loader after install.
        fs::write(boot.join("BOOTX64.EFI"), b"serviced fallback").unwrap();
        let err = uninstall(&esp, true).unwrap_err();
        assert!(
            err.contains("skipped") && err.contains("BOOTX64.EFI"),
            "{err}"
        );
        let bootapp = esp.join("EFI").join("BOOTAPP");
        assert!(!bootapp.join("BOOTAPPX64.EFI").exists());
        assert!(!bootapp.join("bootapp.cfg").exists());
        assert_eq!(
            fs::read(boot.join("BOOTX64.EFI")).unwrap(),
            b"serviced fallback"
        );
        assert_eq!(
            fs::read(bootapp.join("backup").join("BOOTX64.EFI")).unwrap(),
            b"vendor fallback"
        );
        assert!(bootapp.join("install-manifest.txt").exists());
    }

    #[test]
    fn create_never_replaces_and_cleans_its_temp() {
        let f = Fixture::new("create");
        let target = f.root.join("esp").join("appeared.efi");
        fs::write(&target, b"appeared after planning").unwrap();
        assert!(put_file(&target, b"ours", Put::New, None).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"appeared after planning");
        assert!(put_file(&target, b"ours", Put::Replace, Some(b"something else")).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"appeared after planning");
        let names: Vec<_> = fs::read_dir(f.root.join("esp")).unwrap().collect();
        assert_eq!(names.len(), 1, "temp files must be removed on failure");
        assert_ne!(temp_sibling(&target), temp_sibling(&target));
    }

    #[test]
    fn foreign_temp_leftovers_are_not_deleted() {
        let f = Fixture::new("leftover");
        let esp = f.root.join("esp");
        install(&f.args(true)).unwrap();
        let stray = esp
            .join("EFI")
            .join("BOOTAPP")
            .join("BOOTAPPX64.EFI.bootapp-1-0.tmp");
        fs::write(&stray, b"partial from a crashed run").unwrap();
        uninstall(&esp, true).unwrap();
        assert_eq!(fs::read(&stray).unwrap(), b"partial from a crashed run");
    }

    fn make_dir_link(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
        }
    }

    #[test]
    fn link_components_are_refused() {
        let f = Fixture::new("links");
        let esp = f.root.join("esp");
        let elsewhere = f.root.join("elsewhere");
        fs::create_dir_all(elsewhere.join("BOOTAPP")).unwrap();
        if !make_dir_link(&esp.join("EFI"), &elsewhere) {
            eprintln!("cannot create a directory link here; test skipped");
            return;
        }
        let err = install(&f.args(true)).unwrap_err();
        assert!(err.contains("symlink, junction or reparse point"), "{err}");
        assert!(
            fs::read_dir(elsewhere.join("BOOTAPP"))
                .unwrap()
                .next()
                .is_none()
        );
        // Remove the junction/symlink itself (not its target).
        #[cfg(windows)]
        fs::remove_dir(esp.join("EFI")).unwrap();
        #[cfg(unix)]
        fs::remove_file(esp.join("EFI")).unwrap();
        // A link deeper down (EFI\BOOT) is refused too.
        fs::create_dir_all(esp.join("EFI")).unwrap();
        if make_dir_link(&esp.join("EFI").join("BOOT"), &elsewhere) {
            let err = install(&f.args(true)).unwrap_err();
            assert!(err.contains(r"EFI\BOOT is a symlink"), "{err}");
            #[cfg(windows)]
            fs::remove_dir(esp.join("EFI").join("BOOT")).unwrap();
            #[cfg(unix)]
            fs::remove_file(esp.join("EFI").join("BOOT")).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn bare_drive_letter_means_volume_root() {
        assert_eq!(normalize_esp(Path::new("S:")), PathBuf::from("S:\\"));
        assert_eq!(normalize_esp(Path::new("s:\\")), PathBuf::from("s:\\"));
        assert_eq!(
            normalize_esp(Path::new("S:\\EFI")),
            PathBuf::from("S:\\EFI")
        );
    }
}

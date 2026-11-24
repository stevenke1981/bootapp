//! UEFI boot variable management (`Boot####`, `BootOrder`, `BootNext`).
//!
//! All logic is written against `FirmwareVars` so it is unit-tested with an
//! in-memory fake; the Windows implementation is the only code touching NVRAM.
//! Variables live under EFI_GLOBAL_VARIABLE_GUID.
//!
//! Enumeration limit: Windows offers no simple Boot#### enumeration API, so the
//! known set is `BootOrder` plus every number from 0000 up to the first absent
//! one. Entries beyond that gap and not in BootOrder are not listed/reused.

use bootapp_core::loadopt::{
    self, BOOT_VARIABLE_ATTRIBUTES, BOOTAPP_DESCRIPTION, LOAD_OPTION_ACTIVE, LoadOption,
    MAX_BOOT_ORDER_ENTRIES,
};

pub trait FirmwareVars {
    /// `Ok(None)` when the variable does not exist.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String>;
    /// Empty `data` deletes the variable.
    fn set(&mut self, name: &str, data: &[u8], attributes: u32) -> Result<(), String>;
}

/// One variable write (empty data = delete).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub data: Vec<u8>,
    pub attributes: u32,
}

fn change(name: impl Into<String>, data: Vec<u8>) -> Change {
    Change {
        name: name.into(),
        data,
        attributes: BOOT_VARIABLE_ATTRIBUTES,
    }
}

pub fn print_changes(changes: &[Change]) {
    if changes.is_empty() {
        println!("  (no firmware variable changes)");
    }
    for c in changes {
        if c.data.is_empty() {
            println!(
                "  DELETE {}-{}",
                c.name,
                loadopt::EFI_GLOBAL_VARIABLE_GUID_TEXT
            );
        } else {
            println!(
                "  SET {}-{} attributes=0x{:08X} {} bytes:",
                c.name,
                loadopt::EFI_GLOBAL_VARIABLE_GUID_TEXT,
                c.attributes,
                c.data.len()
            );
            for line in c.data.chunks(32) {
                println!("    {}", crate::hex(line));
            }
        }
    }
}

/// Apply in order; on failure report exactly what was already applied.
pub fn apply_changes(vars: &mut dyn FirmwareVars, changes: &[Change]) -> Result<(), String> {
    for (index, c) in changes.iter().enumerate() {
        vars.set(&c.name, &c.data, c.attributes).map_err(|e| {
            let done: Vec<&str> = changes[..index].iter().map(|c| c.name.as_str()).collect();
            format!(
                "writing {} failed: {e}. Already applied: [{}]",
                c.name,
                done.join(", ")
            )
        })?;
        println!(
            "done: {} {}",
            if c.data.is_empty() { "deleted" } else { "set" },
            c.name
        );
    }
    Ok(())
}

pub fn read_boot_order(vars: &dyn FirmwareVars) -> Result<Vec<u16>, String> {
    match vars.get("BootOrder")? {
        None => Ok(Vec::new()),
        Some(bytes) => loadopt::decode_boot_order(&bytes).map_err(|e| format!("BootOrder: {e}")),
    }
}

fn read_u16(vars: &dyn FirmwareVars, name: &str) -> Result<Option<u16>, String> {
    match vars.get(name)? {
        None => Ok(None),
        Some(b) if b.len() == 2 => Ok(Some(u16::from_le_bytes([b[0], b[1]]))),
        Some(b) => Err(format!("{name} has unexpected length {}", b.len())),
    }
}

/// Raw Boot#### bytes, decoded.
pub fn read_entry(
    vars: &dyn FirmwareVars,
    number: u16,
) -> Result<Option<Result<LoadOption, String>>, String> {
    let name = loadopt::boot_variable_name(number);
    Ok(vars
        .get(&name)?
        .map(|bytes| LoadOption::decode(&bytes).map_err(|e| e.to_string())))
}

/// BootOrder plus the contiguous run of existing numbers from 0000.
fn known_numbers(vars: &dyn FirmwareVars, order: &[u16]) -> Result<Vec<u16>, String> {
    let mut known = order.to_vec();
    for number in 0..=u16::MAX {
        if vars.get(&loadopt::boot_variable_name(number))?.is_none() {
            break;
        }
        if !known.contains(&number) {
            known.push(number);
        }
    }
    Ok(known)
}

pub fn render_list(vars: &dyn FirmwareVars) -> Result<String, String> {
    let mut out = String::new();
    let order = read_boot_order(vars)?;
    let fmt = |v: Option<u16>| v.map_or("(not set)".to_string(), |n| format!("{n:04X}"));
    out += &format!("BootCurrent: {}\n", fmt(read_u16(vars, "BootCurrent")?));
    out += &format!("BootNext: {}\n", fmt(read_u16(vars, "BootNext")?));
    if let Some(timeout) = read_u16(vars, "Timeout")? {
        out += &format!("Timeout: {timeout} seconds\n");
    }
    let joined: Vec<String> = order.iter().map(|n| format!("{n:04X}")).collect();
    out += &format!(
        "BootOrder: {}\n",
        if joined.is_empty() {
            "(empty)".into()
        } else {
            joined.join(",")
        }
    );
    for number in known_numbers(vars, &order)? {
        let suffix = if order.contains(&number) {
            ""
        } else {
            "  (not in BootOrder)"
        };
        let name = loadopt::boot_variable_name(number);
        match read_entry(vars, number)? {
            None => out += &format!("{name}  (referenced by BootOrder but missing)\n"),
            Some(Err(e)) => out += &format!("{name}  (invalid load option: {e}){suffix}\n"),
            Some(Ok(option)) => {
                out += &format!(
                    "{name}{} {} | {}{suffix}\n",
                    if option.attributes & LOAD_OPTION_ACTIVE != 0 {
                        "*"
                    } else {
                        " "
                    },
                    option.description,
                    loadopt::describe_device_path(&option.device_path)
                )
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub number: u16,
    /// An existing BootApp entry for the same target was reused.
    pub reused: bool,
    /// Payload of the reused entry before this registration (restored on uninstall).
    pub prior_payload: Option<Vec<u8>>,
    /// Index of the reused entry in BootOrder before this registration.
    pub prior_position: Option<usize>,
    pub changes: Vec<Change>,
}

/// What an install manifest records about its registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootRecord {
    pub number: u16,
    pub created: bool,
    pub device_path: Vec<u8>,
    pub prior_payload: Option<Vec<u8>>,
    pub prior_position: Option<usize>,
}

/// A BootApp entry for the same target: same file (case-insensitive) on the
/// same partition (identical HD node), per `loadopt::same_boot_target`.
fn is_ours(option: &LoadOption, device_path: &[u8]) -> bool {
    option.description == BOOTAPP_DESCRIPTION
        && loadopt::same_boot_target(&option.device_path, device_path)
}

/// Plan Boot#### + BootOrder for a BootApp device path (BRIEF BootOrder semantics).
/// When BootOrder does not exist at all, it is created as our number followed by
/// every known existing Boot#### in ascending order (so other entries stay bootable).
pub fn plan_register(vars: &dyn FirmwareVars, device_path: &[u8]) -> Result<Registration, String> {
    loadopt::device_path_file_name(device_path).ok_or("device path has no file path node")?;
    let order_present = vars.get("BootOrder")?.is_some();
    let order = read_boot_order(vars)?;
    let known = known_numbers(vars, &order)?;
    let mut reuse = None;
    for &number in &known {
        if let Some(Ok(option)) = read_entry(vars, number)?
            && is_ours(&option, device_path)
        {
            reuse = Some(number);
            break;
        }
    }
    let number = match reuse {
        Some(number) => number,
        None => {
            let mut used = known.clone();
            loop {
                let candidate = loadopt::first_free_boot_number(&used)
                    .ok_or("no free Boot#### number (all 65536 in use)")?;
                if vars.get(&loadopt::boot_variable_name(candidate))?.is_none() {
                    break candidate;
                }
                used.push(candidate);
            }
        }
    };
    let payload = LoadOption {
        attributes: LOAD_OPTION_ACTIVE,
        description: BOOTAPP_DESCRIPTION.into(),
        device_path: device_path.to_vec(),
        optional_data: Vec::new(),
    }
    .encode();
    let name = loadopt::boot_variable_name(number);
    let existing = vars.get(&name)?;
    let mut changes = Vec::new();
    if existing.as_deref() != Some(payload.as_slice()) {
        changes.push(change(name, payload));
    }
    let base = if order_present {
        order.clone()
    } else {
        let mut existing_numbers = Vec::new();
        for &n in &known {
            if vars.get(&loadopt::boot_variable_name(n))?.is_some() {
                existing_numbers.push(n);
            }
        }
        existing_numbers.sort_unstable();
        existing_numbers
    };
    let new_order = loadopt::prepend_unique(&base, number);
    if new_order.len() > MAX_BOOT_ORDER_ENTRIES {
        return Err("BootOrder would exceed the entry bound".into());
    }
    if !order_present || new_order != order {
        changes.push(change("BootOrder", loadopt::encode_boot_order(&new_order)));
    }
    Ok(Registration {
        number,
        reused: reuse.is_some(),
        prior_payload: reuse.and(existing),
        prior_position: reuse.and_then(|n| order.iter().position(|&o| o == n)),
        changes,
    })
}

/// Plan removal of a BootApp registration recorded in an install manifest.
/// * created: BootOrder loses the number first, then the variable is deleted;
/// * reused: the prior payload is written back and the number returns to its
///   prior BootOrder position (or leaves BootOrder if it was not in it).
///
/// Nothing is touched if the variable no longer describes our target.
pub fn plan_unregister(
    vars: &dyn FirmwareVars,
    record: &BootRecord,
) -> Result<(Vec<Change>, Vec<String>), String> {
    let number = record.number;
    let file = loadopt::device_path_file_name(&record.device_path).unwrap_or_default();
    let name = loadopt::boot_variable_name(number);
    let mut notes = Vec::new();
    let mut changes = Vec::new();
    let current = vars.get(&name)?;
    match current.as_deref().map(LoadOption::decode) {
        None => notes.push(format!("{name} no longer exists")),
        Some(Ok(option)) if is_ours(&option, &record.device_path) => {}
        Some(_) => {
            notes.push(format!(
                "{name} no longer describes BootApp {file}; leaving it and BootOrder untouched"
            ));
            return Ok((changes, notes));
        }
    }
    let keep_variable = !record.created && current.is_some();
    if keep_variable
        && let Some(prior) = &record.prior_payload
        && current.as_deref() != Some(prior.as_slice())
    {
        changes.push(change(name.clone(), prior.clone()));
    }
    let order = read_boot_order(vars)?;
    let mut new_order = loadopt::remove_number(&order, number);
    if keep_variable && let Some(position) = record.prior_position {
        new_order.insert(position.min(new_order.len()), number);
    }
    if new_order != order {
        changes.push(change("BootOrder", loadopt::encode_boot_order(&new_order)));
    }
    if record.created && current.is_some() {
        changes.push(change(name, Vec::new()));
    }
    Ok((changes, notes))
}

fn existing_number(vars: &dyn FirmwareVars, text: &str) -> Result<u16, String> {
    let number = loadopt::parse_boot_number(text).ok_or_else(|| {
        format!("invalid boot number {text:?} (expected 4 hex digits, e.g. 0001)")
    })?;
    match read_entry(vars, number)? {
        None => Err(format!(
            "{} does not exist",
            loadopt::boot_variable_name(number)
        )),
        Some(Err(e)) => Err(format!(
            "{} is not a valid load option: {e}",
            loadopt::boot_variable_name(number)
        )),
        Some(Ok(_)) => Ok(number),
    }
}

pub fn plan_set_order(vars: &dyn FirmwareVars, list: &str) -> Result<Vec<Change>, String> {
    let mut order = Vec::new();
    for item in list.split(',') {
        let number = existing_number(vars, item.trim())?;
        if order.contains(&number) {
            return Err(format!("{number:04X} is listed twice"));
        }
        if order.len() >= MAX_BOOT_ORDER_ENTRIES {
            return Err("too many BootOrder entries".into());
        }
        order.push(number);
    }
    let current = read_boot_order(vars)?;
    let dropped: Vec<String> = current
        .iter()
        .filter(|n| !order.contains(n))
        .map(|n| format!("{n:04X}"))
        .collect();
    if !dropped.is_empty() {
        println!(
            "NOTE: {} will be removed from BootOrder (the Boot#### variables are kept).",
            dropped.join(",")
        );
    }
    Ok(vec![change(
        "BootOrder",
        loadopt::encode_boot_order(&order),
    )])
}

pub fn plan_boot_next(vars: &dyn FirmwareVars, text: &str) -> Result<Vec<Change>, String> {
    let number = existing_number(vars, text.trim())?;
    Ok(vec![change("BootNext", number.to_le_bytes().to_vec())])
}

/// Firmware variables of the running system.
pub fn system_vars() -> Result<Box<dyn FirmwareVars>, String> {
    #[cfg(windows)]
    {
        Ok(Box::new(windows::WindowsVars::open()?))
    }
    #[cfg(not(windows))]
    {
        Err(
            "firmware variable access is implemented only on Windows (use efibootmgr on Linux)"
                .into(),
        )
    }
}

#[cfg(windows)]
mod windows {
    use super::FirmwareVars;
    use crate::win;
    use bootapp_core::loadopt::{EFI_GLOBAL_VARIABLE_GUID_TEXT, MAX_LOAD_OPTION_BYTES};

    pub struct WindowsVars {
        guid: Vec<u16>,
    }

    fn explain(code: u32, action: &str, name: &str) -> String {
        let hint = match code {
            win::ERROR_INVALID_FUNCTION => {
                " — this Windows installation booted in legacy BIOS (CSM) mode; UEFI variables are unavailable"
            }
            win::ERROR_PRIVILEGE_NOT_HELD => " — run from an elevated administrator prompt",
            _ => "",
        };
        format!("{action} {name}: {}{hint}", win::error_text(code))
    }

    impl WindowsVars {
        /// Enables SeSystemEnvironmentPrivilege for this process (admin only).
        pub fn open() -> Result<Self, String> {
            const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
            const TOKEN_QUERY: u32 = 0x0008;
            const SE_PRIVILEGE_ENABLED: u32 = 0x0002;
            let mut token: win::Handle = std::ptr::null_mut();
            // SAFETY: GetCurrentProcess returns a pseudo-handle; `token` is a valid
            // out-pointer for the duration of the call.
            if unsafe {
                win::OpenProcessToken(
                    win::GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut token,
                )
            } == 0
            {
                // SAFETY: no preconditions.
                return Err(format!(
                    "OpenProcessToken: {}",
                    win::error_text(unsafe { win::GetLastError() })
                ));
            }
            let result = (|| {
                let name = win::wide("SeSystemEnvironmentPrivilege");
                let mut luid = win::Luid {
                    low_part: 0,
                    high_part: 0,
                };
                // SAFETY: `name` is NUL-terminated UTF-16; `luid` is a valid out-pointer.
                if unsafe { win::LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) }
                    == 0
                {
                    // SAFETY: no preconditions.
                    return Err(format!(
                        "LookupPrivilegeValueW: {}",
                        win::error_text(unsafe { win::GetLastError() })
                    ));
                }
                let state = win::TokenPrivileges {
                    privilege_count: 1,
                    luid,
                    attributes: SE_PRIVILEGE_ENABLED,
                };
                // SAFETY: `token` was opened with TOKEN_ADJUST_PRIVILEGES; `state` is a
                // valid TOKEN_PRIVILEGES with one entry; previous state is not requested.
                let ok = unsafe {
                    win::AdjustTokenPrivileges(
                        token,
                        0,
                        &state,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                // SAFETY: no preconditions; read immediately after the call.
                let last = unsafe { win::GetLastError() };
                if ok == 0 {
                    return Err(format!("AdjustTokenPrivileges: {}", win::error_text(last)));
                }
                if last == win::ERROR_NOT_ALL_ASSIGNED {
                    return Err(format!(
                        "SeSystemEnvironmentPrivilege is not held by this process ({}); firmware variables require an elevated administrator prompt",
                        win::error_text(last)
                    ));
                }
                Ok(())
            })();
            // SAFETY: `token` is a valid handle opened above and closed exactly once.
            unsafe { win::CloseHandle(token) };
            result?;
            Ok(Self {
                guid: win::wide(EFI_GLOBAL_VARIABLE_GUID_TEXT),
            })
        }
    }

    impl FirmwareVars for WindowsVars {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
            let wide_name = win::wide(name);
            let mut buffer = vec![0u8; MAX_LOAD_OPTION_BYTES];
            let mut attributes = 0u32;
            // SAFETY: name/guid are NUL-terminated UTF-16 buffers alive for the call;
            // `buffer` is writable for `buffer.len()` bytes; `attributes` is a valid out-pointer.
            let size = unsafe {
                win::GetFirmwareEnvironmentVariableExW(
                    wide_name.as_ptr(),
                    self.guid.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut attributes,
                )
            };
            if size == 0 {
                // SAFETY: no preconditions; read immediately after the call.
                let code = unsafe { win::GetLastError() };
                return match code {
                    win::ERROR_ENVVAR_NOT_FOUND => Ok(None),
                    win::ERROR_INSUFFICIENT_BUFFER => {
                        Err(format!("{name} exceeds {MAX_LOAD_OPTION_BYTES} bytes"))
                    }
                    _ => Err(explain(code, "reading", name)),
                };
            }
            buffer.truncate(size as usize);
            Ok(Some(buffer))
        }

        fn set(&mut self, name: &str, data: &[u8], attributes: u32) -> Result<(), String> {
            if data.len() > MAX_LOAD_OPTION_BYTES {
                return Err(format!(
                    "{name}: value exceeds {MAX_LOAD_OPTION_BYTES} bytes"
                ));
            }
            let wide_name = win::wide(name);
            // SAFETY: name/guid are NUL-terminated UTF-16; `data` is readable for
            // `data.len()` bytes (size 0 deletes the variable per the Win32 contract).
            let ok = unsafe {
                win::SetFirmwareEnvironmentVariableExW(
                    wide_name.as_ptr(),
                    self.guid.as_ptr(),
                    data.as_ptr().cast(),
                    data.len() as u32,
                    attributes,
                )
            };
            if ok == 0 {
                // SAFETY: no preconditions.
                return Err(explain(unsafe { win::GetLastError() }, "writing", name));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bootapp_core::loadopt::HardDriveNode;
    use std::collections::BTreeMap;

    #[derive(Default)]
    pub struct FakeVars {
        pub vars: BTreeMap<String, (Vec<u8>, u32)>,
        pub fail_on: Option<String>,
    }

    impl FirmwareVars for FakeVars {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.vars.get(name).map(|(d, _)| d.clone()))
        }
        fn set(&mut self, name: &str, data: &[u8], attributes: u32) -> Result<(), String> {
            if self.fail_on.as_deref() == Some(name) {
                return Err("injected failure".into());
            }
            if data.is_empty() {
                self.vars.remove(name);
            } else {
                self.vars.insert(name.into(), (data.to_vec(), attributes));
            }
            Ok(())
        }
    }

    pub fn hd() -> HardDriveNode {
        HardDriveNode {
            partition_number: 1,
            partition_start_lba: 2048,
            partition_size_lba: 204800,
            signature: [7; 16],
            partition_format: 2,
            signature_type: 2,
        }
    }

    fn option(description: &str, file: &str) -> Vec<u8> {
        option_on(description, &hd(), file)
    }

    fn option_on(description: &str, hd: &HardDriveNode, file: &str) -> Vec<u8> {
        LoadOption {
            attributes: LOAD_OPTION_ACTIVE,
            description: description.into(),
            device_path: loadopt::hard_drive_file_device_path(hd, file),
            optional_data: Vec::new(),
        }
        .encode()
    }

    fn ours() -> Vec<u8> {
        loadopt::hard_drive_file_device_path(&hd(), OURS)
    }

    pub fn fake() -> FakeVars {
        let mut f = FakeVars::default();
        f.set(
            "Boot0000",
            &option("Windows Boot Manager", r"\EFI\Microsoft\Boot\bootmgfw.efi"),
            7,
        )
        .unwrap();
        f.set(
            "Boot0001",
            &option("UEFI Shell", r"\EFI\BOOT\BOOTX64.EFI"),
            7,
        )
        .unwrap();
        f.set("Boot0003", &option("Other", r"\EFI\other.efi"), 7)
            .unwrap();
        f.set("BootOrder", &loadopt::encode_boot_order(&[0, 1, 3]), 7)
            .unwrap();
        f.set("BootCurrent", &0u16.to_le_bytes(), 6).unwrap();
        f
    }

    const OURS: &str = r"\EFI\BOOTAPP\BOOTAPPX64.EFI";

    fn order(f: &FakeVars) -> Vec<u16> {
        read_boot_order(f).unwrap()
    }

    #[test]
    fn list_rendering() {
        let mut f = fake();
        f.set("Boot0002", &option("Hidden", r"\EFI\h.efi"), 7)
            .unwrap();
        let text = render_list(&f).unwrap();
        assert!(text.contains("BootCurrent: 0000"), "{text}");
        assert!(text.contains("BootNext: (not set)"), "{text}");
        assert!(text.contains("BootOrder: 0000,0001,0003"), "{text}");
        assert!(text.contains("Boot0000* Windows Boot Manager |"), "{text}");
        assert!(text.contains("Boot0002* Hidden"), "{text}");
        assert!(text.contains("(not in BootOrder)"), "{text}");
        f.set("BootOrder", &loadopt::encode_boot_order(&[9]), 7)
            .unwrap();
        assert!(
            render_list(&f)
                .unwrap()
                .contains("Boot0009  (referenced by BootOrder but missing)")
        );
    }

    #[test]
    fn register_first_free_and_prepend() {
        let mut f = fake();
        let path = loadopt::hard_drive_file_device_path(&hd(), OURS);
        let plan = plan_register(&f, &path).unwrap();
        assert_eq!(plan.number, 2, "first free number");
        assert!(!plan.reused);
        assert_eq!(plan.changes.len(), 2);
        assert_eq!(plan.changes[0].name, "Boot0002");
        assert_eq!(plan.changes[0].attributes, BOOT_VARIABLE_ATTRIBUTES);
        apply_changes(&mut f, &plan.changes).unwrap();
        assert_eq!(order(&f), vec![2, 0, 1, 3]);
        let stored = LoadOption::decode(&f.get("Boot0002").unwrap().unwrap()).unwrap();
        assert_eq!(stored.description, "BootApp");
        // Second registration reuses and changes nothing.
        let again = plan_register(&f, &path).unwrap();
        assert_eq!(again.number, 2);
        assert!(again.reused);
        assert!(again.changes.is_empty());
    }

    #[test]
    fn register_reuses_existing_bootapp_entry_case_insensitively() {
        let mut f = fake();
        f.set(
            "Boot0003",
            &option("BootApp", r"\efi\bootapp\bootappx64.efi"),
            7,
        )
        .unwrap();
        let path = loadopt::hard_drive_file_device_path(&hd(), OURS);
        let plan = plan_register(&f, &path).unwrap();
        assert_eq!(plan.number, 3);
        assert!(plan.reused);
        apply_changes(&mut f, &plan.changes).unwrap();
        assert_eq!(order(&f), vec![3, 0, 1]);
        // A "BootApp" entry for another file is not reused.
        let mut g = fake();
        g.set(
            "Boot0002",
            &option("BootApp", r"\EFI\BOOTAPP\BOOTAPPIA32.EFI"),
            7,
        )
        .unwrap();
        assert_eq!(plan_register(&g, &path).unwrap().number, 4);
    }

    fn rec(
        number: u16,
        created: bool,
        prior_payload: Option<Vec<u8>>,
        prior_position: Option<usize>,
    ) -> BootRecord {
        BootRecord {
            number,
            created,
            device_path: ours(),
            prior_payload,
            prior_position,
        }
    }

    #[test]
    fn reused_entry_payload_and_position_are_restored() {
        let mut f = fake();
        // An older BootApp registration for the same target, inactive, at index 2.
        let mut old = LoadOption::decode(&option("BootApp", OURS)).unwrap();
        old.attributes = 0;
        old.optional_data = b"old".to_vec();
        let old = old.encode();
        f.set("Boot0003", &old, 7).unwrap();
        let plan = plan_register(&f, &ours()).unwrap();
        assert!(plan.reused);
        assert_eq!(plan.number, 3);
        assert_eq!(plan.prior_payload.as_deref(), Some(old.as_slice()));
        assert_eq!(plan.prior_position, Some(2));
        apply_changes(&mut f, &plan.changes).unwrap();
        assert_eq!(order(&f), vec![3, 0, 1]);
        assert_ne!(f.get("Boot0003").unwrap().unwrap(), old);
        let record = rec(3, false, plan.prior_payload.clone(), plan.prior_position);
        let (changes, _) = plan_unregister(&f, &record).unwrap();
        apply_changes(&mut f, &changes).unwrap();
        assert_eq!(f.get("Boot0003").unwrap().unwrap(), old);
        assert_eq!(order(&f), vec![0, 1, 3]);
        // A reused entry that was not in BootOrder leaves BootOrder again.
        let mut g = fake();
        g.set("Boot0002", &old, 7).unwrap();
        let plan = plan_register(&g, &ours()).unwrap();
        assert_eq!((plan.number, plan.prior_position), (2, None));
        apply_changes(&mut g, &plan.changes).unwrap();
        let (changes, _) = plan_unregister(&g, &rec(2, false, plan.prior_payload, None)).unwrap();
        apply_changes(&mut g, &changes).unwrap();
        assert_eq!(order(&g), vec![0, 1, 3]);
        assert_eq!(g.get("Boot0002").unwrap().unwrap(), old);
    }

    #[test]
    fn absent_boot_order_keeps_existing_entries() {
        let mut f = fake();
        f.set("BootOrder", &[], 7).unwrap();
        assert!(f.get("BootOrder").unwrap().is_none());
        let plan = plan_register(&f, &ours()).unwrap();
        assert_eq!(plan.number, 2);
        apply_changes(&mut f, &plan.changes).unwrap();
        assert_eq!(order(&f), vec![2, 0, 1]);
        // Present-but-identical BootOrder is not rewritten.
        let again = plan_register(&f, &ours()).unwrap();
        assert!(again.changes.is_empty());
    }

    #[test]
    fn same_file_on_another_partition_is_not_reused() {
        let mut f = fake();
        let mut other = hd();
        other.signature = [9; 16];
        f.set("Boot0002", &option_on("BootApp", &other, OURS), 7)
            .unwrap();
        f.set("BootOrder", &loadopt::encode_boot_order(&[2, 0, 1, 3]), 7)
            .unwrap();
        let plan = plan_register(&f, &ours()).unwrap();
        assert!(
            !plan.reused,
            "entry on a different partition must not be reused"
        );
        assert_eq!(plan.number, 4);
        apply_changes(&mut f, &plan.changes).unwrap();
        assert_eq!(order(&f), vec![4, 2, 0, 1, 3]);
        assert_eq!(
            f.get("Boot0002").unwrap().unwrap(),
            option_on("BootApp", &other, OURS)
        );
        // Unregistering ours leaves the other disk's BootApp entry alone.
        let (changes, _) = plan_unregister(&f, &rec(4, true, None, None)).unwrap();
        apply_changes(&mut f, &changes).unwrap();
        assert_eq!(order(&f), vec![2, 0, 1, 3]);
        let (changes, notes) = plan_unregister(&f, &rec(2, true, None, None)).unwrap();
        assert!(changes.is_empty() && !notes.is_empty());
    }

    #[test]
    fn unregister_removes_only_what_was_created() {
        let mut f = fake();
        let path = loadopt::hard_drive_file_device_path(&hd(), OURS);
        let plan = plan_register(&f, &path).unwrap();
        apply_changes(&mut f, &plan.changes).unwrap();
        let (changes, _) = plan_unregister(&f, &rec(2, true, None, None)).unwrap();
        assert_eq!(changes[0].name, "BootOrder");
        assert_eq!(changes[1].name, "Boot0002");
        apply_changes(&mut f, &changes).unwrap();
        assert_eq!(order(&f), vec![0, 1, 3]);
        assert!(f.get("Boot0002").unwrap().is_none());
        assert!(f.get("Boot0000").unwrap().is_some());
        // Reused entries stay; foreign entries are not touched.
        let mut g = fake();
        g.set("BootOrder", &loadopt::encode_boot_order(&[1, 0, 3]), 7)
            .unwrap();
        let (changes, notes) = plan_unregister(&g, &rec(1, true, None, None)).unwrap();
        assert!(changes.is_empty());
        assert!(!notes.is_empty());
        g.set("Boot0002", &option("BootApp", OURS), 7).unwrap();
        g.set("BootOrder", &loadopt::encode_boot_order(&[2, 1, 0, 3]), 7)
            .unwrap();
        let (changes, _) = plan_unregister(&g, &rec(2, false, None, None)).unwrap();
        apply_changes(&mut g, &changes).unwrap();
        assert!(g.get("Boot0002").unwrap().is_some());
        assert_eq!(order(&g), vec![1, 0, 3]);
    }

    #[test]
    fn set_order_validation() {
        let mut f = fake();
        let changes = plan_set_order(&f, "0003,0000").unwrap();
        apply_changes(&mut f, &changes).unwrap();
        assert_eq!(order(&f), vec![3, 0]);
        assert!(
            plan_set_order(&f, "0003,0002")
                .unwrap_err()
                .contains("Boot0002 does not exist")
        );
        assert!(plan_set_order(&f, "0003,0003").is_err());
        assert!(plan_set_order(&f, "3").is_err());
        assert!(plan_set_order(&f, "").is_err());
        assert!(plan_set_order(&f, "0000,").is_err());
        f.set("Boot0002", b"garbage", 7).unwrap();
        assert!(
            plan_set_order(&f, "0002")
                .unwrap_err()
                .contains("not a valid load option")
        );
    }

    #[test]
    fn boot_next() {
        let mut f = fake();
        let plan = plan_boot_next(&f, "0001").unwrap();
        apply_changes(&mut f, &plan).unwrap();
        assert_eq!(f.get("BootNext").unwrap().unwrap(), vec![1, 0]);
        assert!(render_list(&f).unwrap().contains("BootNext: 0001"));
        assert!(plan_boot_next(&f, "0002").is_err());
        assert!(plan_boot_next(&f, "zzzz").is_err());
    }

    #[test]
    fn partial_failure_is_reported() {
        let mut f = fake();
        f.fail_on = Some("BootOrder".into());
        let path = loadopt::hard_drive_file_device_path(&hd(), OURS);
        let plan = plan_register(&f, &path).unwrap();
        let err = apply_changes(&mut f, &plan.changes).unwrap_err();
        assert!(err.contains("Already applied: [Boot0002]"), "{err}");
    }
}

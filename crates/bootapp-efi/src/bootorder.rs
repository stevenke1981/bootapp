//! BootOrder self-registration (menu key `F`) and BootCurrent reporting.
//!
//! Only two variables are ever written: one `Boot####` owned by BootApp
//! (reused when it already describes this same file) and `BootOrder`. No
//! variable is deleted. All firmware data is decoded with bounded buffers.

use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use bootapp_core::loadopt::{
    self, BOOT_VARIABLE_ATTRIBUTES, BOOTAPP_DESCRIPTION, LOAD_OPTION_ACTIVE, LoadOption,
    MAX_BOOT_ORDER_ENTRIES, MAX_LOAD_OPTION_BYTES,
};
use uefi::runtime::{self, VariableAttributes, VariableVendor};
use uefi::{CString16, Handle, Status, cstr16};

use crate::volumes;

/// Bound on GetNextVariableName iterations. Exceeding it aborts registration:
/// without a complete list we cannot prove a Boot#### number is unused.
const MAX_VARIABLE_KEYS: usize = 8192;

pub struct Registered {
    pub number: u16,
    pub reused: bool,
    pub order: Vec<u16>,
}

pub fn order_text(order: &[u16]) -> String {
    let mut text = String::new();
    for (index, number) in order.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        text.push_str(&format!("{number:04X}"));
    }
    text
}

/// `BootCurrent` (the Boot#### the firmware used to start this image), if set.
pub fn boot_current() -> Option<u16> {
    let mut buffer = [0u8; 2];
    let (data, _) = runtime::get_variable(
        cstr16!("BootCurrent"),
        &VariableVendor::GLOBAL_VARIABLE,
        &mut buffer,
    )
    .ok()?;
    (data.len() == 2).then(|| u16::from_le_bytes([data[0], data[1]]))
}

/// Existing Boot#### numbers (any vendor-global `Boot` + 4 hex digits).
fn boot_numbers() -> Result<Vec<(u16, CString16)>, Status> {
    let mut numbers = Vec::new();
    for (count, key) in runtime::variable_keys().enumerate() {
        if count >= MAX_VARIABLE_KEYS {
            return Err(Status::BAD_BUFFER_SIZE);
        }
        let key = key.map_err(|e| e.status())?;
        if key.vendor != VariableVendor::GLOBAL_VARIABLE {
            continue;
        }
        let name = key.name.to_string();
        if let Some(number) = name
            .strip_prefix("Boot")
            .filter(|digits| digits.len() == 4)
            .and_then(loadopt::parse_boot_number)
        {
            numbers.push((number, key.name));
        }
    }
    Ok(numbers)
}

/// Current `BootOrder` (empty when the variable does not exist).
pub fn read_boot_order() -> Result<Vec<u16>, Status> {
    let mut buffer = vec![0u8; MAX_BOOT_ORDER_ENTRIES * 2];
    match runtime::get_variable(
        cstr16!("BootOrder"),
        &VariableVendor::GLOBAL_VARIABLE,
        &mut buffer,
    ) {
        Ok((data, _)) => loadopt::decode_boot_order(data).map_err(|_| Status::VOLUME_CORRUPTED),
        Err(error) if error.status() == Status::NOT_FOUND => Ok(Vec::new()),
        Err(error) if error.status() == Status::BUFFER_TOO_SMALL => Err(Status::BAD_BUFFER_SIZE),
        Err(error) => Err(error.status()),
    }
}

/// Make this BootApp image the first firmware boot option.
pub fn make_first(own: Handle, own_path: Option<&str>) -> Result<Registered, Status> {
    let own_path = own_path.ok_or(Status::NOT_FOUND)?;
    let device = volumes::device_path_bytes(own)?;
    let device_path =
        loadopt::append_file_path(&device, own_path).map_err(|_| Status::INVALID_PARAMETER)?;
    // Our path must carry a file name, or no Boot#### could ever match it.
    loadopt::device_path_file_name(&device_path).ok_or(Status::INVALID_PARAMETER)?;
    // Validate BootOrder before writing anything, so a malformed BootOrder
    // leaves the variable store untouched.
    let order = read_boot_order()?;
    let existing = boot_numbers()?;
    let mut reuse = None;
    let mut buffer = vec![0u8; MAX_LOAD_OPTION_BYTES];
    for (number, name) in &existing {
        // Firmware only honours canonical uppercase names; ignore others.
        if reuse.is_some() || name.to_string() != loadopt::boot_variable_name(*number) {
            continue;
        }
        let Ok((data, _)) =
            runtime::get_variable(name, &VariableVendor::GLOBAL_VARIABLE, &mut buffer)
        else {
            continue;
        };
        let Ok(option) = LoadOption::decode(data) else {
            continue;
        };
        // Same file on the same partition (see loadopt::same_boot_target), so a
        // second BootApp install on another disk gets its own Boot####.
        if option.description == BOOTAPP_DESCRIPTION
            && loadopt::same_boot_target(&option.device_path, &device_path)
        {
            reuse = Some(*number);
        }
    }
    let used: Vec<u16> = existing.iter().map(|(number, _)| *number).collect();
    let number = match reuse {
        Some(number) => number,
        None => loadopt::first_free_boot_number(&used).ok_or(Status::OUT_OF_RESOURCES)?,
    };
    let option = LoadOption {
        attributes: LOAD_OPTION_ACTIVE,
        description: String::from(BOOTAPP_DESCRIPTION),
        device_path,
        optional_data: Vec::new(),
    };
    let attributes = VariableAttributes::from_bits_retain(BOOT_VARIABLE_ATTRIBUTES);
    let name = CString16::try_from(loadopt::boot_variable_name(number).as_str())
        .map_err(|_| Status::INVALID_PARAMETER)?;
    runtime::set_variable(
        &name,
        &VariableVendor::GLOBAL_VARIABLE,
        attributes,
        &option.encode(),
    )
    .map_err(|e| e.status())?;
    let order = loadopt::prepend_unique(&order, number);
    runtime::set_variable(
        cstr16!("BootOrder"),
        &VariableVendor::GLOBAL_VARIABLE,
        attributes,
        &loadopt::encode_boot_order(&order),
    )
    .map_err(|e| e.status())?;
    Ok(Registered {
        number,
        reused: reuse.is_some(),
        order,
    })
}

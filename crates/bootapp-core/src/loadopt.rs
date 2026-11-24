//! UEFI `Boot####` / `BootOrder` / `BootNext` variable encoding (CONTRACT).
//!
//! Pure byte-level helpers shared by the EFI manager (self-registration) and the
//! host CLI (installer / `bootorder` commands). No firmware access here.
//! Reference: UEFI 2.10 §3.1.3 EFI_LOAD_OPTION, §10.3 device path nodes.

use alloc::{format, string::String, vec::Vec};
use core::fmt;

/// EFI_GLOBAL_VARIABLE vendor GUID {8BE4DF61-93CA-11D2-AA0D-00E098032B8C}
/// in UEFI on-disk (mixed-endian) byte order.
pub const EFI_GLOBAL_VARIABLE_GUID: [u8; 16] = [
    0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c,
];
/// Registry-style text form used by Windows Get/SetFirmwareEnvironmentVariableExW.
pub const EFI_GLOBAL_VARIABLE_GUID_TEXT: &str = "{8BE4DF61-93CA-11D2-AA0D-00E098032B8C}";

pub const LOAD_OPTION_ACTIVE: u32 = 0x0000_0001;
pub const LOAD_OPTION_HIDDEN: u32 = 0x0000_0008;
/// EFI_VARIABLE_NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS.
pub const BOOT_VARIABLE_ATTRIBUTES: u32 = 0x0000_0007;
/// Description BootApp uses for the entries it creates; used to find/reuse them.
pub const BOOTAPP_DESCRIPTION: &str = "BootApp";
/// Bounds applied while decoding untrusted firmware data.
pub const MAX_LOAD_OPTION_BYTES: usize = 64 * 1024;
pub const MAX_BOOT_ORDER_ENTRIES: usize = 1024;

/// Device path node types and subtypes used here (UEFI 2.10 §10.3).
const END_TYPE: u8 = 0x7F;
const END_ENTIRE_SUBTYPE: u8 = 0xFF;
const END_INSTANCE_SUBTYPE: u8 = 0x01;
const MEDIA_TYPE: u8 = 0x04;
const MEDIA_HARD_DRIVE_SUBTYPE: u8 = 0x01;
const MEDIA_FILE_PATH_SUBTYPE: u8 = 0x04;
/// Header (4) + partition number (4) + start (8) + size (8) + signature (16) + format (1) + sig type (1).
const HARD_DRIVE_NODE_LEN: u16 = 42;
const HARD_DRIVE_PAYLOAD_LEN: usize = HARD_DRIVE_NODE_LEN as usize - 4;
const END_ENTIRE: [u8; 4] = [END_TYPE, END_ENTIRE_SUBTYPE, 4, 0];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOptionError {
    Truncated,
    TooLarge,
    /// Description is not NUL-terminated UTF-16 or contains invalid UTF-16.
    BadDescription,
    /// FilePathListLength exceeds the data or the list lacks an End node.
    BadDevicePath,
    /// BootOrder byte length is odd or exceeds the entry bound.
    BadBootOrder,
}

impl fmt::Display for LoadOptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "load option is truncated",
            Self::TooLarge => "load option exceeds size bound",
            Self::BadDescription => "load option description is not valid UTF-16",
            Self::BadDevicePath => "load option device path is malformed",
            Self::BadBootOrder => "BootOrder length is invalid",
        })
    }
}

/// EFI_LOAD_OPTION: Attributes(u32) FilePathListLength(u16) Description(UTF-16 NUL)
/// FilePathList[FilePathListLength bytes] OptionalData[rest].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOption {
    pub attributes: u32,
    pub description: String,
    /// Raw device path list including its End-Entire node.
    pub device_path: Vec<u8>,
    pub optional_data: Vec<u8>,
}

impl LoadOption {
    /// Encodes the option. `FilePathListLength` is `device_path.len()` as u16;
    /// a list longer than 65535 bytes saturates and will not decode. The
    /// description must not contain NUL (it would terminate early).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            6 + 2 * (self.description.len() + 1)
                + self.device_path.len()
                + self.optional_data.len(),
        );
        out.extend_from_slice(&self.attributes.to_le_bytes());
        let list_len = u16::try_from(self.device_path.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&list_len.to_le_bytes());
        for unit in self.description.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&self.device_path);
        out.extend_from_slice(&self.optional_data);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoadOptionError> {
        if bytes.len() > MAX_LOAD_OPTION_BYTES {
            return Err(LoadOptionError::TooLarge);
        }
        let header = bytes.get(..6).ok_or(LoadOptionError::Truncated)?;
        let attributes = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let list_len = usize::from(u16::from_le_bytes([header[4], header[5]]));
        let (description, desc_len) = decode_description(&bytes[6..])?;
        let list_start = 6 + desc_len;
        let list_end = list_start
            .checked_add(list_len)
            .filter(|&end| end <= bytes.len())
            .ok_or(LoadOptionError::Truncated)?;
        let list = &bytes[list_start..list_end];
        // Validates node framing and the End-Entire node; the list is kept verbatim.
        split_nodes(list)?;
        Ok(Self {
            attributes,
            description,
            device_path: list.to_vec(),
            optional_data: bytes[list_end..].to_vec(),
        })
    }
}

/// Reads a NUL-terminated UTF-16LE string; returns the text and bytes consumed
/// (including the terminator).
fn decode_description(data: &[u8]) -> Result<(String, usize), LoadOptionError> {
    let mut units: Vec<u16> = Vec::new();
    let mut pos = 0usize;
    loop {
        let pair = data.get(pos..pos + 2).ok_or(LoadOptionError::Truncated)?;
        pos += 2;
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let text = String::from_utf16(&units).map_err(|_| LoadOptionError::BadDescription)?;
    Ok((text, pos))
}

/// One device path node: type, subtype and payload (bytes after the 4-byte header).
#[derive(Debug, Clone, Copy)]
struct DpNode<'a> {
    kind: u8,
    sub: u8,
    data: &'a [u8],
}

/// Splits a device path list (UEFI 2.10 §10.3.1: a packed array of instances)
/// and returns the nodes of the first instance, without its End node.
///
/// Each instance is terminated by an End node: End-Entire (0x7F,0xFF) or
/// End-This-Instance (0x7F,0x01). Every node length is checked in every
/// instance, and the list must end with a terminator. Instances after the
/// first are validated but not returned; callers keep those bytes verbatim.
fn split_nodes(list: &[u8]) -> Result<Vec<DpNode<'_>>, LoadOptionError> {
    let mut first: Option<Vec<DpNode<'_>>> = None;
    let mut current: Vec<DpNode<'_>> = Vec::new();
    let mut terminated = false;
    let mut pos = 0usize;
    loop {
        if pos == list.len() {
            return match first {
                Some(nodes) if terminated => Ok(nodes),
                _ => Err(LoadOptionError::BadDevicePath),
            };
        }
        let header = list
            .get(pos..pos + 4)
            .ok_or(LoadOptionError::BadDevicePath)?;
        let kind = header[0];
        let sub = header[1];
        let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if len < 4 {
            return Err(LoadOptionError::BadDevicePath);
        }
        let end = pos
            .checked_add(len)
            .filter(|&end| end <= list.len())
            .ok_or(LoadOptionError::BadDevicePath)?;
        if kind == END_TYPE && (sub == END_ENTIRE_SUBTYPE || sub == END_INSTANCE_SUBTYPE) {
            // The End node itself must be exactly 4 bytes. An instance with no nodes
            // (a bare End-Entire, as some firmware writes for an empty path) is accepted.
            if len != 4 {
                return Err(LoadOptionError::BadDevicePath);
            }
            if first.is_none() {
                first = Some(core::mem::take(&mut current));
            } else {
                current.clear();
            }
            terminated = true;
        } else {
            current.push(DpNode {
                kind,
                sub,
                data: &list[pos + 4..end],
            });
            terminated = false;
        }
        pos = end;
    }
}

/// `BootOrder` is a packed little-endian u16 array.
pub fn encode_boot_order(order: &[u16]) -> Vec<u8> {
    order.iter().flat_map(|n| n.to_le_bytes()).collect()
}

pub fn decode_boot_order(bytes: &[u8]) -> Result<Vec<u16>, LoadOptionError> {
    if bytes.len() % 2 != 0 || bytes.len() / 2 > MAX_BOOT_ORDER_ENTRIES {
        return Err(LoadOptionError::BadBootOrder);
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect())
}

/// `Boot0007` style name (four uppercase hex digits).
pub fn boot_variable_name(number: u16) -> String {
    format!("Boot{number:04X}")
}

/// Inverse of `boot_variable_name`; accepts uppercase or lowercase hex, exactly 4 digits.
pub fn parse_boot_number(text: &str) -> Option<u16> {
    if text.len() != 4 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(text, 16).ok()
}

/// `number` first, then `order` without any other occurrence of `number`.
pub fn prepend_unique(order: &[u16], number: u16) -> Vec<u16> {
    let mut out = Vec::with_capacity(order.len() + 1);
    out.push(number);
    out.extend(order.iter().copied().filter(|&n| n != number));
    out
}

/// `order` without any occurrence of `number`.
pub fn remove_number(order: &[u16], number: u16) -> Vec<u16> {
    order.iter().copied().filter(|&n| n != number).collect()
}

/// Lowest number in 0x0000..=0xFFFF not in `used`.
pub fn first_free_boot_number(used: &[u16]) -> Option<u16> {
    let mut sorted = Vec::from(used);
    sorted.sort_unstable();
    let mut candidate: u32 = 0;
    for value in sorted {
        let value = u32::from(value);
        if value == candidate {
            candidate += 1;
        } else if value > candidate {
            break;
        }
    }
    u16::try_from(candidate).ok()
}

/// Media / Hard Drive device path node (type 4, subtype 1, length 42).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardDriveNode {
    pub partition_number: u32,
    pub partition_start_lba: u64,
    pub partition_size_lba: u64,
    /// GPT partition GUID in on-disk byte order, or MBR signature in bytes 0..4.
    pub signature: [u8; 16],
    /// 0x01 = MBR (PC-AT), 0x02 = GPT.
    pub partition_format: u8,
    /// 0x01 = 32-bit MBR signature, 0x02 = GUID.
    pub signature_type: u8,
}

/// HD node + Media/FilePath node (type 4, subtype 4, UTF-16 NUL path) + End-Entire
/// (0x7F, 0xFF, length 4). `file_path` must satisfy `crate::validate_efi_path`
/// (which bounds it well below the u16 node length); a longer or NUL-containing
/// path produces a node that does not describe the intended file.
pub fn hard_drive_file_device_path(hd: &HardDriveNode, file_path: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(usize::from(HARD_DRIVE_NODE_LEN) + 64);
    out.push(MEDIA_TYPE);
    out.push(MEDIA_HARD_DRIVE_SUBTYPE);
    out.extend_from_slice(&HARD_DRIVE_NODE_LEN.to_le_bytes());
    out.extend_from_slice(&hd.partition_number.to_le_bytes());
    out.extend_from_slice(&hd.partition_start_lba.to_le_bytes());
    out.extend_from_slice(&hd.partition_size_lba.to_le_bytes());
    out.extend_from_slice(&hd.signature);
    out.push(hd.partition_format);
    out.push(hd.signature_type);
    push_file_path_node(&mut out, file_path);
    out.extend_from_slice(&END_ENTIRE);
    out
}

/// Appends a Media/FilePath node for `file_path` (UTF-16LE, NUL terminated).
fn push_file_path_node(out: &mut Vec<u8>, file_path: &str) {
    let unit_count = file_path.encode_utf16().count() + 1;
    let node_len = u16::try_from(4 + 2 * unit_count).unwrap_or(u16::MAX);
    out.push(MEDIA_TYPE);
    out.push(MEDIA_FILE_PATH_SUBTYPE);
    out.extend_from_slice(&node_len.to_le_bytes());
    for unit in file_path.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&[0, 0]);
}

/// Concatenate an existing device path (without its End node) with a FilePath
/// node and End node. `device` may include a trailing End node; it is stripped.
/// An End-Instance node or a malformed node in `device`, a NUL in `file_path`,
/// or a result longer than 65535 bytes is rejected with `BadDevicePath`.
pub fn append_file_path(device: &[u8], file_path: &str) -> Result<Vec<u8>, LoadOptionError> {
    if file_path.contains('\0') {
        return Err(LoadOptionError::BadDevicePath);
    }
    let mut pos = 0usize;
    let mut body_len = device.len();
    while pos < device.len() {
        let header = device
            .get(pos..pos + 4)
            .ok_or(LoadOptionError::BadDevicePath)?;
        let kind = header[0];
        let sub = header[1];
        let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = pos
            .checked_add(len)
            .filter(|&end| len >= 4 && end <= device.len())
            .ok_or(LoadOptionError::BadDevicePath)?;
        if kind == END_TYPE {
            if sub != END_ENTIRE_SUBTYPE || len != 4 || end != device.len() {
                return Err(LoadOptionError::BadDevicePath);
            }
            body_len = pos;
            break;
        }
        pos = end;
    }
    let unit_count = file_path.encode_utf16().count() + 1;
    let total = body_len + 4 + 2 * unit_count + END_ENTIRE.len();
    if total > usize::from(u16::MAX) {
        return Err(LoadOptionError::BadDevicePath);
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&device[..body_len]);
    push_file_path_node(&mut out, file_path);
    out.extend_from_slice(&END_ENTIRE);
    Ok(out)
}

/// Concatenated text of all Media/FilePath nodes, e.g. `\EFI\BOOTAPP\BOOTAPPX64.EFI`.
/// Consecutive nodes without a separator are joined with `\`. Returns `None`
/// for a malformed list or when no file text is present.
pub fn device_path_file_name(device_path: &[u8]) -> Option<String> {
    let nodes = split_nodes(device_path).ok()?;
    let mut name = String::new();
    for node in nodes
        .iter()
        .filter(|node| node.kind == MEDIA_TYPE && node.sub == MEDIA_FILE_PATH_SUBTYPE)
    {
        let text = file_path_text(node.data)?;
        if !name.is_empty() && !name.ends_with('\\') && !text.starts_with('\\') {
            name.push('\\');
        }
        name.push_str(&text);
    }
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Decodes a FilePath node payload: UTF-16LE with trailing NUL(s) removed.
fn file_path_text(data: &[u8]) -> Option<String> {
    if data.len() % 2 != 0 {
        return None;
    }
    let mut units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    if units.contains(&0) {
        return None;
    }
    String::from_utf16(&units).ok()
}

/// Short human-readable rendering for listings, e.g.
/// `HD(1,GPT,<guid>)/\EFI\BOOTAPP\BOOTAPPX64.EFI`; unknown nodes as `Node(t,s)`.
/// Malformed input renders as `<malformed>`.
pub fn describe_device_path(device_path: &[u8]) -> String {
    let Ok(nodes) = split_nodes(device_path) else {
        return String::from("<malformed>");
    };
    let mut out = String::new();
    for (index, node) in nodes.iter().enumerate() {
        if index > 0 {
            out.push('/');
        }
        let text = if node.kind == MEDIA_TYPE && node.sub == MEDIA_HARD_DRIVE_SUBTYPE {
            describe_hard_drive(node.data)
        } else if node.kind == MEDIA_TYPE && node.sub == MEDIA_FILE_PATH_SUBTYPE {
            match file_path_text(node.data) {
                Some(text) => text,
                None => return String::from("<malformed>"),
            }
        } else {
            format!("Node({},{})", node.kind, node.sub)
        };
        out.push_str(&text);
    }
    if out.is_empty() {
        String::from("<malformed>")
    } else {
        out
    }
}

fn describe_hard_drive(data: &[u8]) -> String {
    if data.len() != HARD_DRIVE_PAYLOAD_LEN {
        return format!("Node({},{})", MEDIA_TYPE, MEDIA_HARD_DRIVE_SUBTYPE);
    }
    let partition = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let signature = &data[20..36];
    let format_byte = data[36];
    let signature_type = data[37];
    match (format_byte, signature_type) {
        (0x02, 0x02) => format!("HD({},GPT,{})", partition, guid_text(signature)),
        (0x01, 0x01) => format!(
            "HD({},MBR,{:08X})",
            partition,
            u32::from_le_bytes([signature[0], signature[1], signature[2], signature[3]])
        ),
        _ => format!("Node({},{})", MEDIA_TYPE, MEDIA_HARD_DRIVE_SUBTYPE),
    }
}

/// Standard `8-4-4-4-12` uppercase text for a 16-byte mixed-endian GUID.
fn guid_text(bytes: &[u8]) -> String {
    let data1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let data2 = u16::from_le_bytes([bytes[4], bytes[5]]);
    let data3 = u16::from_le_bytes([bytes[6], bytes[7]]);
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        data1,
        data2,
        data3,
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

/// Whether two device paths name the same BootApp boot target, used to decide
/// whether an existing `Boot####` may be reused. File names must match
/// (case-insensitively). When both paths contain a Media/HardDrive node, those
/// nodes must be identical (same partition: number, start, size, signature),
/// so a firmware full path (`PciRoot/.../HD/File`) matches the short form
/// (`HD/File`) the host installer writes. Without HD nodes on both sides the
/// paths must be byte-identical. Malformed paths never match.
pub fn same_boot_target(a: &[u8], b: &[u8]) -> bool {
    let (Some(file_a), Some(file_b)) = (device_path_file_name(a), device_path_file_name(b)) else {
        return false;
    };
    if !file_a.eq_ignore_ascii_case(&file_b) {
        return false;
    }
    let hard_drive = |path: &[u8]| -> Option<Vec<u8>> {
        split_nodes(path).ok()?.into_iter().find_map(|node| {
            (node.kind == MEDIA_TYPE && node.sub == MEDIA_HARD_DRIVE_SUBTYPE)
                .then(|| node.data.to_vec())
        })
    };
    match (hard_drive(a), hard_drive(b)) {
        (Some(left), Some(right)) => left == right,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const SIGNATURE: [u8; 16] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15,
        0x16,
    ];

    fn hd(signature: [u8; 16], format_byte: u8, signature_type: u8) -> HardDriveNode {
        HardDriveNode {
            partition_number: 1,
            partition_start_lba: 2048,
            partition_size_lba: 0x10_0000,
            signature,
            partition_format: format_byte,
            signature_type,
        }
    }

    fn sample_option() -> LoadOption {
        LoadOption {
            attributes: LOAD_OPTION_ACTIVE,
            description: String::from(BOOTAPP_DESCRIPTION),
            device_path: hard_drive_file_device_path(
                &hd(SIGNATURE, 2, 2),
                "\\EFI\\BOOTAPP\\BOOTAPPX64.EFI",
            ),
            optional_data: b"quiet".to_vec(),
        }
    }

    #[test]
    fn load_option_round_trips() {
        let option = sample_option();
        let bytes = option.encode();
        assert_eq!(LoadOption::decode(&bytes), Ok(option.clone()));

        let empty = LoadOption {
            attributes: 0,
            description: String::new(),
            device_path: vec![END_TYPE, END_ENTIRE_SUBTYPE, 4, 0],
            optional_data: Vec::new(),
        };
        assert_eq!(LoadOption::decode(&empty.encode()), Ok(empty));
    }

    #[test]
    fn encode_layout_matches_spec() {
        let option = LoadOption {
            attributes: 0x0102_0304,
            description: String::from("A"),
            device_path: vec![END_TYPE, END_ENTIRE_SUBTYPE, 4, 0],
            optional_data: vec![0xEE],
        };
        assert_eq!(
            option.encode(),
            vec![
                0x04, 0x03, 0x02, 0x01, // attributes LE
                0x04, 0x00, // FilePathListLength = 4
                0x41, 0x00, 0x00, 0x00, // "A" + NUL
                0x7F, 0xFF, 0x04, 0x00, // End-Entire
                0xEE, // optional data
            ]
        );
    }

    #[test]
    fn truncation_at_every_prefix_never_panics() {
        let option = sample_option();
        let bytes = option.encode();
        // Everything up to the end of the device path is mandatory.
        let mandatory = 6 + 2 * (option.description.len() + 1) + option.device_path.len();
        for len in 0..=bytes.len() {
            let result = LoadOption::decode(&bytes[..len]);
            if len < mandatory {
                assert!(result.is_err(), "prefix {len} should fail");
            } else {
                assert!(result.is_ok(), "prefix {len} should decode");
            }
        }
    }

    #[test]
    fn decode_rejects_oversized_input() {
        let big = vec![0u8; MAX_LOAD_OPTION_BYTES + 1];
        assert_eq!(LoadOption::decode(&big), Err(LoadOptionError::TooLarge));
    }

    #[test]
    fn decode_rejects_bad_description() {
        // Unpaired surrogate 0xD800 followed by NUL.
        let raw = [0, 0, 0, 0, 4, 0, 0x00, 0xD8, 0x00, 0x00, 0x7F, 0xFF, 4, 0];
        assert_eq!(
            LoadOption::decode(&raw),
            Err(LoadOptionError::BadDescription)
        );
        // No terminator before the data ends.
        let raw = [0, 0, 0, 0, 4, 0, b'A', 0];
        assert_eq!(LoadOption::decode(&raw), Err(LoadOptionError::Truncated));
    }

    #[test]
    fn decode_rejects_list_length_beyond_data() {
        let raw = [0, 0, 0, 0, 0x40, 0, 0, 0, 0x7F, 0xFF, 4, 0];
        assert_eq!(LoadOption::decode(&raw), Err(LoadOptionError::Truncated));
    }

    #[test]
    fn decode_rejects_malformed_device_path_nodes() {
        let cases: [&[u8]; 6] = [
            &[0x04, 0x04, 0, 0, 0x7F, 0xFF, 4, 0],    // node length 0
            &[0x04, 0x04, 2, 0, 0x7F, 0xFF, 4, 0],    // node length 2
            &[0x04, 0x04, 3, 0, 0x7F, 0xFF, 4, 0],    // node length 3
            &[0x04, 0x04, 0x20, 0, 0x7F, 0xFF, 4, 0], // node runs past the list
            &[0x04, 0x04, 4, 0],                      // missing End node
            &[0x7F, 0xFF, 8, 0],                      // End node with wrong length
        ];
        for device in cases {
            let option = LoadOption {
                attributes: 0,
                description: String::new(),
                device_path: device.to_vec(),
                optional_data: Vec::new(),
            };
            assert_eq!(
                LoadOption::decode(&option.encode()),
                Err(LoadOptionError::BadDevicePath),
                "device path {device:02X?}"
            );
        }
        // Empty device path list.
        assert_eq!(
            LoadOption::decode(&[0, 0, 0, 0, 0, 0, 0, 0]),
            Err(LoadOptionError::BadDevicePath)
        );
    }

    #[test]
    fn boot_order_round_trip_and_bounds() {
        let order = [0x0007u16, 0x0001, 0xFFFF];
        let bytes = encode_boot_order(&order);
        assert_eq!(bytes, vec![7, 0, 1, 0, 0xFF, 0xFF]);
        assert_eq!(decode_boot_order(&bytes), Ok(order.to_vec()));
        assert_eq!(decode_boot_order(&[1]), Err(LoadOptionError::BadBootOrder));
        assert_eq!(
            decode_boot_order(&[1, 0, 2]),
            Err(LoadOptionError::BadBootOrder)
        );
        let too_many = vec![0u8; (MAX_BOOT_ORDER_ENTRIES + 1) * 2];
        assert_eq!(
            decode_boot_order(&too_many),
            Err(LoadOptionError::BadBootOrder)
        );
        let max = vec![0u8; MAX_BOOT_ORDER_ENTRIES * 2];
        assert_eq!(
            decode_boot_order(&max).map(|v| v.len()),
            Ok(MAX_BOOT_ORDER_ENTRIES)
        );
    }

    #[test]
    fn boot_variable_names() {
        assert_eq!(boot_variable_name(7), "Boot0007");
        assert_eq!(boot_variable_name(0xABCD), "BootABCD");
        assert_eq!(parse_boot_number("Boot0007"), None);
        assert_eq!(parse_boot_number("0007"), Some(7));
        assert_eq!(parse_boot_number("000a"), Some(10));
        assert_eq!(parse_boot_number("000A"), Some(10));
        assert_eq!(parse_boot_number("000G"), None);
        assert_eq!(parse_boot_number("00070"), None);
        assert_eq!(parse_boot_number("+007"), None);
        assert_eq!(parse_boot_number("-001"), None);
        assert_eq!(parse_boot_number(""), None);
    }

    #[test]
    fn boot_order_list_helpers() {
        assert_eq!(prepend_unique(&[3, 5, 3], 5), vec![5, 3, 3]);
        assert_eq!(prepend_unique(&[], 9), vec![9]);
        assert_eq!(remove_number(&[3, 5, 3], 3), vec![5]);
        assert_eq!(remove_number(&[1], 2), vec![1]);
        assert_eq!(first_free_boot_number(&[]), Some(0));
        assert_eq!(first_free_boot_number(&[0, 1, 3]), Some(2));
        assert_eq!(first_free_boot_number(&[1, 0, 0, 2]), Some(3));
        assert_eq!(first_free_boot_number(&[u16::MAX]), Some(0));
        let all: Vec<u16> = (0..=u16::MAX).collect();
        assert_eq!(first_free_boot_number(&all), None);
    }

    #[test]
    fn hard_drive_file_vector_is_byte_exact() {
        let node = HardDriveNode {
            partition_number: 1,
            partition_start_lba: 2048,
            partition_size_lba: 0x10_0000,
            signature: SIGNATURE,
            partition_format: 0x02,
            signature_type: 0x02,
        };
        let expected: [u8; 64] = [
            // HD node: type 4, subtype 1, length 42
            0x04, 0x01, 0x2A, 0x00, // partition number 1
            0x01, 0x00, 0x00, 0x00, // start 2048 (0x800)
            0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // size 0x100000
            0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, // signature
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x10, 0x11, 0x12, 0x13, 0x14,
            0x15, 0x16, // partition format GPT, signature type GUID
            0x02, 0x02, // FilePath node: type 4, subtype 4, length 18, "\A.EFI" + NUL
            0x04, 0x04, 0x12, 0x00, 0x5C, 0x00, 0x41, 0x00, 0x2E, 0x00, 0x45, 0x00, 0x46, 0x00,
            0x49, 0x00, 0x00, 0x00, // End-Entire
            0x7F, 0xFF, 0x04, 0x00,
        ];
        let bytes = hard_drive_file_device_path(&node, "\\A.EFI");
        assert_eq!(bytes.as_slice(), &expected[..]);
        assert_eq!(device_path_file_name(&bytes).as_deref(), Some("\\A.EFI"));
        assert_eq!(
            describe_device_path(&bytes),
            "HD(1,GPT,04030201-0605-0807-0910-111213141516)/\\A.EFI"
        );
    }

    #[test]
    fn mbr_hard_drive_description_uses_eight_hex_digits() {
        let mut signature = [0u8; 16];
        signature[..4].copy_from_slice(&[0x78, 0x56, 0x34, 0x12]);
        let bytes = hard_drive_file_device_path(&hd(signature, 0x01, 0x01), "\\EFI\\X.EFI");
        assert_eq!(
            describe_device_path(&bytes),
            "HD(1,MBR,12345678)/\\EFI\\X.EFI"
        );
    }

    #[test]
    fn efi_global_guid_text_matches_constant() {
        let bytes = hard_drive_file_device_path(
            &hd(EFI_GLOBAL_VARIABLE_GUID, 0x02, 0x02),
            "\\EFI\\BOOTAPP\\BOOTAPPX64.EFI",
        );
        let text = describe_device_path(&bytes);
        let expected = &EFI_GLOBAL_VARIABLE_GUID_TEXT[1..EFI_GLOBAL_VARIABLE_GUID_TEXT.len() - 1];
        assert!(text.contains(expected), "{text}");
        assert!(
            text.starts_with("HD(1,GPT,8BE4DF61-93CA-11D2-AA0D-00E098032B8C)"),
            "{text}"
        );
    }

    #[test]
    fn describe_generic_and_malformed_nodes() {
        // PciRoot (2,1) with length 8 then End-Entire.
        let generic = [0x02, 0x01, 8, 0, 0x0A, 0x0B, 0x0C, 0x0D, 0x7F, 0xFF, 4, 0];
        assert_eq!(describe_device_path(&generic), "Node(2,1)");
        assert_eq!(describe_device_path(&[]), "<malformed>");
        assert_eq!(describe_device_path(&[0x04, 0x04, 2, 0]), "<malformed>");
        assert_eq!(
            describe_device_path(&[0x04, 0x04, 0x12, 0x00, 0x7F, 0xFF, 4, 0]),
            "<malformed>"
        );
    }

    #[test]
    fn append_file_path_strips_end_and_appends() {
        let hd_only = hard_drive_file_device_path(&hd(SIGNATURE, 2, 2), "\\unused");
        // Drop the file node: HD node is the first 42 bytes.
        let device = &hd_only[..42];
        let with_end = {
            let mut v = device.to_vec();
            v.extend_from_slice(&END_ENTIRE);
            v
        };
        let appended = append_file_path(&with_end, "\\EFI\\BOOTAPP\\BOOTAPPX64.EFI").unwrap();
        assert_eq!(&appended[..42], device);
        assert_eq!(&appended[appended.len() - 4..], &END_ENTIRE[..]);
        assert_eq!(
            device_path_file_name(&appended).as_deref(),
            Some("\\EFI\\BOOTAPP\\BOOTAPPX64.EFI")
        );
        // Without a trailing End node the result is identical.
        assert_eq!(
            append_file_path(device, "\\EFI\\BOOTAPP\\BOOTAPPX64.EFI").unwrap(),
            appended
        );
    }

    #[test]
    fn append_file_path_rejects_malformed_input() {
        assert_eq!(
            append_file_path(&[0x04, 0x04, 0, 0], "\\A.EFI"),
            Err(LoadOptionError::BadDevicePath)
        );
        assert_eq!(
            append_file_path(&[0x7F, 0x01, 4, 0], "\\A.EFI"),
            Err(LoadOptionError::BadDevicePath)
        );
        assert_eq!(
            append_file_path(&[0x7F, 0xFF, 4, 0, 0x04, 0x04, 4, 0], "\\A.EFI"),
            Err(LoadOptionError::BadDevicePath)
        );
        assert_eq!(
            append_file_path(&[], "\\A\0B.EFI"),
            Err(LoadOptionError::BadDevicePath)
        );
        let long = format!("\\{}", "A".repeat(40_000));
        assert_eq!(
            append_file_path(&[], &long),
            Err(LoadOptionError::BadDevicePath)
        );
    }

    #[test]
    fn file_name_joins_nodes_with_single_separator() {
        let mut list = Vec::new();
        push_file_path_node(&mut list, "\\A");
        push_file_path_node(&mut list, "B");
        list.extend_from_slice(&END_ENTIRE);
        assert_eq!(device_path_file_name(&list).as_deref(), Some("\\A\\B"));

        let mut list = Vec::new();
        push_file_path_node(&mut list, "\\A");
        push_file_path_node(&mut list, "\\B");
        list.extend_from_slice(&END_ENTIRE);
        assert_eq!(device_path_file_name(&list).as_deref(), Some("\\A\\B"));

        // No file nodes at all.
        let mut list = Vec::new();
        list.extend_from_slice(&END_ENTIRE);
        assert_eq!(device_path_file_name(&list), None);
        assert_eq!(device_path_file_name(&[0x04, 0x04, 2, 0]), None);
    }

    #[test]
    fn describe_is_bounded_by_input_and_never_panics() {
        let mut list = Vec::new();
        for _ in 0..4096 {
            list.extend_from_slice(&[0x02, 0x01, 4, 0]);
        }
        list.extend_from_slice(&END_ENTIRE);
        let text = describe_device_path(&list);
        assert!(text.starts_with("Node(2,1)/Node(2,1)"));
        assert!(text.len() < 64 * 1024);
    }

    #[test]
    fn same_boot_target_requires_same_partition_and_file() {
        let hd = |signature: u8| HardDriveNode {
            partition_number: 1,
            partition_start_lba: 2048,
            partition_size_lba: 204_800,
            signature: [signature; 16],
            partition_format: 0x02,
            signature_type: 0x02,
        };
        let file = r"\EFI\BOOTAPP\BOOTAPPX64.EFI";
        let short = hard_drive_file_device_path(&hd(7), file);
        // Firmware-style full path: an opaque prefix node, then HD, then file.
        let mut device = vec![0x02, 0x01, 0x0c, 0x00, 0xd0, 0x41, 0x03, 0x0a, 0, 0, 0, 0];
        device.extend_from_slice(&short[..42]);
        let full = append_file_path(&device, r"\efi\bootapp\bootappx64.efi").unwrap();
        assert!(same_boot_target(&short, &full));
        assert!(!same_boot_target(
            &short,
            &hard_drive_file_device_path(&hd(8), file)
        ));
        let other_file = hard_drive_file_device_path(&hd(7), r"\EFI\BOOT\BOOTX64.EFI");
        assert!(!same_boot_target(&short, &other_file));
        let no_hd = append_file_path(&[], file).unwrap();
        assert!(same_boot_target(&no_hd, &no_hd));
        assert!(!same_boot_target(&no_hd, &short));
        assert!(!same_boot_target(&[1, 2, 3], &short));
    }

    #[test]
    fn multi_instance_list_decodes_and_keeps_bytes_verbatim() {
        let first = append_file_path(&[], r"\EFI\BOOTAPP\BOOTAPPX64.EFI").unwrap();
        let second = append_file_path(&[], r"\OTHER.EFI").unwrap();
        let mut list = first.clone();
        list.extend_from_slice(&second);
        let option = LoadOption {
            attributes: LOAD_OPTION_ACTIVE,
            description: String::from("Multi"),
            device_path: list.clone(),
            optional_data: vec![0xAB],
        };
        let decoded = LoadOption::decode(&option.encode()).unwrap();
        assert_eq!(decoded.device_path, list);
        assert_eq!(decoded.optional_data, vec![0xAB]);
        // Only the first instance names the file.
        assert_eq!(
            device_path_file_name(&list).as_deref(),
            Some(r"\EFI\BOOTAPP\BOOTAPPX64.EFI")
        );
        assert_eq!(describe_device_path(&list), r"\EFI\BOOTAPP\BOOTAPPX64.EFI");
    }

    #[test]
    fn multi_instance_list_still_rejects_malformed_nodes() {
        let first = append_file_path(&[], r"\A.EFI").unwrap();
        // Second instance has a node whose length (2) is invalid.
        let mut bad_second = first.clone();
        bad_second.extend_from_slice(&[0x04, 0x04, 2, 0, 0x7F, 0xFF, 4, 0]);
        assert_eq!(
            LoadOption::decode(
                &LoadOption {
                    attributes: 0,
                    description: String::new(),
                    device_path: bad_second,
                    optional_data: Vec::new(),
                }
                .encode()
            ),
            Err(LoadOptionError::BadDevicePath)
        );
        // Instance terminated by End-This-Instance, then a final instance without a terminator.
        let mut unterminated = first.clone();
        unterminated.extend_from_slice(&[0x04, 0x04, 4, 0]);
        assert!(split_nodes(&unterminated).is_err());
        // A bare End-Entire after a complete instance is an empty instance: accepted.
        let mut empty_instance = first.clone();
        empty_instance.extend_from_slice(&END_ENTIRE);
        assert!(split_nodes(&empty_instance).is_ok());
        // A malformed End node (length 8) in a later instance is rejected.
        let mut bad_end = first.clone();
        bad_end.extend_from_slice(&[0x7F, 0xFF, 8, 0]);
        assert!(split_nodes(&bad_end).is_err());
        // End-This-Instance followed by a valid second instance is accepted.
        let mut this_instance = first.clone();
        this_instance[first.len() - 3] = END_INSTANCE_SUBTYPE;
        this_instance.extend_from_slice(&append_file_path(&[], r"\B.EFI").unwrap());
        assert!(split_nodes(&this_instance).is_ok());
    }
}

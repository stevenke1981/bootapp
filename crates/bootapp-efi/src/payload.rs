#![cfg_attr(target_os = "uefi", no_std)]
#![cfg_attr(target_os = "uefi", no_main)]

extern crate alloc;

mod options_text;
#[cfg(target_os = "uefi")]
mod runtime_support;
#[cfg(target_os = "uefi")]
mod trace;

/// Options are read from a bounded prefix; `options_text` bounds the output.
#[cfg(target_os = "uefi")]
const MAX_OPTION_BYTES: usize = 2 * options_text::MAX_OPTION_UNITS + 2;

#[cfg(target_os = "uefi")]
#[uefi::entry]
fn efi_main() -> uefi::Status {
    // The parent initializes the crate's helpers in its own image; this child
    // must initialize its separate allocator/system-table state too.
    if let Err(error) = uefi::helpers::init() {
        return error.status();
    }
    // Report exactly what the parent passed so tests can verify the options /
    // initrd= plumbing a Linux EFI stub would receive. LoadedImage of our own
    // image handle is not bound BY_DRIVER by anyone, so EXCLUSIVE disconnects
    // nothing; it is closed before StartImage returns to the parent.
    let options = uefi::boot::open_protocol_exclusive::<uefi::proto::loaded_image::LoadedImage>(
        uefi::boot::image_handle(),
    )
    .ok()
    .map(|image| {
        let bytes = image.load_options_as_bytes().unwrap_or(&[]);
        options_text::load_options_text(&bytes[..bytes.len().min(MAX_OPTION_BYTES)])
    })
    .unwrap_or_default();
    trace::trace(&alloc::format!("BOOTAPP_PAYLOAD_OPTIONS {options}"));
    trace::trace("BOOTAPP_PAYLOAD_OK");
    uefi::println!("BOOTAPP_PAYLOAD_OK: child EFI application executed and returned.");
    uefi::Status::SUCCESS
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    eprintln!("bootapp-payload is an EFI chainloading acceptance fixture.");
}

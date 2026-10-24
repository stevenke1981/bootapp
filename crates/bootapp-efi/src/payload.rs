#![cfg_attr(target_os = "uefi", no_std)]
#![cfg_attr(target_os = "uefi", no_main)]

#[cfg(target_os = "uefi")]
mod runtime_support;
#[cfg(target_os = "uefi")]
mod trace;

#[cfg(target_os = "uefi")]
#[uefi::entry]
fn efi_main() -> uefi::Status {
    // The parent initializes the crate's helpers in its own image; this child
    // must initialize its separate allocator/system-table state too.
    if let Err(error) = uefi::helpers::init() {
        return error.status();
    }
    trace::trace("BOOTAPP_PAYLOAD_OK");
    uefi::println!("BOOTAPP_PAYLOAD_OK: child EFI application executed and returned.");
    uefi::Status::SUCCESS
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    eprintln!("bootapp-payload is an EFI chainloading acceptance fixture.");
}

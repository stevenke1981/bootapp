#![cfg_attr(target_os = "uefi", no_std)]
#![cfg_attr(target_os = "uefi", no_main)]

#[cfg(target_os = "uefi")]
extern crate alloc;
mod discovery_policy;
#[cfg(target_os = "uefi")]
mod firmware;
#[cfg(target_os = "uefi")]
mod runtime_support;

#[cfg(target_os = "uefi")]
#[uefi::entry]
fn efi_main() -> uefi::Status {
    if let Err(error) = uefi::helpers::init() {
        return error.status();
    }
    match firmware::run() {
        Ok(()) => uefi::Status::SUCCESS,
        Err(status) => {
            uefi::println!("BootApp cannot continue: {:?}", status);
            status
        }
    }
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    eprintln!("Build bootapp-efi for i686-, x86_64-, or aarch64-unknown-uefi.");
}

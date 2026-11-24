/// Best-effort serial trace through firmware protocols. Failure never blocks boot.
///
/// The Serial I/O protocol is opened with GET_PROTOCOL, not EXCLUSIVE: the
/// firmware's terminal driver holds it BY_DRIVER to provide the serial
/// console, and an EXCLUSIVE open makes the firmware disconnect that driver
/// (observed with OVMF: after the first v0.2 trace the menu stopped appearing
/// on the serial console and serial keyboard input was lost).
pub fn trace(message: &str) {
    use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
    use uefi::proto::console::serial::Serial;
    let Ok(handle) = boot::get_handle_for_protocol::<Serial>() else {
        return;
    };
    // SAFETY: GET_PROTOCOL is non-exclusive. The handle came from the firmware
    // just above and this image never uninstalls or reinstalls Serial I/O; the
    // interface is used only for the two Write calls below and closed on drop
    // before returning. The terminal driver shares the port, but UEFI is
    // single-threaded and Serial I/O Write calls are not reentered by us.
    let opened = unsafe {
        boot::open_protocol::<Serial>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    };
    if let Ok(mut serial) = opened {
        let _ = serial.write(message.as_bytes());
        let _ = serial.write(b"\r\n");
    }
}

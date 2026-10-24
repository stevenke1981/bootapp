/// Best-effort serial trace through firmware protocols. Failure never blocks boot.
pub fn trace(message: &str) {
    use uefi::boot;
    use uefi::proto::console::serial::Serial;
    if let Ok(handle) = boot::get_handle_for_protocol::<Serial>()
        && let Ok(mut serial) = boot::open_protocol_exclusive::<Serial>(handle)
    {
        let _ = serial.write(message.as_bytes());
        let _ = serial.write(b"\r\n");
    }
}

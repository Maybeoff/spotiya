// Use the running system's hostname, including its per-device suffix. Firmware
// /etc/hostname may contain only a model name and is not the live device name.
pub fn system_name() -> String {
    let mut buffer = [0u8; 256];
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        if let Ok(name) = std::str::from_utf8(&buffer[..end])
            && let Some(name) = usable_name(name)
        {
            return name;
        }
    }
    "spotiya".to_owned()
}
fn usable_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || matches!(name, "localhost" | "localhost.localdomain" | "(none)")
        || name.chars().any(char::is_control)
    {
        None
    } else {
        Some(name.to_owned())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_per_device_and_not_tied_to_a_model() {
        for name in [
            "yandex-mini2-ABCD",
            "yandex-duomax-ABCD",
            "living-room-speaker",
        ] {
            assert_eq!(usable_name(name).as_deref(), Some(name));
        }
        for name in ["", "localhost", "(none)", "bad\nname"] {
            assert_eq!(usable_name(name), None);
        }
    }
}

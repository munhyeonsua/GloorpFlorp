//! Global key state for push-to-talk. Works while a game has focus by polling
//! `GetAsyncKeyState` (no keyboard hooks, nothing injected into the game).
//! Key codes are Windows virtual-key codes, including mouse buttons
//! (VK_XBUTTON1 = 0x05 "Mouse 4", VK_XBUTTON2 = 0x06 "Mouse 5").
//! Non-Windows: nothing is ever pressed.

/// Is the key held right now?
pub fn is_down(vk: u32) -> bool {
    #[cfg(windows)]
    {
        let Ok(vk) = i32::try_from(vk) else {
            return false;
        };
        // SAFETY: GetAsyncKeyState accepts any value; unknown codes read as up.
        let state = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) };
        state as u16 & 0x8000 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
        false
    }
}

/// Keys considered by [`first_pressed`]: everything except left/right mouse
/// and the generic Shift/Ctrl/Alt codes (their left/right variants are kept).
fn bindable(vk: u32) -> bool {
    (0x03..=0xFE).contains(&vk) && !(0x10..=0x12).contains(&vk)
}

/// The first key currently held, ignoring left/right mouse buttons
/// (used for "press a key to bind push-to-talk").
pub fn first_pressed() -> Option<u32> {
    (0x03..=0xFE_u32)
        .filter(|&vk| bindable(vk))
        .find(|&vk| is_down(vk))
}

/// Human name, e.g. "Mouse 5", "V", "Caps Lock", "F13".
pub fn name(vk: u32) -> String {
    if let Some(n) = known_name(vk) {
        return n;
    }
    #[cfg(windows)]
    if let Some(n) = os_name(vk) {
        return n;
    }
    format!("Key 0x{vk:02X}")
}

fn known_name(vk: u32) -> Option<String> {
    let fixed = match vk {
        0x01 => "Mouse 1",
        0x02 => "Mouse 2",
        0x04 => "Mouse 3",
        0x05 => "Mouse 4",
        0x06 => "Mouse 5",
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0C => "Clear",
        0x0D => "Enter",
        0x10 => "Shift",
        0x11 => "Ctrl",
        0x12 => "Alt",
        0x13 => "Pause",
        0x14 => "Caps Lock",
        0x1B => "Esc",
        0x20 => "Space",
        0x21 => "Page Up",
        0x22 => "Page Down",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left Arrow",
        0x26 => "Up Arrow",
        0x27 => "Right Arrow",
        0x28 => "Down Arrow",
        0x2C => "Print Screen",
        0x2D => "Insert",
        0x2E => "Delete",
        0x5B => "Left Windows",
        0x5C => "Right Windows",
        0x5D => "Menu",
        0x6A => "Numpad *",
        0x6B => "Numpad +",
        0x6C => "Numpad Separator",
        0x6D => "Numpad -",
        0x6E => "Numpad .",
        0x6F => "Numpad /",
        0x90 => "Num Lock",
        0x91 => "Scroll Lock",
        0xA0 => "Left Shift",
        0xA1 => "Right Shift",
        0xA2 => "Left Ctrl",
        0xA3 => "Right Ctrl",
        0xA4 => "Left Alt",
        0xA5 => "Right Alt",
        0xAD => "Volume Mute",
        0xAE => "Volume Down",
        0xAF => "Volume Up",
        0xB0 => "Next Track",
        0xB1 => "Previous Track",
        0xB3 => "Play/Pause",
        0xBA => ";",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "`",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        _ => "",
    };
    if !fixed.is_empty() {
        return Some(fixed.to_string());
    }
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from),
        0x60..=0x69 => Some(format!("Numpad {}", vk - 0x60)),
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        _ => None,
    }
}

/// Keyboard-layout name from Windows for keys not in our table.
#[cfg(windows)]
fn os_name(vk: u32) -> Option<String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC,
    };
    // SAFETY: plain lookups; the buffer length is passed by the wrapper.
    unsafe {
        let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        if scan == 0 || scan > 0xFF {
            return None;
        }
        let mut buf = [0u16; 64];
        let n = GetKeyNameTextW((scan << 16) as i32, &mut buf);
        let n = usize::try_from(n).ok().filter(|&n| n > 0)?.min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_names() {
        assert_eq!(name(0x04), "Mouse 3");
        assert_eq!(name(0x05), "Mouse 4");
        assert_eq!(name(0x06), "Mouse 5");
        assert_eq!(name(0x56), "V");
        assert_eq!(name(0x41), "A");
        assert_eq!(name(0x5A), "Z");
        assert_eq!(name(0x30), "0");
        assert_eq!(name(0x39), "9");
        assert_eq!(name(0x14), "Caps Lock");
        assert_eq!(name(0x70), "F1");
        assert_eq!(name(0x7C), "F13");
        assert_eq!(name(0x87), "F24");
        assert_eq!(name(0x20), "Space");
        assert_eq!(name(0xA0), "Left Shift");
        assert_eq!(name(0xA5), "Right Alt");
        assert_eq!(name(0x60), "Numpad 0");
        assert_eq!(name(0x69), "Numpad 9");
        assert_eq!(name(0x6B), "Numpad +");
        assert_eq!(name(0xC0), "`");
        assert_eq!(name(0xDC), "\\");
        assert_eq!(name(0xDE), "'");
        assert_eq!(name(0x26), "Up Arrow");
        assert_eq!(name(0x22), "Page Down");
    }

    #[test]
    fn every_bindable_key_has_a_name() {
        for vk in 0..=0xFF {
            assert!(!name(vk).trim().is_empty(), "vk {vk:#04x}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unknown_key_fallback() {
        assert_eq!(name(0xE5), "Key 0xE5");
    }

    #[test]
    fn bindable_range() {
        assert!(!bindable(0x01));
        assert!(!bindable(0x02));
        assert!(bindable(0x05));
        assert!(!bindable(0x10));
        assert!(!bindable(0x11));
        assert!(!bindable(0x12));
        assert!(bindable(0xA0));
        assert!(bindable(0xFE));
        assert!(!bindable(0xFF));
    }

    #[test]
    fn nothing_held_in_tests() {
        // No keyboard on CI; on Linux this is always false.
        if cfg!(not(windows)) {
            assert!(!is_down(0x41));
            assert_eq!(first_pressed(), None);
        }
        assert!(!is_down(u32::MAX));
    }
}

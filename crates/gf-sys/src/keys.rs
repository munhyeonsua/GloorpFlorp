//! Global key state for push-to-talk. Works while a game has focus by polling
//! `GetAsyncKeyState` (no keyboard hooks, nothing injected into the game).
//! Key codes are Windows virtual-key codes, including mouse buttons
//! (VK_XBUTTON1 = 0x05 "Mouse 4", VK_XBUTTON2 = 0x06 "Mouse 5").
//! Non-Windows: nothing is ever pressed.

/// Is the key held right now?
pub fn is_down(vk: u32) -> bool {
    todo!()
}

/// The first key currently held, ignoring left/right mouse buttons
/// (used for "press a key to bind push-to-talk").
pub fn first_pressed() -> Option<u32> {
    todo!()
}

/// Human name, e.g. "Mouse 5", "V", "Caps Lock", "F13".
pub fn name(vk: u32) -> String {
    todo!()
}

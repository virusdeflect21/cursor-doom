//! Doom key codes (subset of doomgeneric's `doomkeys.h`) and the mapping from
//! Windows virtual keys to them.
//!
//! Cursor Doom keeps its footprint tiny on purpose: **arrows and Ctrl are the
//! only keys it takes from the desktop**, everything else stays with the
//! application you are using.
//!
//! * arrows - walk and turn (`key_up`, `key_down`, `key_left`, `key_right`)
//! * Ctrl   - the one action key: fire, use and confirm in menus
//!
//! Doom's default bindings (`m_controls.c`) refer to `KEY_UPARROW`,
//! `KEY_DOWNARROW`, `KEY_LEFTARROW`, `KEY_RIGHTARROW`, `KEY_FIRE`, `KEY_USE`,
//! `KEY_ENTER` (menu select) and `KEY_ESCAPE` (menu).

pub const KEY_LEFTARROW: u8 = 0xac;
pub const KEY_RIGHTARROW: u8 = 0xae;
pub const KEY_UPARROW: u8 = 0xad;
pub const KEY_DOWNARROW: u8 = 0xaf;
pub const KEY_USE: u8 = 0xa2;
pub const KEY_FIRE: u8 = 0xa3;
pub const KEY_RSHIFT: u8 = 0x80 + 0x36;
pub const KEY_ESCAPE: u8 = 27;
pub const KEY_ENTER: u8 = 13;
pub const KEY_TAB: u8 = 9;
pub const KEY_BACKSPACE: u8 = 0x7f;

// Windows virtual key codes we care about.
const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_ESCAPE: u32 = 0x1b;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_DIGIT_1: u32 = 0x31;
const VK_DIGIT_9: u32 = 0x39;
const VK_N: u32 = 0x4e;
const VK_Y: u32 = 0x59;
const VK_LSHIFT: u32 = 0xa0;
const VK_RSHIFT: u32 = 0xa1;
const VK_LCONTROL: u32 = 0xa2;
const VK_RCONTROL: u32 = 0xa3;

/// What an intercepted key does.
#[derive(Clone, Copy, Debug)]
pub struct Mapping {
    /// Up to four Doom keys produced together by this Windows key.
    keys: [u8; 4],
    len: usize,
    /// `true` = the key is swallowed so it does not reach the app below the
    /// cursor. Only arrows and Ctrl are swallowed.
    pub swallow: bool,
}

impl Mapping {
    const fn single(key: u8, swallow: bool) -> Self {
        Self {
            keys: [key, 0, 0, 0],
            len: 1,
            swallow,
        }
    }

    const fn many(keys: [u8; 4], len: usize, swallow: bool) -> Self {
        Self { keys, len, swallow }
    }

    /// The Doom keys this mapping stands for.
    pub fn keys(&self) -> &[u8] {
        &self.keys[..self.len]
    }
}

/// Ctrl is fire, use and "confirm" in one: in game it shoots, on a door or
/// switch it uses, and in the menus it picks the highlighted entry.
const CTRL_ACTION: Mapping = Mapping::many([KEY_FIRE, KEY_USE, KEY_ENTER, 0], 3, true);

/// Maps a Windows virtual key to the doom keys it should produce, if any.
///
/// NOTE: vanilla Doom (which doomgeneric is) has no vertical look, so the
/// left/right arrows turn the player while up/down arrows walk forward/back -
/// that is Doom's own default arrow binding.
pub fn map_vk(vk: u32) -> Option<Mapping> {
    match vk {
        // Arrows: walk (up/down) and look around (left/right). Captured.
        VK_LEFT => Some(Mapping::single(KEY_LEFTARROW, true)),
        VK_RIGHT => Some(Mapping::single(KEY_RIGHTARROW, true)),
        VK_UP => Some(Mapping::single(KEY_UPARROW, true)),
        VK_DOWN => Some(Mapping::single(KEY_DOWNARROW, true)),

        // Ctrl: fire / use / confirm. Captured.
        VK_LCONTROL | VK_RCONTROL => Some(CTRL_ACTION),

        // Escape opens Doom's menu but is *not* swallowed: apps keep their Esc.
        VK_ESCAPE => Some(Mapping::single(KEY_ESCAPE, false)),
        // Menu-adjacent keys that stay with the desktop.
        VK_TAB => Some(Mapping::single(KEY_TAB, false)),
        VK_BACK => Some(Mapping::single(KEY_BACKSPACE, false)),
        // Shift is the Doom "run" key; it still reaches the app as usual.
        VK_LSHIFT | VK_RSHIFT => Some(Mapping::single(KEY_RSHIFT, false)),
        // Answering Doom's own yes/no prompts.
        VK_Y => Some(Mapping::single(b'y', false)),
        VK_N => Some(Mapping::single(b'n', false)),
        // Weapon selection (1-9), not swallowed.
        VK_DIGIT_1..=VK_DIGIT_9 => Some(Mapping::single((vk - VK_DIGIT_1) as u8 + b'1', false)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_of(vk: u32) -> Option<Vec<u8>> {
        map_vk(vk).map(|m| m.keys().to_vec())
    }

    #[test]
    fn only_arrows_and_ctrl_are_swallowed() {
        for vk in 0u32..=0xFF {
            if let Some(mapping) = map_vk(vk) {
                if mapping.swallow {
                    assert!(
                        matches!(
                            vk,
                            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_LCONTROL | VK_RCONTROL
                        ),
                        "vk {vk:#x} is swallowed but should not be"
                    );
                }
            }
        }
    }

    #[test]
    fn wasd_enter_and_space_are_not_game_keys() {
        for vk in [0x57u32, 0x41, 0x53, 0x44, 0x0d, 0x20] {
            assert!(keys_of(vk).is_none(), "vk {vk:#x} should not be mapped");
        }
    }

    #[test]
    fn ctrl_fires_uses_and_confirms() {
        let mapping = map_vk(VK_LCONTROL).unwrap();
        assert!(mapping.swallow);
        assert_eq!(mapping.keys(), &[KEY_FIRE, KEY_USE, KEY_ENTER]);

        let right = map_vk(VK_RCONTROL).unwrap();
        assert_eq!(right.keys(), mapping.keys());
    }

    #[test]
    fn arrows_walk_and_turn() {
        assert_eq!(keys_of(VK_UP).unwrap(), vec![KEY_UPARROW]);
        assert_eq!(keys_of(VK_DOWN).unwrap(), vec![KEY_DOWNARROW]);
        assert_eq!(keys_of(VK_LEFT).unwrap(), vec![KEY_LEFTARROW]);
        assert_eq!(keys_of(VK_RIGHT).unwrap(), vec![KEY_RIGHTARROW]);
    }

    #[test]
    fn escape_reaches_doom_and_the_desktop() {
        let mapping = map_vk(VK_ESCAPE).unwrap();
        assert_eq!(mapping.keys(), &[KEY_ESCAPE]);
        assert!(!mapping.swallow);
    }
}

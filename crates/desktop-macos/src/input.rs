//! Mouse and keyboard synthesis through `CGEvent`.
//!
//! Text and chords take different paths, and the split is forced by real
//! limitations rather than preference:
//!
//! * Bulk text goes through `CGEventKeyboardSetUnicodeString`, which is
//!   layout-independent — but it truncates past 20 UTF-16 units per event,
//!   is dropped entirely when a chunk begins with a newline or tab, and is
//!   ignored by toolkits that re-derive characters from the keycode.
//! * Individual keys and modifier chords go through real virtual keycodes,
//!   because a modifier chord has no Unicode representation at all.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGMouseButton,
};

use desktop_core::{
    errors::{DesktopError, Result},
    models::{
        backend::Platform,
        chord::{Chord, Key, NamedKey},
        geometry::{CoordinateSpace, Point, ScrollDelta},
    },
    ports::{InputPort, KEYSTROKE_INTERVAL, MouseButton},
};

/// `CGEventKeyboardSetUnicodeString` silently truncates beyond this many UTF-16
/// units, so text is sent in chunks no larger than this.
const UNICODE_CHUNK: usize = 20;

/// A zero-width space, prefixed to a chunk that would otherwise begin with a
/// newline or tab — which `CGEventKeyboardSetUnicodeString` drops outright.
const ZERO_WIDTH_SPACE: u16 = 0x200B;

pub struct CoreGraphicsInput;

impl CoreGraphicsInput {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn source() -> Result<objc2_core_foundation::CFRetained<CGEventSource>> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| DesktopError::backend("cannot create a Core Graphics event source"))
    }

    fn post_mouse(point: Point, kind: MouseEventKind, button: MouseButton) -> Result<()> {
        let source = Self::source()?;
        let position = CGPoint {
            x: f64::from(point.x),
            y: f64::from(point.y),
        };
        let (event_type, cg_button) = kind.resolve(button);
        let event = CGEvent::new_mouse_event(Some(&source), event_type, position, cg_button)
            .ok_or_else(|| DesktopError::backend("cannot create a mouse event"))?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    /// Posts one key event with an explicit modifier mask.
    ///
    /// The flags are set rather than inherited: ambient modifier state would
    /// otherwise leak into the event and turn a plain keystroke into a
    /// shortcut.
    fn post_key(keycode: u16, down: bool, flags: CGEventFlags) -> Result<()> {
        let source = Self::source()?;
        let event = CGEvent::new_keyboard_event(Some(&source), keycode, down)
            .ok_or_else(|| DesktopError::backend("cannot create a keyboard event"))?;
        CGEvent::set_flags(Some(&event), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    fn post_text_chunk(chunk: &[u16]) -> Result<()> {
        let source = Self::source()?;
        let event = CGEvent::new_keyboard_event(Some(&source), 0, true)
            .ok_or_else(|| DesktopError::backend("cannot create a keyboard event"))?;
        CGEvent::set_flags(Some(&event), CGEventFlags::empty());
        // SAFETY: the slice is a valid, initialised UTF-16 buffer whose length
        // is passed alongside it, and the event outlives the call.
        unsafe {
            CGEvent::keyboard_set_unicode_string(Some(&event), chunk.len() as u64, chunk.as_ptr());
        }
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }
}

impl Default for CoreGraphicsInput {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
enum MouseEventKind {
    Moved,
    Down,
    Up,
}

impl MouseEventKind {
    fn resolve(self, button: MouseButton) -> (objc2_core_graphics::CGEventType, CGMouseButton) {
        use objc2_core_graphics::CGEventType;
        match (self, button) {
            (Self::Moved, _) => (CGEventType::MouseMoved, CGMouseButton::Left),
            (Self::Down, MouseButton::Left) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (Self::Up, MouseButton::Left) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (Self::Down, MouseButton::Right) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (Self::Up, MouseButton::Right) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (Self::Down, MouseButton::Middle) => {
                (CGEventType::OtherMouseDown, CGMouseButton::Center)
            }
            (Self::Up, MouseButton::Middle) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        }
    }
}

impl InputPort for CoreGraphicsInput {
    fn move_mouse(&self, point: Point, _space: &CoordinateSpace) -> Result<()> {
        Self::post_mouse(point, MouseEventKind::Moved, MouseButton::Left)
    }

    fn click(
        &self,
        point: Point,
        _space: &CoordinateSpace,
        button: MouseButton,
        count: u8,
    ) -> Result<()> {
        Self::post_mouse(point, MouseEventKind::Moved, button)?;
        for _ in 0..count.max(1) {
            Self::post_mouse(point, MouseEventKind::Down, button)?;
            Self::post_mouse(point, MouseEventKind::Up, button)?;
        }
        Ok(())
    }

    /// Types literal text, in chunks, paced.
    ///
    /// Paced for the same reason as the X11 adapter: an application that
    /// rebuilds its input state on a keystroke silently drops whatever lands
    /// mid-rebuild, and a whole string posted at once arrives faster than any
    /// keyboard could produce it. Qt applications on macOS are additionally
    /// known to re-derive characters from the keycode and ignore the Unicode
    /// payload when events arrive too quickly.
    fn type_text(&self, text: &str) -> Result<()> {
        let mut first = true;
        for chunk in chunk_utf16(text) {
            if !first {
                std::thread::sleep(KEYSTROKE_INTERVAL);
            }
            first = false;
            Self::post_text_chunk(&chunk)?;
        }
        Ok(())
    }

    fn key(&self, chord: &Chord) -> Result<()> {
        let resolved = chord.resolve(Platform::Macos);
        let keycode = keycode_for(chord.key).ok_or_else(|| {
            DesktopError::invalid_argument(
                "this key has no virtual keycode on the current keyboard layout",
            )
        })?;

        let mut flags = CGEventFlags::empty();
        if resolved.modifiers.ctrl {
            flags |= CGEventFlags::MaskControl;
        }
        if resolved.modifiers.alt {
            flags |= CGEventFlags::MaskAlternate;
        }
        if resolved.modifiers.shift {
            flags |= CGEventFlags::MaskShift;
        }
        if resolved.modifiers.meta {
            flags |= CGEventFlags::MaskCommand;
        }

        Self::post_key(keycode, true, flags)?;
        Self::post_key(keycode, false, flags)
    }

    /// Scrolls by a logical distance, in pixel units rather than lines, so the
    /// caller's distance means what it says.
    fn scroll(&self, delta: ScrollDelta, _space: &CoordinateSpace) -> Result<()> {
        use objc2_core_graphics::CGScrollEventUnit;
        let source = Self::source()?;
        let event = CGEvent::new_scroll_wheel_event2(
            Some(&source),
            CGScrollEventUnit::Pixel,
            2,
            -delta.y,
            -delta.x,
            0,
        )
        .ok_or_else(|| DesktopError::backend("cannot create a scroll event"))?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }
}

/// Splits text into UTF-16 chunks that `CGEventKeyboardSetUnicodeString` will
/// actually deliver.
///
/// Two constraints shape this: the 20-unit truncation, and the fact that a
/// chunk starting with `\n`, `\r` or `\t` is dropped silently. The second is
/// worked around by prefixing a zero-width space, which the target receives as
/// an invisible character rather than losing the line break entirely.
fn chunk_utf16(text: &str) -> Vec<Vec<u16>> {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut index = 0;
    while index < units.len() {
        let end = (index + UNICODE_CHUNK).min(units.len());
        let slice = &units[index..end];
        let leads_with_whitespace = matches!(slice.first(), Some(0x0A | 0x0D | 0x09));
        if leads_with_whitespace {
            let mut prefixed = Vec::with_capacity(slice.len() + 1);
            prefixed.push(ZERO_WIDTH_SPACE);
            prefixed.extend_from_slice(slice);
            chunks.push(prefixed);
        } else {
            chunks.push(slice.to_vec());
        }
        index = end;
    }
    chunks
}

/// Virtual keycodes for the US layout.
///
/// Only the keys that have no Unicode representation are mapped here: letters
/// and punctuation go through the Unicode path instead, which works on any
/// layout. A chord over a letter still needs a keycode, and for a non-US layout
/// that mapping should come from `UCKeyTranslate` — see the note in the README.
fn keycode_for(key: Key) -> Option<u16> {
    Some(match key {
        Key::Named(named) => match named {
            NamedKey::Return => 0x24,
            NamedKey::Tab => 0x30,
            NamedKey::Space => 0x31,
            NamedKey::Backspace => 0x33,
            NamedKey::Escape => 0x35,
            NamedKey::Delete => 0x75,
            NamedKey::Home => 0x73,
            NamedKey::End => 0x77,
            NamedKey::PageUp => 0x74,
            NamedKey::PageDown => 0x79,
            NamedKey::Left => 0x7B,
            NamedKey::Right => 0x7C,
            NamedKey::Down => 0x7D,
            NamedKey::Up => 0x7E,
            NamedKey::Insert => return None,
            NamedKey::Function(n) => match n {
                1 => 0x7A,
                2 => 0x78,
                3 => 0x63,
                4 => 0x76,
                5 => 0x60,
                6 => 0x61,
                7 => 0x62,
                8 => 0x64,
                9 => 0x65,
                10 => 0x6D,
                11 => 0x67,
                12 => 0x6F,
                _ => return None,
            },
        },
        Key::Char(character) => match character.to_ascii_lowercase() {
            'a' => 0x00,
            'b' => 0x0B,
            'c' => 0x08,
            'd' => 0x02,
            'e' => 0x0E,
            'f' => 0x03,
            'g' => 0x05,
            'h' => 0x04,
            'i' => 0x22,
            'j' => 0x26,
            'k' => 0x28,
            'l' => 0x25,
            'm' => 0x2E,
            'n' => 0x2D,
            'o' => 0x1F,
            'p' => 0x23,
            'q' => 0x0C,
            'r' => 0x0F,
            's' => 0x01,
            't' => 0x11,
            'u' => 0x20,
            'v' => 0x09,
            'w' => 0x0D,
            'x' => 0x07,
            'y' => 0x10,
            'z' => 0x06,
            '0' => 0x1D,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '5' => 0x17,
            '6' => 0x16,
            '7' => 0x1A,
            '8' => 0x1C,
            '9' => 0x19,
            '-' => 0x1B,
            '=' => 0x18,
            '[' => 0x21,
            ']' => 0x1E,
            '\\' => 0x2A,
            ';' => 0x29,
            '\'' => 0x27,
            ',' => 0x2B,
            '.' => 0x2F,
            '/' => 0x2C,
            '`' => 0x32,
            ' ' => 0x31,
            _ => return None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_sent_as_a_single_chunk() {
        let chunks = chunk_utf16("hello");
        assert_eq!(chunks.len(), 1);
        assert_eq!(String::from_utf16_lossy(&chunks[0]), "hello");
    }

    #[test]
    fn long_text_is_split_at_the_twenty_unit_truncation_limit() {
        // Past 20 UTF-16 units the API silently drops the remainder, so text
        // longer than that has to be sent as several events.
        let text = "a".repeat(45);
        let chunks = chunk_utf16(&text);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|chunk| chunk.len() <= UNICODE_CHUNK + 1));
        let rejoined: String = chunks
            .iter()
            .map(|chunk| String::from_utf16_lossy(chunk))
            .collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn a_chunk_starting_with_a_newline_is_prefixed_so_it_is_not_dropped() {
        // The API drops any chunk whose first unit is \n, \r or \t.
        let chunks = chunk_utf16("\nhello");
        assert_eq!(chunks[0][0], ZERO_WIDTH_SPACE);
        assert_eq!(String::from_utf16_lossy(&chunks[0][1..]), "\nhello");
    }

    #[test]
    fn a_chunk_starting_with_a_tab_is_prefixed_too() {
        let chunks = chunk_utf16("\tindented");
        assert_eq!(chunks[0][0], ZERO_WIDTH_SPACE);
    }

    #[test]
    fn a_newline_in_the_middle_of_a_chunk_needs_no_prefix() {
        let chunks = chunk_utf16("a\nb");
        assert_ne!(chunks[0][0], ZERO_WIDTH_SPACE);
    }

    #[test]
    fn empty_text_produces_no_events_at_all() {
        assert!(chunk_utf16("").is_empty());
    }

    #[test]
    fn text_outside_the_basic_plane_is_chunked_by_utf16_units_not_characters() {
        // An emoji is two UTF-16 units, and splitting one across events would
        // send two lone surrogates.
        let text = "😀".repeat(15);
        let chunks = chunk_utf16(&text);
        let rejoined: String = chunks
            .iter()
            .map(|chunk| String::from_utf16_lossy(chunk))
            .collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn letters_and_digits_have_virtual_keycodes_for_use_in_chords() {
        assert_eq!(keycode_for(Key::Char('a')), Some(0x00));
        assert_eq!(keycode_for(Key::Char('s')), Some(0x01));
        assert_eq!(keycode_for(Key::Char('1')), Some(0x12));
    }

    #[test]
    fn uppercase_and_lowercase_map_to_the_same_physical_key() {
        assert_eq!(keycode_for(Key::Char('A')), keycode_for(Key::Char('a')));
    }

    #[test]
    fn named_keys_map_to_the_documented_virtual_keycodes() {
        assert_eq!(keycode_for(Key::Named(NamedKey::Return)), Some(0x24));
        assert_eq!(keycode_for(Key::Named(NamedKey::Escape)), Some(0x35));
        assert_eq!(keycode_for(Key::Named(NamedKey::Left)), Some(0x7B));
        assert_eq!(keycode_for(Key::Named(NamedKey::Function(1))), Some(0x7A));
    }

    #[test]
    fn keys_with_no_mac_equivalent_are_refused_rather_than_mapped_to_zero() {
        // Keycode 0 is the letter A; falling back to it would type a stray
        // character instead of failing.
        assert_eq!(keycode_for(Key::Named(NamedKey::Insert)), None);
        assert_eq!(keycode_for(Key::Named(NamedKey::Function(20))), None);
        assert_eq!(keycode_for(Key::Char('é')), None);
    }
}

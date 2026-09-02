//! Mouse and keyboard synthesis through `CGEvent`.
//!
//! Text and chords take different paths, and the split is forced by real
//! limitations rather than preference:
//!
//! * Bulk text goes through `CGEventKeyboardSetUnicodeString`, which is
//!   layout-independent — but it truncates past 20 UTF-16 units per event,
//!   is dropped entirely when a chunk begins with a newline or tab, and is
//!   ignored by toolkits that re-derive characters from the keycode.
//! * Character chords use the same Unicode payload plus modifier flags, so
//!   they do not assume US physical key positions. Named non-text keys use
//!   Apple's stable virtual keycodes.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGMouseButton,
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

    fn post_mouse(
        point: Point,
        kind: MouseEventKind,
        button: MouseButton,
        click_state: i64,
    ) -> Result<()> {
        let source = Self::source()?;
        let position = CGPoint {
            x: f64::from(point.x),
            y: f64::from(point.y),
        };
        let (event_type, cg_button) = kind.resolve(button);
        let event = CGEvent::new_mouse_event(Some(&source), event_type, position, cg_button)
            .ok_or_else(|| DesktopError::backend("cannot create a mouse event"))?;
        if click_state > 0 {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventClickState,
                click_state,
            );
        }
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

    fn post_text_chunk(chunk: &[u16], flags: CGEventFlags) -> Result<()> {
        let source = Self::source()?;
        let down = CGEvent::new_keyboard_event(Some(&source), 0, true)
            .ok_or_else(|| DesktopError::backend("cannot create a keyboard event"))?;
        CGEvent::set_flags(Some(&down), flags);
        // SAFETY: the slice is a valid, initialised UTF-16 buffer whose length
        // is passed alongside it, and the event outlives the call.
        unsafe {
            CGEvent::keyboard_set_unicode_string(Some(&down), chunk.len() as u64, chunk.as_ptr());
        }
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&down));
        let up = CGEvent::new_keyboard_event(Some(&source), 0, false)
            .ok_or_else(|| DesktopError::backend("cannot create a keyboard release event"))?;
        CGEvent::set_flags(Some(&up), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&up));
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
        Self::post_mouse(point, MouseEventKind::Moved, MouseButton::Left, 0)
    }

    fn click(
        &self,
        point: Point,
        _space: &CoordinateSpace,
        button: MouseButton,
        count: u8,
    ) -> Result<()> {
        Self::post_mouse(point, MouseEventKind::Moved, button, 0)?;
        for click_state in 1..=count.max(1) {
            Self::post_mouse(point, MouseEventKind::Down, button, i64::from(click_state))?;
            Self::post_mouse(point, MouseEventKind::Up, button, i64::from(click_state))?;
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
        for event in text_events(text) {
            if !first {
                std::thread::sleep(KEYSTROKE_INTERVAL);
            }
            first = false;
            match event {
                TextEvent::Unicode(chunk) => Self::post_text_chunk(&chunk, CGEventFlags::empty())?,
                TextEvent::Named(key) => {
                    let keycode = keycode_for_named(key)
                        .expect("Return and Tab always have macOS virtual keycodes");
                    Self::post_key(keycode, true, CGEventFlags::empty())?;
                    Self::post_key(keycode, false, CGEventFlags::empty())?;
                }
            }
        }
        Ok(())
    }

    fn key(&self, chord: &Chord) -> Result<()> {
        let resolved = chord.resolve(Platform::Macos);
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
        match chord.key {
            Key::Char(character) => {
                let mut units = [0_u16; 2];
                Self::post_text_chunk(character.encode_utf16(&mut units), flags)
            }
            Key::Named(named) => {
                let keycode = keycode_for_named(named).ok_or_else(|| {
                    DesktopError::invalid_argument("this named key has no macOS virtual keycode")
                })?;
                Self::post_key(keycode, true, flags)?;
                Self::post_key(keycode, false, flags)
            }
        }
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

/// Events that together reproduce literal text without adding hidden bytes.
#[derive(Debug, Eq, PartialEq)]
enum TextEvent {
    Unicode(Vec<u16>),
    Named(NamedKey),
}

/// Plans lossless text events without splitting a UTF-16 surrogate pair or
/// injecting an invisible workaround character into the target.
fn text_events(text: &str) -> Vec<TextEvent> {
    let mut events = Vec::new();
    let mut chunk = Vec::new();
    let flush = |events: &mut Vec<TextEvent>, chunk: &mut Vec<u16>| {
        if !chunk.is_empty() {
            events.push(TextEvent::Unicode(std::mem::take(chunk)));
        }
    };

    for character in text.chars() {
        let named = match character {
            '\n' | '\r' => Some(NamedKey::Return),
            '\t' => Some(NamedKey::Tab),
            _ => None,
        };
        if let Some(named) = named {
            flush(&mut events, &mut chunk);
            events.push(TextEvent::Named(named));
            continue;
        }

        let mut units = [0; 2];
        let encoded = character.encode_utf16(&mut units);
        if chunk.len() + encoded.len() > UNICODE_CHUNK {
            flush(&mut events, &mut chunk);
        }
        chunk.extend_from_slice(encoded);
    }
    flush(&mut events, &mut chunk);
    events
}

fn keycode_for_named(named: NamedKey) -> Option<u16> {
    Some(match named {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_sent_as_a_single_chunk() {
        assert_eq!(
            text_events("hello"),
            vec![TextEvent::Unicode("hello".encode_utf16().collect())]
        );
    }

    #[test]
    fn long_text_is_split_at_the_twenty_unit_truncation_limit() {
        // Past 20 UTF-16 units the API silently drops the remainder, so text
        // longer than that has to be sent as several events.
        let text = "a".repeat(45);
        let events = text_events(&text);
        assert_eq!(events.len(), 3);
        let chunks: Vec<_> = events
            .iter()
            .map(|event| match event {
                TextEvent::Unicode(chunk) => chunk,
                TextEvent::Named(_) => panic!("plain text produced a named key"),
            })
            .collect();
        assert!(chunks.iter().all(|chunk| chunk.len() <= UNICODE_CHUNK));
        let rejoined: String = chunks
            .iter()
            .map(|chunk| String::from_utf16_lossy(chunk))
            .collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn a_leading_newline_is_a_return_event_without_injected_text() {
        assert_eq!(
            text_events("\nhello"),
            vec![
                TextEvent::Named(NamedKey::Return),
                TextEvent::Unicode("hello".encode_utf16().collect())
            ]
        );
    }

    #[test]
    fn a_leading_tab_is_a_tab_event_without_injected_text() {
        assert_eq!(
            text_events("\tindented")[0],
            TextEvent::Named(NamedKey::Tab)
        );
    }

    #[test]
    fn a_newline_in_the_middle_is_its_own_event() {
        assert_eq!(text_events("a\nb").len(), 3);
        assert_eq!(text_events("a\nb")[1], TextEvent::Named(NamedKey::Return));
    }

    #[test]
    fn empty_text_produces_no_events_at_all() {
        assert!(text_events("").is_empty());
    }

    #[test]
    fn text_outside_the_basic_plane_is_chunked_by_utf16_units_not_characters() {
        // An emoji is two UTF-16 units, and splitting one across events would
        // send two lone surrogates.
        let text = "😀".repeat(15);
        let events = text_events(&text);
        let chunks: Vec<_> = events
            .iter()
            .map(|event| match event {
                TextEvent::Unicode(chunk) => chunk,
                TextEvent::Named(_) => panic!("emoji produced a named key"),
            })
            .collect();
        assert!(chunks.iter().all(|chunk| {
            !matches!(chunk.first(), Some(0xDC00..=0xDFFF))
                && !matches!(chunk.last(), Some(0xD800..=0xDBFF))
        }));
        let rejoined: String = chunks
            .iter()
            .map(|chunk| String::from_utf16_lossy(chunk))
            .collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn character_shortcuts_are_representable_as_layout_independent_utf16() {
        let mut units = [0_u16; 2];
        assert_eq!('é'.encode_utf16(&mut units), &[0x00e9]);
        assert_eq!('😀'.encode_utf16(&mut units), &[0xd83d, 0xde00]);
    }

    #[test]
    fn named_keys_map_to_the_documented_virtual_keycodes() {
        assert_eq!(keycode_for_named(NamedKey::Return), Some(0x24));
        assert_eq!(keycode_for_named(NamedKey::Escape), Some(0x35));
        assert_eq!(keycode_for_named(NamedKey::Left), Some(0x7B));
        assert_eq!(keycode_for_named(NamedKey::Function(1)), Some(0x7A));
    }

    #[test]
    fn keys_with_no_mac_equivalent_are_refused_rather_than_mapped_to_zero() {
        // Keycode 0 is the letter A; falling back to it would type a stray
        // character instead of failing.
        assert_eq!(keycode_for_named(NamedKey::Insert), None);
        assert_eq!(keycode_for_named(NamedKey::Function(20)), None);
    }
}

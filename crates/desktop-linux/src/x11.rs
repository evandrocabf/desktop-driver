//! X11 capture and input.
//!
//! Selected only in a genuine X11 session. Under Wayland these calls still
//! succeed via XWayland but act on an empty, parallel world — see the backend
//! selection rules in `desktop_core::models::backend`.

use std::sync::Mutex;

use desktop_core::{
    errors::{DesktopError, Result},
    models::{
        backend::Backend,
        chord::{Chord, Key, NamedKey},
        geometry::{CoordinateSpace, Point, ScaleFactor, ScrollDelta},
        image::Image,
    },
    ports::{CapturePort, CaptureTarget, InputPort, KEYSTROKE_INTERVAL, MouseButton},
};
use x11rb::{
    connection::Connection as _,
    protocol::{
        xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, ImageFormat},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

/// X11 button numbers. 4/5 are vertical wheel, 6/7 horizontal.
const BUTTON_LEFT: u8 = 1;
const BUTTON_MIDDLE: u8 = 2;
const BUTTON_RIGHT: u8 = 3;
const WHEEL_UP: u8 = 4;
const WHEEL_DOWN: u8 = 5;
const WHEEL_LEFT: u8 = 6;
const WHEEL_RIGHT: u8 = 7;

/// One wheel click. X11 has no notion of pixel scrolling, so a logical delta is
/// converted into discrete clicks at this rate.
const PIXELS_PER_WHEEL_CLICK: i32 = 100;

const PRESS: u8 = x11rb::protocol::xproto::BUTTON_PRESS_EVENT;
const RELEASE: u8 = x11rb::protocol::xproto::BUTTON_RELEASE_EVENT;
const KEY_PRESS: u8 = x11rb::protocol::xproto::KEY_PRESS_EVENT;
const KEY_RELEASE: u8 = x11rb::protocol::xproto::KEY_RELEASE_EVENT;

/// How to reach an X server.
///
/// The agent's display is addressed explicitly rather than through `DISPLAY`
/// and `XAUTHORITY`, because setting environment variables in a running process
/// is unsound once any thread exists — and this process has a D-Bus reactor
/// thread by the time a backend is built.
#[derive(Clone, Debug, Default)]
pub struct DisplayTarget {
    /// `:97`, or `None` for whatever `DISPLAY` names.
    pub display: Option<String>,
    /// The `MIT-MAGIC-COOKIE-1` to present, or `None` to look one up the
    /// ordinary way.
    pub cookie: Option<Vec<u8>>,
}

impl DisplayTarget {
    #[must_use]
    pub const fn host() -> Self {
        Self {
            display: None,
            cookie: None,
        }
    }
}

/// Opens a connection to the X server a target names.
///
/// When the caller supplies credentials there is nothing to discover, so the
/// socket is opened directly and the cookie handed to the setup request rather
/// than looked up through `DISPLAY` and `XAUTHORITY`.
fn connect(target: &DisplayTarget) -> Result<(RustConnection, usize)> {
    let unavailable = |error: String| DesktopError::BackendUnavailable {
        backend: Backend::X11,
        reason: format!("cannot connect to the X server: {error}"),
    };

    let (Some(display), Some(cookie)) = (target.display.as_deref(), target.cookie.as_ref()) else {
        return x11rb::connect(target.display.as_deref())
            .map_err(|error| unavailable(error.to_string()));
    };

    let number: u32 = display
        .trim_start_matches(':')
        .split('.')
        .next()
        .unwrap_or_default()
        .parse()
        .map_err(|_| unavailable(format!("{display} is not a display number")))?;
    let socket = crate::session::display_socket_path(number);
    let stream = std::os::unix::net::UnixStream::connect(&socket)
        .map_err(|error| unavailable(format!("{}: {error}", socket.display())))?;
    let (stream, _) = x11rb::rust_connection::DefaultStream::from_unix_stream(stream)
        .map_err(|error| unavailable(error.to_string()))?;
    let connection = RustConnection::connect_to_stream_with_auth_info(
        stream,
        0,
        b"MIT-MAGIC-COOKIE-1".to_vec(),
        cookie.clone(),
    )
    .map_err(|error| unavailable(error.to_string()))?;
    Ok((connection, 0))
}

/// Raising a window and giving it the keyboard.
///
/// A seam rather than a direct call because only X11 can do it: Wayland has no
/// client-initiated raise, so under a Wayland session there is nothing to
/// implement and the refusal has to reach the caller intact.
pub trait WindowActivator: Send + Sync {
    /// Activates the window matching `pid` and `title`.
    ///
    /// `Ok(false)` means no window matched — distinct from an error, because
    /// the caller can still try the accessibility route.
    fn activate(&self, pid: Option<u32>, title: Option<&str>) -> Result<bool>;
}

/// The EWMH properties a window manager publishes.
///
/// Interned once: an atom lookup is a round trip, and focusing would otherwise
/// cost five of them before doing any work.
#[derive(Clone, Copy, Debug)]
struct Atoms {
    active_window: u32,
    client_list: u32,
    wm_name: u32,
    wm_pid: u32,
    utf8_string: u32,
}

impl Atoms {
    fn intern(connection: &RustConnection) -> Result<Self> {
        let mut cookies = Vec::new();
        for name in [
            "_NET_ACTIVE_WINDOW",
            "_NET_CLIENT_LIST",
            "_NET_WM_NAME",
            "_NET_WM_PID",
            "UTF8_STRING",
        ] {
            cookies.push(
                connection
                    .intern_atom(false, name.as_bytes())
                    .map_err(|error| {
                        DesktopError::backend(format!("cannot intern {name}: {error}"))
                    })?,
            );
        }
        let mut atoms = Vec::new();
        for cookie in cookies {
            atoms.push(
                cookie
                    .reply()
                    .map_err(|error| DesktopError::backend(format!("cannot intern atom: {error}")))?
                    .atom,
            );
        }
        Ok(Self {
            active_window: atoms[0],
            client_list: atoms[1],
            wm_name: atoms[2],
            wm_pid: atoms[3],
            utf8_string: atoms[4],
        })
    }
}

/// Window management through the EWMH protocol every X11 window manager speaks.
pub struct Ewmh {
    connection: RustConnection,
    screen: usize,
    atoms: Atoms,
}

impl Ewmh {
    pub fn connect(target: &DisplayTarget) -> Result<Self> {
        let (connection, screen) = connect(target)?;
        let atoms = Atoms::intern(&connection)?;
        Ok(Self {
            connection,
            screen,
            atoms,
        })
    }

    fn root(&self) -> Result<u32> {
        self.connection
            .setup()
            .roots
            .get(self.screen)
            .map(|screen| screen.root)
            .ok_or_else(|| DesktopError::backend("X server reported no screens"))
    }

    /// The managed windows, in the window manager's order.
    fn client_list(&self) -> Result<Vec<u32>> {
        let root = self.root()?;
        let reply = self
            .connection
            .get_property(
                false,
                root,
                self.atoms.client_list,
                AtomEnum::WINDOW,
                0,
                u32::MAX,
            )
            .map_err(|error| DesktopError::backend(format!("cannot list windows: {error}")))?
            .reply()
            .map_err(|error| DesktopError::backend(format!("cannot list windows: {error}")))?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }

    fn pid(&self, window: u32) -> Option<u32> {
        let reply = self
            .connection
            .get_property(false, window, self.atoms.wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.next()
    }

    /// The window's title, preferring the UTF-8 property over the legacy one.
    fn title(&self, window: u32) -> Option<String> {
        for (property, kind) in [
            (self.atoms.wm_name, self.atoms.utf8_string),
            (u32::from(AtomEnum::WM_NAME), u32::from(AtomEnum::STRING)),
        ] {
            let reply = self
                .connection
                .get_property(false, window, property, kind, 0, u32::MAX)
                .ok()
                .and_then(|cookie| cookie.reply().ok());
            if let Some(reply) = reply
                && !reply.value.is_empty()
            {
                return String::from_utf8(reply.value).ok();
            }
        }
        None
    }

    fn active_window(&self) -> Option<u32> {
        let root = self.root().ok()?;
        let reply = self
            .connection
            .get_property(
                false,
                root,
                self.atoms.active_window,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.next()
    }

    /// Asks the window manager to activate a window, the way a taskbar does.
    ///
    /// Source indication 2 ("pager") is what window managers honour without the
    /// focus-stealing prevention they apply to applications.
    ///
    /// A `_NET_ACTIVE_WINDOW` message rather than `SetInputFocus`: focus and
    /// stacking are the window manager's to decide, and going around it leaves
    /// a window that has the keyboard but is still behind another one.
    fn request_activation(&self, window: u32) -> Result<()> {
        let root = self.root()?;
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms.active_window,
            [2, x11rb::CURRENT_TIME, 0, 0, 0],
        );
        self.connection
            .send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )
            .map_err(|error| DesktopError::backend(format!("cannot activate window: {error}")))?;
        self.connection
            .flush()
            .map_err(|error| DesktopError::backend(format!("cannot activate window: {error}")))
    }
}

impl WindowActivator for Ewmh {
    /// Matches on pid first and title second.
    ///
    /// The pid is exact; the title is what is left when an application does not
    /// publish one, and matching it loosely would activate the wrong window of
    /// the right application.
    ///
    /// The result is verified rather than assumed: a window manager may refuse,
    /// and a focus change that did not happen sends every later keystroke
    /// somewhere the caller did not look.
    fn activate(&self, pid: Option<u32>, title: Option<&str>) -> Result<bool> {
        let windows = self.client_list()?;

        let matched = windows
            .iter()
            .find(|window| pid.is_some() && self.pid(**window) == pid)
            .or_else(|| {
                title.and_then(|title| {
                    windows
                        .iter()
                        .find(|window| self.title(**window).as_deref() == Some(title))
                })
            });
        let Some(&window) = matched else {
            return Ok(false);
        };

        self.request_activation(window)?;

        let deadline = std::time::Instant::now() + ACTIVATION_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if self.active_window() == Some(window) {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(self.active_window() == Some(window))
    }
}

/// How long a window manager gets to honour an activation request.
const ACTIVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

pub struct X11Capture {
    connection: RustConnection,
    screen: usize,
}

impl X11Capture {
    pub fn connect(target: &DisplayTarget) -> Result<Self> {
        let (connection, screen) = connect(target)?;
        Ok(Self { connection, screen })
    }
}

impl CapturePort for X11Capture {
    /// Captures the screen, or one window.
    ///
    /// X11 can read any drawable, so a window is captured directly rather than
    /// cropped out of a full-screen grab.
    fn capture(&self, target: &CaptureTarget) -> Result<Image> {
        let setup = self.connection.setup();
        let screen = setup
            .roots
            .get(self.screen)
            .ok_or_else(|| DesktopError::backend("X server reported no screens"))?;

        let (drawable, width, height, space) = match target {
            CaptureTarget::Screen => (
                screen.root,
                screen.width_in_pixels,
                screen.height_in_pixels,
                CoordinateSpace::primary_screen(),
            ),
            CaptureTarget::Window(id) => {
                let window = id.get();
                let geometry = self
                    .connection
                    .get_geometry(window)
                    .map_err(|error| DesktopError::backend(format!("bad window: {error}")))?
                    .reply()
                    .map_err(|error| DesktopError::backend(format!("bad window: {error}")))?;
                (
                    window,
                    geometry.width,
                    geometry.height,
                    CoordinateSpace::Window(*id),
                )
            }
        };

        let reply = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                0,
                0,
                width,
                height,
                u32::MAX,
            )
            .map_err(|error| DesktopError::backend(format!("capture failed: {error}")))?
            .reply()
            .map_err(|error| DesktopError::backend(format!("capture failed: {error}")))?;

        let pixels = bgrx_to_rgba(&reply.data, u32::from(width), u32::from(height))?;
        Image::new(
            u32::from(width),
            u32::from(height),
            ScaleFactor::ONE,
            space,
            pixels,
        )
        .map_err(|error| DesktopError::backend(error.to_string()))
    }
}

/// X11 `Z_PIXMAP` on a 24/32-bit visual arrives as little-endian BGRX.
fn bgrx_to_rgba(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| DesktopError::backend("image dimensions overflow"))?;
    if data.len() < pixels * 4 {
        return Err(DesktopError::backend(format!(
            "X server returned {} bytes for a {width}x{height} image, expected {}",
            data.len(),
            pixels * 4
        )));
    }

    let mut out = Vec::with_capacity(pixels * 4);
    for chunk in data.chunks_exact(4).take(pixels) {
        out.push(chunk[2]);
        out.push(chunk[1]);
        out.push(chunk[0]);
        out.push(0xff);
    }
    Ok(out)
}

pub struct X11Input {
    connection: RustConnection,
    screen: usize,
    /// Keysym-to-keycode lookups need the server keymap, which is fetched once
    /// and reused; a chord otherwise costs a round trip per key.
    keymap: Mutex<Option<Keymap>>,
}

impl X11Input {
    pub fn connect(target: &DisplayTarget) -> Result<Self> {
        let (connection, screen) = connect(target)?;
        Ok(Self {
            connection,
            screen,
            keymap: Mutex::new(None),
        })
    }

    fn root(&self) -> Result<u32> {
        self.connection
            .setup()
            .roots
            .get(self.screen)
            .map(|screen| screen.root)
            .ok_or_else(|| DesktopError::backend("X server reported no screens"))
    }

    /// Returns only once the X server has *processed* everything sent so far.
    ///
    /// Flushing writes to the socket; it says nothing about when the server
    /// acts on it. X11 orders requests within a connection and makes no promise
    /// at all between connections — and every `desktop` command is a new
    /// process, so a new connection. Without a round-trip here, `desktop key
    /// ctrl+l` can still have its Control release sitting unprocessed when the
    /// next command's keystrokes arrive, and they are delivered as though
    /// Control were held: observed against Firefox, where typing `github.com`
    /// after a chord fired Ctrl+T and Ctrl+I instead of entering a URL.
    ///
    /// `sync` is a request with a reply, so waiting for the reply proves the
    /// input queue ahead of it has drained.
    fn sync(&self) -> Result<()> {
        self.connection
            .sync()
            .map_err(|error| DesktopError::backend(format!("cannot sync with X server: {error}")))
    }

    fn fake_button(&self, button: u8, press: bool) -> Result<()> {
        self.connection
            .xtest_fake_input(
                if press { PRESS } else { RELEASE },
                button,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )
            .map_err(|error| DesktopError::backend(format!("input failed: {error}")))?;
        Ok(())
    }

    fn fake_key(&self, keycode: u8, press: bool) -> Result<()> {
        self.connection
            .xtest_fake_input(
                if press { KEY_PRESS } else { KEY_RELEASE },
                keycode,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )
            .map_err(|error| DesktopError::backend(format!("input failed: {error}")))?;
        Ok(())
    }

    fn warp(&self, point: Point) -> Result<()> {
        let root = self.root()?;
        self.connection
            .xtest_fake_input(
                x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
                0,
                x11rb::CURRENT_TIME,
                root,
                i16::try_from(point.x).unwrap_or(i16::MAX),
                i16::try_from(point.y).unwrap_or(i16::MAX),
                0,
            )
            .map_err(|error| DesktopError::backend(format!("cannot move pointer: {error}")))?;
        Ok(())
    }

    /// Finds the keycode (and whether shift is needed) for a keysym.
    fn lookup(&self, keysym: u32) -> Result<(u8, bool)> {
        let mut guard = self
            .keymap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(Keymap::fetch(&self.connection)?);
        }
        let keymap = guard.as_ref().expect("keymap was just populated");
        keymap.find(keysym).ok_or_else(|| {
            DesktopError::invalid_argument(format!(
                "no key on the current keyboard layout produces keysym {keysym:#x}"
            ))
        })
    }

    /// Presses a chord, releasing modifiers in reverse so the state unwinds
    /// the way a real keyboard would report it.
    fn press_chord(&self, chord: &Chord) -> Result<()> {
        let keysym = keysym_for(chord.key)
            .ok_or_else(|| DesktopError::invalid_argument("this key cannot be sent on X11"))?;
        let (keycode, needs_shift) = self.lookup(keysym)?;

        let mut modifiers = Vec::new();
        let resolved = chord
            .modifiers
            .resolve(desktop_core::models::backend::Platform::Linux);
        if resolved.ctrl {
            modifiers.push(self.lookup(XK_CONTROL_L)?.0);
        }
        if resolved.alt {
            modifiers.push(self.lookup(XK_ALT_L)?.0);
        }
        if resolved.shift || needs_shift {
            modifiers.push(self.lookup(XK_SHIFT_L)?.0);
        }
        if resolved.meta {
            modifiers.push(self.lookup(XK_SUPER_L)?.0);
        }

        for code in &modifiers {
            self.fake_key(*code, true)?;
        }
        self.fake_key(keycode, true)?;
        self.fake_key(keycode, false)?;
        for code in modifiers.iter().rev() {
            self.fake_key(*code, false)?;
        }
        self.sync()
    }
}

impl InputPort for X11Input {
    fn move_mouse(&self, point: Point, _space: &CoordinateSpace) -> Result<()> {
        self.warp(point)?;
        self.sync()
    }

    fn click(
        &self,
        point: Point,
        _space: &CoordinateSpace,
        button: MouseButton,
        count: u8,
    ) -> Result<()> {
        self.warp(point)?;
        let code = match button {
            MouseButton::Left => BUTTON_LEFT,
            MouseButton::Middle => BUTTON_MIDDLE,
            MouseButton::Right => BUTTON_RIGHT,
        };
        for _ in 0..count.max(1) {
            self.fake_button(code, true)?;
            self.fake_button(code, false)?;
        }
        self.sync()
    }

    /// Types literal text, one character at a time.
    ///
    /// Each character is delivered before the next is sent. A whole string
    /// flushed at once arrives faster than a keyboard can produce it, and
    /// applications that rebuild their input on a keystroke drop whatever lands
    /// mid-rebuild — observed with gnome-calculator, which turned `7+3` into
    /// `7`. Losing characters silently is worse than typing at human speed.
    fn type_text(&self, text: &str) -> Result<()> {
        for character in text.chars() {
            let Some(keysym) = keysym_for_char(character) else {
                return Err(DesktopError::invalid_argument(format!(
                    "cannot type {character:?} on the current keyboard layout"
                )));
            };
            let (keycode, needs_shift) = self.lookup(keysym)?;
            let shift = if needs_shift {
                Some(self.lookup(XK_SHIFT_L)?.0)
            } else {
                None
            };
            if let Some(code) = shift {
                self.fake_key(code, true)?;
            }
            self.fake_key(keycode, true)?;
            self.fake_key(keycode, false)?;
            if let Some(code) = shift {
                self.fake_key(code, false)?;
            }

            self.sync()?;
            std::thread::sleep(KEYSTROKE_INTERVAL);
        }
        Ok(())
    }

    fn key(&self, chord: &Chord) -> Result<()> {
        self.press_chord(chord)
    }

    /// Scrolls by a logical distance.
    ///
    /// A negative `y` means "scroll up" in the CLI's sense, matching how a page
    /// moves rather than how a wheel turns.
    fn scroll(&self, delta: ScrollDelta, _space: &CoordinateSpace) -> Result<()> {
        let vertical = wheel_clicks(delta.y);
        let horizontal = wheel_clicks(delta.x);

        let (v_button, v_count) = if delta.y < 0 {
            (WHEEL_UP, vertical)
        } else {
            (WHEEL_DOWN, vertical)
        };
        let (h_button, h_count) = if delta.x < 0 {
            (WHEEL_LEFT, horizontal)
        } else {
            (WHEEL_RIGHT, horizontal)
        };

        for _ in 0..v_count {
            self.fake_button(v_button, true)?;
            self.fake_button(v_button, false)?;
        }
        for _ in 0..h_count {
            self.fake_button(h_button, true)?;
            self.fake_button(h_button, false)?;
        }
        self.sync()
    }
}

/// Converts a logical scroll distance into discrete wheel clicks, rounding a
/// non-zero request up so a small scroll still does something.
fn wheel_clicks(delta: i32) -> u32 {
    let magnitude = delta.unsigned_abs();
    if magnitude == 0 {
        return 0;
    }
    let clicks = magnitude / PIXELS_PER_WHEEL_CLICK as u32;
    clicks.max(1)
}

struct Keymap {
    first_keycode: u8,
    per_code: usize,
    keysyms: Vec<u32>,
}

impl Keymap {
    fn fetch(connection: &RustConnection) -> Result<Self> {
        let setup = connection.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - setup.min_keycode + 1;

        let reply = connection
            .get_keyboard_mapping(first, count)
            .map_err(|error| DesktopError::backend(format!("cannot read keymap: {error}")))?
            .reply()
            .map_err(|error| DesktopError::backend(format!("cannot read keymap: {error}")))?;

        Ok(Self {
            first_keycode: first,
            per_code: reply.keysyms_per_keycode as usize,
            keysyms: reply.keysyms,
        })
    }

    /// Returns the keycode producing `keysym`, and whether shift is required.
    ///
    /// Column 0 is the unshifted symbol and column 1 the shifted one, which is
    /// what makes an uppercase letter reachable without hardcoding a layout.
    fn find(&self, keysym: u32) -> Option<(u8, bool)> {
        if self.per_code == 0 {
            return None;
        }
        for (index, chunk) in self.keysyms.chunks(self.per_code).enumerate() {
            for (column, candidate) in chunk.iter().enumerate() {
                if *candidate == keysym {
                    let keycode = u8::try_from(index).ok()?.checked_add(self.first_keycode)?;
                    return Some((keycode, column % 2 == 1));
                }
            }
        }
        None
    }
}

const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;

/// Maps a normalized key onto an X11 keysym.
fn keysym_for(key: Key) -> Option<u32> {
    match key {
        Key::Char(c) => keysym_for_char(c),
        Key::Named(named) => Some(match named {
            NamedKey::Return => 0xff0d,
            NamedKey::Tab => 0xff09,
            NamedKey::Escape => 0xff1b,
            NamedKey::Space => 0x0020,
            NamedKey::Backspace => 0xff08,
            NamedKey::Delete => 0xffff,
            NamedKey::Insert => 0xff63,
            NamedKey::Up => 0xff52,
            NamedKey::Down => 0xff54,
            NamedKey::Left => 0xff51,
            NamedKey::Right => 0xff53,
            NamedKey::Home => 0xff50,
            NamedKey::End => 0xff57,
            NamedKey::PageUp => 0xff55,
            NamedKey::PageDown => 0xff56,
            NamedKey::Function(n) => 0xffbe + u32::from(n) - 1,
        }),
    }
}

/// Maps a character onto an X11 keysym.
///
/// Latin-1 characters map to their own code point; everything else uses the
/// Unicode keysym range, which X has supported for decades. Control characters
/// have no keysym beyond the few named keys handled explicitly.
fn keysym_for_char(character: char) -> Option<u32> {
    let code = character as u32;
    if (0x20..=0xff).contains(&code) {
        Some(code)
    } else if code >= 0x100 {
        Some(code + 0x0100_0000)
    } else {
        match character {
            '\n' | '\r' => Some(0xff0d),
            '\t' => Some(0xff09),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_characters_map_to_their_own_keysym() {
        assert_eq!(keysym_for_char('a'), Some(0x61));
        assert_eq!(keysym_for_char('A'), Some(0x41));
        assert_eq!(keysym_for_char(' '), Some(0x20));
        assert_eq!(keysym_for_char('~'), Some(0x7e));
    }

    #[test]
    fn non_latin_characters_use_the_unicode_keysym_range() {
        // U+00E9 is Latin-1 so it stays bare; U+4E2D needs the Unicode offset.
        assert_eq!(keysym_for_char('é'), Some(0xe9));
        assert_eq!(keysym_for_char('中'), Some(0x0100_4e2d));
        assert_eq!(keysym_for_char('😀'), Some(0x0101_f600));
    }

    #[test]
    fn newline_and_tab_are_typed_as_the_keys_a_user_would_press() {
        assert_eq!(keysym_for_char('\n'), Some(0xff0d));
        assert_eq!(keysym_for_char('\r'), Some(0xff0d));
        assert_eq!(keysym_for_char('\t'), Some(0xff09));
    }

    #[test]
    fn other_control_characters_are_refused_rather_than_typed_as_garbage() {
        assert_eq!(keysym_for_char('\u{0}'), None);
        assert_eq!(keysym_for_char('\u{7}'), None);
    }

    #[test]
    fn named_keys_map_to_the_standard_keysyms() {
        assert_eq!(keysym_for(Key::Named(NamedKey::Return)), Some(0xff0d));
        assert_eq!(keysym_for(Key::Named(NamedKey::Escape)), Some(0xff1b));
        assert_eq!(keysym_for(Key::Named(NamedKey::Left)), Some(0xff51));
    }

    #[test]
    fn function_keys_are_numbered_from_the_f1_keysym() {
        assert_eq!(keysym_for(Key::Named(NamedKey::Function(1))), Some(0xffbe));
        assert_eq!(keysym_for(Key::Named(NamedKey::Function(4))), Some(0xffc1));
        assert_eq!(keysym_for(Key::Named(NamedKey::Function(12))), Some(0xffc9));
    }

    #[test]
    fn a_keymap_lookup_reports_whether_shift_is_needed() {
        // Two keycodes, two columns each: unshifted then shifted.
        let keymap = Keymap {
            first_keycode: 8,
            per_code: 2,
            keysyms: vec![0x61, 0x41, 0x62, 0x42],
        };
        assert_eq!(keymap.find(0x61), Some((8, false)));
        assert_eq!(keymap.find(0x41), Some((8, true)));
        assert_eq!(keymap.find(0x62), Some((9, false)));
        assert_eq!(keymap.find(0x42), Some((9, true)));
        assert_eq!(keymap.find(0xdead), None);
    }

    #[test]
    fn an_empty_keymap_reports_no_match_instead_of_dividing_by_zero() {
        let keymap = Keymap {
            first_keycode: 8,
            per_code: 0,
            keysyms: Vec::new(),
        };
        assert_eq!(keymap.find(0x61), None);
    }

    #[test]
    fn scroll_distances_become_at_least_one_wheel_click_when_non_zero() {
        assert_eq!(wheel_clicks(0), 0);
        assert_eq!(wheel_clicks(1), 1);
        assert_eq!(wheel_clicks(-1), 1);
        assert_eq!(wheel_clicks(500), 5);
        assert_eq!(wheel_clicks(-500), 5);
        assert_eq!(wheel_clicks(250), 2);
    }

    #[test]
    fn bgrx_pixels_are_reordered_to_rgba_with_an_opaque_alpha() {
        // X11 hands back little-endian BGRX; a naive memcpy would swap red and
        // blue in every screenshot.
        let data = vec![0x10, 0x20, 0x30, 0x00, 0x40, 0x50, 0x60, 0x00];
        let rgba = bgrx_to_rgba(&data, 2, 1).expect("converts");
        assert_eq!(rgba, vec![0x30, 0x20, 0x10, 0xff, 0x60, 0x50, 0x40, 0xff]);
    }

    #[test]
    fn a_short_pixel_buffer_is_reported_rather_than_producing_a_skewed_image() {
        let error = bgrx_to_rgba(&[0, 0, 0, 0], 4, 4).expect_err("must reject");
        assert!(error.to_string().contains("expected"), "got {error}");
    }
}

//! Wayland capture and input, through xdg-desktop-portal.
//!
//! Two things force raw D-Bus here rather than `ashpd`'s typed API.
//!
//! First, absolute pointer positioning requires a *joint* session:
//! `NotifyPointerMotionAbsolute` takes a PipeWire stream id and interprets the
//! coordinates in that stream's logical space, so a RemoteDesktop session must
//! also have ScreenCast sources selected on it. `ashpd` 0.13 implements
//! `IsScreencastSession` only for `Screencast`, and its `Session::path` is
//! crate-private, so the two halves cannot be joined from outside the crate.
//!
//! Second, the restore token has to be read back off the `Start` response and
//! rewritten every run, because the portal rotates it.
//!
//! What this module deliberately does *not* do is consume PipeWire frames.
//! Clicking needs only the stream's id and logical size, which arrive in the
//! `Start` response — so pointer input stays a few D-Bus round trips rather
//! than a video pipeline.

use std::{
    collections::HashMap,
    pin::Pin,
    sync::atomic::{AtomicU32, Ordering},
};

use atspi::zbus::{
    self, Connection,
    export::futures_core::Stream as _,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};
use desktop_core::{
    errors::{DesktopError, Result},
    models::{
        backend::Backend,
        chord::{Chord, Key, NamedKey},
        geometry::{CoordinateSpace, Point, ScaleFactor, ScrollDelta},
        image::Image,
    },
    ports::{CapturePort, CaptureTarget, InputPort, MouseButton},
};

use crate::{
    portal::{TokenKind, TokenStore},
    runtime,
};

const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const SCREENSHOT: &str = "org.freedesktop.portal.Screenshot";
const REQUEST: &str = "org.freedesktop.portal.Request";

/// `SelectDevices` device bitmask: keyboard | pointer.
const DEVICES_KEYBOARD_POINTER: u32 = 1 | 2;
/// `SelectSources` source bitmask: monitor.
const SOURCE_MONITOR: u32 = 1;
/// `PersistMode::ExplicitlyRevoked` — the setting that buys "one dialog, ever".
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// evdev button codes, which is what the portal expects.
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;

/// `NotifyPointerAxis` axis indices.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

/// Unique per request within this process; the portal keys its Request objects
/// on it and a collision would cross two replies.
static TOKEN_COUNTER: AtomicU32 = AtomicU32::new(0);

fn next_token(prefix: &str) -> String {
    let counter = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("desktopdriver_{prefix}_{}_{counter}", std::process::id())
}

/// Turns a unique bus name into the form the portal embeds in object paths.
fn sender_token(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').replace('.', "_")
}

/// Awaits the next item of a signal stream without pulling in `futures_util`.
async fn next_signal(stream: &mut zbus::proxy::SignalStream<'_>) -> Option<zbus::Message> {
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// A PipeWire stream offered by the portal, as reported by `Start`.
#[derive(Clone, Copy, Debug)]
pub struct StreamInfo {
    pub node_id: u32,
    /// Logical size, which is the coordinate space
    /// `NotifyPointerMotionAbsolute` interprets its arguments in.
    pub size: (i32, i32),
}

impl StreamInfo {
    /// Whether a point falls inside this stream's logical area.
    ///
    /// A stream that reported no size is treated as unbounded rather than
    /// empty, so a backend that omits the property does not reject everything.
    #[must_use]
    pub const fn contains(&self, point: Point) -> bool {
        let (width, height) = self.size;
        if width <= 0 || height <= 0 {
            return true;
        }
        point.x >= 0 && point.y >= 0 && point.x < width && point.y < height
    }
}

/// A live RemoteDesktop session with a monitor screencast attached.
pub struct PortalSession {
    connection: Connection,
    session: OwnedObjectPath,
    streams: Vec<StreamInfo>,
}

impl PortalSession {
    /// Opens the joint session, replaying a stored restore token so the
    /// approval dialog appears on first use only.
    ///
    /// Four portal calls, in order:
    ///
    /// 1. `CreateSession`.
    /// 2. `SelectDevices` — keyboard and pointer, remembered until revoked.
    /// 3. `SelectSources` on the *same* session, which is the step that makes
    ///    absolute pointer positioning possible at all.
    /// 4. `Start`, which shows the dialog unless a valid token was replayed.
    ///
    /// `SelectSources` deliberately carries no `persist_mode`: on a joint
    /// session GNOME rejects it outright — "Remote desktop sessions cannot
    /// persist" — and fails the whole call. Persistence is negotiated once, on
    /// `SelectDevices`, and covers the session as a whole.
    ///
    /// The token returned by `Start` is written back, because the portal issues
    /// a *new* one every time and invalidates the old; not storing it silently
    /// reintroduces the dialog on the next run.
    pub async fn open(tokens: &TokenStore) -> Result<Self> {
        let connection =
            Connection::session()
                .await
                .map_err(|error| DesktopError::BackendUnavailable {
                    backend: Backend::RemoteDesktopPortal,
                    reason: format!("cannot reach the session bus: {error}"),
                })?;

        let unique = connection
            .unique_name()
            .map(|name| sender_token(name.as_str()))
            .ok_or_else(|| DesktopError::backend("D-Bus connection has no unique name"))?;

        let session_token = next_token("session");
        let session_path =
            OwnedObjectPath::try_from(format!("{PORTAL_PATH}/session/{unique}/{session_token}"))
                .map_err(|error| DesktopError::internal(format!("bad session path: {error}")))?;

        let mut options = HashMap::new();
        options.insert("session_handle_token", Value::from(session_token.clone()));
        Self::request(
            &connection,
            &unique,
            REMOTE_DESKTOP,
            "CreateSession",
            Leading::None,
            options,
        )
        .await?;

        let stored = tokens.load(TokenKind::ScreenInput);

        let mut options = HashMap::new();
        options.insert("types", Value::from(DEVICES_KEYBOARD_POINTER));
        options.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
        if let Some(token) = &stored {
            options.insert("restore_token", Value::from(token.clone()));
        }
        Self::request(
            &connection,
            &unique,
            REMOTE_DESKTOP,
            "SelectDevices",
            Leading::Session(session_path.clone()),
            options,
        )
        .await?;

        let mut options = HashMap::new();
        options.insert("types", Value::from(SOURCE_MONITOR));
        options.insert("multiple", Value::from(false));
        Self::request(
            &connection,
            &unique,
            SCREEN_CAST,
            "SelectSources",
            Leading::Session(session_path.clone()),
            options,
        )
        .await?;

        let results = Self::request(
            &connection,
            &unique,
            REMOTE_DESKTOP,
            "Start",
            Leading::SessionAndParent(session_path.clone(), ""),
            HashMap::new(),
        )
        .await?;

        if let Some(token) = results.get("restore_token").and_then(value_as_string) {
            tokens.store(TokenKind::ScreenInput, &token)?;
        }

        let streams = results
            .get("streams")
            .map(parse_streams)
            .unwrap_or_default();

        Ok(Self {
            connection,
            session: session_path,
            streams,
        })
    }

    /// Issues a portal request and waits for its `Response` signal.
    ///
    /// The signal subscription is established *before* the method call,
    /// because the portal is free to answer immediately and a reply that
    /// arrives first would otherwise be missed and the call would hang.
    ///
    /// Response code 0 succeeded. 1 means the user dismissed the dialog and 2
    /// means the interaction ended some other way — most often the dialog
    /// timing out unanswered. Both are "the grant was not completed", which is
    /// something the user can fix rather than a backend fault.
    async fn request(
        connection: &Connection,
        sender: &str,
        interface: &str,
        method: &str,
        leading: Leading<'_>,
        mut options: HashMap<&str, Value<'_>>,
    ) -> Result<HashMap<String, OwnedValue>> {
        let handle_token = next_token("request");
        let request_path = format!("{PORTAL_PATH}/request/{sender}/{handle_token}");
        options.insert("handle_token", Value::from(handle_token));

        let request_proxy = zbus::Proxy::new(
            connection,
            PORTAL_BUS,
            ObjectPath::try_from(request_path.clone())
                .map_err(|error| DesktopError::internal(format!("bad request path: {error}")))?,
            REQUEST,
        )
        .await
        .map_err(|error| DesktopError::backend(format!("portal unreachable: {error}")))?;

        let mut responses = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|error| DesktopError::backend(format!("cannot await portal: {error}")))?;

        let proxy = zbus::Proxy::new(connection, PORTAL_BUS, PORTAL_PATH, interface)
            .await
            .map_err(|error| DesktopError::backend(format!("portal unreachable: {error}")))?;

        leading.call(&proxy, method, &options).await?;

        let message = next_signal(&mut responses)
            .await
            .ok_or_else(|| DesktopError::backend("the portal closed without responding"))?;

        let (code, results): (u32, HashMap<String, OwnedValue>) = message
            .body()
            .deserialize()
            .map_err(|error| DesktopError::backend(format!("malformed portal reply: {error}")))?;

        match code {
            0 => Ok(results),
            1 | 2 => Err(DesktopError::SetupRequired {
                permission: permission_for(interface),
            }),
            other => Err(DesktopError::backend(format!(
                "the portal refused {interface}.{method} (response {other})"
            ))),
        }
    }

    /// The stream absolute coordinates are interpreted against.
    ///
    /// Without one the portal has no coordinate space, and a relative-only
    /// fallback would move the pointer from wherever it happens to be — which
    /// is not a position anybody knows.
    fn stream(&self) -> Result<StreamInfo> {
        self.streams.first().copied().ok_or_else(|| {
            DesktopError::backend(
                "the portal session carries no screencast stream, so absolute pointer \
                 positioning is unavailable",
            )
        })
    }

    async fn proxy(&self) -> Result<zbus::Proxy<'_>> {
        zbus::Proxy::new(&self.connection, PORTAL_BUS, PORTAL_PATH, REMOTE_DESKTOP)
            .await
            .map_err(|error| DesktopError::backend(format!("portal unreachable: {error}")))
    }

    /// Moves the pointer to an absolute position in the stream's space.
    ///
    /// A position outside the stream is refused rather than sent: the portal
    /// discards it silently, which would look like a click that did nothing.
    pub async fn move_pointer(&self, point: Point) -> Result<()> {
        let stream = self.stream()?;
        if !stream.contains(point) {
            return Err(DesktopError::invalid_argument(format!(
                "({}, {}) is outside the captured area ({}x{})",
                point.x, point.y, stream.size.0, stream.size.1
            )));
        }
        let proxy = self.proxy().await?;
        proxy
            .call_method(
                "NotifyPointerMotionAbsolute",
                &(
                    &self.session,
                    empty_options(),
                    stream.node_id,
                    f64::from(point.x),
                    f64::from(point.y),
                ),
            )
            .await
            .map(|_| ())
            .map_err(|error| DesktopError::backend(format!("cannot move the pointer: {error}")))
    }

    pub async fn button(&self, button: MouseButton, pressed: bool) -> Result<()> {
        let code = match button {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Right => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
        };
        let proxy = self.proxy().await?;
        proxy
            .call_method(
                "NotifyPointerButton",
                &(
                    &self.session,
                    empty_options(),
                    code,
                    if pressed { KEY_PRESSED } else { KEY_RELEASED },
                ),
            )
            .await
            .map(|_| ())
            .map_err(|error| DesktopError::backend(format!("cannot click: {error}")))
    }

    pub async fn keysym(&self, keysym: i32, pressed: bool) -> Result<()> {
        let proxy = self.proxy().await?;
        proxy
            .call_method(
                "NotifyKeyboardKeysym",
                &(
                    &self.session,
                    empty_options(),
                    keysym,
                    if pressed { KEY_PRESSED } else { KEY_RELEASED },
                ),
            )
            .await
            .map(|_| ())
            .map_err(|error| DesktopError::backend(format!("cannot send key: {error}")))
    }

    pub async fn axis(&self, axis: u32, steps: i32) -> Result<()> {
        let proxy = self.proxy().await?;
        proxy
            .call_method(
                "NotifyPointerAxisDiscrete",
                &(&self.session, empty_options(), axis, steps),
            )
            .await
            .map(|_| ())
            .map_err(|error| DesktopError::backend(format!("cannot scroll: {error}")))
    }
}

/// The empty `a{sv}` every `Notify*` method takes as its options argument.
fn empty_options() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

/// The fixed argument shapes portal methods take before their options map.
///
/// These must be sent with their real D-Bus types: wrapping an object path or
/// a string in a `Value` sends a *variant*, and the portal rejects the call
/// with a signature mismatch.
enum Leading<'a> {
    /// `(a{sv})` — CreateSession.
    None,
    /// `(s, a{sv})` — Screenshot, where the string is the parent window.
    Parent(&'a str),
    /// `(o, a{sv})` — SelectDevices, SelectSources.
    Session(OwnedObjectPath),
    /// `(o, s, a{sv})` — Start.
    SessionAndParent(OwnedObjectPath, &'a str),
}

impl Leading<'_> {
    async fn call(
        &self,
        proxy: &zbus::Proxy<'_>,
        method: &str,
        options: &HashMap<&str, Value<'_>>,
    ) -> Result<()> {
        let outcome = match self {
            Self::None => proxy.call_method(method, &(options,)).await,
            Self::Parent(parent) => proxy.call_method(method, &(*parent, options)).await,
            Self::Session(session) => proxy.call_method(method, &(session, options)).await,
            Self::SessionAndParent(session, parent) => {
                proxy
                    .call_method(method, &(session, *parent, options))
                    .await
            }
        };
        outcome
            .map(|_| ())
            .map_err(|error| DesktopError::backend(format!("{method} failed: {error}")))
    }
}

/// Which grant the user is being asked for, so the error names the right one.
fn permission_for(interface: &str) -> desktop_core::errors::Permission {
    use desktop_core::errors::Permission;
    match interface {
        SCREENSHOT | SCREEN_CAST => Permission::ScreenCastPortal,
        _ => Permission::RemoteDesktopPortal,
    }
}

fn value_as_string(value: &OwnedValue) -> Option<String> {
    <&str>::try_from(value).ok().map(str::to_owned)
}

/// Decodes the `a(ua{sv})` stream array from a `Start` response.
fn parse_streams(value: &OwnedValue) -> Vec<StreamInfo> {
    let Ok(array) = <Vec<(u32, HashMap<String, OwnedValue>)>>::try_from(value.clone()) else {
        return Vec::new();
    };
    array
        .into_iter()
        .map(|(node_id, properties)| StreamInfo {
            node_id,
            size: properties
                .get("size")
                .and_then(|v| <(i32, i32)>::try_from(v.clone()).ok())
                .unwrap_or((0, 0)),
        })
        .collect()
}

/// Input through the RemoteDesktop portal.
///
/// The session is opened lazily on first use, so `desktop info` and friends
/// never trigger a permission dialog.
pub struct PortalInput {
    tokens: TokenStore,
}

impl PortalInput {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tokens: TokenStore::at_default_path(),
        }
    }

    fn with_session<T>(&self, action: impl AsyncFnOnce(&PortalSession) -> Result<T>) -> Result<T> {
        runtime::try_block_on(async {
            let session = PortalSession::open(&self.tokens).await?;
            action(&session).await
        })?
    }
}

impl Default for PortalInput {
    fn default() -> Self {
        Self::new()
    }
}

impl InputPort for PortalInput {
    fn move_mouse(&self, point: Point, _space: &CoordinateSpace) -> Result<()> {
        self.with_session(async |session| session.move_pointer(point).await)
    }

    fn click(
        &self,
        point: Point,
        _space: &CoordinateSpace,
        button: MouseButton,
        count: u8,
    ) -> Result<()> {
        self.with_session(async |session| {
            session.move_pointer(point).await?;
            for _ in 0..count.max(1) {
                session.button(button, true).await?;
                session.button(button, false).await?;
            }
            Ok(())
        })
    }

    /// Types literal text.
    ///
    /// Sent as keysyms rather than keycodes: the portal accepts them directly,
    /// which sidesteps the whole keyboard-layout mapping problem that the X11
    /// and macOS backends have to solve.
    fn type_text(&self, text: &str) -> Result<()> {
        let keysyms: Vec<i32> = text
            .chars()
            .map(|character| {
                keysym_for_char(character).ok_or_else(|| {
                    DesktopError::invalid_argument(format!("cannot type {character:?}"))
                })
            })
            .collect::<Result<Vec<u32>>>()?
            .into_iter()
            .map(|value| value as i32)
            .collect();

        self.with_session(async |session| {
            for keysym in &keysyms {
                session.keysym(*keysym, true).await?;
                session.keysym(*keysym, false).await?;
            }
            Ok(())
        })
    }

    fn key(&self, chord: &Chord) -> Result<()> {
        let resolved = chord.resolve(desktop_core::models::backend::Platform::Linux);
        let key = keysym_for(chord.key)
            .ok_or_else(|| DesktopError::invalid_argument("this key cannot be sent"))?
            as i32;

        let mut modifiers = Vec::new();
        if resolved.modifiers.ctrl {
            modifiers.push(XK_CONTROL_L as i32);
        }
        if resolved.modifiers.alt {
            modifiers.push(XK_ALT_L as i32);
        }
        if resolved.modifiers.shift {
            modifiers.push(XK_SHIFT_L as i32);
        }
        if resolved.modifiers.meta {
            modifiers.push(XK_SUPER_L as i32);
        }

        self.with_session(async |session| {
            for keysym in &modifiers {
                session.keysym(*keysym, true).await?;
            }
            session.keysym(key, true).await?;
            session.keysym(key, false).await?;
            for keysym in modifiers.iter().rev() {
                session.keysym(*keysym, false).await?;
            }
            Ok(())
        })
    }

    fn scroll(&self, delta: ScrollDelta, _space: &CoordinateSpace) -> Result<()> {
        let vertical = wheel_steps(delta.y);
        let horizontal = wheel_steps(delta.x);
        self.with_session(async |session| {
            if vertical != 0 {
                session.axis(AXIS_VERTICAL, vertical).await?;
            }
            if horizontal != 0 {
                session.axis(AXIS_HORIZONTAL, horizontal).await?;
            }
            Ok(())
        })
    }
}

/// Converts a logical scroll distance into discrete wheel steps, preserving
/// sign and never rounding a real request down to nothing.
fn wheel_steps(delta: i32) -> i32 {
    const PIXELS_PER_STEP: i32 = 100;
    if delta == 0 {
        return 0;
    }
    let steps = delta / PIXELS_PER_STEP;
    if steps == 0 { delta.signum() } else { steps }
}

/// Full-screen capture through the Screenshot portal.
///
/// The Screenshot portal returns a `file://` URI rather than pixels, and offers
/// no way to name a window: its `target` option reached version 3 of the spec
/// but neither the GNOME nor the KDE backend implements it. Window capture
/// therefore refuses rather than quietly returning the whole screen.
pub struct PortalCapture {
    info: desktop_core::models::backend::BackendInfo,
}

impl PortalCapture {
    #[must_use]
    pub const fn new(info: desktop_core::models::backend::BackendInfo) -> Self {
        Self { info }
    }
}

impl CapturePort for PortalCapture {
    /// Captures the screen through the Screenshot portal.
    ///
    /// The file the portal produced is removed afterwards: it writes into the
    /// user's Pictures directory or a temporary file, and leaving a copy behind
    /// on every screenshot would be a surprise.
    fn capture(&self, target: &CaptureTarget) -> Result<Image> {
        if matches!(target, CaptureTarget::Window(_) | CaptureTarget::App(_)) {
            return Err(DesktopError::unsupported(
                desktop_core::models::capability::Capability::WindowScreenshots,
                self.info.screenshot,
                &self.info,
            ));
        }

        let uri = runtime::try_block_on(screenshot_uri())??;
        let path = uri
            .strip_prefix("file://")
            .map(percent_decode)
            .ok_or_else(|| {
                DesktopError::backend(format!("the portal returned an unusable URI: {uri}"))
            })?;

        let decoded = image::open(&path)
            .map_err(|error| DesktopError::backend(format!("cannot read the capture: {error}")))?
            .to_rgba8();
        let (width, height) = decoded.dimensions();

        let _ = std::fs::remove_file(&path);

        Image::new(
            width,
            height,
            ScaleFactor::ONE,
            CoordinateSpace::primary_screen(),
            decoded.into_raw(),
        )
        .map_err(|error| DesktopError::backend(error.to_string()))
    }
}

async fn screenshot_uri() -> Result<String> {
    let connection = Connection::session()
        .await
        .map_err(|error| DesktopError::backend(format!("cannot reach the session bus: {error}")))?;
    let unique = connection
        .unique_name()
        .map(|name| sender_token(name.as_str()))
        .ok_or_else(|| DesktopError::backend("D-Bus connection has no unique name"))?;

    let mut options = HashMap::new();
    options.insert("interactive", Value::from(false));
    options.insert("modal", Value::from(false));

    let results = PortalSession::request(
        &connection,
        &unique,
        SCREENSHOT,
        "Screenshot",
        Leading::Parent(""),
        options,
    )
    .await?;

    results
        .get("uri")
        .and_then(value_as_string)
        .ok_or_else(|| DesktopError::backend("the portal returned no screenshot URI"))
}

/// Minimal percent-decoding for the `file://` URI the portal returns.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;

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
    fn a_unique_bus_name_becomes_the_token_the_portal_embeds_in_paths() {
        // The portal builds /…/request/<sender>/<handle>, where <sender> is the
        // unique name with the colon stripped and dots replaced.
        assert_eq!(sender_token(":1.234"), "1_234");
        assert_eq!(sender_token("1.234"), "1_234");
        assert_eq!(sender_token(":1.2.3"), "1_2_3");
    }

    #[test]
    fn request_tokens_are_unique_so_two_replies_cannot_be_crossed() {
        let first = next_token("request");
        let second = next_token("request");
        assert_ne!(first, second);
        assert!(first.starts_with("desktopdriver_request_"), "got {first}");
    }

    #[test]
    fn generated_tokens_contain_only_characters_valid_in_a_dbus_path() {
        let token = next_token("session");
        assert!(
            token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "got {token}"
        );
    }

    #[test]
    fn scroll_distances_preserve_direction_and_never_round_down_to_nothing() {
        assert_eq!(wheel_steps(0), 0);
        assert_eq!(wheel_steps(500), 5);
        assert_eq!(wheel_steps(-500), -5);
        // A small request still scrolls, in the direction asked for.
        assert_eq!(wheel_steps(10), 1);
        assert_eq!(wheel_steps(-10), -1);
    }

    #[test]
    fn percent_escapes_in_the_portal_uri_are_decoded() {
        assert_eq!(
            percent_decode("/home/u/Pictures/Screenshot%20from%202026.png"),
            "/home/u/Pictures/Screenshot from 2026.png"
        );
        assert_eq!(percent_decode("/tmp/plain.png"), "/tmp/plain.png");
    }

    #[test]
    fn a_trailing_percent_sign_does_not_panic() {
        assert_eq!(percent_decode("/tmp/odd%"), "/tmp/odd%");
        assert_eq!(percent_decode("/tmp/odd%2"), "/tmp/odd%2");
    }

    #[test]
    fn characters_map_to_the_same_keysyms_the_x11_backend_uses() {
        // Both Linux backends must agree, or the same `desktop type` would
        // produce different text under X11 and Wayland.
        assert_eq!(keysym_for_char('a'), Some(0x61));
        assert_eq!(keysym_for_char('中'), Some(0x0100_4e2d));
        assert_eq!(keysym_for(Key::Named(NamedKey::Return)), Some(0xff0d));
        assert_eq!(keysym_for(Key::Named(NamedKey::Function(4))), Some(0xffc1));
    }

    #[test]
    fn a_point_outside_the_captured_area_is_rejected_rather_than_silently_dropped() {
        let stream = StreamInfo {
            node_id: 1,
            size: (1920, 1080),
        };
        assert!(stream.contains(Point::new(0, 0)));
        assert!(stream.contains(Point::new(1919, 1079)));
        assert!(!stream.contains(Point::new(1920, 500)));
        assert!(!stream.contains(Point::new(500, 1080)));
        assert!(!stream.contains(Point::new(-1, 5)));
    }

    #[test]
    fn a_stream_that_reports_no_size_is_treated_as_unbounded() {
        // Rejecting everything would be worse than trusting the caller when
        // the backend omits the property.
        let stream = StreamInfo {
            node_id: 1,
            size: (0, 0),
        };
        assert!(stream.contains(Point::new(4000, 4000)));
    }

    #[test]
    fn evdev_button_codes_are_what_the_portal_expects() {
        assert_eq!(BTN_LEFT, 272);
        assert_eq!(BTN_RIGHT, 273);
        assert_eq!(BTN_MIDDLE, 274);
    }

    #[test]
    fn persistence_is_requested_until_explicitly_revoked() {
        // Anything less means the approval dialog returns on the next run.
        assert_eq!(PERSIST_UNTIL_REVOKED, 2);
    }

    #[test]
    fn window_capture_refuses_instead_of_returning_the_whole_screen() {
        use desktop_core::models::backend::{
            BackendInfo, DesktopEnvironment, DisplayServer, Platform,
        };
        let capture = PortalCapture::new(BackendInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            desktop_environment: DesktopEnvironment::Gnome,
            accessibility: Backend::AtSpi,
            windows: Backend::AtSpi,
            screenshot: Backend::XdgDesktopPortal,
            input: Backend::RemoteDesktopPortal,
        });
        let error = capture
            .capture(&CaptureTarget::Window(
                desktop_core::models::ids::WindowId::new(1),
            ))
            .expect_err("must refuse");
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "unsupported_capability");
        assert_eq!(json["capability"], "window_screenshots");
    }
}

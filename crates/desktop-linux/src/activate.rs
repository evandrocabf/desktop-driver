//! Raising a window by asking the application that owns it.
//!
//! Wayland has no client-initiated raise, and GNOME's own window APIs are not
//! open to us: `org.gnome.Shell.Introspect` and `org.gnome.Shell.Screenshot`
//! both check the caller against an allowlist holding the two portal
//! implementations and nothing else. There is no compositor-level lever here at
//! all.
//!
//! What is left is the application. A program built on GApplication exports
//! `org.freedesktop.Application`, whose `Activate` asks it to present itself —
//! and an application presenting its own window is a request the compositor
//! treats quite differently from a stranger demanding focus.
//!
//! The application is found by its *interface* rather than by its name. The
//! obvious route is the desktop entry specification's: an application id is a
//! well-known bus name, and the object path is that id with the dots turned
//! into slashes. Probed against gnome-calculator on a private session bus, that
//! route finds nothing — the process holds a connection, exports
//! `org.freedesktop.Application` at `/org/gnome/Calculator`, and owns no
//! well-known name at all. Activating it works; deriving its path from a name
//! it does not have does not. So the connection is located by pid and its
//! object tree walked for the interface.
//!
//! Two things this is not. It is not a raise: mutter may answer a present by
//! marking the window as demanding attention instead, and the caller finds out
//! only by looking afterwards, which is why every path through here is verified
//! by the accessibility layer rather than trusted. And it does not reach
//! everything: `xterm` and anything else not built on a toolkit that speaks
//! D-Bus exports no such interface, and is reported as a focus that did not
//! happen rather than as one that did.

use std::collections::VecDeque;

use atspi::zbus;
use desktop_core::errors::{DesktopError, Result};

use crate::runtime;

/// How long the whole lookup-and-ask sequence may take.
///
/// A focus that has not happened within this is a failure the caller needs to
/// hear about, and an application blocking its own activation must not wedge
/// the command that asked.
const ACTIVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The interface an application exports to be activated through.
const APPLICATION: &str = "org.freedesktop.Application";

/// How many objects of one connection are examined before giving up.
///
/// A GApplication publishes its interface within three or four levels of `/`.
/// Anything that has not appeared by here is a tree being walked in hope, and
/// the pointer is not moving while that happens.
const MAX_OBJECTS: usize = 64;

/// Asks the application running as `pid` to present its window.
///
/// `Ok(false)` means there was nothing to ask: no connection belongs to that
/// process, or none of them exports [`APPLICATION`]. That is not an error — the
/// caller has its own refusal to report — so it is kept distinct from a call
/// that was made and failed.
///
/// Nothing here proves the window was raised. Only that the application was
/// asked.
pub fn present_application(pid: i32) -> Result<bool> {
    runtime::try_block_on(async move {
        match tokio::time::timeout(ACTIVATION_TIMEOUT, present(pid)).await {
            Ok(outcome) => outcome,
            Err(_) => Ok(false),
        }
    })?
}

async fn present(pid: i32) -> Result<bool> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| DesktopError::backend(format!("cannot reach the session bus: {error}")))?;
    let bus = zbus::fdo::DBusProxy::new(&connection)
        .await
        .map_err(|error| DesktopError::backend(format!("cannot query the session bus: {error}")))?;

    for name in connections_of(&bus, pid).await {
        let Some(path) = application_object(&connection, &name).await else {
            continue;
        };
        let proxy =
            match zbus::Proxy::new(&connection, name.as_str(), path.as_str(), APPLICATION).await {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };
        let platform_data: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
            std::collections::HashMap::new();
        if proxy
            .call_method("Activate", &(platform_data,))
            .await
            .is_ok()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Every bus name belonging to `pid`, unique names included.
///
/// Unique names are the point rather than an afterthought: an application that
/// never acquired its well-known name still holds a connection, and that
/// connection is where its activation interface lives.
async fn connections_of(bus: &zbus::fdo::DBusProxy<'_>, pid: i32) -> Vec<String> {
    let Ok(pid) = u32::try_from(pid) else {
        return Vec::new();
    };
    let Ok(names) = bus.list_names().await else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for name in names {
        let name = name.as_str();
        if name.starts_with("org.freedesktop.DBus") {
            continue;
        }
        let Ok(owned) = zbus::names::BusName::try_from(name.to_owned()) else {
            continue;
        };
        if bus.get_connection_unix_process_id(owned).await == Ok(pid) {
            out.push(name.to_owned());
        }
    }
    out
}

/// The object on `name` that exports the activation interface.
///
/// Breadth-first from `/`, because the interface sits shallow and the
/// interesting part of the tree is the top of it. Anything unreadable is
/// skipped rather than failing the search: a connection is free to refuse
/// introspection on one object and answer on the next.
async fn application_object(connection: &zbus::Connection, name: &str) -> Option<String> {
    let mut queue = VecDeque::from(["/".to_owned()]);
    let mut examined = 0;

    while let Some(path) = queue.pop_front() {
        examined += 1;
        if examined > MAX_OBJECTS {
            return None;
        }

        let Some(xml) = introspect(connection, name, &path).await else {
            continue;
        };
        if declares_application(&xml) {
            return Some(path);
        }
        queue.extend(child_paths(&xml, &path));
    }
    None
}

async fn introspect(connection: &zbus::Connection, name: &str, path: &str) -> Option<String> {
    let proxy = zbus::fdo::IntrospectableProxy::builder(connection)
        .destination(name.to_owned())
        .ok()?
        .path(path.to_owned())
        .ok()?
        .build()
        .await
        .ok()?;
    proxy.introspect().await.ok()
}

/// Whether an introspection document declares the activation interface.
fn declares_application(xml: &str) -> bool {
    xml.contains(&format!("\"{APPLICATION}\""))
}

/// The child object names an introspection document lists.
///
/// Hand-parsed rather than through an XML crate: the shape is fixed by the
/// D-Bus specification and this needs one attribute out of one element, which
/// is not worth a dependency that would have to be audited for a tool that
/// links no C libraries.
fn child_nodes(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find("<node name=\"") {
        rest = &rest[start + "<node name=\"".len()..];
        let Some(end) = rest.find('"') else { break };
        let name = &rest[..end];
        if !name.is_empty() && !name.contains('/') {
            out.push(name.to_owned());
        }
        rest = &rest[end..];
    }
    out
}

/// The full paths of an object's children, ready to be walked.
///
/// The root is the case worth naming: its children are `/org`, not `//org`,
/// and a doubled separator is not the same object path.
fn child_paths(xml: &str, parent: &str) -> Vec<String> {
    child_nodes(xml)
        .into_iter()
        .map(|child| {
            if parent == "/" {
                format!("/{child}")
            } else {
                format!("{parent}/{child}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asks a real application to present itself, which is the only part of
    /// this that a parser cannot check.
    ///
    /// `#[ignore]`d because it needs a session bus with an application on it.
    /// Give it one of its own rather than the desktop's:
    ///
    /// ```text
    /// desktop session start --headless
    /// eval "$(desktop session env)"
    /// desktop session run gnome-calculator
    /// DESKTOP_DRIVER_ACTIVATE_PID=$(pgrep -f gnome-calculator) \
    ///   cargo test -p desktop-linux -- --ignored
    /// ```
    ///
    /// Naming the pid rather than discovering it keeps the test about
    /// activation: finding the process is the caller's job, and on a busy
    /// machine guessing at it would activate somebody else's window.
    #[test]
    #[ignore = "needs a running application on the session bus"]
    fn a_running_application_can_be_asked_to_present_itself() {
        let Ok(pid) = std::env::var("DESKTOP_DRIVER_ACTIVATE_PID") else {
            return;
        };
        let pid: i32 = pid.trim().parse().expect("the pid must be a number");
        assert!(
            present_application(pid).expect("the session bus must be reachable"),
            "no connection of pid {pid} exported {APPLICATION}"
        );
    }

    /// The other half of the same claim: a process with no D-Bus presence is
    /// reported as unreachable rather than as activated.
    #[test]
    #[ignore = "needs a session bus"]
    fn a_process_that_is_not_on_the_bus_is_reported_as_not_asked() {
        if std::env::var("DESKTOP_DRIVER_ACTIVATE_PID").is_err() {
            return;
        }
        assert!(
            !present_application(1).expect("the session bus must be reachable"),
            "pid 1 does not speak D-Bus"
        );
    }

    /// The document gnome-calculator answers with at `/`, trimmed to shape.
    const ROOT: &str = r#"
<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"><arg name="xml" type="s" direction="out"/></method>
  </interface>
  <node name="org"/>
</node>"#;

    const LEAF: &str = r#"
<node>
  <interface name="org.gtk.Actions"/>
  <interface name="org.gtk.Application"/>
  <interface name="org.freedesktop.Application">
    <method name="Activate"><arg type="a{sv}" name="platform_data" direction="in"/></method>
  </interface>
  <node name="window"/>
</node>"#;

    #[test]
    fn the_activation_interface_is_recognised_where_it_is_declared() {
        assert!(declares_application(LEAF));
        assert!(!declares_application(ROOT));
    }

    /// `org.gtk.Application` is a different interface with the same tail, and
    /// matching loosely would call `Activate` with the wrong signature.
    #[test]
    fn a_similarly_named_interface_is_not_mistaken_for_it() {
        let gtk_only = r#"<node><interface name="org.gtk.Application"/></node>"#;
        assert!(!declares_application(gtk_only));
    }

    #[test]
    fn child_objects_are_read_out_of_the_document() {
        assert_eq!(child_nodes(ROOT), vec!["org".to_owned()]);
        assert_eq!(child_nodes(LEAF), vec!["window".to_owned()]);
        assert!(child_nodes("<node/>").is_empty());
    }

    /// An absolute child name would be joined into a path that names something
    /// else entirely.
    #[test]
    fn a_child_name_containing_a_separator_is_ignored() {
        assert!(child_nodes(r#"<node><node name="a/b"/></node>"#).is_empty());
    }

    /// The root is the case that matters: `//org` is not the same object path
    /// as `/org`, and the walk would introspect nothing from there on.
    #[test]
    fn child_paths_are_absolute_and_the_root_does_not_double_its_separator() {
        assert_eq!(child_paths(ROOT, "/"), vec!["/org".to_owned()]);
        assert_eq!(
            child_paths(LEAF, "/org/gnome/Calculator"),
            vec!["/org/gnome/Calculator/window".to_owned()]
        );
        assert!(child_paths("<node/>", "/").is_empty());
    }

    /// The walk as it runs against gnome-calculator: three documents deep, the
    /// interface at the leaf, ending on the path `Activate` is sent to.
    #[test]
    fn the_walk_ends_on_the_object_that_declares_the_interface() {
        let tree = [
            ("/", ROOT),
            ("/org", r#"<node><node name="gnome"/></node>"#),
            ("/org/gnome", r#"<node><node name="Calculator"/></node>"#),
            ("/org/gnome/Calculator", LEAF),
        ];
        let fetch = |path: &str| {
            tree.iter()
                .find(|(known, _)| *known == path)
                .map(|(_, xml)| *xml)
        };

        let mut queue = VecDeque::from(["/".to_owned()]);
        let mut found = None;
        while let Some(path) = queue.pop_front() {
            let Some(xml) = fetch(&path) else { continue };
            if declares_application(xml) {
                found = Some(path);
                break;
            }
            queue.extend(child_paths(xml, &path));
        }
        assert_eq!(found.as_deref(), Some("/org/gnome/Calculator"));
    }
}

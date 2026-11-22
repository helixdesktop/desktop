//! What X11 apps write into properties, and what the window manager
//! publishes: the edge cases of `tests/x11_cases.md` about malformed and
//! hostile properties, transient cycles, `_NET_WM_STATE` messages, the
//! window manager's identity and `_XSETTINGS_SETTINGS` (#191).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::Duration;

use common::xwm::{self, until, X};
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, Window};

const MAXIMIZED: [&str; 2] = ["_NET_WM_STATE_MAXIMIZED_HORZ", "_NET_WM_STATE_MAXIMIZED_VERT"];

/// Write every property the window manager reads, wrong in every way.
fn write_garbage(x: &X, window: Window, other: Window) {
    let (wm_hints, atom, string, cardinal) = (u32::from(AtomEnum::WM_HINTS), u32::from(AtomEnum::ATOM), u32::from(AtomEnum::STRING), u32::from(AtomEnum::CARDINAL));
    x.set32(window, "WM_HINTS", wm_hints, &[1, 0]);
    x.set32(window, "WM_HINTS", atom, &[1]);
    x.set32(window, "WM_NORMAL_HINTS", AtomEnum::WM_SIZE_HINTS, &[0xffff_ffff, 1, 2]);
    x.set8(window, "WM_CLASS", string, b"no-terminator");
    x.set8(window, "WM_NAME", string, &[0xff, 0xfe, 0, 0xe9]);
    x.set8(window, "_NET_WM_NAME", x.atom("UTF8_STRING"), &[0xc3, 0x28, 0xa0, 0xa1]);
    x.set8(window, "_NET_WM_PID", cardinal, &[1, 2]);
    x.set8(window, "_NET_WM_WINDOW_TYPE", string, b"_NET_WM_WINDOW_TYPE_DIALOG");
    x.set8(window, "WM_TRANSIENT_FOR", AtomEnum::WINDOW, &[1]);
    x.set32(window, "WM_TRANSIENT_FOR", AtomEnum::WINDOW, &[other, other, 0]);
    x.set8(window, "WM_PROTOCOLS", atom, b"WM_DELETE_WINDOW");
    x.set32(window, "_NET_WM_STATE", atom, &vec![x.atom("_NET_WM_STATE_ABOVE"); 2000]);
    // A 1000×1000 icon with 3 pixels, an overflowing size, an empty one.
    x.set32(window, "_NET_WM_ICON", cardinal, &[1000, 1000, 1, 2, 3]);
    x.set32(window, "_NET_WM_ICON", cardinal, &[0xffff_ffff, 0xffff_ffff, 1]);
    x.set32(window, "_NET_WM_ICON", cardinal, &[0, 0]);
    x.set32(window, "_MOTIF_WM_HINTS", x.atom("_MOTIF_WM_HINTS"), &[2]);
    x.set32(window, "_GTK_FRAME_EXTENTS", cardinal, &[10, 10]);
    x.set32(window, "_NET_WM_WINDOW_OPACITY", cardinal, &[]);
    x.set8(window, "_NET_WM_SYNC_REQUEST_COUNTER", cardinal, &[9]);
}

/// X45: properties too short, of the wrong format or type, unterminated,
/// huge or overflowing, before and after mapping: the window is managed
/// and the window manager keeps working.
#[test]
fn malformed_properties_do_no_harm() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let other = x.mapped();
    let window = x.window();
    write_garbage(&x, window, other);
    x.map(window);
    until("the window with malformed properties is not managed", || x.managed(window));
    write_garbage(&x, window, other);
    for property in ["WM_HINTS", "WM_NORMAL_HINTS", "WM_CLASS", "WM_NAME", "_NET_WM_NAME", "WM_TRANSIENT_FOR", "WM_PROTOCOLS", "_NET_WM_ICON", "_MOTIF_WM_HINTS"] {
        x.delete(window, property);
    }
    std::thread::sleep(Duration::from_millis(300));
    xwm::wm_still_works(&session);
}

/// X46: transient-for cycles (A for B, B for A, C for itself): no hang,
/// all managed, all can take focus.
#[test]
fn transient_cycles_do_no_harm() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let (a, b, c) = (x.window(), x.window(), x.window());
    x.set32(a, "WM_TRANSIENT_FOR", AtomEnum::WINDOW, &[b]);
    x.set32(b, "WM_TRANSIENT_FOR", AtomEnum::WINDOW, &[a]);
    x.set32(c, "WM_TRANSIENT_FOR", AtomEnum::WINDOW, &[c]);
    for w in [a, b, c] {
        x.map_managed(w);
    }
    for w in [a, b, c] {
        x.focus(w);
    }
    xwm::wm_still_works(&session);
}

/// X47: changing the window type after mapping leaves `_NET_WM_STATE`
/// alone.
#[test]
fn a_window_type_change_leaves_the_state_alone() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_ABOVE"]);
    until("Always on Top was not set", || x.has_state(window, "_NET_WM_STATE_ABOVE"));
    x.set_atoms(window, "_NET_WM_WINDOW_TYPE", &["_NET_WM_WINDOW_TYPE_DIALOG"]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(x.has_state(window, "_NET_WM_STATE_ABOVE"), "the type change dropped Always on Top");
}

/// X48: the window manager says who it is (`_NET_SUPPORTING_WM_CHECK`
/// and `_NET_WM_NAME`); Java tells reparenting window managers from
/// others by it.
#[test]
fn the_window_manager_identifies_itself() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let check = x.get32(x.root, "_NET_SUPPORTING_WM_CHECK").and_then(|v| v.first().copied()).expect("no _NET_SUPPORTING_WM_CHECK on the root");
    assert_eq!(x.get32(check, "_NET_SUPPORTING_WM_CHECK").and_then(|v| v.first().copied()), Some(check), "the check window does not point at itself");
    let (kind, name) = x.get8(check, "_NET_WM_NAME").expect("the check window has no _NET_WM_NAME");
    assert_eq!(kind, x.atom("UTF8_STRING"));
    assert!(!name.is_empty() && std::str::from_utf8(&name).is_ok(), "the window manager's name: {name:?}");
}

/// X49: the window manager and compositing manager selections are owned
/// (apps use ARGB visuals only with a compositing manager).
#[test]
fn the_manager_selections_are_owned() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    for selection in ["WM_S0", "_NET_WM_CM_S0"] {
        let owner = x.conn.get_selection_owner(x.atom(selection)).unwrap().reply().unwrap().owner;
        assert_ne!(owner, x11rb::NONE, "nobody owns {selection}");
    }
}

/// X50: `_NET_SUPPORTED` lists the states and requests the window manager
/// handles (some apps check before asking).
#[test]
fn net_supported_lists_what_is_handled() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let supported = x.get32(x.root, "_NET_SUPPORTED").expect("no _NET_SUPPORTED");
    let missing: Vec<&str> = [
        "_NET_WM_STATE",
        "_NET_WM_STATE_FULLSCREEN",
        "_NET_WM_STATE_MAXIMIZED_HORZ",
        "_NET_WM_STATE_MAXIMIZED_VERT",
        "_NET_WM_STATE_HIDDEN",
        "_NET_WM_STATE_ABOVE",
        "_NET_WM_STATE_MODAL",
        "_NET_WM_STATE_SKIP_TASKBAR",
        "_NET_WM_STATE_FOCUSED",
        "_NET_WM_STATE_DEMANDS_ATTENTION",
        "_NET_ACTIVE_WINDOW",
        "_NET_WM_MOVERESIZE",
        "_NET_CLIENT_LIST",
        "_NET_CLIENT_LIST_STACKING",
        "_NET_WM_PING",
    ]
    .into_iter()
    .filter(|a| !supported.contains(&x.atom(a)))
    .collect();
    assert!(missing.is_empty(), "_NET_SUPPORTED lacks {missing:?}");
}

/// X51: one `_NET_WM_STATE` message carries two states (maximize does,
/// in either order) and toggles by the current state; a message of the
/// wrong format is ignored.
#[test]
fn state_messages_with_two_states() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    std::thread::sleep(Duration::from_millis(300));
    let restored = x.size(window);
    x.net_wm_state(window, 2, &[MAXIMIZED[1], MAXIMIZED[0]]);
    until("toggling both maximized states did not maximize", || x.size(window).0 == xwm::SCREEN.0);
    assert!(x.has_state(window, MAXIMIZED[0]) && x.has_state(window, MAXIMIZED[1]), "_NET_WM_STATE: {:?}", x.net_states(window));
    x.net_wm_state(window, 2, &MAXIMIZED);
    until("toggling again did not restore", || x.size(window) == restored);
    let bad = ClientMessageEvent::new(8, window, x.atom("_NET_WM_STATE"), [1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    x.conn.send_event(false, x.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, bad).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(x.size(window), restored, "a format-8 message changed the window");
    xwm::wm_still_works(&session);
}

/// X52: after mapping, `_NET_WM_STATE` is the window manager's: an app
/// writing it directly instead of asking changes nothing.
#[test]
fn writing_the_state_directly_after_mapping_changes_nothing() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    std::thread::sleep(Duration::from_millis(300));
    let size = x.size(window);
    x.set_atoms(window, "_NET_WM_STATE", &["_NET_WM_STATE_FULLSCREEN"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(x.size(window), size, "writing _NET_WM_STATE made the window fullscreen");
}

/// X53: `_XSETTINGS_SETTINGS` reads back whole with an independent
/// decoder: strings padded to 4 bytes, every setting after them intact
/// (GTK misread everything after the first string).
#[test]
fn xsettings_read_back_whole() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let owner = x.conn.get_selection_owner(x.atom("_XSETTINGS_S0")).unwrap().reply().unwrap().owner;
    assert_ne!(owner, x11rb::NONE, "nobody owns _XSETTINGS_S0");
    let mut data = Vec::new();
    until("no _XSETTINGS_SETTINGS yet", || {
        data = x.get8(owner, "_XSETTINGS_SETTINGS").map(|(_, d)| d).unwrap_or_default();
        !data.is_empty()
    });
    let settings = decode_xsettings(&data).unwrap_or_else(|e| panic!("_XSETTINGS_SETTINGS does not decode: {e}"));
    assert!(settings.iter().any(|(_, s)| matches!(s, Setting::Text(_))), "no string settings: {settings:?}");
    assert!(settings.iter().any(|(n, _)| n == "Net/ThemeName" || n == "Gtk/CursorThemeName"), "no theme settings: {settings:?}");
}

#[derive(Debug)]
#[allow(dead_code)]
enum Setting {
    Int(i32),
    Text(String),
    Color([u16; 4]),
}

/// The XSETTINGS format (freedesktop's xsettings spec), read strictly.
fn decode_xsettings(data: &[u8]) -> Result<Vec<(String, Setting)>, String> {
    let little = match data.first() {
        Some(0) => true,
        Some(1) => false,
        other => return Err(format!("byte order {other:?}")),
    };
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], String> {
        let bytes = data.get(at..at + n).ok_or_else(|| format!("{n} bytes past {at} of {}", data.len()))?;
        at += n;
        Ok(bytes)
    };
    let u16_ = |b: &[u8]| if little { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
    let u32_ = |b: &[u8]| if little { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) };
    let pad = |n: usize| (4 - n % 4) % 4;
    take(4)?;
    let _serial = u32_(take(4)?);
    let count = u32_(take(4)?);
    let mut settings = Vec::new();
    for _ in 0..count {
        let head = take(4)?;
        let (kind, name_len) = (head[0], usize::from(u16_(&head[2..4])));
        let name = String::from_utf8(take(name_len)?.to_vec()).map_err(|e| e.to_string())?;
        take(pad(name_len))?;
        take(4)?;
        let value = match kind {
            0 => Setting::Int(u32_(take(4)?) as i32),
            1 => {
                let len = u32_(take(4)?) as usize;
                let text = String::from_utf8(take(len)?.to_vec()).map_err(|e| format!("{name}: {e}"))?;
                take(pad(len))?;
                Setting::Text(text)
            }
            2 => {
                let b = take(8)?;
                Setting::Color([u16_(&b[0..2]), u16_(&b[2..4]), u16_(&b[4..6]), u16_(&b[6..8])])
            }
            other => return Err(format!("{name}: type {other}")),
        };
        settings.push((name, value));
    }
    if at != data.len() {
        return Err(format!("{} bytes left over", data.len() - at));
    }
    Ok(settings)
}

/// X98 (W43): `_MOTIF_WM_HINTS` saying the app decorates itself (Steam,
/// Chromium): Helix draws no title bar for it, so maximized it gets the
/// whole screen; without the hints it gets one (and the screen below it).
#[test]
fn motif_hints_without_decorations_get_no_title_bar() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let plain = x.window();
    let undecorated = x.window();
    let motif = x.atom("_MOTIF_WM_HINTS");
    // flags: decorations given; decorations: none.
    x.set32(undecorated, "_MOTIF_WM_HINTS", motif, &[2, 0, 0, 0, 0]);
    for window in [plain, undecorated] {
        x.map_managed(window);
        x.net_wm_state(window, 1, &MAXIMIZED);
        until("the window was not maximized", || x.size(window).0 == xwm::SCREEN.0);
    }
    std::thread::sleep(Duration::from_millis(300));
    let (plain_at, undecorated_at) = (x.position(plain), x.position(undecorated));
    assert_eq!((undecorated_at, x.size(undecorated)), ((0, 0), xwm::SCREEN), "the undecorated window maximized");
    assert!(plain_at.1 > 0, "the decorated window has no room for its title bar: at {plain_at:?}");
    assert_eq!(i32::from(x.size(plain).1) + i32::from(plain_at.1), i32::from(xwm::SCREEN.1), "the decorated window maximized under its title bar");
}

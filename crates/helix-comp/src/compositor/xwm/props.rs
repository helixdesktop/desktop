//! Reading what clients say about their windows (ICCCM 4.1.2, EWMH).
//! Every property is checked for its type and length: any client can
//! set any bytes.

use x11rb::connection::SequenceNumber;
use x11rb::errors::ConnectionError;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, GetPropertyReply};
use x11rb::rust_connection::RustConnection;

use super::atoms::Atoms;
use super::window::XWin;

/// The properties read when a window is managed or shown, and again when
/// one changes.
fn watched(a: &Atoms) -> [u32; 12] {
    [
        a.WM_NAME,
        a._NET_WM_NAME,
        a.WM_CLASS,
        a.WM_HINTS,
        a.WM_NORMAL_HINTS,
        a.WM_TRANSIENT_FOR,
        a.WM_PROTOCOLS,
        a._NET_WM_PID,
        a._NET_WM_WINDOW_TYPE,
        a._MOTIF_WM_HINTS,
        a._GTK_FRAME_EXTENTS,
        a._NET_WM_SYNC_REQUEST_COUNTER,
    ]
}

/// Asks for all of them, plus the EWMH states the client set before
/// mapping (`_NET_WM_STATE`), at once: which property each request is.
pub fn request_all(conn: &RustConnection, a: &Atoms, id: u32) -> Result<Vec<(u32, SequenceNumber)>, ConnectionError> {
    watched(a).into_iter().chain([a._NET_WM_STATE]).map(|atom| Ok((atom, super::keep(conn.get_property(false, id, atom, AtomEnum::ANY, 0, 4096)?)))).collect()
}

/// The answers to [`request_all`]. A failed one: the window went away
/// meanwhile, and the rest are left.
pub fn apply_all(a: &Atoms, win: &XWin, atoms: &[u32], replies: impl IntoIterator<Item = Option<Vec<u8>>>) {
    for (atom, reply) in atoms.iter().zip(replies) {
        let Some(reply) = super::parse::<GetPropertyReply>(reply) else { return };
        apply(a, win, *atom, Some(&reply));
    }
}

/// Asks for one property that changed, if it is one Helix reads.
pub fn request(conn: &RustConnection, a: &Atoms, id: u32, atom: u32) -> Result<Option<SequenceNumber>, ConnectionError> {
    if !watched(a).contains(&atom) {
        return Ok(None);
    }
    Ok(Some(super::keep(conn.get_property(false, id, atom, AtomEnum::ANY, 0, 4096)?)))
}

/// The answer to [`request`].
pub fn apply_one(a: &Atoms, win: &XWin, atom: u32, reply: Option<Vec<u8>>) {
    if let Some(reply) = super::parse::<GetPropertyReply>(reply) {
        apply(a, win, atom, Some(&reply));
    }
}

fn cardinals(reply: Option<&GetPropertyReply>) -> Vec<u32> {
    reply.filter(|r| r.format == 32).and_then(|r| r.value32()).map(|v| v.collect()).unwrap_or_default()
}

fn text(reply: Option<&GetPropertyReply>) -> Option<String> {
    let reply = reply.filter(|r| r.format == 8 && r.type_ != u32::from(AtomEnum::NONE))?;
    // UTF8_STRING, or Latin-1 STRING (and COMPOUND_TEXT read as such).
    Some(match String::from_utf8(reply.value.clone()) {
        Ok(s) => s,
        Err(_) => reply.value.iter().map(|b| char::from(*b)).collect(),
    })
}

fn apply(a: &Atoms, win: &XWin, atom: u32, reply: Option<&GetPropertyReply>) {
    let mut s = win.state();
    match atom {
        x if x == a._NET_WM_NAME => s.net_name = text(reply).filter(|_| reply.is_some_and(|r| r.type_ == a.UTF8_STRING)),
        x if x == a.WM_NAME => s.wm_name = text(reply).unwrap_or_default(),
        x if x == a.WM_CLASS => {
            let raw = reply.filter(|r| r.format == 8).map(|r| r.value.clone()).unwrap_or_default();
            let mut parts = raw.split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).into_owned());
            s.instance = parts.next().unwrap_or_default();
            s.class = parts.next().unwrap_or_default();
        }
        x if x == a.WM_HINTS => {
            let v = cardinals(reply);
            s.hints_set = !v.is_empty();
            let flags = v.first().copied().unwrap_or(0);
            // No WM_HINTS, or no input flag: it takes input.
            s.hints.input = if flags & 1 != 0 { v.get(1).is_none_or(|i| *i != 0) } else { true };
            s.hints.iconic = flags & 2 != 0 && v.get(2) == Some(&3);
            s.hints.urgent = flags & 256 != 0;
        }
        x if x == a.WM_NORMAL_HINTS => {
            let v = cardinals(reply);
            let flags = v.first().copied().unwrap_or(0);
            let pair = |i: usize| match (v.get(i), v.get(i + 1)) {
                (Some(w), Some(h)) => Some((*w as i32, *h as i32)),
                _ => None,
            };
            // The base size stands in for a minimum when none is given
            // (ICCCM 4.1.2.3).
            s.hints.min = if flags & 16 != 0 { pair(5) } else if flags & 256 != 0 { pair(15) } else { None };
            s.hints.min = s.hints.min.filter(|(w, h)| *w > 0 || *h > 0);
            s.hints.max = if flags & 32 != 0 { pair(7) } else { None };
            s.hints.max = s.hints.max.filter(|(w, h)| *w > 0 || *h > 0);
        }
        x if x == a.WM_TRANSIENT_FOR => {
            s.transient_for = cardinals(reply).first().copied().filter(|w| *w != x11rb::NONE && *w != win.window_id());
        }
        x if x == a.WM_PROTOCOLS => {
            let v = cardinals(reply);
            s.delete_window = v.contains(&a.WM_DELETE_WINDOW);
            s.take_focus = v.contains(&a.WM_TAKE_FOCUS);
            s.sync.request = v.contains(&a._NET_WM_SYNC_REQUEST);
        }
        x if x == a._NET_WM_SYNC_REQUEST_COUNTER => s.sync.counter = cardinals(reply).first().copied().filter(|c| *c != x11rb::NONE),
        x if x == a._NET_WM_PID => s.pid = cardinals(reply).first().copied().filter(|p| *p > 0),
        x if x == a._NET_WM_WINDOW_TYPE => s.window_type = cardinals(reply),
        x if x == a._MOTIF_WM_HINTS => {
            let v = cardinals(reply);
            // flags, functions, decorations: decorations given and none.
            s.client_decorated = v.first().is_some_and(|f| f & 2 != 0) && v.get(2) == Some(&0);
        }
        x if x == a._GTK_FRAME_EXTENTS => {
            let v = cardinals(reply);
            s.frame_extents = if v.len() >= 4 { [v[0] as i32, v[1] as i32, v[2] as i32, v[3] as i32].map(|e| e.clamp(0, 1000)) } else { [0; 4] };
        }
        x if x == a._NET_WM_STATE => {
            s.net_state = cardinals(reply).into_iter().filter(|a| *a != x11rb::NONE).collect();
        }
        _ => {}
    }
}

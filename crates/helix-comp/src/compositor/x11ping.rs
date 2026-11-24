//! `_NET_WM_PING` and `_NET_WM_ICON` for X11 apps (helixdesktop/spec
//! `contracts/wayland/README.md`, "Unresponsive apps" and "Window
//! switching"). Smithay's window manager does not pass these on, so Helix
//! keeps a connection of its own to Xwayland: it sends the pings, reads
//! icons, and a thread watches for ping answers and icon changes.
//!
//! It never waits for an X reply on the compositor's thread (#249): this
//! is an ordinary X client, so one holding the X server (XGrabServer)
//! would stop the whole desktop. What needs a reply (whether a window
//! takes pings, its app's pid, its icon) is asked on a thread of its own,
//! and the answer comes back as an [`X11Note`].

use std::sync::Arc;

use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::x11rb::connection::Connection;
use smithay::reexports::x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, EventMask, PropMode, Window,
};
use smithay::reexports::x11rb::protocol::Event;
use smithay::reexports::x11rb::rust_connection::RustConnection;
use smithay::reexports::x11rb::wrapper::ConnectionExt as _;

pub struct X11Ping {
    conn: Arc<RustConnection>,
    wm_protocols: Atom,
    net_wm_ping: Atom,
    net_wm_state: Atom,
    net_wm_state_hidden: Atom,
    /// Questions for the asking thread.
    asks: std::sync::mpsc::Sender<Ask>,
}

/// What the asking thread finds out.
enum Ask {
    /// Whether the window takes pings, and its app's pid.
    Ping(Window),
    /// Its icon (and, with `watch`, follow its changes from now on).
    Icon(Window, bool),
    /// Its startup notification id.
    StartupId(Window),
}

/// What the watching and asking threads report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X11Note {
    /// Whether a window takes pings (`WM_PROTOCOLS` lists `_NET_WM_PING`)
    /// and its app: its X11 client (resource id base) and `_NET_WM_PID`
    /// (0 if unset), so a client slot reused by another app is not taken
    /// for the old one ([`X11Ping::ask_ping`]).
    PingInfo { window: Window, takes_pings: bool, client: (u32, u32) },
    /// A window's icon, read ([`X11Ping::ask_icon`]).
    Icon(Window, Option<X11Icon>),
    /// A window answered a ping.
    Pong(Window),
    /// A window's `_NET_WM_ICON` changed.
    IconChanged(Window),
    /// An app rang the bell (XKB), for a window (0: none).
    Bell(Window),
    /// A window asked (or no longer asks) to be allowed keyboard grabs
    /// (`_XWAYLAND_MAY_GRAB_KEYBOARD`).
    MayGrab(Window, bool),
    /// `_NET_WM_STATE` asks to change `_NET_WM_STATE_DEMANDS_ATTENTION`
    /// (window, action: 0 remove, 1 add, 2 toggle).
    Attention(Window, u32),
    /// A window's startup notification id (`_NET_STARTUP_ID`), read
    /// ([`X11Ping::ask_startup_id`]).
    StartupId(Window, Option<String>),
}

/// An icon: premultiplied ARGB8888 (native byte order) and its size.
pub type X11Icon = (Vec<u8>, u32);

/// The best image of a `_NET_WM_ICON` value (width, height, then
/// width×height straight-alpha ARGB pixels, repeated): the largest square
/// one, else the largest, centered on a transparent square.
pub fn pick_icon(values: &[u32]) -> Option<X11Icon> {
    let mut images: Vec<(usize, usize, &[u32])> = Vec::new();
    let mut rest = values;
    while let [w, h, tail @ ..] = rest {
        let (w, h) = (*w as usize, *h as usize);
        let n = w.checked_mul(h)?;
        if n == 0 || tail.len() < n {
            break;
        }
        if w <= 512 && h <= 512 {
            images.push((w, h, &tail[..n]));
        }
        rest = &tail[n..];
    }
    let (w, h, pixels) = images
        .iter()
        .filter(|(w, h, _)| w == h)
        .max_by_key(|(w, _, _)| *w)
        .or_else(|| images.iter().max_by_key(|(w, h, _)| w * h))
        .copied()?;
    let size = w.max(h);
    let (dx, dy) = ((size - w) / 2, (size - h) / 2);
    let mut out = vec![0u8; size * size * 4];
    for y in 0..h {
        for x in 0..w {
            let p = pixels[y * w + x];
            let a = p >> 24;
            let m = |c: u32| (c * a + 127) / 255;
            let premul = (a << 24) | (m((p >> 16) & 0xff) << 16) | (m((p >> 8) & 0xff) << 8) | m(p & 0xff);
            let at = ((y + dy) * size + x + dx) * 4;
            out[at..at + 4].copy_from_slice(&premul.to_ne_bytes());
        }
    }
    Some((out, size as u32))
}

/// A `_NET_STARTUP_ID` value (UTF-8 text; any client can set any bytes).
fn startup_id(format: u8, value: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(value).ok().filter(|_| format == 8)?;
    let text = text.trim_end_matches('\0');
    (!text.is_empty() && !text.contains('\0')).then(|| text.to_owned())
}

impl X11Ping {
    /// Connect to Xwayland `:display`; answers (their window) go to `answers`.
    pub fn start(display: u32, answers: Sender<X11Note>) -> Result<Self, String> {
        let (conn, screen) = helix_comp::xauth::connect(display)?;
        let root = conn.setup().roots[screen].root;
        let atom = |name: &str| -> Result<Atom, String> {
            Ok(conn.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
        };
        let (wm_protocols, net_wm_ping, net_wm_icon) = (atom("WM_PROTOCOLS")?, atom("_NET_WM_PING")?, atom("_NET_WM_ICON")?);
        let may_grab = atom("_XWAYLAND_MAY_GRAB_KEYBOARD")?;
        let net_wm_pid = atom("_NET_WM_PID")?;
        let (net_startup_id, client_leader) = (atom("_NET_STARTUP_ID")?, atom("WM_CLIENT_LEADER")?);
        let net_wm_state_hidden = atom("_NET_WM_STATE_HIDDEN")?;
        let (net_wm_state, demands_attention) = (atom("_NET_WM_STATE")?, atom("_NET_WM_STATE_DEMANDS_ATTENTION")?);
        // Answers are sent to the root window with the substructure masks.
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY))
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;
        // X11 bells come as XKB bell notifications.
        let bells = (|| -> Result<(), String> {
            use smithay::reexports::x11rb::protocol::xkb::{self, ConnectionExt as _};
            conn.xkb_use_extension(1, 0).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
            conn.xkb_select_events(
                xkb::ID::USE_CORE_KBD.into(),
                xkb::EventType::from(0u16),
                xkb::EventType::BELL_NOTIFY,
                xkb::MapPart::from(0u16),
                xkb::MapPart::from(0u16),
                &xkb::SelectEventsAux::new(),
            )
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())
        })();
        if let Err(err) = bells {
            tracing::info!(err, "no X11 bells");
        }
        let conn = Arc::new(conn);
        let (asks, asked) = std::sync::mpsc::channel::<Ask>();
        {
            let conn = conn.clone();
            let answers = answers.clone();
            let base_mask = !conn.setup().resource_id_mask;
            std::thread::Builder::new()
                .name("helix-x11-ask".into())
                .spawn(move || {
                    let property = |window: Window, atom: Atom, kind: AtomEnum, len: u32| conn.get_property(false, window, atom, kind, 0, len).ok().and_then(|c| c.reply().ok());
                    while let Ok(ask) = asked.recv() {
                        let note = match ask {
                            Ask::Ping(window) => {
                                let takes_pings = property(window, wm_protocols, AtomEnum::ATOM, 32).and_then(|r| r.value32().map(|mut v| v.any(|a| a == net_wm_ping))).unwrap_or(false);
                                let pid = property(window, net_wm_pid, AtomEnum::CARDINAL, 1).and_then(|r| r.value32().and_then(|mut v| v.next())).unwrap_or(0);
                                X11Note::PingInfo { window, takes_pings, client: (window & base_mask, pid) }
                            }
                            Ask::Icon(window, watch) => {
                                if watch {
                                    let _ = conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE));
                                }
                                let icon = property(window, net_wm_icon, AtomEnum::CARDINAL, 4 * 1024 * 1024).and_then(|r| r.value32().map(|v| v.collect::<Vec<u32>>())).and_then(|v| pick_icon(&v));
                                X11Note::Icon(window, icon)
                            }
                            Ask::StartupId(window) => {
                                // The window's own, else its client leader's
                                // (GTK 3 sets it there for windows it
                                // presents for a later launch).
                                let id = |w: Window| property(w, net_startup_id, AtomEnum::ANY, 256).and_then(|r| startup_id(r.format, &r.value));
                                let leader = || property(window, client_leader, AtomEnum::WINDOW, 1).and_then(|r| r.value32().and_then(|mut v| v.next())).filter(|l| *l != smithay::reexports::x11rb::NONE && *l != window);
                                X11Note::StartupId(window, id(window).or_else(|| leader().and_then(id)))
                            }
                        };
                        if answers.send(note).is_err() {
                            return;
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        let reader = conn.clone();
        std::thread::Builder::new()
            .name("helix-x11-ping".into())
            .spawn(move || {
                while let Ok(event) = reader.wait_for_event() {
                    let note = match event {
                        Event::ClientMessage(msg) if msg.type_ == may_grab => {
                            let data = msg.data.as_data32();
                            Some(X11Note::MayGrab(msg.window, data[0] != 0))
                        }
                        Event::ClientMessage(msg) if msg.type_ == net_wm_state => {
                            let data = msg.data.as_data32();
                            (data[1] == demands_attention || data[2] == demands_attention).then_some(X11Note::Attention(msg.window, data[0]))
                        }
                        Event::ClientMessage(msg) => {
                            let data = msg.data.as_data32();
                            (msg.type_ == wm_protocols && data[0] == net_wm_ping).then_some(X11Note::Pong(data[2]))
                        }
                        Event::PropertyNotify(p) if p.atom == net_wm_icon => Some(X11Note::IconChanged(p.window)),
                        Event::XkbBellNotify(bell) => Some(X11Note::Bell(bell.window)),
                        _ => None,
                    };
                    if let Some(note) = note {
                        if answers.send(note).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { conn, wm_protocols, net_wm_ping, net_wm_state, net_wm_state_hidden, asks })
    }

    /// Find out whether `window` takes pings, and its app: answered with
    /// [`X11Note::PingInfo`].
    pub fn ask_ping(&self, window: Window) {
        let _ = self.asks.send(Ask::Ping(window));
    }

    /// Ping `window` (one that takes pings, from [`X11Note::PingInfo`]).
    pub fn ping(&self, window: Window) -> bool {
        let event = ClientMessageEvent::new(32, window, self.wm_protocols, [self.net_wm_ping, 0, window, 0, 0]);
        let sent = self.conn.send_event(false, window, EventMask::NO_EVENT, event).is_ok();
        let _ = self.conn.flush();
        sent
    }

    /// Read `window`'s icon (and with `watch`, follow its changes):
    /// answered with [`X11Note::Icon`].
    pub fn ask_icon(&self, window: Window, watch: bool) {
        let _ = self.asks.send(Ask::Icon(window, watch));
    }

    /// Read `window`'s startup notification id: answered with
    /// [`X11Note::StartupId`].
    pub fn ask_startup_id(&self, window: Window) {
        let _ = self.asks.send(Ask::StartupId(window));
    }

    /// Add `_NET_WM_STATE_HIDDEN` to `window`'s `_NET_WM_STATE` (appended,
    /// so nothing waits on a reply). For Smithay's window manager, which
    /// takes a window starting Iconic as hidden but writes the property
    /// only when its state changes (#288).
    pub fn add_hidden_state(&self, window: Window) {
        let _ = self.conn.change_property32(PropMode::APPEND, window, self.net_wm_state, AtomEnum::ATOM, &[self.net_wm_state_hidden]);
        let _ = self.conn.flush();
    }

    /// Force Quit: close the app's connection to the display.
    pub fn kill(&self, window: Window) {
        let _ = self.conn.kill_client(window);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_square_icon() {
        // A 2x1 (not square), a 1x1 and a 2x2.
        let mut v = vec![2, 1, 0xFFFF0000, 0xFFFF0000, 1, 1, 0xFF00FF00, 2, 2];
        v.extend([0x80FFFFFF, 0xFF0000FF, 0, 0xFF000000]);
        let (pixels, size) = pick_icon(&v).unwrap();
        assert_eq!(size, 2);
        // Half-transparent white premultiplied: 0x80808080.
        assert_eq!(u32::from_ne_bytes(pixels[0..4].try_into().unwrap()), 0x80808080);
        assert_eq!(u32::from_ne_bytes(pixels[4..8].try_into().unwrap()), 0xFF0000FF);
        assert!(pick_icon(&[3, 3, 1]).is_none(), "truncated");
        // Only a 2x3 image: centered on a 3x3 square (columns of padding
        // are whole pixels, so it starts at column 0).
        let (pixels, size) = pick_icon(&[2, 3, 0xFF0000FF, 0xFF00FF00, 0xFF0000FF, 0xFF00FF00, 0xFF0000FF, 0xFF00FF00]).unwrap();
        assert_eq!(size, 3);
        assert_eq!(u32::from_ne_bytes(pixels[0..4].try_into().unwrap()), 0xFF0000FF);
        assert_eq!(u32::from_ne_bytes(pixels[8..12].try_into().unwrap()), 0, "padding on the right");
        // A square one wins over a larger non-square one.
        let mut v = vec![4, 2];
        v.extend([0xFFFFFFFF; 8]);
        v.extend([1, 1, 0xFF000000]);
        assert_eq!(pick_icon(&v).unwrap().1, 1);
        assert!(pick_icon(&[]).is_none());
    }

    #[test]
    fn startup_ids() {
        assert_eq!(startup_id(8, b"abc"), Some("abc".into()));
        assert_eq!(startup_id(8, b"abc\0"), Some("abc".into()), "a terminating NUL");
        assert_eq!(startup_id(8, b""), None);
        assert_eq!(startup_id(32, b"abcd"), None, "not text");
        assert_eq!(startup_id(8, b"a\0b"), None);
        assert_eq!(startup_id(8, &[0xff, 0xfe]), None, "not UTF-8");
    }
}

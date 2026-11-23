//! Drag and drop between X11 and Wayland apps (XDND, version 5) for
//! Helix's own window manager (helixdesktop/desktop#191, step 5).
//!
//! An X11 app drags (it owns XdndSelection while a button is held): Helix
//! carries the drag in its seat's drag-and-drop grab with a source of the
//! X11 app's ([`XSource`]). While the pointer is over a Wayland window the
//! window manager's drop window covers the screen: XDND-aware, named (for
//! Chromium), above everything, so the X11 app sends its XDND messages
//! there, and they become the Wayland drag's offer and data. Over an X11
//! window it steps aside, and the two X11 apps drag between themselves.
//!
//! A Wayland app drags over an X11 window: the window manager owns
//! XdndSelection (from its `owner` window) and sends the window XDND
//! messages, through its `XdndProxy` when that names itself, and serves the
//! data of each type asked for from the Wayland source.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use smithay::input::dnd::{DndAction, OfferData, Source, SourceMetadata};
use smithay::utils::{IsAlive, Logical, Point};
use x11rb::connection::Connection;
use x11rb::errors::ReplyOrIdError;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    StackMode, Window as XId, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::selection::{DndServe, Selections};
use super::{Atoms, XWin};

/// The XDND version spoken.
const VERSION: u32 = 5;

/// What an X11 app's drag source asks of the window manager, from the
/// seat's grab (through the compositor's event loop).
#[derive(Debug)]
pub enum DragCmd {
    /// The Wayland target accepted (or no longer does), or chose an
    /// action: tell the X11 app.
    Status,
    /// A Wayland app takes the drop as this type, into this pipe.
    Fetch(String, OwnedFd),
    /// The drag is over: dropped and taken (`true`), or not.
    Finish(bool),
}

/// An X11 app's drag as the Wayland side sees it.
#[derive(Debug, Default)]
pub struct XDrag {
    /// The types the X11 app offers, as Wayland types, with the atom each
    /// is asked for by.
    pub types: Vec<(String, u32)>,
    /// The action the X11 app asks for.
    pub offered: DndAction,
    /// The Wayland target takes one of the types, with this action.
    pub accepted: bool,
    pub action: DndAction,
    pub ended: bool,
}

/// The source of an X11 app's drag, in Helix's drag-and-drop grab.
pub struct XSource {
    shared: Arc<Mutex<XDrag>>,
    tx: Mutex<smithay::reexports::calloop::channel::Sender<DragCmd>>,
}

impl std::fmt::Debug for XSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XSource").field("drag", &self.shared).finish()
    }
}

impl XSource {
    fn tell(&self, cmd: DragCmd) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(cmd);
        }
    }
}

impl IsAlive for XSource {
    fn alive(&self) -> bool {
        self.shared.lock().is_ok_and(|s| !s.ended)
    }
}

impl Source for XSource {
    fn metadata(&self) -> Option<SourceMetadata> {
        let s = self.shared.lock().ok()?;
        let mut metadata = SourceMetadata { mime_types: s.types.iter().map(|(m, _)| m.clone()).collect(), ..SourceMetadata::default() };
        for action in [s.offered, DndAction::Copy, DndAction::Move] {
            if action != DndAction::None && !metadata.dnd_actions.contains(&action) {
                metadata.dnd_actions.push(action);
            }
        }
        Some(metadata)
    }

    fn accepted(&self, mime_type: Option<String>) {
        if let Ok(mut s) = self.shared.lock() {
            s.accepted = mime_type.is_some();
        }
        self.tell(DragCmd::Status);
    }

    fn choose_action(&self, action: DndAction) {
        if let Ok(mut s) = self.shared.lock() {
            s.action = action;
        }
        self.tell(DragCmd::Status);
    }

    fn send(&self, mime_type: &str, fd: OwnedFd) {
        self.tell(DragCmd::Fetch(mime_type.to_owned(), fd));
    }

    fn drop_performed(&self) {}

    fn cancel(&self) {
        self.tell(DragCmd::Finish(false));
    }

    fn finished(&self) {
        self.tell(DragCmd::Finish(true));
    }
}

/// An X11 app's drag, on the X side.
struct FromX {
    shared: Arc<Mutex<XDrag>>,
    /// The window its XDND messages come from (not always the selection
    /// owner: Chromium's come from another window of its own).
    source: XId,
    /// It is over the drop window (between its XdndEnter and XdndLeave).
    over_proxy: bool,
    /// Its XdndDrop on the drop window came, at this time.
    dropped: Option<u32>,
    /// The time of its last XdndPosition, for fetching before the drop.
    time: u32,
    /// The Wayland side finished before the X11 app dropped.
    finish: Option<bool>,
    /// The seat's grab ended: the drag finishes by then, or is given up
    /// (a Wayland app that never finishes can't keep the drop window up,
    /// which would take X11 input).
    deadline: Option<std::time::Instant>,
}

/// A Wayland drag's offer to an X11 window.
#[derive(Debug, Default)]
pub struct ToXState {
    accepted: bool,
    disabled: bool,
}

/// The drag's offer data for an X11 window (Smithay's `OfferData`).
#[derive(Debug)]
pub struct XOffer(Arc<Mutex<ToXState>>);

impl OfferData for XOffer {
    fn disable(&self) {
        if let Ok(mut s) = self.0.lock() {
            s.disabled = true;
        }
    }

    fn drop(&self) {}

    fn validated(&self) -> bool {
        self.0.lock().is_ok_and(|s| s.accepted && !s.disabled)
    }
}

/// A Wayland drag over (or dropped on) an X11 window.
struct ToX {
    source: Arc<dyn Source>,
    /// The window it is over (what the messages name), and the window
    /// they go to (it, or its XDND proxy).
    target: XId,
    to: XId,
    state: Arc<Mutex<ToXState>>,
    action: DndAction,
    dropped: bool,
}

pub struct Dnd {
    conn: Arc<RustConnection>,
    atoms: Atoms,
    root: XId,
    /// The drop window for X11 drags over Wayland windows, and whether it
    /// is shown.
    proxy: XId,
    proxy_shown: bool,
    /// Owns XdndSelection for Wayland drags, and sends their messages.
    owner: XId,
    from_x: Option<FromX>,
    to_x: Option<ToX>,
}

impl Dnd {
    pub(super) fn new(conn: &Arc<RustConnection>, atoms: Atoms, root: XId, selections: &mut Selections) -> Result<Self, ReplyOrIdError> {
        use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
        // An input-only window: it takes the X11 app's messages and has
        // nothing to show (no surface of its own).
        let proxy = conn.generate_id()?;
        conn.create_window(0, proxy, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, x11rb::COPY_FROM_PARENT, &CreateWindowAux::new().override_redirect(1))?;
        conn.change_property32(PropMode::REPLACE, proxy, atoms.XdndAware, AtomEnum::ATOM, &[VERSION])?;
        // Chromium only drops on windows with a name.
        conn.change_property8(PropMode::REPLACE, proxy, atoms.WM_NAME, AtomEnum::STRING, b"Helix drop")?;
        conn.change_property8(PropMode::REPLACE, proxy, atoms._NET_WM_NAME, atoms.UTF8_STRING, b"Helix drop")?;
        let owner = conn.generate_id()?;
        conn.create_window(0, owner, root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, x11rb::COPY_FROM_PARENT, &CreateWindowAux::new().override_redirect(1))?;
        let events = SelectionEventMask::SET_SELECTION_OWNER | SelectionEventMask::SELECTION_WINDOW_DESTROY | SelectionEventMask::SELECTION_CLIENT_CLOSE;
        conn.xfixes_select_selection_input(owner, atoms.XdndSelection, events)?;
        selections.adopt(proxy);
        selections.adopt(owner);
        Ok(Self { conn: conn.clone(), atoms, root, proxy, proxy_shown: false, owner, from_x: None, to_x: None })
    }

    /// Whether an X11 app's drag is going on.
    pub fn dragging_from_x(&self) -> bool {
        self.from_x.is_some()
    }

    // ----- X11 apps' drags -------------------------------------------------

    /// XFixes: XdndSelection has a new owner. `Some(true)`: an X11 app may
    /// be starting a drag; `Some(false)`: the X11 app's drag is gone.
    pub(super) fn owner_changed(&mut self, owner: XId) -> Option<bool> {
        if owner == self.owner {
            return None;
        }
        if owner == x11rb::NONE {
            // The X11 app (or its window) went away.
            return self.from_x.is_some().then(|| {
                self.end_from_x();
                false
            });
        }
        if self.from_x.is_some() {
            // A new drag of its: the old one is stale.
            self.end_from_x();
        }
        Some(true)
    }

    /// Helix carries an X11 app's drag: its source, and the drop window
    /// shown until the pointer is over an X11 window.
    pub fn begin_from_x(&mut self, tx: smithay::reexports::calloop::channel::Sender<DragCmd>) -> XSource {
        let shared = Arc::new(Mutex::new(XDrag { offered: DndAction::Copy, action: DndAction::Copy, ..XDrag::default() }));
        self.from_x = Some(FromX { shared: shared.clone(), source: x11rb::NONE, over_proxy: false, dropped: None, time: x11rb::CURRENT_TIME, finish: None, deadline: None });
        self.show_proxy(true);
        tracing::debug!("an X11 app's drag");
        XSource { shared, tx: Mutex::new(tx) }
    }

    /// The X11 app's drag is over an X11 window (the drop window steps
    /// aside) or not.
    pub fn over_x11(&mut self, x11: bool) {
        if self.from_x.is_some() {
            self.show_proxy(!x11);
        }
    }

    /// The seat's grab ended (dropped or cancelled): the drag waits for
    /// its finish, for a while. The drop window stays until then: the X11
    /// app drops on it when it hears the button go up.
    pub fn grab_ended(&mut self) {
        if let Some(drag) = self.from_x.as_mut() {
            drag.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
        }
    }

    /// When an X11 app's drag is given up if it hasn't finished.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.from_x.as_ref().and_then(|d| d.deadline)
    }

    /// An X11 app's drag that didn't finish in time is given up.
    pub fn expired(&mut self) {
        if self.from_x.as_ref().and_then(|d| d.deadline).is_some_and(|d| d <= std::time::Instant::now()) {
            tracing::debug!("an X11 drag never finished");
            if self.from_x.as_ref().is_some_and(|d| d.dropped.is_some()) {
                let _ = self.finished(false);
            } else {
                self.end_from_x();
            }
        }
    }

    fn show_proxy(&mut self, show: bool) {
        if show == self.proxy_shown {
            return;
        }
        self.proxy_shown = show;
        let result = (|| -> Result<(), ReplyOrIdError> {
            if show {
                let root = self.conn.get_geometry(self.root)?.reply()?;
                let aux = ConfigureWindowAux::new().x(0).y(0).width(u32::from(root.width)).height(u32::from(root.height)).stack_mode(StackMode::ABOVE);
                self.conn.configure_window(self.proxy, &aux)?;
                self.conn.map_window(self.proxy)?;
            } else {
                self.conn.unmap_window(self.proxy)?;
            }
            self.conn.flush()?;
            Ok(())
        })();
        if let Err(err) = result {
            tracing::debug!(%err, "cannot show the drop window");
        }
    }

    fn end_from_x(&mut self) {
        if let Some(drag) = self.from_x.take() {
            if let Ok(mut s) = drag.shared.lock() {
                s.ended = true;
            }
        }
        self.show_proxy(false);
    }

    /// Tells the X11 app whether the Wayland target takes the drop
    /// (XdndStatus).
    fn status(&self) -> Result<(), ReplyOrIdError> {
        let Some(drag) = self.from_x.as_ref().filter(|d| d.over_proxy && d.source != x11rb::NONE) else { return Ok(()) };
        let (accepted, action) = drag.shared.lock().map(|s| (s.accepted, s.action)).unwrap_or((false, DndAction::None));
        let action = if accepted { self.action_atom(action) } else { x11rb::NONE };
        // Bit 1: positions wanted even inside a rectangle (none given).
        let flags = u32::from(accepted) | 2;
        self.message(drag.source, drag.source, self.atoms.XdndStatus, [self.proxy, flags, 0, 0, action])
    }

    fn action_atom(&self, action: DndAction) -> u32 {
        match action {
            DndAction::Copy => self.atoms.XdndActionCopy,
            DndAction::Move => self.atoms.XdndActionMove,
            DndAction::Ask => self.atoms.XdndActionAsk,
            DndAction::None => x11rb::NONE,
        }
    }

    fn action_of(&self, atom: u32) -> DndAction {
        match atom {
            a if a == self.atoms.XdndActionMove => DndAction::Move,
            a if a == self.atoms.XdndActionAsk => DndAction::Ask,
            a if a == x11rb::NONE => DndAction::None,
            _ => DndAction::Copy,
        }
    }

    fn message(&self, to: XId, window: XId, kind: u32, data: [u32; 5]) -> Result<(), ReplyOrIdError> {
        let event = ClientMessageEvent::new(32, window, kind, data);
        self.conn.send_event(false, to, EventMask::NO_EVENT, event)?;
        self.conn.flush()?;
        Ok(())
    }

    /// What the X11 app's source asks (through the seat's grab).
    pub(super) fn cmd(&mut self, cmd: DragCmd, selections: &mut Selections) -> Result<(), ReplyOrIdError> {
        match cmd {
            DragCmd::Status => self.status(),
            DragCmd::Fetch(mime, fd) => {
                let Some(drag) = self.from_x.as_ref() else { return Ok(()) };
                let asked = drag.shared.lock().ok().and_then(|s| s.types.iter().find(|(m, _)| *m == mime).map(|(_, a)| *a));
                let Some(asked) = asked else {
                    tracing::debug!(mime, "the X11 drag has no such type");
                    return Ok(());
                };
                let time = drag.dropped.unwrap_or(drag.time);
                selections.receive_drop(asked, fd, time, &mime)
            }
            DragCmd::Finish(ok) => {
                let Some(drag) = self.from_x.as_mut() else { return Ok(()) };
                if !drag.over_proxy && drag.dropped.is_none() {
                    // Over an X11 window: the X11 apps finish between
                    // themselves.
                    self.end_from_x();
                    return Ok(());
                }
                if drag.dropped.is_none() {
                    // Its XdndDrop is still to come.
                    drag.finish = Some(ok);
                    return Ok(());
                }
                self.finished(ok)
            }
        }
    }

    /// XdndFinished to the X11 app, and its drag is over.
    fn finished(&mut self, ok: bool) -> Result<(), ReplyOrIdError> {
        let Some(drag) = self.from_x.as_ref() else { return Ok(()) };
        let action = if ok { drag.shared.lock().map(|s| self.action_atom(s.action)).unwrap_or(x11rb::NONE) } else { x11rb::NONE };
        let source = drag.source;
        self.end_from_x();
        if source != x11rb::NONE {
            self.message(source, source, self.atoms.XdndFinished, [self.proxy, u32::from(ok), action, 0, 0])?;
        }
        Ok(())
    }

    /// An XDND message to the drop window (from an X11 app's drag), or to
    /// the owner window (from an X11 drop target). `true` if it was one.
    pub(super) fn client_message(&mut self, e: &ClientMessageEvent, selections: &mut Selections) -> Result<bool, ReplyOrIdError> {
        if e.window == self.proxy {
            self.at_proxy(e)?;
            return Ok(true);
        }
        if e.window == self.owner {
            self.at_owner(e, selections)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn at_proxy(&mut self, e: &ClientMessageEvent) -> Result<(), ReplyOrIdError> {
        let a = self.atoms;
        let data = e.data.as_data32();
        if self.from_x.is_none() {
            return Ok(());
        }
        if e.type_ == a.XdndEnter {
            let source = data[0];
            let atoms: Vec<u32> = if data[1] & 1 == 1 {
                // More than three types: in the source's XdndTypeList.
                let reply = self.conn.get_property(false, source, a.XdndTypeList, AtomEnum::ATOM, 0, 1024)?.reply()?;
                reply.value32().map(|v| v.collect()).unwrap_or_default()
            } else {
                data[2..].iter().copied().filter(|t| *t != x11rb::NONE).collect()
            };
            let types = self.mimes(&atoms)?;
            let drag = self.from_x.as_mut().unwrap();
            drag.source = source;
            drag.over_proxy = true;
            if let Ok(mut s) = drag.shared.lock() {
                s.types = types;
            }
        } else if e.type_ == a.XdndPosition {
            let offered = self.action_of(data[4]);
            let drag = self.from_x.as_mut().unwrap();
            if drag.source == x11rb::NONE {
                return Ok(());
            }
            drag.time = data[3];
            if let Ok(mut s) = drag.shared.lock() {
                s.offered = offered;
            }
            self.status()?;
        } else if e.type_ == a.XdndLeave {
            self.from_x.as_mut().unwrap().over_proxy = false;
        } else if e.type_ == a.XdndDrop {
            let drag = self.from_x.as_mut().unwrap();
            drag.dropped = Some(data[2]);
            if let Some(ok) = drag.finish.take() {
                self.finished(ok)?;
            }
        }
        Ok(())
    }

    /// X11 type atoms as Wayland types (text as `text/plain`), with the
    /// atom each is asked for by.
    fn mimes(&self, atoms: &[u32]) -> Result<Vec<(String, u32)>, ReplyOrIdError> {
        let cookies: Vec<_> = atoms.iter().map(|a| self.conn.get_atom_name(*a)).collect::<Result<_, _>>()?;
        let mut types: Vec<(String, u32)> = Vec::new();
        for (atom, cookie) in atoms.iter().zip(cookies) {
            let Ok(reply) = cookie.reply() else { continue };
            let name = String::from_utf8_lossy(&reply.name).into_owned();
            let mime = match name.as_str() {
                "UTF8_STRING" => "text/plain;charset=utf-8".to_owned(),
                "TEXT" | "STRING" => "text/plain".to_owned(),
                n if n.contains('/') => name.clone(),
                _ => continue,
            };
            if !types.iter().any(|(m, _)| *m == mime) {
                types.push((mime, *atom));
            }
        }
        Ok(types)
    }

    // ----- Wayland drags onto X11 windows ---------------------------------

    /// A Wayland drag comes over `win` at `at` (on its surface): XdndEnter
    /// if the window takes drops. Its offer data, if so.
    pub fn enter(&mut self, win: &XWin, source: Arc<dyn Source>, at: Point<f64, Logical>, selections: &mut Selections) -> Option<XOffer> {
        match self.try_enter(win, source, at, selections) {
            Ok(offer) => offer,
            Err(err) => {
                tracing::debug!(%err, "cannot offer the drag to an X11 window");
                None
            }
        }
    }

    fn try_enter(&mut self, win: &XWin, source: Arc<dyn Source>, at: Point<f64, Logical>, selections: &mut Selections) -> Result<Option<XOffer>, ReplyOrIdError> {
        let a = self.atoms;
        let target = win.window_id();
        let aware = self.conn.get_property(false, target, a.XdndAware, AtomEnum::ATOM, 0, 1)?.reply()?;
        let Some(version) = aware.value32().and_then(|mut v| v.next()) else { return Ok(None) };
        let version = version.min(VERSION);
        // Its XDND proxy, if that names itself as one (else not followed).
        let to = match self.conn.get_property(false, target, a.XdndProxy, AtomEnum::WINDOW, 0, 1)?.reply()?.value32().and_then(|mut v| v.next()) {
            Some(proxy) if proxy != x11rb::NONE => {
                let named = self.conn.get_property(false, proxy, a.XdndProxy, AtomEnum::WINDOW, 0, 1)?.reply().ok().and_then(|r| r.value32().and_then(|mut v| v.next()));
                if named == Some(proxy) {
                    proxy
                } else {
                    target
                }
            }
            _ => target,
        };
        let Some(metadata) = source.metadata() else { return Ok(None) };
        // The drag's types as X11 targets: text as UTF8_STRING.
        let mut atoms: Vec<u32> = Vec::new();
        for mime in &metadata.mime_types {
            let atom = if matches!(mime.as_str(), "text/plain;charset=utf-8" | "text/plain;charset=UTF-8" | "UTF8_STRING") {
                a.UTF8_STRING
            } else {
                self.conn.intern_atom(false, mime.as_bytes())?.reply()?.atom
            };
            if !atoms.contains(&atom) {
                atoms.push(atom);
            }
        }
        if atoms.is_empty() {
            return Ok(None);
        }
        self.conn.set_selection_owner(self.owner, a.XdndSelection, x11rb::CURRENT_TIME)?;
        if atoms.len() > 3 {
            self.conn.change_property32(PropMode::REPLACE, self.owner, a.XdndTypeList, AtomEnum::ATOM, &atoms)?;
        }
        let mut types = [x11rb::NONE; 3];
        for (slot, atom) in types.iter_mut().zip(&atoms) {
            *slot = *atom;
        }
        let more = u32::from(atoms.len() > 3);
        selections.dnd = Some(DndServe { owner: self.owner, types: metadata.mime_types.clone(), source: source.clone() });
        let action = [DndAction::Copy, DndAction::Move, DndAction::Ask].into_iter().find(|x| metadata.dnd_actions.contains(x)).unwrap_or(DndAction::Copy);
        let state = Arc::new(Mutex::new(ToXState::default()));
        self.to_x = Some(ToX { source, target, to, state: state.clone(), action, dropped: false });
        self.message(to, target, a.XdndEnter, [self.owner, (version << 24) | more, types[0], types[1], types[2]])?;
        self.motion(win, at, x11rb::CURRENT_TIME);
        Ok(Some(XOffer(state)))
    }

    /// The drag moved over `win`, to `at` on its surface (XdndPosition).
    pub fn motion(&mut self, win: &XWin, at: Point<f64, Logical>, time: u32) {
        let Some(drag) = self.to_x.as_ref().filter(|d| d.target == win.window_id() && !d.dropped) else { return };
        let loc = win.last_configure().loc;
        let x = win.logical_to_x((f64::from(loc.x) + at.x).round() as i32);
        let y = win.logical_to_x((f64::from(loc.y) + at.y).round() as i32);
        let at = ((x as u32 & 0xffff) << 16) | (y as u32 & 0xffff);
        let action = self.action_atom(drag.action);
        let (to, target) = (drag.to, drag.target);
        if let Err(err) = self.message(to, target, self.atoms.XdndPosition, [self.owner, 0, at, time, action]) {
            tracing::debug!(%err, "cannot move the drag over an X11 window");
        }
    }

    /// The drag left the X11 window (XdndLeave), unless it was dropped
    /// there.
    pub fn leave(&mut self, selections: &mut Selections) {
        let Some(drag) = self.to_x.as_ref() else { return };
        if drag.dropped {
            return;
        }
        let (to, target) = (drag.to, drag.target);
        self.to_x = None;
        selections.dnd = None;
        let _ = self.message(to, target, self.atoms.XdndLeave, [self.owner, 0, 0, 0, 0]);
    }

    /// Dropped on the X11 window: XdndDrop if it takes it, else XdndLeave.
    pub fn drop(&mut self, selections: &mut Selections) {
        let Some(accepted) = self.to_x.as_ref().map(|d| d.state.lock().is_ok_and(|s| s.accepted)) else { return };
        if !accepted {
            self.leave(selections);
            return;
        }
        let Some(drag) = self.to_x.as_mut() else { return };
        drag.dropped = true;
        let (to, target) = (drag.to, drag.target);
        let _ = self.message(to, target, self.atoms.XdndDrop, [self.owner, 0, x11rb::CURRENT_TIME, 0, 0]);
    }

    /// XdndStatus or XdndFinished from the X11 drop target.
    fn at_owner(&mut self, e: &ClientMessageEvent, selections: &mut Selections) -> Result<(), ReplyOrIdError> {
        let a = self.atoms;
        let data = e.data.as_data32();
        let action = self.action_of(data[4]);
        let finished_action = self.action_of(data[2]);
        let Some(drag) = self.to_x.as_mut() else { return Ok(()) };
        // Only from the window the drag is over (a message naming another
        // window is someone else's, or forged).
        if data[0] != drag.target {
            tracing::debug!(window = data[0], "ignoring an XDND message for another window");
            return Ok(());
        }
        if e.type_ == a.XdndStatus {
            if drag.dropped {
                return Ok(());
            }
            let accepted = data[1] & 1 == 1;
            let changed = {
                let Ok(mut s) = drag.state.lock() else { return Ok(()) };
                let changed = s.accepted != accepted;
                s.accepted = accepted;
                changed
            };
            if changed {
                let mime = drag.source.metadata().and_then(|m| m.mime_types.first().cloned());
                drag.source.accepted(if accepted { mime } else { None });
            }
            if accepted {
                drag.source.choose_action(action);
            }
        } else if e.type_ == a.XdndFinished {
            if !drag.dropped {
                return Ok(());
            }
            let source = drag.source.clone();
            self.to_x = None;
            selections.dnd = None;
            if data[1] & 1 == 1 {
                source.choose_action(finished_action);
                source.finished();
            } else {
                source.cancel();
            }
        }
        Ok(())
    }
}

//! CLIPBOARD and PRIMARY between X11 and Wayland apps (ICCCM 2.2–2.7),
//! for Helix's own window manager (helixdesktop/desktop#191, step 4).
//!
//! X11 to Wayland: XFixes says who owns a selection; the window manager
//! asks the owner for its TARGETS and offers them to Wayland apps as
//! types. A Wayland app's paste becomes a ConvertSelection from a window
//! of its own for that transfer, read whole or incrementally (INCR) and
//! written to the app's pipe as it is willing to take it.
//!
//! Wayland to X11: for a Wayland copy the window manager owns the
//! selection; each X11 request is answered from the Wayland app through a
//! pipe, in one property or incrementally once it is larger than a chunk.
//!
//! Any X11 client may read, whatever has focus, as in GNOME (#172).
//! Transfers never block the compositor: the pipes are watched in its
//! event loop ([`Action::Watch`]), and every transfer has a deadline so a
//! hung app on either side can't hold the clipboard.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::wayland::selection::SelectionTarget;
use x11rb::connection::Connection;
use x11rb::errors::ReplyOrIdError;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, Window as XId, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{parse, Action, Atoms, Later};

/// A transfer goes incrementally above this many bytes, in chunks of it.
const CHUNK: usize = 64 * 1024;
/// How long a transfer may go without progress.
const STALL: Duration = Duration::from_secs(5);

/// How much of a transfer is held at most before the side that gives it
/// is made to wait for the side that takes it (#228).
const BUFFERED: usize = 4 * 1024 * 1024;

/// The text types a Wayland copy may have, best first.
const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain;charset=UTF-8", "UTF8_STRING", "text/plain", "STRING"];

/// One selection (CLIPBOARD or PRIMARY).
struct Sel {
    target: SelectionTarget,
    atom: u32,
    /// The window manager's window owning it for Wayland copies, and
    /// asking X11 owners for their TARGETS.
    window: XId,
    /// Who owns it on the X side (from XFixes).
    owner: XId,
    /// While Helix owns it for a Wayland copy: its types, and when it
    /// took it (for TIMESTAMP).
    wayland: Option<Vec<String>>,
    timestamp: u32,
    /// The owner (and the time) whose TARGETS were asked for, while the
    /// answer is still current: an answer for an earlier owner, or one
    /// coming after a Wayland copy, is stale (#197).
    targets_for: Option<(XId, u32)>,
    /// The X11 owner's types, as Wayland types, with the atom each is
    /// asked for by.
    offered: Vec<(String, u32)>,
}

/// An X11 app's selection on its way to a Wayland app.
struct Incoming {
    /// CLIPBOARD or PRIMARY; `None` for a drag's XdndSelection.
    selection: Option<SelectionTarget>,
    /// The window asking for it (one per transfer).
    window: XId,
    /// The Wayland app's pipe; `None` once it stopped reading.
    fd: Option<std::fs::File>,
    buf: Vec<u8>,
    incr: bool,
    /// The X11 owner sent all of it.
    done: bool,
    watching: bool,
    /// An incremental chunk is waiting in the property, not read yet:
    /// reading it asks for the next, so it waits until the Wayland app has
    /// taken most of what's buffered.
    chunk_waiting: bool,
    /// Progress on either side (the X11 owner sending, the Wayland app
    /// reading) moves it on.
    deadline: Instant,
}

/// A Wayland app's selection on its way to an X11 app.
struct Outgoing {
    requestor: XId,
    property: u32,
    /// The type it goes as.
    kind: u32,
    request: SelectionRequestEvent,
    fd: std::fs::File,
    buf: Vec<u8>,
    eof: bool,
    /// The requestor was told (and, when incremental, given the INCR
    /// property).
    answered: bool,
    incr: bool,
    /// A chunk is in the requestor's property, not yet deleted.
    waiting: bool,
    /// Reading the Wayland source paused: enough is buffered for now.
    paused: bool,
    deadline: Instant,
}

/// A Wayland drag's data, offered as XdndSelection by the window manager's
/// `owner` window while the drag is over (or dropped on) an X11 window.
pub(super) struct DndServe {
    pub owner: XId,
    pub types: Vec<String>,
    pub source: Arc<dyn smithay::input::dnd::Source>,
}

pub struct Selections {
    conn: Arc<RustConnection>,
    atoms: Atoms,
    root: XId,
    sels: [Sel; 2],
    incoming: HashMap<u64, Incoming>,
    outgoing: HashMap<u64, Outgoing>,
    next: u64,
    /// Windows the window manager made for selections, until X says they
    /// are gone (never managed).
    own: HashSet<XId>,
    atom_cache: HashMap<String, u32>,
    /// The transfer whose pipe is being handled from the event loop now:
    /// its watch goes when [`Selections::ready`] says so.
    in_ready: Option<u64>,
    /// A Wayland drag over an X11 window (`dnd`).
    pub(super) dnd: Option<DndServe>,
    /// Atom names already asked for (TARGETS, requested types).
    names: HashMap<u32, String>,
    /// What to do with replies when they come ([`super::Xwm::absorb`]).
    pub(super) later: Vec<Later>,
}

impl Selections {
    pub(super) fn new(conn: &Arc<RustConnection>, atoms: Atoms, root: XId) -> Result<Self, ReplyOrIdError> {
        conn.xfixes_query_version(5, 0)?.reply()?;
        let mut own = HashSet::new();
        let mut make = |target: SelectionTarget, atom: u32| -> Result<Sel, ReplyOrIdError> {
            let window = conn.generate_id()?;
            conn.create_window(0, window, root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, x11rb::COPY_FROM_PARENT, &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
            let events = xfixes::SelectionEventMask::SET_SELECTION_OWNER | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
            conn.xfixes_select_selection_input(window, atom, events)?;
            own.insert(window);
            Ok(Sel { target, atom, window, owner: x11rb::NONE, wayland: None, timestamp: x11rb::CURRENT_TIME, targets_for: None, offered: Vec::new() })
        };
        let sels = [make(SelectionTarget::Clipboard, atoms.CLIPBOARD)?, make(SelectionTarget::Primary, atoms.PRIMARY)?];
        Ok(Self { conn: conn.clone(), atoms, root, sels, incoming: HashMap::new(), outgoing: HashMap::new(), next: 0, own, atom_cache: HashMap::new(), in_ready: None, dnd: None, names: HashMap::new(), later: Vec::new() })
    }

    /// Whether the window manager made this window for selections.
    pub fn is_own(&self, id: XId) -> bool {
        self.own.contains(&id)
    }

    /// A window the window manager made (for drag and drop): never managed.
    pub(super) fn adopt(&mut self, id: XId) {
        self.own.insert(id);
    }

    fn sel(&mut self, target: SelectionTarget) -> &mut Sel {
        match target {
            SelectionTarget::Clipboard => &mut self.sels[0],
            SelectionTarget::Primary => &mut self.sels[1],
        }
    }

    /// Interns `names` not known yet; known when the replies come (X
    /// events wait for them: a TARGETS request finds them).
    pub(super) fn intern_ahead(&mut self, names: &[String]) -> Result<(), ReplyOrIdError> {
        let missing: Vec<String> = names.iter().filter(|n| !self.atom_cache.contains_key(*n)).cloned().collect();
        if missing.is_empty() {
            return Ok(());
        }
        let seqs = missing.iter().map(|n| Ok(super::keep(self.conn.intern_atom(false, n.as_bytes())?))).collect::<Result<Vec<_>, ReplyOrIdError>>()?;
        self.later.push((
            seqs,
            Box::new(move |xwm, replies, _| {
                for (name, reply) in missing.into_iter().zip(replies) {
                    if let Some(r) = parse::<x11rb::protocol::xproto::InternAtomReply>(reply) {
                        xwm.selections.names.insert(r.atom, name.clone());
                        xwm.selections.atom_cache.insert(name, r.atom);
                    }
                }
                Ok(())
            }),
        ));
        Ok(())
    }

    /// An interned type's atom (see [`Selections::intern_ahead`]).
    pub(super) fn atom_of(&self, name: &str) -> Option<u32> {
        self.atom_cache.get(name).copied()
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    // ----- What the compositor tells it ---------------------------------

    /// A Wayland copy (`Some` with its types) or none: own the X
    /// selection for it, or let it go if Helix had it.
    pub fn set_wayland(&mut self, target: SelectionTarget, types: Option<Vec<String>>) -> Result<(), ReplyOrIdError> {
        let conn = self.conn.clone();
        let sel = self.sel(target);
        // An X11 owner's TARGETS still to come are no longer the
        // selection: this copy came after it (#197).
        sel.targets_for = None;
        match types {
            Some(types) => {
                sel.wayland = Some(types.clone());
                sel.offered.clear();
                conn.set_selection_owner(sel.window, sel.atom, x11rb::CURRENT_TIME)?;
                self.intern_ahead(&types)?;
            }
            None => {
                if sel.wayland.take().is_some() && sel.owner == sel.window {
                    conn.set_selection_owner(x11rb::NONE, sel.atom, x11rb::CURRENT_TIME)?;
                }
            }
        }
        conn.flush()?;
        Ok(())
    }

    /// A Wayland app pastes the X11 owner's selection as `mime` into `fd`.
    pub fn receive(&mut self, target: SelectionTarget, mime: &str, fd: OwnedFd, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let _ = actions;
        let sel = self.sel(target);
        if sel.owner == x11rb::NONE || sel.owner == sel.window {
            return Ok(());
        }
        let Some(&(_, asked)) = sel.offered.iter().find(|(m, _)| m == mime) else {
            tracing::debug!(mime, "the X11 selection has no such type");
            return Ok(());
        };
        let selection = sel.atom;
        self.convert(Some(target), selection, asked, fd, x11rb::CURRENT_TIME, mime)
    }

    /// A Wayland app takes a drop of an X11 app's drag as `asked` (an atom
    /// of the drag's types) into `fd`: asked of XdndSelection's owner at
    /// the drop's `time`.
    pub(super) fn receive_drop(&mut self, asked: u32, fd: OwnedFd, time: u32, mime: &str) -> Result<(), ReplyOrIdError> {
        let selection = self.atoms.XdndSelection;
        self.convert(None, selection, asked, fd, time, mime)
    }

    /// Asks the owner of `selection` for it as `asked`, from a window of
    /// this transfer's own, for the Wayland app's pipe `fd`.
    fn convert(&mut self, target: Option<SelectionTarget>, selection: u32, asked: u32, fd: OwnedFd, time: u32, mime: &str) -> Result<(), ReplyOrIdError> {
        let conn = self.conn.clone();
        let property = self.atoms._HELIX_SELECTION;
        set_nonblocking(&fd);
        let window = conn.generate_id()?;
        conn.create_window(0, window, self.root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, x11rb::COPY_FROM_PARENT, &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
        self.own.insert(window);
        conn.convert_selection(window, selection, asked, property, time)?;
        conn.flush()?;
        let id = self.id();
        tracing::debug!(id, mime, "X11 selection asked for");
        self.incoming.insert(id, Incoming { selection: target, window, fd: Some(std::fs::File::from(fd)), buf: Vec::new(), incr: false, done: false, watching: false, chunk_waiting: false, deadline: Instant::now() + STALL });
        Ok(())
    }

    /// The pipe of transfer `id` is ready (the Wayland app's to write to,
    /// or the Wayland source's to read from). Whether to keep watching it.
    pub fn ready(&mut self, id: u64, actions: &mut Vec<Action>) -> bool {
        self.in_ready = Some(id);
        let keep = if self.incoming.contains_key(&id) {
            self.drain(id, actions);
            self.incoming.get(&id).is_some_and(|t| t.watching)
        } else if self.outgoing.contains_key(&id) {
            if let Err(err) = self.read_wayland(id, actions) {
                tracing::debug!(%err, "a selection transfer to X11 failed");
                self.drop_outgoing(id, actions);
            }
            self.outgoing.get(&id).is_some_and(|t| !t.eof && !t.paused)
        } else {
            false
        };
        self.in_ready = None;
        if !keep {
            if let Some(t) = self.incoming.get_mut(&id) {
                t.watching = false;
            }
        }
        keep
    }

    /// When the next transfer gives up waiting.
    pub fn deadline(&self) -> Option<Instant> {
        self.incoming.values().map(|t| t.deadline).chain(self.outgoing.values().map(|t| t.deadline)).min()
    }

    /// Transfers that made no progress for too long end.
    pub fn expired(&mut self, actions: &mut Vec<Action>) {
        let now = Instant::now();
        let stale: Vec<u64> = self.incoming.iter().filter(|(_, t)| t.deadline <= now).map(|(id, _)| *id).collect();
        for id in stale {
            tracing::debug!(id, "an X11 selection owner stopped answering");
            self.finish_incoming(id, actions);
        }
        let stale: Vec<u64> = self.outgoing.iter().filter(|(_, t)| t.deadline <= now).map(|(id, _)| *id).collect();
        for id in stale {
            tracing::debug!(id, "a selection transfer to X11 stalled");
            self.drop_outgoing(id, actions);
        }
        let _ = self.conn.flush();
    }

    // ----- X events -------------------------------------------------------

    /// Handles `event` if it is about selections. `true` when nothing else
    /// needs it.
    pub(super) fn handle(&mut self, event: &Event, actions: &mut Vec<Action>) -> Result<bool, ReplyOrIdError> {
        match event {
            Event::XfixesSelectionNotify(e) => {
                self.owner_changed(e, actions)?;
                Ok(true)
            }
            Event::SelectionRequest(e) => {
                if self.sels.iter().any(|s| s.window == e.owner) || self.dnd.as_ref().is_some_and(|d| d.owner == e.owner) {
                    self.request(*e, actions)?;
                    self.conn.flush()?;
                }
                Ok(true)
            }
            Event::SelectionNotify(e) => {
                if let Some(i) = self.sels.iter().position(|s| s.window == e.requestor) {
                    self.targets(i, e, actions)?;
                } else if let Some(id) = self.incoming_of(e.requestor) {
                    self.converted(id, e, actions)?;
                }
                self.conn.flush()?;
                Ok(true)
            }
            Event::PropertyNotify(e) => {
                if e.state == Property::NEW_VALUE && e.atom == self.atoms._HELIX_SELECTION {
                    if let Some(id) = self.incoming_of(e.window) {
                        let t = self.incoming.get_mut(&id).unwrap();
                        if t.incr {
                            if t.buf.len() >= BUFFERED {
                                // The Wayland app is behind: read it (and
                                // so ask for the next) once it caught up.
                                t.chunk_waiting = true;
                            } else {
                                self.chunk(id, actions)?;
                                self.conn.flush()?;
                            }
                        }
                        return Ok(true);
                    }
                }
                if e.state == Property::DELETE {
                    let id = self.outgoing.iter().find(|(_, t)| t.requestor == e.window && t.property == e.atom && t.incr && t.waiting).map(|(id, _)| *id);
                    if let Some(id) = id {
                        let t = self.outgoing.get_mut(&id).unwrap();
                        t.waiting = false;
                        t.deadline = Instant::now() + STALL;
                        self.send_next(id, actions)?;
                        self.conn.flush()?;
                        return Ok(true);
                    }
                }
                Ok(self.own.contains(&e.window))
            }
            Event::CreateNotify(e) => Ok(self.own.contains(&e.window)),
            Event::DestroyNotify(e) => {
                let gone: Vec<u64> = self.outgoing.iter().filter(|(_, t)| t.requestor == e.window).map(|(id, _)| *id).collect();
                for id in gone {
                    tracing::debug!(id, "the X11 app asking for the selection went away");
                    self.drop_outgoing(id, actions);
                }
                Ok(self.own.remove(&e.window))
            }
            _ => Ok(false),
        }
    }

    /// XFixes: someone else (or the window manager) owns the selection now.
    fn owner_changed(&mut self, e: &xfixes::SelectionNotifyEvent, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let Some(i) = self.sels.iter().position(|s| s.atom == e.selection) else { return Ok(()) };
        let targets = self.atoms.TARGETS;
        let property = self.atoms._HELIX_TARGETS;
        let conn = self.conn.clone();
        let sel = &mut self.sels[i];
        sel.owner = e.owner;
        sel.targets_for = None;
        if e.owner == sel.window {
            sel.timestamp = e.timestamp;
            return Ok(());
        }
        // The Wayland copy's types stay: this can be the late news of an
        // X11 owner Helix has replaced since (X sends requests only to the
        // owner, and the next Wayland copy replaces them).
        sel.offered.clear();
        if e.owner == x11rb::NONE {
            actions.push(Action::SelectionCleared(sel.target));
            return Ok(());
        }
        sel.targets_for = Some((e.owner, e.timestamp));
        conn.convert_selection(sel.window, sel.atom, targets, property, e.timestamp)?;
        conn.flush()?;
        Ok(())
    }

    /// The X11 owner's TARGETS: offer them to Wayland apps, if this answer
    /// is still about the selection.
    fn targets(&mut self, i: usize, e: &SelectionNotifyEvent, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        if e.property == x11rb::NONE {
            // An owner that doesn't answer TARGETS (old Xt and Motif apps):
            // offered as text, which every owner can give (#234). Not
            // leaving the last offer up, which no one serves any more.
            let sel = &mut self.sels[i];
            if sel.targets_for.is_some_and(|(owner, _)| owner == sel.owner) {
                sel.offered = vec![("text/plain;charset=utf-8".to_owned(), self.atoms.UTF8_STRING), ("text/plain".to_owned(), u32::from(AtomEnum::STRING))];
                actions.push(Action::SelectionOffered(sel.target, sel.offered.iter().map(|(m, _)| m.clone()).collect()));
            }
            return Ok(());
        }
        let seq = super::keep(self.conn.get_property(true, self.sels[i].window, e.property, AtomEnum::ANY, 0, 4096)?);
        self.later.push((
            vec![seq],
            Box::new(move |xwm, replies, _| {
                let s = &mut xwm.selections;
                // Still the owner asked (whatever time it answered with:
                // owners don't all echo the request's).
                let current = s.sels[i].targets_for.is_some_and(|(owner, _)| owner == s.sels[i].owner);
                if !current {
                    tracing::debug!("ignoring stale X11 selection TARGETS");
                    return Ok(());
                }
                let Some(reply) = parse::<x11rb::protocol::xproto::GetPropertyReply>(replies.into_iter().next().flatten()) else { return Ok(()) };
                let Some(atoms) = reply.value32() else { return Ok(()) };
                let atoms: Vec<u32> = atoms.collect();
                s.with_names(atoms, move |xwm, named, actions| {
                    let s = &mut xwm.selections;
                    if !s.sels[i].targets_for.is_some_and(|(owner, _)| owner == s.sels[i].owner) {
                        return;
                    }
                    let mut offered: Vec<(String, u32)> = Vec::new();
                    for (atom, name) in named {
                        let mime = match name.as_str() {
                            "UTF8_STRING" => "text/plain;charset=utf-8".to_owned(),
                            "TEXT" | "STRING" => "text/plain".to_owned(),
                            n if n.contains('/') => name.clone(),
                            _ => continue,
                        };
                        if !offered.iter().any(|(m, _)| *m == mime) {
                            offered.push((mime, atom));
                        }
                    }
                    let sel = &mut s.sels[i];
                    sel.offered = offered.clone();
                    actions.push(Action::SelectionOffered(sel.target, offered.into_iter().map(|(m, _)| m).collect()));
                })
            }),
        ));
        Ok(())
    }

    /// The names of `atoms` (asked for those not known yet), in order;
    /// then `then`. Atoms that have no name are left out.
    pub(super) fn with_names(&mut self, atoms: Vec<u32>, then: impl FnOnce(&mut super::Xwm, Vec<(u32, String)>, &mut Vec<Action>) + 'static) -> Result<(), ReplyOrIdError> {
        let missing: Vec<u32> = atoms.iter().copied().filter(|a| !self.names.contains_key(a)).collect::<HashSet<_>>().into_iter().collect();
        let seqs = missing.iter().map(|a| Ok(super::keep(self.conn.get_atom_name(*a)?))).collect::<Result<Vec<_>, ReplyOrIdError>>()?;
        let named = move |s: &Selections| atoms.iter().filter_map(|a| s.names.get(a).map(|n| (*a, n.clone()))).collect::<Vec<_>>();
        if missing.is_empty() {
            // Known already: still after anything pending, in order.
            self.later.push((Vec::new(), Box::new(move |xwm, _, actions| {
                let named = named(&xwm.selections);
                then(xwm, named, actions);
                Ok(())
            })));
            return Ok(());
        }
        self.later.push((
            seqs,
            Box::new(move |xwm, replies, actions| {
                for (atom, reply) in missing.into_iter().zip(replies) {
                    if let Some(r) = parse::<x11rb::protocol::xproto::GetAtomNameReply>(reply) {
                        let name = String::from_utf8_lossy(&r.name).into_owned();
                        xwm.selections.atom_cache.insert(name.clone(), atom);
                        xwm.selections.names.insert(atom, name);
                    }
                }
                let named = named(&xwm.selections);
                then(xwm, named, actions);
                Ok(())
            }),
        ));
        Ok(())
    }

    // ----- X11 to Wayland -------------------------------------------------

    fn incoming_of(&self, window: XId) -> Option<u64> {
        self.incoming.iter().find(|(_, t)| t.window == window).map(|(id, _)| *id)
    }

    /// The X11 owner answered a transfer's ConvertSelection.
    fn converted(&mut self, id: u64, e: &SelectionNotifyEvent, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        if e.property == x11rb::NONE {
            tracing::debug!(id, "the X11 selection owner refused the transfer");
            self.finish_incoming(id, actions);
            return Ok(());
        }
        let _ = actions;
        let window = self.incoming[&id].window;
        let seq = super::keep(self.conn.get_property(true, window, e.property, AtomEnum::ANY, 0, u32::MAX / 4)?);
        self.later.push((
            vec![seq],
            Box::new(move |xwm, replies, actions| {
                let s = &mut xwm.selections;
                let incr = s.atoms.INCR;
                let Some(t) = s.incoming.get_mut(&id) else { return Ok(()) };
                let Some(reply) = parse::<x11rb::protocol::xproto::GetPropertyReply>(replies.into_iter().next().flatten()) else {
                    s.finish_incoming(id, actions);
                    return Ok(());
                };
                t.deadline = Instant::now() + STALL;
                if reply.type_ == incr {
                    // Only a size hint: the data comes in the chunks after.
                    t.incr = true;
                    return Ok(());
                }
                t.buf.extend_from_slice(&reply.value);
                t.done = true;
                s.drain(id, actions);
                Ok(())
            }),
        ));
        Ok(())
    }

    /// The next chunk of an incremental transfer. Reading it (and deleting
    /// it) asks the owner for the one after; it is not deleted again.
    fn chunk(&mut self, id: u64, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let _ = actions;
        let window = self.incoming[&id].window;
        let seq = super::keep(self.conn.get_property(true, window, self.atoms._HELIX_SELECTION, AtomEnum::ANY, 0, u32::MAX / 4)?);
        self.later.push((
            vec![seq],
            Box::new(move |xwm, replies, actions| {
                let s = &mut xwm.selections;
                let Some(t) = s.incoming.get_mut(&id) else { return Ok(()) };
                let Some(reply) = parse::<x11rb::protocol::xproto::GetPropertyReply>(replies.into_iter().next().flatten()) else {
                    s.finish_incoming(id, actions);
                    return Ok(());
                };
                t.deadline = Instant::now() + STALL;
                if reply.value.is_empty() {
                    t.done = true;
                } else {
                    t.buf.extend_from_slice(&reply.value);
                }
                s.drain(id, actions);
                Ok(())
            }),
        ));
        Ok(())
    }

    /// Writes what the Wayland app will take; watches its pipe for the
    /// rest. A pipe it closed: the rest is read from the owner and dropped
    /// (the owner would wait forever otherwise).
    fn drain(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(t) = self.incoming.get_mut(&id) else { return };
        if let Some(fd) = t.fd.as_mut() {
            while !t.buf.is_empty() {
                match fd.write(&t.buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        t.buf.drain(..n);
                        // The Wayland app reading is progress too: a slow
                        // reader isn't cut off once the owner is done.
                        t.deadline = Instant::now() + STALL;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        tracing::debug!(%err, id, "the Wayland app stopped reading the X11 selection");
                        t.fd = None;
                        t.buf.clear();
                        break;
                    }
                }
            }
        } else {
            t.buf.clear();
        }
        if t.chunk_waiting && t.buf.len() < BUFFERED / 2 {
            t.chunk_waiting = false;
            let ok = self.chunk(id, actions).and_then(|()| self.conn.flush().map_err(Into::into));
            if let Err(err) = ok {
                tracing::debug!(%err, id, "cannot ask the X11 selection owner for more");
                self.finish_incoming(id, actions);
                return;
            }
        }
        let Some(t) = self.incoming.get_mut(&id) else { return };
        if t.buf.is_empty() {
            if t.done {
                self.finish_incoming(id, actions);
            } else {
                t.watching = false;
            }
            return;
        }
        if !t.watching {
            if let Some(dup) = t.fd.as_ref().and_then(|f| f.try_clone().ok()) {
                t.watching = true;
                if self.in_ready != Some(id) {
                    actions.push(Action::Watch { id, fd: dup.into(), write: true });
                }
            }
        }
    }

    fn finish_incoming(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(t) = self.incoming.remove(&id) else { return };
        tracing::debug!(id, selection = ?t.selection, "X11 selection transfer ended");
        if t.watching && self.in_ready != Some(id) {
            actions.push(Action::Unwatch(id));
        }
        let _ = self.conn.destroy_window(t.window);
    }

    // ----- Wayland to X11 -------------------------------------------------

    /// An X11 app asks for the selection Helix owns for a Wayland copy.
    fn request(&mut self, e: SelectionRequestEvent, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let _ = actions;
        let dnd = self.dnd.as_ref().filter(|d| d.owner == e.owner && e.selection == self.atoms.XdndSelection).map(|d| (d.types.clone(), d.source.clone()));
        let i = self.sels.iter().position(|s| s.window == e.owner && s.atom == e.selection);
        let (types, timestamp) = match (&dnd, i) {
            (Some((types, _)), _) => (types.clone(), x11rb::CURRENT_TIME),
            (None, Some(i)) => match self.sels[i].wayland.clone() {
                Some(types) => (types, self.sels[i].timestamp),
                None => return self.answer(&e, false),
            },
            (None, None) => return self.answer(&e, false),
        };
        // Obsolete clients give no property: the target is the property.
        let property = if e.property == x11rb::NONE { e.target } else { e.property };
        let e = SelectionRequestEvent { property, ..e };
        let a = self.atoms;
        if e.target == a.TARGETS {
            let mut list = vec![a.TARGETS, a.TIMESTAMP];
            let text = types.iter().any(|t| TEXT_MIMES.contains(&t.as_str()));
            for t in &types {
                let Some(atom) = self.atom_of(t) else { continue };
                if !list.contains(&atom) {
                    list.push(atom);
                }
            }
            if text {
                for atom in [a.UTF8_STRING, u32::from(AtomEnum::STRING), a.TEXT] {
                    if !list.contains(&atom) {
                        list.push(atom);
                    }
                }
            }
            self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &list)?;
            return self.answer(&e, true);
        }
        if e.target == a.TIMESTAMP {
            self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::INTEGER, &[timestamp])?;
            return self.answer(&e, true);
        }
        if e.target == a.DELETE {
            return self.answer(&e, true);
        }
        // The rest once the target's name is known.
        self.with_names(vec![e.target], move |xwm, named, actions| {
            let s = &mut xwm.selections;
            let result = match named.into_iter().next() {
                Some((_, name)) => s.request_named(e, name, types, dnd, i, actions),
                None => s.answer(&e, false),
            };
            if let Err(err) = result.and_then(|()| Ok(s.conn.flush()?)) {
                tracing::debug!(%err, "cannot answer an X11 selection request");
            }
        })
    }

    fn request_named(&mut self, e: SelectionRequestEvent, name: String, types: Vec<String>, dnd: Option<(Vec<String>, Arc<dyn smithay::input::dnd::Source>)>, i: Option<usize>, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let a = self.atoms;
        let property = e.property;
        let Some(mime) = mime_for(&name, &types) else {
            tracing::debug!(target = name, "an X11 app asked for a type the Wayland copy does not have");
            return self.answer(&e, false);
        };
        // An app that asks again before reading its last answer only
        // reads the newest (fcitx, Slack): the older ones end. Not for a
        // drop, whose types are asked for at once (Chromium).
        if dnd.is_none() {
            // The same property: two types asked for at once into
            // different properties are both answered (#234).
            let older: Vec<u64> = self.outgoing.iter().filter(|(_, t)| t.requestor == e.requestor && t.property == property && !t.answered).map(|(id, _)| *id).collect();
            for id in older {
                self.drop_outgoing(id, actions);
            }
        }
        // The type it goes as: TEXT is no encoding of its own.
        let kind = if e.target == a.TEXT { a.UTF8_STRING } else { e.target };
        let (read, write) = pipe()?;
        let id = self.id();
        let watch = read.try_clone().map_err(|e| ReplyOrIdError::ConnectionError(e.into()))?;
        tracing::debug!(id, mime, "the Wayland selection asked for by an X11 app");
        self.outgoing.insert(id, Outgoing { requestor: e.requestor, property, kind, request: e, fd: std::fs::File::from(read), buf: Vec::new(), eof: false, answered: false, incr: false, waiting: false, paused: false, deadline: Instant::now() + STALL });
        match (dnd, i) {
            // The drag's source writes it.
            (Some((_, source)), _) => source.send(&mime, write),
            (None, Some(i)) => {
                let target = self.sels[i].target;
                actions.push(Action::SelectionWanted { selection: target, mime, fd: write });
            }
            (None, None) => {}
        }
        actions.push(Action::Watch { id, fd: watch, write: false });
        Ok(())
    }

    fn answer(&self, e: &SelectionRequestEvent, ok: bool) -> Result<(), ReplyOrIdError> {
        answer(&self.conn, e, ok)
    }
}

/// Tells the requestor its request is done (`ok`) or refused.
fn answer(conn: &RustConnection, e: &SelectionRequestEvent, ok: bool) -> Result<(), ReplyOrIdError> {
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: e.time,
        requestor: e.requestor,
        selection: e.selection,
        target: e.target,
        property: if ok { e.property } else { x11rb::NONE },
    };
    conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
    Ok(())
}

impl Selections {
    /// Reads what the Wayland source wrote, and passes it on.
    fn read_wayland(&mut self, id: u64, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        {
            let t = self.outgoing.get_mut(&id).unwrap();
            let mut chunk = [0u8; CHUNK];
            loop {
                match t.fd.read(&mut chunk) {
                    Ok(0) => {
                        t.eof = true;
                        break;
                    }
                    Ok(n) => {
                        t.buf.extend_from_slice(&chunk[..n]);
                        t.deadline = Instant::now() + STALL;
                        if t.buf.len() >= BUFFERED {
                            // The X11 app is behind: read on once it took
                            // most of it (send_next).
                            t.paused = true;
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => return Err(ReplyOrIdError::ConnectionError(err.into())),
                }
            }
        }
        self.progress(id, actions)?;
        self.conn.flush()?;
        Ok(())
    }

    fn progress(&mut self, id: u64, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let a = self.atoms;
        let Some(t) = self.outgoing.get_mut(&id) else { return Ok(()) };
        if !t.incr {
            if t.buf.len() > CHUNK {
                // Incrementally (ICCCM 2.7.2): the requestor deletes each
                // chunk to ask for the next, and the owner listens for that
                // itself, keeping whatever it listens for already (#193).
                // What the window manager selects on any client window
                // (managed ones: property and focus changes; focus changes
                // on others are ignored), so it is kept without asking.
                if t.requestor != self.root {
                    self.conn.change_window_attributes(t.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE))?;
                }
                let hint = u32::try_from(t.buf.len()).unwrap_or(u32::MAX);
                self.conn.change_property32(PropMode::REPLACE, t.requestor, t.property, a.INCR, &[hint])?;
                t.incr = true;
                t.waiting = true;
                t.answered = true;
                answer(&self.conn, &t.request, true)?;
            } else if t.eof {
                self.conn.change_property8(PropMode::REPLACE, t.requestor, t.property, t.kind, &t.buf)?;
                answer(&self.conn, &t.request, true)?;
                self.outgoing.remove(&id);
                if self.in_ready != Some(id) {
                    actions.push(Action::Unwatch(id));
                }
            }
            return Ok(());
        }
        if !t.waiting {
            self.send_next(id, actions)?;
        }
        Ok(())
    }

    /// The next chunk to the requestor, or the empty one ending it.
    fn send_next(&mut self, id: u64, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let Some(t) = self.outgoing.get_mut(&id) else { return Ok(()) };
        if !t.buf.is_empty() {
            let n = t.buf.len().min(CHUNK);
            self.conn.change_property8(PropMode::REPLACE, t.requestor, t.property, t.kind, &t.buf[..n])?;
            t.buf.drain(..n);
            t.waiting = true;
            if t.paused && t.buf.len() < BUFFERED / 2 {
                // Caught up: read the Wayland source again (from its own
                // readiness, it just keeps being watched).
                t.paused = false;
                if self.in_ready != Some(id) {
                    if let Ok(fd) = t.fd.try_clone() {
                        actions.push(Action::Watch { id, fd: fd.into(), write: false });
                    }
                }
            }
        } else if t.eof {
            self.conn.change_property8(PropMode::REPLACE, t.requestor, t.property, t.kind, &[])?;
            tracing::debug!(id, "selection transfer to X11 ended");
            self.outgoing.remove(&id);
            if self.in_ready != Some(id) {
                actions.push(Action::Unwatch(id));
            }
        }
        Ok(())
    }

    /// A transfer to X11 that won't finish: refused if it wasn't answered
    /// yet.
    fn drop_outgoing(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(t) = self.outgoing.remove(&id) else { return };
        if !t.answered {
            let _ = self.answer(&t.request, false);
        }
        if !t.eof && self.in_ready != Some(id) {
            actions.push(Action::Unwatch(id));
        }
    }
}

/// The Wayland type an X11 target asks for, if the copy has it: its own
/// name, or for X11's text targets the copy's text.
fn mime_for(target: &str, types: &[String]) -> Option<String> {
    if types.iter().any(|t| t == target) {
        return Some(target.to_owned());
    }
    if matches!(target, "UTF8_STRING" | "STRING" | "TEXT" | "text/plain" | "text/plain;charset=utf-8") {
        return TEXT_MIMES.iter().find(|m| types.iter().any(|t| t == *m)).map(|m| (*m).to_owned());
    }
    None
}

/// A pipe whose read end (ours) doesn't block and whose write end (the
/// Wayland app's) does: an app writing in one blocking call past the
/// pipe's buffer would otherwise fail (#193).
fn pipe() -> Result<(OwnedFd, OwnedFd), ReplyOrIdError> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for both ends.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(ReplyOrIdError::ConnectionError(std::io::Error::last_os_error().into()));
    }
    // SAFETY: pipe2 just made these and nothing else owns them.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_nonblocking(&read);
    Ok((read, write))
}

fn set_nonblocking(fd: &OwnedFd) {
    // SAFETY: fcntl on a file descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mime_for;

    #[test]
    fn x11_text_targets_get_the_copys_text() {
        let types = vec!["text/plain;charset=utf-8".to_owned(), "text/html".to_owned()];
        for target in ["UTF8_STRING", "STRING", "TEXT", "text/plain"] {
            assert_eq!(mime_for(target, &types).as_deref(), Some("text/plain;charset=utf-8"), "{target}");
        }
        assert_eq!(mime_for("text/html", &types).as_deref(), Some("text/html"));
        assert_eq!(mime_for("image/png", &types), None);
        assert_eq!(mime_for("STRING", &["image/png".to_owned()]), None);
    }
}

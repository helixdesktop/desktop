//! The clipboard stays after the app that copied exits
//! (helixdesktop/desktop#284; helixdesktop/spec `contracts/wayland/README.md`,
//! "The clipboard after its app exits"), as in GNOME (mutter's
//! `MetaClipboardManager`).
//!
//! Whenever an app, Wayland or X11, sets the clipboard, Helix reads it and
//! keeps a copy: its best text type (UTF-8 first) up to [`TEXT_LIMIT`],
//! and its best image type (PNG first) up to [`IMAGE_LIMIT`]. Each read
//! runs on a thread of its own, never on the compositor's, gives up when
//! the app stops writing for [`STALL`] (or after [`TOTAL`]), and a newer
//! selection cancels it. When the owner goes away (its Wayland client
//! disconnects, the X11 owner's window or connection is gone) and the copy
//! was complete, Helix owns the clipboard with what it kept, for Wayland
//! and X11 apps alike. A clipboard an app cleared on purpose (Wayland
//! `set_selection(NULL)`, X11 `SetSelectionOwner(None)`) stays empty.
//!
//! Not kept: the primary selection (nor does mutter), types other than
//! text and images, a type over its limit, and a copy marked secret
//! (`x-kde-passwordManagerHint`, as password managers mark what they copy;
//! the shell's agent clipboard honours it too): that goes with its app.
//!
//! X11 apps also find a clipboard manager (`CLIPBOARD_MANAGER`): GTK 3
//! and Qt apps ask it to save the clipboard (`SAVE_TARGETS`) before they
//! exit, and are answered once the copy is kept ([`super::x11ping`] owns
//! it, for either window manager). And an X11 app that takes the clipboard
//! but never says what it offers (its `TARGETS` never answered) doesn't
//! leave the last copy offered to Wayland apps (#270).
//!
//! What the clipboard holds is never logged, only its types' count and size.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel::Sender;
use smithay::wayland::selection::data_device::{clear_data_device_selection, request_data_device_client_selection, set_data_device_selection};
use smithay::wayland::selection::{SelectionSource, SelectionTarget};

use super::{Helix, ServerSelection};

/// The most text kept (as mutter).
pub const TEXT_LIMIT: usize = 4 << 20;
/// The most of an image kept (mutter keeps up to 200 MiB; Helix holds
/// less in the compositor, as much as an agent may copy).
pub const IMAGE_LIMIT: usize = 16 << 20;
/// A read gives up when the owner wrote nothing for this long…
const STALL: Duration = Duration::from_secs(5);
/// …or once it took this long in all.
const TOTAL: Duration = Duration::from_secs(30);
/// How long an X11 app taking the clipboard may take to say what it
/// offers (answer `TARGETS`) before the last copy stops being offered.
const TARGETS_WAIT: Duration = Duration::from_secs(3);
/// The longest an X11 app's `SAVE_TARGETS` waits for its answer (GTK 3
/// waits 10 seconds).
const SAVE_WAIT: Duration = Duration::from_secs(8);

/// The type password managers mark a secret copy with.
pub const SECRET_HINT: &str = "x-kde-passwordManagerHint";

/// Text types, best first: UTF-8 ones, then plain text of no charset.
const TEXT_TYPES: [&str; 6] = ["text/plain;charset=utf-8", "text/plain;charset=UTF-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];
/// Of those, the UTF-8 ones (offered again under every text type).
const UTF8_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain;charset=UTF-8", "UTF8_STRING"];
/// Image types, best first (mutter's, the other way round).
const IMAGE_TYPES: [&str; 7] = ["image/png", "image/svg+xml", "image/webp", "image/jpeg", "image/gif", "image/bmp", "image/tiff"];

/// The types to keep of a copy offering `types`, each with its limit: its
/// best text type and its best image type.
pub fn pick(types: &[String]) -> Vec<(String, usize)> {
    let best = |list: &[&str]| list.iter().find(|t| types.iter().any(|o| o == *t)).map(|t| (*t).to_owned());
    best(&TEXT_TYPES).map(|t| (t, TEXT_LIMIT)).into_iter().chain(best(&IMAGE_TYPES).map(|t| (t, IMAGE_LIMIT))).collect()
}

/// Whether a copy offering `types` is marked secret.
pub fn secret(types: &[String]) -> bool {
    types.iter().any(|t| t == SECRET_HINT)
}

/// What was kept, by the type it was read as.
pub type Kept = Arc<Vec<(String, Arc<Vec<u8>>)>>;

/// The types Helix offers what it kept as: UTF-8 text as every text type
/// (as the shell's copies), other text as plain text, images as theirs.
pub fn offered(kept: &[(String, Arc<Vec<u8>>)]) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    for (mime, _) in kept {
        let as_: Vec<&str> = if UTF8_TYPES.contains(&mime.as_str()) {
            super::capture::TEXT_MIMES.to_vec()
        } else if TEXT_TYPES.contains(&mime.as_str()) {
            vec!["text/plain"]
        } else {
            vec![mime.as_str()]
        };
        for t in as_ {
            if !types.iter().any(|x| x == t) {
                types.push(t.to_owned());
            }
        }
    }
    types
}

/// What was kept for `mime`, if any (see [`offered`]).
fn kept_as(kept: &[(String, Arc<Vec<u8>>)], mime: &str) -> Option<Arc<Vec<u8>>> {
    let text = super::capture::TEXT_MIMES.contains(&mime);
    kept.iter()
        .find(|(m, _)| m == mime)
        .or_else(|| kept.iter().find(|(m, _)| text && UTF8_TYPES.contains(&m.as_str())))
        .or_else(|| kept.iter().find(|(m, _)| matches!(mime, "text/plain" | "STRING" | "TEXT") && TEXT_TYPES.contains(&m.as_str())))
        .map(|(_, d)| d.clone())
}

/// Write what was kept as `mime` into `fd` (nothing, `fd` closed, if it
/// was not kept as that).
pub fn serve(kept: &Kept, mime: &str, fd: OwnedFd) {
    let Some(data) = kept_as(kept, mime) else { return };
    std::thread::spawn(move || {
        use std::io::Write;
        if let Err(err) = std::fs::File::from(fd).write_all(&data) {
            tracing::debug!(%err, "kept clipboard not delivered");
        }
    });
}

/// Who owns the clipboard being kept.
#[derive(Debug, Clone, Default)]
pub enum Owner {
    #[default]
    None,
    Wayland(SelectionSource),
    X11,
}

/// A read of the clipboard ended: its data, or why not.
pub struct Done {
    serial: u64,
    mime: String,
    data: Result<Vec<u8>, &'static str>,
}

/// An X11 app's `SAVE_TARGETS`, waiting to be answered.
struct Save {
    request: super::x11ping::SaveRequest,
    until: Instant,
}

#[derive(Default)]
pub struct Keep {
    /// Bumped with every selection: reads of an earlier one are stale.
    serial: u64,
    owner: Owner,
    /// Who set it (said again when Helix takes it over).
    source: super::clipboard::Source,
    /// Reads under way.
    reading: usize,
    kept: Vec<(String, Arc<Vec<u8>>)>,
    cancel: Option<Arc<AtomicBool>>,
    /// Helix owns the clipboard with what it kept.
    taken: bool,
    saves: Vec<Save>,
    tx: Option<Sender<Done>>,
    /// An X11 app took the clipboard: whether it says what it offers in
    /// time (its window, and since when).
    targets: Option<(u32, Instant)>,
}

/// Never the content.
impl std::fmt::Debug for Keep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keep").field("serial", &self.serial).field("reading", &self.reading).field("kept", &self.kept.len()).field("taken", &self.taken).finish_non_exhaustive()
    }
}

impl Keep {
    /// Nothing kept, nothing being read.
    fn reset(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.owner = Owner::None;
        self.reading = 0;
        self.kept.clear();
        self.taken = false;
    }
}

impl Helix {
    /// A Wayland or X11 app set the clipboard, offering `types`: keep a
    /// copy of it.
    pub(super) fn keep_new_selection(&mut self, owner: Owner, types: &[String], source: super::clipboard::Source) {
        // An X11 app's SAVE_TARGETS waits on: it may be this copy's.
        self.clipboard.keep.reset();
        let keep = &mut self.clipboard.keep;
        keep.owner = owner.clone();
        keep.source = source;
        if secret(types) {
            tracing::info!("clipboard marked secret: not kept");
            self.keep_answer_saves();
            return;
        }
        let picked = pick(types);
        if picked.is_empty() {
            tracing::debug!(types = types.len(), "nothing of the clipboard to keep");
            self.keep_answer_saves();
            return;
        }
        if matches!(owner, Owner::Wayland(_)) {
            // Smithay makes it the selection once this handler returns:
            // read it from there then.
            let serial = self.clipboard.keep.serial;
            self.loop_handle.insert_idle(move |state| {
                if state.clipboard.keep.serial == serial {
                    state.keep_read(picked);
                }
            });
            return;
        }
        self.keep_read(picked);
    }

    /// Read `picked` (types and their limits) of the clipboard, each on a
    /// thread of its own.
    fn keep_read(&mut self, picked: Vec<(String, usize)>) {
        let owner = self.clipboard.keep.owner.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.clipboard.keep.cancel = Some(cancel.clone());
        let tx = self.keep_sender();
        let serial = self.clipboard.keep.serial;
        for (mime, limit) in picked {
            let Some((read, write)) = pipe() else { continue };
            match &owner {
                Owner::Wayland(_) => {
                    if let Err(err) = request_data_device_client_selection(&self.seat, mime.clone(), write) {
                        tracing::debug!(%err, "cannot read the clipboard to keep it");
                        continue;
                    }
                }
                Owner::X11 => self.x11_paste_selection(SelectionTarget::Clipboard, mime.clone(), write),
                Owner::None => continue,
            }
            self.clipboard.keep.reading += 1;
            let (tx, cancel) = (tx.clone(), cancel.clone());
            std::thread::spawn(move || {
                let data = read_all(read, limit, &cancel);
                let _ = tx.send(Done { serial, mime, data });
            });
        }
        if self.clipboard.keep.reading == 0 {
            self.keep_answer_saves();
        }
    }

    /// The clipboard is no longer what was kept (cleared on purpose, or
    /// set by Helix, the shell or an agent): forget it.
    pub(super) fn keep_forget(&mut self) {
        self.clipboard.keep.reset();
        self.keep_answer_saves();
    }

    fn keep_sender(&mut self) -> Sender<Done> {
        if let Some(tx) = self.clipboard.keep.tx.clone() {
            return tx;
        }
        let (tx, rx) = smithay::reexports::calloop::channel::channel::<Done>();
        let inserted = self.loop_handle.insert_source(rx, |event, _, state| {
            if let smithay::reexports::calloop::channel::Event::Msg(done) = event {
                state.keep_done(done);
            }
        });
        if let Err(err) = inserted {
            tracing::warn!(%err, "cannot keep the clipboard");
        }
        self.clipboard.keep.tx = Some(tx.clone());
        tx
    }

    fn keep_done(&mut self, done: Done) {
        let keep = &mut self.clipboard.keep;
        if done.serial != keep.serial || keep.reading == 0 {
            return;
        }
        keep.reading -= 1;
        match done.data {
            Ok(data) if !data.is_empty() => keep.kept.push((done.mime, Arc::new(data))),
            Ok(_) => tracing::debug!("the clipboard gave nothing to keep"),
            Err(why) => tracing::info!(why, "a clipboard type not kept"),
        }
        if keep.reading > 0 {
            return;
        }
        // Text first, as offered.
        keep.kept.sort_by_key(|(m, _)| !TEXT_TYPES.contains(&m.as_str()));
        let bytes: usize = keep.kept.iter().map(|(_, d)| d.len()).sum();
        tracing::info!(types = keep.kept.len(), bytes, "clipboard kept");
        self.keep_answer_saves();
    }

    /// The clipboard's owner went away (not clearing it): Helix takes it
    /// over with what it kept, if it kept it all.
    pub(super) fn keep_owner_gone(&mut self) {
        let keep = &self.clipboard.keep;
        if keep.taken || matches!(keep.owner, Owner::None) {
            return;
        }
        let wayland = matches!(keep.owner, Owner::Wayland(_));
        if keep.reading > 0 || keep.kept.is_empty() {
            if keep.reading > 0 {
                tracing::info!("the clipboard's app went away before its copy was kept");
            }
            self.keep_forget();
            if wayland {
                // Smithay dropped the selection without a word: X11 apps
                // and the shell stop being offered it too.
                self.x11_offer_selection(SelectionTarget::Clipboard, None);
                self.clipboard_changed(Vec::new(), super::clipboard::Source::default());
            }
            return;
        }
        let kept: Kept = Arc::new(keep.kept.clone());
        let source = keep.source.clone();
        let types = offered(&kept);
        tracing::info!(types = types.len(), "the clipboard's app went away; Helix keeps its copy");
        set_data_device_selection(&self.display_handle, &self.seat, types.clone(), ServerSelection::Kept(kept));
        self.x11_offer_selection(SelectionTarget::Clipboard, Some(types.clone()));
        self.clipboard_changed(types, source);
        self.clipboard.keep.taken = true;
    }

    /// What Helix's own X connection saw of CLIPBOARD's owner.
    pub(super) fn keep_x11_owner(&mut self, owner: u32, ours: bool, cleared: bool) {
        let keep = &mut self.clipboard.keep;
        keep.targets = (owner != 0 && !ours).then(|| (owner, Instant::now()));
        if owner != 0 && !ours && (keep.taken || !matches!(keep.owner, Owner::X11)) {
            // An X11 app took it from a Wayland app's copy (or Helix's):
            // that one is no longer the clipboard, whatever becomes of its
            // app. (An X11 owner's copy stays until the new one's types
            // replace it: they may have come already.)
            keep.reset();
            return;
        }
        if owner != 0 || !matches!(keep.owner, Owner::X11) || keep.taken {
            return;
        }
        if cleared {
            tracing::info!("an X11 app cleared the clipboard; it stays empty");
            self.keep_forget();
        } else {
            self.keep_owner_gone();
        }
    }

    /// The X11 app owning CLIPBOARD said what it offers (or refused to).
    pub(super) fn keep_x11_answered(&mut self, owner: u32) {
        let keep = &mut self.clipboard.keep;
        if keep.targets.is_some_and(|(o, _)| o == owner) {
            keep.targets = None;
        }
    }

    /// An X11 app asks the clipboard manager to save the clipboard
    /// (`SAVE_TARGETS`): answered once it is kept. (GTK and Qt ask only
    /// while they own it: an X11 owner whose copy Helix hasn't heard of
    /// yet is waited for too.)
    pub(super) fn keep_save_targets(&mut self, request: super::x11ping::SaveRequest) {
        let keep = &mut self.clipboard.keep;
        let pending = keep.reading > 0 || keep.targets.is_some() || !matches!(keep.owner, Owner::X11);
        keep.saves.push(Save { request, until: Instant::now() + SAVE_WAIT });
        if !pending {
            self.keep_answer_saves();
        }
    }

    /// Answer the `SAVE_TARGETS` waiting: saved if Helix holds a copy of
    /// the clipboard.
    fn keep_answer_saves(&mut self) {
        let keep = &mut self.clipboard.keep;
        if keep.saves.is_empty() {
            return;
        }
        let ok = !keep.kept.is_empty() && keep.reading == 0;
        let saves = std::mem::take(&mut keep.saves);
        for save in saves {
            tracing::info!(saved = ok, "an X11 app's SAVE_TARGETS answered");
            if let Some(ping) = self.x11ping.as_ref() {
                ping.answer_save(&save.request, ok);
            }
        }
    }

    /// Every tick: a Wayland owner gone, an X11 owner silent too long,
    /// `SAVE_TARGETS` waiting too long.
    pub fn keep_tick(&mut self) {
        let gone = matches!(&self.clipboard.keep.owner, Owner::Wayland(source) if source.client().is_none());
        if gone && !self.clipboard.keep.taken {
            self.keep_owner_gone();
        }
        let now = Instant::now();
        if self.clipboard.keep.targets.is_some_and(|(_, since)| now >= since + TARGETS_WAIT) {
            self.clipboard.keep.targets = None;
            // It owns the X clipboard but offers nothing anyone can paste:
            // the last copy is no longer the clipboard (#270).
            tracing::info!("an X11 app took the clipboard but never said what it offers; the clipboard is empty");
            clear_data_device_selection(&self.display_handle, &self.seat);
            self.x11_offer_selection(SelectionTarget::Clipboard, None);
            self.clipboard_changed(Vec::new(), super::clipboard::Source::default());
            self.keep_forget();
        }
        if self.clipboard.keep.saves.iter().any(|s| now >= s.until) {
            let keep = &mut self.clipboard.keep;
            let (late, waiting): (Vec<Save>, Vec<Save>) = std::mem::take(&mut keep.saves).into_iter().partition(|s| now >= s.until);
            keep.saves = waiting;
            for save in late {
                tracing::info!("an X11 app's SAVE_TARGETS answered: not saved in time");
                if let Some(ping) = self.x11ping.as_ref() {
                    ping.answer_save(&save.request, false);
                }
            }
        }
    }

    /// Whether Helix's own X connection watches CLIPBOARD (else an X11
    /// owner's going is taken from the window manager, cleared or not).
    pub(super) fn keep_watches_x11(&self) -> bool {
        self.x11ping.as_ref().is_some_and(|p| p.watches_clipboard)
    }
}

/// Reads `fd` to its end, at most `limit` bytes; gives up when cancelled,
/// or when the writer stalls.
fn read_all(fd: OwnedFd, limit: usize, cancel: &AtomicBool) -> Result<Vec<u8>, &'static str> {
    use std::io::Read;
    let raw = fd.as_raw_fd();
    let mut file = std::fs::File::from(fd);
    let (start, mut last) = (Instant::now(), Instant::now());
    let mut data = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("replaced");
        }
        let now = Instant::now();
        if now >= last + STALL || now >= start + TOTAL {
            return Err("timed out");
        }
        let mut poll = libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, for a descriptor `file` owns.
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready <= 0 {
            continue;
        }
        match file.read(&mut chunk) {
            Ok(0) => return Ok(data),
            Ok(n) => {
                if data.len() + n > limit {
                    return Err("too large");
                }
                data.extend_from_slice(&chunk[..n]);
                last = Instant::now();
            }
            Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {}
            Err(_) => return Err("failed"),
        }
    }
}

/// A pipe: the read end Helix's, not blocking; the write end the owner's.
fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for both ends.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        tracing::warn!(err = %std::io::Error::last_os_error(), "cannot make a pipe to keep the clipboard");
        return None;
    }
    // SAFETY: pipe2 just made these and nothing else owns them.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(read.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    Some((read, write))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn best_text_and_best_image() {
        let picked = pick(&types(&["text/html", "text/plain", "text/plain;charset=utf-8", "image/jpeg", "image/png", "application/x-thing"]));
        assert_eq!(picked, [("text/plain;charset=utf-8".to_owned(), TEXT_LIMIT), ("image/png".to_owned(), IMAGE_LIMIT)]);
        assert_eq!(pick(&types(&["UTF8_STRING", "STRING"])), [("UTF8_STRING".to_owned(), TEXT_LIMIT)]);
        assert!(pick(&types(&["text/html", "application/octet-stream"])).is_empty());
        assert!(secret(&types(&["text/plain", SECRET_HINT])) && !secret(&types(&["text/plain"])));
    }

    #[test]
    fn kept_text_is_offered_as_every_text_type() {
        let kept = vec![("UTF8_STRING".to_owned(), Arc::new(b"words".to_vec())), ("image/png".to_owned(), Arc::new(vec![1, 2]))];
        let o = offered(&kept);
        for t in super::super::capture::TEXT_MIMES {
            assert!(o.iter().any(|x| x == t), "{t} in {o:?}");
            assert_eq!(kept_as(&kept, t).as_deref().map(Vec::as_slice), Some(&b"words"[..]));
        }
        assert!(o.iter().any(|x| x == "image/png"));
        assert_eq!(kept_as(&kept, "image/png").as_deref().map(Vec::as_slice), Some(&[1u8, 2][..]));
        assert!(kept_as(&kept, "image/jpeg").is_none());
        // Plain text of no charset stays that.
        let plain = vec![("text/plain".to_owned(), Arc::new(b"x".to_vec()))];
        assert_eq!(offered(&plain), ["text/plain"]);
        assert!(kept_as(&plain, "text/plain;charset=utf-8").is_none());
    }

    #[test]
    fn a_read_over_its_limit_is_not_kept() {
        let (read, write) = pipe().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut std::fs::File::from(write), &[7u8; 300_000]);
        });
        assert_eq!(read_all(read, 200_000, &AtomicBool::new(false)), Err("too large"));
        writer.join().unwrap();
        let (read, write) = pipe().unwrap();
        std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut std::fs::File::from(write), b"fits");
        });
        assert_eq!(read_all(read, 200_000, &AtomicBool::new(false)).as_deref(), Ok(&b"fits"[..]));
    }
}

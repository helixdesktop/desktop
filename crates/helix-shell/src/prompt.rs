//! The prompt dialog (helixdesktop/spec
//! `org.freedesktop.PolicyKit1.AuthenticationAgent.md`, "Prompt dialog"):
//! asking the user for a password from the D-Bus side.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::auth::Secret;
use crate::UiEvent;

/// What the dialog shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prompt {
    pub title: String,
    pub message: String,
    /// Smaller text below the message.
    pub description: String,
    /// A plain text field above the password: its label and first text.
    pub entry: Option<(String, String)>,
    /// The password field's label; `None`: no field.
    pub password: Option<String>,
    /// A second field (confirming a new password), its label.
    pub confirm: Option<String>,
    /// A check box: its label and state.
    pub choice: Option<(String, bool)>,
    /// More check boxes below it (labels and states).
    pub checks: Vec<(String, bool)>,
    /// Rows to pick one of, and the one picked at first.
    pub list: Vec<String>,
    pub selected: usize,
    /// Rows can be picked together (clicks and Space toggle them).
    pub multiple: bool,
    /// Clicking the current row again continues.
    pub activate_rows: bool,
    /// With `multiple`: rows that cannot be picked (they act as without
    /// `multiple`).
    pub unpickable: Vec<usize>,
    /// A picture below the text (e.g. a background to preview).
    pub picture: Option<std::sync::Arc<helix_draw::Image>>,
    /// An icon above the title (48 pixels).
    pub icon: Option<std::sync::Arc<helix_draw::Image>>,
    /// Who is authenticating: the name shown and the picture (none: the
    /// name's initials), as an avatar with the name above the fields.
    pub identity: Option<(String, Option<std::sync::Arc<helix_draw::Image>>)>,
    /// Shown in a warning color, e.g. after a wrong password.
    pub warning: String,
    /// Earlier entries, oldest first: Up and Down in the text field go
    /// through them.
    pub history: Vec<String>,
    /// Tab in the text field completes a command name from `$PATH`.
    pub complete_commands: bool,
    pub continue_label: String,
    pub cancel_label: String,
}

/// The user's answer.
pub enum Answer {
    /// `picked`: with `multiple`, the rows picked (else none).
    Continue { entry: Option<String>, password: Option<Secret>, confirm: Option<Secret>, choice: bool, checks: Vec<bool>, selected: usize, picked: Vec<usize> },
    Cancel,
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::Continue { choice, .. } => write!(f, "Continue {{ choice: {choice} }}"),
            Answer::Cancel => write!(f, "Cancel"),
        }
    }
}

/// A prompt and where its answer goes.
pub struct Request {
    pub prompt: Prompt,
    pub reply: oneshot::Sender<Answer>,
}

/// A [`Request`] that can travel in a [`UiEvent`].
#[derive(Clone)]
pub struct PromptSlot(Arc<Mutex<Option<Request>>>);

impl std::fmt::Debug for PromptSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PromptSlot")
    }
}

impl PromptSlot {
    pub fn take(&self) -> Option<Request> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for PromptSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Show `prompt` (or update the one shown) and wait for the answer. The
/// dialog stays up, busy, until the next prompt or [`done`].
pub async fn ask(ui: &mpsc::UnboundedSender<UiEvent>, prompt: Prompt) -> Answer {
    let (reply, answer) = oneshot::channel();
    let slot = PromptSlot(Arc::new(Mutex::new(Some(Request { prompt, reply }))));
    if ui.send(UiEvent::Prompt(slot)).is_err() {
        return Answer::Cancel;
    }
    answer.await.unwrap_or(Answer::Cancel)
}

/// Who holds the turn: where it was taken, since when, and which hold.
type Holder = (&'static std::panic::Location<'static>, std::time::Instant, u64);

static HOLDER: Mutex<Option<Holder>> = Mutex::new(None);

/// How long a flow may wait for the turn before the holder is named in
/// the log (a holder that never gives it back blocks every prompt and file
/// chooser after it: helixdesktop/desktop#221, #242).
const WAIT_WARNING: std::time::Duration = std::time::Duration::from_secs(60);

/// The turn, while held (see [`turn`]); dropping it gives it back.
pub struct Turn {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    id: u64,
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut holder = HOLDER.lock().unwrap_or_else(|e| e.into_inner());
        if holder.is_some_and(|h| h.2 == self.id) {
            *holder = None;
        }
    }
}

/// Whole prompt flows (a polkit authentication, a keyring client's
/// prompts) take turns: hold this while one runs. A flow kept waiting
/// long logs who holds the turn, and for how long.
#[track_caller]
pub fn turn() -> impl std::future::Future<Output = Turn> {
    static FLOW: std::sync::LazyLock<Arc<tokio::sync::Mutex<()>>> = std::sync::LazyLock::new(Default::default);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let here = std::panic::Location::caller();
    async move {
        let lock = FLOW.clone().lock_owned();
        tokio::pin!(lock);
        let waited = std::time::Instant::now();
        let guard = loop {
            tokio::select! {
                guard = &mut lock => break guard,
                _ = tokio::time::sleep(WAIT_WARNING) => {
                    let holder = *HOLDER.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some((at, since, _)) = holder {
                        tracing::warn!(waiting = %here, waited_s = waited.elapsed().as_secs(), held_by = %at, held_s = since.elapsed().as_secs(), "a prompt is waiting for the turn");
                    }
                }
            }
        };
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *HOLDER.lock().unwrap_or_else(|e| e.into_inner()) = Some((here, std::time::Instant::now(), id));
        Turn { _guard: guard, id }
    }
}

/// Who holds the turn now (where it was taken), if anyone.
pub fn turn_holder() -> Option<String> {
    HOLDER.lock().unwrap_or_else(|e| e.into_inner()).map(|h| h.0.to_string())
}

/// Close the prompt dialog.
pub fn done(ui: &mpsc::UnboundedSender<UiEvent>) {
    let _ = ui.send(UiEvent::PromptDone);
}

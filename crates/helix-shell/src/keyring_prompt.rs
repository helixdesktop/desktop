//! gcr's system prompter (helixdesktop/spec
//! `org.gnome.keyring.internal.Prompter.md`): keyring and other gcr
//! password prompts, in the shell's prompt dialog.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

use crate::prompt::{self, Answer, Prompt};
use crate::secret_exchange::Exchange;
use crate::UiEvent;
use helix_i18n::{n_, tr};

pub const BUS_NAME: &str = "org.gnome.keyring.SystemPrompter";
pub const OBJECT_PATH: &str = "/org/gnome/keyring/Prompter";
const CALLBACK: &str = "org.gnome.keyring.internal.Prompter.Callback";
const MISMATCH: &str = n_("The passwords do not match.");

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.gnome.keyring.Prompter")]
pub enum PrompterError {
    #[zbus(error)]
    ZBus(zbus::Error),
    InProgress(String),
    Failed(String),
}

/// A client: its unique bus name and callback object.
type Client = (String, OwnedObjectPath);

/// The prompt properties so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Props {
    pub title: String,
    pub message: String,
    pub description: String,
    pub warning: String,
    pub password_new: bool,
    pub choice_label: String,
    pub choice_chosen: bool,
    pub continue_label: String,
    pub cancel_label: String,
}

impl Props {
    pub fn update(&mut self, changed: &HashMap<String, OwnedValue>) {
        for (key, value) in changed {
            let text = || match &**value {
                Value::Str(s) => s.to_string(),
                _ => String::new(),
            };
            let flag = || matches!(&**value, Value::Bool(true));
            match key.as_str() {
                "title" => self.title = text(),
                "message" => self.message = text(),
                "description" => self.description = text(),
                "warning" => self.warning = text(),
                "password-new" => self.password_new = flag(),
                "choice-label" => self.choice_label = text(),
                "choice-chosen" => self.choice_chosen = flag(),
                "continue-label" => self.continue_label = text(),
                "cancel-label" => self.cancel_label = text(),
                _ => {}
            }
        }
    }

    /// The dialog for a prompt of `kind` (`password` or `confirm`).
    pub fn prompt(&self, password: bool) -> Prompt {
        let or = |s: &str, d: &str| if s.is_empty() { d.to_owned() } else { s.to_owned() };
        Prompt {
            title: or(&self.title, tr("Unlock")),
            message: self.message.clone(),
            description: self.description.clone(),
            password: password.then(|| tr("Password").to_owned()),
            confirm: (password && self.password_new).then(|| tr("Confirm").to_owned()),
            choice: (!self.choice_label.is_empty()).then(|| (self.choice_label.clone(), self.choice_chosen)),
            warning: self.warning.clone(),
            continue_label: or(&self.continue_label, tr("Continue")),
            cancel_label: or(&self.cancel_label, tr("Cancel")),
            ..Prompt::default()
        }
    }
}

struct Active {
    client: Client,
    /// Tells this activation from a later one of the same client.
    generation: u64,
    exchange: Exchange,
    props: Props,
    /// Counts the client's calls; a wait for the client is over when it
    /// changes (see [`await_client`]).
    calls: u64,
    _turn: prompt::Turn,
}

#[derive(Default)]
struct State {
    waiting: VecDeque<Client>,
    active: Option<Active>,
    starting: bool,
    generation: u64,
}

type Shared = Arc<Mutex<State>>;

/// How long a client may take to answer a PromptReady (with another
/// PerformPrompt, or StopPrompting) before its prompting is ended: while it
/// is active it holds the prompt turn, and nothing else may prompt
/// (helixdesktop/desktop#242). gcr clients answer at once.
fn client_timeout() -> std::time::Duration {
    std::env::var("HELIX_KEYRING_CLIENT_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).map_or(std::time::Duration::from_secs(30), std::time::Duration::from_millis)
}

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

async fn call_ready(conn: &Connection, client: &Client, reply: &str, properties: HashMap<&str, Value<'_>>, exchange: &str) -> zbus::Result<()> {
    let body = (reply, properties, exchange);
    let call = conn.call_method(Some(client.0.as_str()), client.1.as_str(), Some(CALLBACK), "PromptReady", &body);
    match tokio::time::timeout(client_timeout(), call).await {
        Ok(reply) => reply.map(drop),
        Err(_) => Err(zbus::Error::Failure("the client did not take PromptReady".into())),
    }
}

/// End `generation`'s prompting if it is still the active one: the dialog
/// closes, the client is told, and the next client gets its turn.
fn end_active(state: &Shared, ui: &mpsc::UnboundedSender<UiEvent>, conn: &Connection, generation: u64) {
    let client = {
        let mut s = lock(state);
        if s.active.as_ref().is_none_or(|a| a.generation != generation) {
            return;
        }
        s.active.take().map(|a| a.client)
    };
    prompt::done(ui);
    if let Some(client) = client {
        let conn2 = conn.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(client_timeout(), call_done(&conn2, &client)).await;
        });
    }
    advance(state.clone(), conn.clone(), ui.clone());
}

/// After a PromptReady: unless the client calls again within the timeout,
/// its prompting ends.
fn await_client(state: &Shared, ui: &mpsc::UnboundedSender<UiEvent>, conn: &Connection, generation: u64) {
    let Some(calls) = lock(state).active.as_ref().filter(|a| a.generation == generation).map(|a| a.calls) else { return };
    let (state, ui, conn) = (state.clone(), ui.clone(), conn.clone());
    tokio::spawn(async move {
        tokio::time::sleep(client_timeout()).await;
        let idle = lock(&state).active.as_ref().is_some_and(|a| a.generation == generation && a.calls == calls);
        if idle {
            tracing::warn!("keyring prompt client stopped answering; ending its prompting");
            end_active(&state, &ui, &conn, generation);
        }
    });
}

async fn call_done(conn: &Connection, client: &Client) {
    let _ = conn.call_method(Some(client.0.as_str()), client.1.as_str(), Some(CALLBACK), "PromptDone", &()).await;
}

/// Start the next waiting client, if none is active.
fn advance(state: Shared, conn: Connection, ui: mpsc::UnboundedSender<UiEvent>) {
    {
        let mut s = lock(&state);
        if s.active.is_some() || s.starting || s.waiting.is_empty() {
            return;
        }
        s.starting = true;
    }
    tokio::spawn(async move {
        let turn = prompt::turn().await;
        let (client, begin, generation) = {
            let mut s = lock(&state);
            s.starting = false;
            let Some(client) = s.waiting.pop_front() else { return };
            let exchange = Exchange::new();
            let begin = exchange.begin();
            s.generation += 1;
            let generation = s.generation;
            s.active = Some(Active { client: client.clone(), generation, exchange, props: Props::default(), calls: 0, _turn: turn });
            (client, begin, generation)
        };
        match call_ready(&conn, &client, "", HashMap::new(), &begin).await {
            Ok(()) => await_client(&state, &ui, &conn, generation),
            Err(err) => {
                tracing::info!(%err, client = client.0, "keyring prompt client is gone");
                end_active(&state, &ui, &conn, generation);
            }
        }
    });
}

/// End `client`'s prompting (stopped, or gone from the bus).
fn finish(state: &Shared, ui: &mpsc::UnboundedSender<UiEvent>, client: &Client) -> bool {
    let mut s = lock(state);
    if s.active.as_ref().is_some_and(|a| &a.client == client) {
        s.active = None;
        drop(s);
        prompt::done(ui);
        return true;
    }
    let before = s.waiting.len();
    s.waiting.retain(|c| c != client);
    s.waiting.len() != before
}

/// The served prompter (clones share it).
#[derive(Clone)]
pub struct Prompter {
    state: Shared,
    ui: mpsc::UnboundedSender<UiEvent>,
}

impl Prompter {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { state: Default::default(), ui }
    }

    /// Close the prompts of clients that leave the bus.
    pub fn follow_clients(&self, conn: Connection) {
        let (state, ui) = (self.state.clone(), self.ui.clone());
        tokio::spawn(async move {
            let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else { return };
            let Ok(mut changes) = dbus.receive_name_owner_changed().await else { return };
            while let Some(change) = changes.next().await {
                let Ok(args) = change.args() else { continue };
                let name = args.name().to_string();
                if !name.starts_with(':') || args.new_owner().is_some() {
                    continue;
                }
                let clients: Vec<Client> = {
                    let s = lock(&state);
                    s.active.iter().map(|a| a.client.clone()).chain(s.waiting.iter().cloned()).filter(|c| c.0 == name).collect()
                };
                for client in clients {
                    tracing::info!(client = client.0, "keyring prompt client left");
                    finish(&state, &ui, &client);
                }
                advance(state.clone(), conn.clone(), ui.clone());
            }
        });
    }
}

fn client_of(header: &Header<'_>, callback: OwnedObjectPath) -> Client {
    (header.sender().map(|s| s.to_string()).unwrap_or_default(), callback)
}

#[interface(name = "org.gnome.keyring.internal.Prompter")]
impl Prompter {
    async fn begin_prompting(&self, callback: OwnedObjectPath, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) {
        let client = client_of(&header, callback);
        tracing::debug!(client = client.0, "keyring prompt requested");
        lock(&self.state).waiting.push_back(client);
        advance(self.state.clone(), conn.clone(), self.ui.clone());
    }

    async fn perform_prompt(
        &self,
        callback: OwnedObjectPath,
        kind: &str,
        properties: HashMap<String, OwnedValue>,
        exchange: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), PrompterError> {
        let client = client_of(&header, callback);
        let password = match kind {
            "password" => true,
            "confirm" => false,
            other => return Err(PrompterError::Failed(format!("unknown prompt type {other}"))),
        };
        let (props, generation) = {
            let mut s = lock(&self.state);
            let active = s.active.as_mut().filter(|a| a.client == client).ok_or_else(|| PrompterError::InProgress("not this client's turn".into()))?;
            active.calls += 1;
            active.props.update(&properties);
            active.exchange.receive(exchange).map_err(PrompterError::Failed)?;
            (active.props.clone(), active.generation)
        };
        let (state, ui, conn) = (self.state.clone(), self.ui.clone(), conn.clone());
        tokio::spawn(async move {
            let mut dialog = props.prompt(password);
            let answer = loop {
                match prompt::ask(&ui, dialog.clone()).await {
                    Answer::Continue { password: Some(p), confirm: Some(c), choice, .. } if p.bytes() != c.bytes() => {
                        dialog.warning = tr(MISMATCH).into();
                        dialog.choice = dialog.choice.map(|(l, _)| (l, choice));
                    }
                    other => break other,
                }
            };
            let (reply, secret, choice) = match answer {
                Answer::Continue { password, choice, .. } => ("yes", password, choice),
                Answer::Cancel => ("no", None, props.choice_chosen),
            };
            // Still this client's prompt?
            let sent = {
                let mut s = lock(&state);
                let Some(active) = s.active.as_mut().filter(|a| a.generation == generation) else { return };
                active.props.choice_chosen = choice;
                let secret = if reply == "yes" && password { secret.as_ref().map(|s| s.bytes()) } else { None };
                active.exchange.send(secret)
            };
            let Ok(sent) = sent else { return };
            // Values go back wrapped in a second variant, as gcr reads them.
            let boxed = |v: Value<'static>| Value::Value(Box::new(v));
            let mut changed: HashMap<&str, Value<'_>> = HashMap::from([("choice-chosen", boxed(Value::from(choice)))]);
            if password {
                let strength = i32::from(secret.as_ref().is_some_and(|s| !s.bytes().is_empty()));
                changed.insert("password-strength", boxed(Value::from(strength)));
            }
            match call_ready(&conn, &client, reply, changed, &sent).await {
                Ok(()) => await_client(&state, &ui, &conn, generation),
                // Gone or stuck: its prompting ends, and the turn with it
                // (it held every other prompt back: #242).
                Err(err) => {
                    tracing::info!(%err, "keyring prompt client did not take the answer");
                    end_active(&state, &ui, &conn, generation);
                }
            }
        });
        Ok(())
    }

    async fn stop_prompting(&self, callback: OwnedObjectPath, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) {
        let client = client_of(&header, callback);
        if let Some(active) = lock(&self.state).active.as_mut().filter(|a| a.client == client) {
            active.calls += 1;
        }
        if finish(&self.state, &self.ui, &client) {
            let _ = tokio::time::timeout(client_timeout(), call_done(conn, &client)).await;
        }
        advance(self.state.clone(), conn.clone(), self.ui.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_to_dialog() {
        let mut props = Props::default();
        let owned = |v: Value<'_>| OwnedValue::try_from(v).unwrap();
        props.update(&HashMap::from([
            ("title".to_owned(), owned(Value::from("Unlock Keyring"))),
            ("password-new".to_owned(), owned(Value::from(true))),
            ("choice-label".to_owned(), owned(Value::from("Remember"))),
            ("continue-label".to_owned(), owned(Value::from("Unlock"))),
        ]));
        let p = props.prompt(true);
        assert_eq!(p.title, "Unlock Keyring");
        assert_eq!(p.confirm.as_deref(), Some("Confirm"));
        assert_eq!(p.choice, Some(("Remember".into(), false)));
        assert_eq!((p.continue_label.as_str(), p.cancel_label.as_str()), ("Unlock", "Cancel"));
        let c = Props::default().prompt(false);
        assert_eq!((c.title.as_str(), c.password.is_none()), ("Unlock", true));
    }
}

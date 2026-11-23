//! The keyring prompter over a private bus, with a fake gcr client
//! (helixdesktop/desktop#242): a client that stops answering, or can't take
//! the answer, must not keep the prompt turn (every other prompt and file
//! chooser waits for it).

use std::collections::HashMap;
use std::time::Duration;

use helix_shell::auth::Secret;
use helix_shell::keyring_prompt::{Prompter, OBJECT_PATH};
use helix_shell::prompt::{self, Answer};
use helix_shell::secret_exchange::Exchange;
use helix_shell::UiEvent;
use helix_test_support::TestBus;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue};

const CALLBACK_PATH: &str = "/org/helixdesktop/TestCallback";

/// The client's callback object: says what the prompter called.
struct Callback {
    calls: UnboundedSender<&'static str>,
    /// Refuse the answer (the PromptReady after a PerformPrompt).
    refuse_answer: bool,
}

#[interface(name = "org.gnome.keyring.internal.Prompter.Callback")]
impl Callback {
    async fn prompt_ready(&self, reply: &str, _properties: HashMap<String, OwnedValue>, _exchange: &str) -> zbus::fdo::Result<()> {
        let _ = self.calls.send(if reply.is_empty() { "ready" } else { "answer" });
        if self.refuse_answer && !reply.is_empty() {
            return Err(zbus::fdo::Error::Failed("not now".into()));
        }
        Ok(())
    }

    async fn prompt_done(&self) {
        let _ = self.calls.send("done");
    }
}

async fn next<T>(rx: &mut UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("in time").expect("open")
}

/// Answer the next prompt dialog with a password.
async fn answer_prompt(ui: &mut UnboundedReceiver<UiEvent>) {
    loop {
        if let UiEvent::Prompt(slot) = next(ui).await {
            let request = slot.take().expect("request");
            let _ = request.reply.send(Answer::Continue { entry: None, password: Some(Secret::new("secret".into())), confirm: None, choice: false, checks: Vec::new(), selected: 0, picked: Vec::new() });
            return;
        }
    }
}

/// A client asks for a password and gets the answer; `refuse_answer` makes
/// its callback refuse that answer.
async fn prompt_once(bus: &TestBus, prompter_name: &str, ui: &mut UnboundedReceiver<UiEvent>, refuse_answer: bool) -> (zbus::Connection, UnboundedReceiver<&'static str>) {
    let (tx, mut calls) = mpsc::unbounded_channel();
    let client = bus.builder().serve_at(CALLBACK_PATH, Callback { calls: tx, refuse_answer }).unwrap().build().await.unwrap();
    let callback = ObjectPath::try_from(CALLBACK_PATH).unwrap();
    client.call_method(Some(prompter_name), OBJECT_PATH, Some("org.gnome.keyring.internal.Prompter"), "BeginPrompting", &(&callback,)).await.unwrap();
    assert_eq!(next(&mut calls).await, "ready");
    let exchange = Exchange::new();
    let properties: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::from([("title", "Unlock Keyring".into())]);
    client
        .call_method(Some(prompter_name), OBJECT_PATH, Some("org.gnome.keyring.internal.Prompter"), "PerformPrompt", &(&callback, "password", properties, exchange.begin()))
        .await
        .unwrap();
    answer_prompt(ui).await;
    assert_eq!(next(&mut calls).await, "answer");
    (client, calls)
}

/// The prompt turn is free again within `within`.
async fn turn_free(within: Duration) -> bool {
    tokio::time::timeout(within, prompt::turn()).await.is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_keyring_client_gives_the_turn_back() {
    let Some(bus) = TestBus::start() else { return };
    let (ui_tx, mut ui) = mpsc::unbounded_channel();
    let name = "org.helixdesktop.TestPrompter";
    let _prompter = bus.builder().serve_at(OBJECT_PATH, Prompter::new(ui_tx)).unwrap().name(name).unwrap().build().await.unwrap();

    // A client that takes the answer, then never calls again (no
    // StopPrompting): its prompting ends after the client timeout, and it
    // is told so.
    // SAFETY: this test binary runs this one test, which sets it before
    // anything reads it.
    unsafe { std::env::set_var("HELIX_KEYRING_CLIENT_TIMEOUT_MS", "500") };
    let (_client, mut calls) = prompt_once(&bus, name, &mut ui, false).await;
    assert!(!turn_free(Duration::from_millis(200)).await, "the active client holds the turn");
    assert!(turn_free(Duration::from_secs(3)).await, "a client that stopped answering kept the prompt turn");
    assert_eq!(next(&mut calls).await, "done", "the stuck client is told its prompting ended");

    // A client that can't take the answer: its prompting ends at once, not
    // after the timeout.
    // SAFETY: as above.
    unsafe { std::env::set_var("HELIX_KEYRING_CLIENT_TIMEOUT_MS", "60000") };
    let (_client, _calls) = prompt_once(&bus, name, &mut ui, true).await;
    assert!(turn_free(Duration::from_secs(3)).await, "a client that refused the answer kept the prompt turn");
}

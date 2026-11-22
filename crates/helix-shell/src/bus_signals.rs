//! Signal streams that never stop a D-Bus connection.
//!
//! A zbus stream holds a few messages. Once one is full, zbus stops reading
//! the connection's socket until the stream is read. A loop that handles a
//! signal by calling a method on the same connection therefore deadlocks
//! when enough signals arrive during the call: the reply sits behind them,
//! unread, and every call on that connection hangs (for the shell's session
//! connection, the whole of `org.gnome.Shell`).
//!
//! These helpers read a stream on a task of its own that never waits on
//! anything else. The task ends with the stream or once its receiver is
//! dropped, which also drops the stream's match rule.

use futures_util::{Stream, StreamExt};
use tokio::sync::{mpsc, watch};

/// Every item of `stream`, queued without bound.
pub fn forward<S>(stream: S) -> mpsc::UnboundedReceiver<S::Item>
where
    S: Stream + Send + 'static,
    S::Item: Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        loop {
            tokio::select! {
                item = stream.next() => {
                    let Some(item) = item else { break };
                    if tx.send(item).is_err() {
                        break;
                    }
                }
                () = tx.closed() => break,
            }
        }
    });
    rx
}

/// A receiver that changes whenever `stream` yields, for loops that only
/// need to know that something arrived (and read the state afresh). It
/// errors once the stream has ended.
pub fn nudges<S>(stream: S) -> watch::Receiver<u64>
where
    S: Stream + Send + 'static,
{
    let (tx, rx) = watch::channel(0u64);
    tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        loop {
            tokio::select! {
                item = stream.next() => {
                    if item.is_none() {
                        break;
                    }
                    tx.send_modify(|n| *n = n.wrapping_add(1));
                }
                () = tx.closed() => break,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwards_everything_then_ends() {
        let mut rx = forward(futures_util::stream::iter(0..1000));
        for n in 0..1000 {
            assert_eq!(rx.recv().await, Some(n));
        }
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn nudges_then_ends() {
        let mut rx = nudges(futures_util::stream::iter(0..3));
        assert!(rx.changed().await.is_ok());
        rx.borrow_and_update();
        // The rest arrive and the stream ends: no more changes after them.
        while rx.changed().await.is_ok() {
            rx.borrow_and_update();
        }
        assert_eq!(*rx.borrow(), 3);
    }

    #[tokio::test]
    async fn the_task_ends_when_the_receiver_goes() {
        let (alive, gone) = tokio::sync::oneshot::channel::<()>();
        // A stream that never yields and tells when it is dropped.
        let stream = futures_util::stream::pending::<()>().chain(futures_util::stream::once(async move {
            drop(alive);
        }));
        drop(forward(stream));
        tokio::time::timeout(std::time::Duration::from_secs(5), gone).await.expect("the stream was dropped").unwrap_err();
    }
}

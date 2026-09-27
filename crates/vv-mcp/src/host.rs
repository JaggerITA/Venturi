//! Between the MCP transport and the owner of the `Session`: the transport
//! submits calls through a `McpHandle`, the owner drains the `McpInbox`.

use std::sync::{Arc, Weak, mpsc};
use std::time::Duration;

use tokio::sync::oneshot;
use vv_session::{Session, Waker};

use crate::{Dispatch, Pending, ToolCall, ToolError, ToolResult, dispatch};

pub struct McpRequest {
    pub call: ToolCall,
    pub reply: oneshot::Sender<ToolResult>,
}

enum Message {
    Request(McpRequest),
    Wake,
}

type Notify = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct McpHandle {
    tx: Arc<mpsc::Sender<Message>>,
    notify: Notify,
}

impl McpHandle {
    /// The receiver yields the result, or an error if the host is gone.
    pub fn submit(&self, call: ToolCall) -> oneshot::Receiver<ToolResult> {
        let (reply, result) = oneshot::channel();
        if let Err(mpsc::SendError(Message::Request(request))) =
            self.tx.send(Message::Request(McpRequest { call, reply }))
        {
            let _ = request
                .reply
                .send(Err(ToolError("Venturi is shutting down".into())));
        }
        (self.notify)();
        result
    }
}

#[derive(Debug)]
pub struct Disconnected;

pub struct McpInbox {
    rx: mpsc::Receiver<Message>,
    /// Weak, so that a session waker does not keep the channel open after
    /// the last `McpHandle` is gone.
    tx: Weak<mpsc::Sender<Message>>,
    notify: Notify,
}

impl McpInbox {
    pub fn try_recv(&self) -> Option<McpRequest> {
        self.rx.try_iter().find_map(|message| match message {
            Message::Request(request) => Some(request),
            Message::Wake => None,
        })
    }

    /// `Err` once every `McpHandle` is dropped; `Ok(None)` on a timeout or a
    /// wake-up.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<McpRequest>, Disconnected> {
        match self.rx.recv_timeout(timeout) {
            Ok(Message::Request(request)) => Ok(Some(request)),
            Ok(Message::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Disconnected),
        }
    }

    /// A session waker that interrupts `recv_timeout`.
    pub fn waker(&self) -> Waker {
        let tx = self.tx.clone();
        let notify = self.notify.clone();
        Waker::new(move || {
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(Message::Wake);
            }
            notify();
        })
    }
}

/// `notify` runs on every submitted call, e.g. to wake a GUI event loop.
pub fn channel(notify: impl Fn() + Send + Sync + 'static) -> (McpHandle, McpInbox) {
    let (tx, rx) = mpsc::channel();
    let tx = Arc::new(tx);
    let notify: Notify = Arc::new(notify);
    let inbox = McpInbox {
        rx,
        tx: Arc::downgrade(&tx),
        notify: notify.clone(),
    };
    (McpHandle { tx, notify }, inbox)
}

/// The calls waiting for background work, with where to reply.
#[derive(Default)]
pub struct PendingCalls(Vec<(Pending, oneshot::Sender<ToolResult>)>);

impl PendingCalls {
    pub fn push(&mut self, pending: Pending, reply: oneshot::Sender<ToolResult>) {
        self.0.push((pending, reply));
    }

    pub fn resolve(&mut self, session: &mut Session, event: &vv_session::SessionEvent) {
        let mut index = 0;
        while index < self.0.len() {
            match self.0[index].0.resolve(session, event) {
                Some(result) => {
                    let (_, reply) = self.0.swap_remove(index);
                    let _ = reply.send(result);
                }
                None => index += 1,
            }
        }
    }
}

/// Serves calls on `session` until every `McpHandle` is dropped, then
/// returns it. No UI: the whole program is this loop.
pub fn run_headless(mut session: Session, inbox: McpInbox) -> Session {
    session.set_waker(inbox.waker());
    let mut pending = PendingCalls::default();
    loop {
        match inbox.recv_timeout(Duration::from_millis(50)) {
            Ok(Some(request)) => match dispatch(&mut session, request.call) {
                Dispatch::Handled(result) => {
                    let _ = request.reply.send(result);
                }
                Dispatch::Deferred(call) => pending.push(call, request.reply),
            },
            Ok(None) => {}
            Err(Disconnected) => return session,
        }
        for event in session.tick() {
            pending.resolve(&mut session, &event);
        }
    }
}

#[cfg(test)]
#[path = "tests/host.rs"]
mod tests;

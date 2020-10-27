//! Curl agent that executes multiple requests simultaneously.
//!
//! The agent is implemented as a single background thread attached to a
//! "handle". The handle communicates with the agent thread by using message
//! passing. The agent executes multiple curl requests simultaneously by using a
//! single "multi" handle.
//!
//! Since request executions are driven through futures, the agent also acts as
//! a specialized task executor for tasks related to requests.

use crate::{
    handler::RequestHandler,
    task::WakerExt,
    Error,
};
use crossbeam_channel::{Receiver, Sender};
use crossbeam_utils::sync::WaitGroup;
use curl::multi::{Multi, Socket, SocketEvents};
use futures_util::task::ArcWake;
use polling::{Event, Poller};
use slab::Slab;
use std::{
    sync::{atomic::{AtomicUsize, Ordering}, Arc, Mutex},
    task::Waker,
    thread,
    time::{Duration, Instant},
};

static NEXT_AGENT_ID: AtomicUsize = AtomicUsize::new(0);
const WAIT_TIMEOUT: Duration = Duration::from_millis(100);

type EasyHandle = curl::easy::Easy2<RequestHandler>;
type MultiMessage = (usize, Result<(), curl::Error>);

/// Builder for configuring and spawning an agent.
#[derive(Debug, Default)]
pub(crate) struct AgentBuilder {
    max_connections: usize,
    max_connections_per_host: usize,
    connection_cache_size: usize,
}

impl AgentBuilder {
    pub(crate) fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    pub(crate) fn max_connections_per_host(mut self, max: usize) -> Self {
        self.max_connections_per_host = max;
        self
    }

    pub(crate) fn connection_cache_size(mut self, size: usize) -> Self {
        self.connection_cache_size = size;
        self
    }

    /// Spawn a new agent using the configuration in this builder and return a
    /// handle for communicating with the agent.
    pub(crate) fn spawn(&self) -> Result<Handle, Error> {
        let create_start = Instant::now();

        // Initialize libcurl, if necessary, on the current thread.
        //
        // Note that as of 0.4.30, the curl crate will attempt to do this for us
        // on the main thread automatically at program start on most targets,
        // but on other targets must still be initialized on the main thread. We
        // do this here in the hope that the user builds an `HttpClient` on the
        // main thread (as opposed to waiting for `Multi::new()` to do it for
        // us below, which we _know_ is not on the main thread).
        //
        // See #189.
        curl::init();

        // Create an I/O poller for driving curl's sockets.
        let poller = Arc::new(Poller::new()?);

        // Make a waker that will notify the poller.
        let waker = futures_util::task::waker(Arc::new(PollerWaker(poller.clone())));

        let (message_tx, message_rx) = crossbeam_channel::unbounded();

        let wait_group = WaitGroup::new();
        let wait_group_thread = wait_group.clone();

        let max_connections = self.max_connections;
        let max_connections_per_host = self.max_connections_per_host;
        let connection_cache_size = self.connection_cache_size;

        // Create a span for the agent thread that outlives this method call,
        // but rather was caused by it.
        let agent_span = tracing::debug_span!("agent_thread");
        agent_span.follows_from(tracing::Span::current());

        let handle = Handle {
            message_tx: message_tx.clone(),
            waker: waker.clone(),
            join_handle: Mutex::new(Some(
                thread::Builder::new()
                    .name(format!("isahc-agent-{}", NEXT_AGENT_ID.fetch_add(1, Ordering::SeqCst)))
                    .spawn(move || {
                        let _enter = agent_span.enter();
                        let mut multi = Multi::new();

                        if max_connections > 0 {
                            multi.set_max_total_connections(max_connections)?;
                        }

                        if max_connections_per_host > 0 {
                            multi.set_max_host_connections(max_connections_per_host)?;
                        }

                        // Only set maxconnects if greater than 0, because 0 actually means unlimited.
                        if connection_cache_size > 0 {
                            multi.set_max_connects(connection_cache_size)?;
                        }

                        let agent = AgentContext::new(multi, poller, waker, message_tx, message_rx)?;

                        drop(wait_group_thread);

                        tracing::debug!("agent took {:?} to start up", create_start.elapsed());

                        let result = agent.run();

                        if let Err(e) = &result {
                            tracing::error!("agent shut down with error: {}", e);
                        }

                        result
                    })?,
            )),
        };

        // Block until the agent thread responds.
        wait_group.wait();

        Ok(handle)
    }
}

/// A handle to an active agent running in a background thread.
///
/// Dropping the handle will cause the agent thread to shut down and abort any
/// pending transfers.
#[derive(Debug)]
pub(crate) struct Handle {
    /// Used to send messages to the agent thread.
    message_tx: Sender<Message>,

    /// A waker that can wake up the agent thread while it is polling.
    waker: Waker,

    /// A join handle for the agent thread.
    join_handle: Mutex<Option<thread::JoinHandle<Result<(), Error>>>>,
}

#[derive(Debug)]
enum JoinResult {
    AlreadyJoined,
    Ok,
    Err(Error),
    Panic,
}

impl Handle {
    /// Begin executing a request with this agent.
    pub(crate) fn submit_request(&self, request: EasyHandle) -> Result<(), Error> {
        self.send_message(Message::Execute(request))
    }

    /// Send a message to the agent thread.
    ///
    /// If the agent is not connected, an error is returned.
    fn send_message(&self, message: Message) -> Result<(), Error> {
        match self.message_tx.send(message) {
            Ok(()) => {
                // Wake the agent thread up so it will check its messages soon.
                self.waker.wake_by_ref();
                Ok(())
            }
            Err(crossbeam_channel::SendError(_)) => match self.try_join() {
                JoinResult::Err(e) => panic!("agent thread terminated with error: {}", e),
                JoinResult::Panic => panic!("agent thread panicked"),
                _ => panic!("agent thread terminated prematurely"),
            },
        }
    }

    fn try_join(&self) -> JoinResult {
        let mut option = self.join_handle.lock().unwrap();

        if let Some(join_handle) = option.take() {
            match join_handle.join() {
                Ok(Ok(())) => JoinResult::Ok,
                Ok(Err(e)) => JoinResult::Err(e),
                Err(_) => JoinResult::Panic,
            }
        } else {
            JoinResult::AlreadyJoined
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Request the agent thread to shut down.
        if self.send_message(Message::Close).is_err() {
            tracing::error!("agent thread terminated prematurely");
        }

        // Wait for the agent thread to shut down before continuing.
        match self.try_join() {
            JoinResult::Ok => tracing::trace!("agent thread joined cleanly"),
            JoinResult::Err(e) => tracing::error!("agent thread terminated with error: {}", e),
            JoinResult::Panic => tracing::error!("agent thread panicked"),
            _ => {}
        }
    }
}

/// Internal state of an agent thread.
///
/// The agent thread runs the primary client event loop, which is essentially a
/// traditional curl multi event loop with some extra bookkeeping and async
/// features like wakers.
struct AgentContext {
    /// A curl multi handle, of course.
    multi: Multi,

    /// Queue of messages from the multi handle.
    multi_messages: (Sender<MultiMessage>, Receiver<MultiMessage>),

    /// Used to send messages to the agent thread.
    message_tx: Sender<Message>,

    /// Incoming messages from the agent handle.
    message_rx: Receiver<Message>,

    /// Contains all of the active requests.
    requests: Slab<curl::multi::Easy2Handle<RequestHandler>>,

    /// Indicates if the thread has been requested to stop.
    close_requested: bool,

    /// A waker that can wake up the agent thread while it is polling.
    waker: Waker,

    /// All of the sockets that curl has asked us to keep track of.
    sockets: Slab<SocketContext>,

    /// This is the poller we use to poll for socket activity!
    poller: Arc<Poller>,

    /// Socket events that have occurred. We re-use this vec every call for
    /// efficiency.
    socket_events: Vec<Event>,

    /// Queue of socket registration updates from the multi handle.
    socket_updates: Receiver<(Socket, SocketEvents, usize)>,

    timeout_updates: Receiver<Option<Duration>>,
}

/// Context stored about a socket that we are managing on curl's behalf.
struct SocketContext {
    /// The socket handle.
    socket: Socket,

    /// Whether curl is interested in readable events.
    readable: bool,

    /// Whether curl is interested in writable events.
    writable: bool,
}

/// A message sent from the main thread to the agent thread.
#[derive(Debug)]
enum Message {
    /// Requests the agent to close.
    Close,

    /// Begin executing a new request.
    Execute(EasyHandle),

    /// Request to resume reading the request body for the request with the
    /// given ID.
    UnpauseRead(usize),

    /// Request to resume writing the response body for the request with the
    /// given ID.
    UnpauseWrite(usize),
}

impl AgentContext {
    fn new(
        mut multi: Multi,
        poller: Arc<Poller>,
        waker: Waker,
        message_tx: Sender<Message>,
        message_rx: Receiver<Message>,
    ) -> Result<Self, Error> {
        let (socket_updates_tx, socket_updates_rx) = crossbeam_channel::unbounded();
        let (timeout_tx, timeout_rx) = crossbeam_channel::unbounded();

        multi.socket_function(move |socket, events, key| {
            let _ = socket_updates_tx.send((socket, events, key));
        })?;

        multi.timer_function(move |timeout| {
            timeout_tx.send(timeout).is_ok()
        })?;

        Ok(Self {
            multi,
            multi_messages: crossbeam_channel::unbounded(),
            message_tx,
            message_rx,
            requests: Slab::new(),
            close_requested: false,
            waker,
            poller,
            sockets: Slab::new(),
            socket_events: Vec::new(),
            socket_updates: socket_updates_rx,
            timeout_updates: timeout_rx,
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn begin_request(&mut self, mut request: EasyHandle) -> Result<(), Error> {
        // Prepare an entry for storing this request while it executes.
        let entry = self.requests.vacant_entry();
        let id = entry.key();
        let handle = request.raw();

        // Initialize the handler.
        request.get_mut().init(
            id,
            handle,
            {
                let tx = self.message_tx.clone();

                self.waker
                    .chain(move |inner| match tx.send(Message::UnpauseRead(id)) {
                        Ok(()) => inner.wake_by_ref(),
                        Err(_) => tracing::warn!(
                            "agent went away while resuming read for request [id={}]",
                            id
                        ),
                    })
            },
            {
                let tx = self.message_tx.clone();

                self.waker
                    .chain(move |inner| match tx.send(Message::UnpauseWrite(id)) {
                        Ok(()) => inner.wake_by_ref(),
                        Err(_) => tracing::warn!(
                            "agent went away while resuming write for request [id={}]",
                            id
                        ),
                    })
            },
        );

        // Register the request with curl.
        let mut handle = self.multi.add2(request)?;
        handle.set_token(id)?;

        // Add the handle to our bookkeeping structure.
        entry.insert(handle);

        // This will trigger curl to invoke our callbacks for socket structures
        // "very soon".
        self.notify_curl_of_timeout()?;

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn complete_request(
        &mut self,
        token: usize,
        result: Result<(), curl::Error>,
    ) -> Result<(), Error> {
        let handle = self.requests.remove(token);
        let mut handle = self.multi.remove2(handle)?;

        handle.get_mut().on_result(result);

        Ok(())
    }

    /// Polls the message channel for new messages from any agent handles.
    ///
    /// If there are no active requests right now, this function will block
    /// until a message is received.
    #[tracing::instrument(level = "trace", skip(self))]
    fn poll_messages(&mut self) -> Result<(), Error> {
        while !self.close_requested {
            if self.requests.is_empty() {
                match self.message_rx.recv() {
                    Ok(message) => self.handle_message(message)?,
                    _ => {
                        tracing::warn!("agent handle disconnected without close message");
                        self.close_requested = true;
                        break;
                    }
                }
            } else {
                match self.message_rx.try_recv() {
                    Ok(message) => self.handle_message(message)?,
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        tracing::warn!("agent handle disconnected without close message");
                        self.close_requested = true;
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn handle_message(&mut self, message: Message) -> Result<(), Error> {
        tracing::trace!("received message from agent handle");

        match message {
            Message::Close => self.close_requested = true,
            Message::Execute(request) => self.begin_request(request)?,
            Message::UnpauseRead(token) => {
                if let Some(request) = self.requests.get(token) {
                    if let Err(e) = request.unpause_read() {
                        // If unpausing returned an error, it is likely because
                        // curl called our callback inline and the callback
                        // returned an error. Unfortunately this does not affect
                        // the normal state of the transfer, so we need to keep
                        // the transfer alive until it errors through the normal
                        // means, which is likely to happen this turn of the
                        // event loop anyway.
                        tracing::debug!("error unpausing read for request [id={}]: {}", token, e);
                    }
                } else {
                    tracing::warn!(
                        "received unpause request for unknown request token: {}",
                        token
                    );
                }
            }
            Message::UnpauseWrite(token) => {
                if let Some(request) = self.requests.get(token) {
                    if let Err(e) = request.unpause_write() {
                        // If unpausing returned an error, it is likely because
                        // curl called our callback inline and the callback
                        // returned an error. Unfortunately this does not affect
                        // the normal state of the transfer, so we need to keep
                        // the transfer alive until it errors through the normal
                        // means, which is likely to happen this turn of the
                        // event loop anyway.
                        tracing::debug!("error unpausing write for request [id={}]: {}", token, e);
                    }
                } else {
                    tracing::warn!(
                        "received unpause request for unknown request token: {}",
                        token
                    );
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn dispatch(&mut self) -> Result<(), Error> {
        // Collect messages from curl about requests that have completed,
        // whether successfully or with an error.
        self.multi.messages(|message| {
            if let Some(result) = message.result() {
                if let Ok(token) = message.token() {
                    self.multi_messages.0.send((token, result)).unwrap();
                }
            }
        });

        loop {
            match self.multi_messages.1.try_recv() {
                // A request completed.
                Ok((token, result)) => self.complete_request(token, result)?,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => unreachable!(),
            }
        }

        Ok(())
    }

    /// Run the agent in the current thread until requested to stop.
    fn run(mut self) -> Result<(), Error> {
        // Agent main loop.
        loop {
            self.poll_messages()?;

            if self.close_requested {
                break;
            }

            // Perform any pending reads or writes and handle any state changes.
            self.dispatch()?;

            // Block until activity is detected or the timeout passes.
            self.poll_io()?;
        }

        tracing::debug!("agent shutting down");

        self.requests.clear();

        Ok(())
    }

    /// Block until activity is detected or a timeout passes.
    fn poll_io(&mut self) -> Result<(), Error> {
        // Apply any requested socket updates now.
        while let Ok((socket, events, key)) = self.socket_updates.try_recv() {
            self.update_socket(socket, events, key)?;
        }

        // Since our I/O events are oneshot, make sure we re-register sockets
        // with the poller that previously were triggered last time we polled.
        for event in self.socket_events.iter() {
            if let Some(socket_ctx) = self.sockets.get(event.key - 1) {
                self.poller_modify(socket_ctx.socket, event.key, socket_ctx.readable, socket_ctx.writable)?;
            }
        }

        // Get the latest timeout value from curl that we should use, limited to
        // a maximum we chose.
        let timeout = self.timeout_updates
            .try_iter()
            .last()
            .flatten()
            .map(|t| t.min(WAIT_TIMEOUT))
            .unwrap_or(WAIT_TIMEOUT);

        // Clear all events from previous polling.
        self.socket_events.clear();

        // Block until either an I/O event occurs on a socket, the timeout is
        // reached, or the agent handle interrupts us.
        self.poller.wait(&mut self.socket_events,Some(timeout))?;

        // If no events are returned, then the timeout was likely reached.
        if self.socket_events.is_empty() {
            self.notify_curl_of_timeout()?;
        }

        // At least one I/O event occurred, handle them.
        else {
            for event in self.socket_events.iter() {
                debug_assert!(event.key > 0);

                if let Some(socket_ctx) = self.sockets.get(event.key - 1) {
                    let mut events = curl::multi::Events::new();
                    events.input(event.readable);
                    events.output(event.writable);
                    self.multi.action(socket_ctx.socket, &events)?;
                }
            }
        }

        Ok(())
    }

    fn update_socket(&mut self, socket: Socket, events: SocketEvents, key: usize) -> Result<(), Error> {
        // Curl is asking us to stop polling this socket.
        if events.remove() {
            // Curl should never ask us to stop polling a socket it never told
            // us about in the first place!
            debug_assert!(key > 0);

            // Remove this socket from our bookkeeping.
            self.sockets.remove(key - 1);

            // There's a good chance that curl has already closed this
            // socket, at which point the poller implementation may have
            // already forgotten about this socket (e.g. epoll). Therefore
            // if we get an error back we just ignore it, as it is almost
            // certainly benign.
            if let Err(e) = self.poller.delete(socket) {
                tracing::debug!(key = key, fd = socket, "error removing socket from poller: {}", e);
            }
        }

        // This is a new socket, at least from curl's perspective. Set up a new
        // socket context for it, and then register it with the poller.
        else if key == 0 {
            let readable = events.input() || events.input_and_output();
            let writable = events.output() || events.input_and_output();

            let key = self.sockets.insert(SocketContext {
                socket,
                readable,
                writable,
            }) + 1;

            // Tell curl about our chosen key for this socket.
            self.multi.assign(socket, key)?;

            // Add the socket to our poller.
            self.poller_add(socket, key, readable, writable)?;
        }

        // This is a socket we've seen before and are already managing.
        else {
            if let Some(socket_ctx) = self.sockets.get_mut(key - 1) {
                let readable = events.input() || events.input_and_output();
                let writable = events.output() || events.input_and_output();

                // Update the interest we have recorded for this socket.
                socket_ctx.readable = readable;
                socket_ctx.writable = writable;

                // Update the socket interests with our poller.
                self.poller_modify(socket, key, readable, writable)?;
            } else {
                // Curl should never give us a key that we did not first give to
                // curl!
                tracing::warn!("unknown key from curl: {}", key);
            }
        }

        Ok(())
    }

    fn poller_add(&self, socket: Socket, key: usize, readable: bool, writable: bool) -> Result<(), Error> {
        // If this errors, we retry the operation as a modification instead.
        // This is because this new socket might re-use a file descriptor that
        // was previously closed, but is still registered with the poller.
        // Retrying the operation as a modification is sufficient to handle
        // this.
        //
        // This is especially common with the epoll backend.
        if let Err(e) = self.poller.add(socket, Event {
            key,
            readable,
            writable,
        }) {
            tracing::debug!("failed to add interest for socket key {}, retrying as a modify: {}", key, e);
            self.poller.modify(socket, Event {
                key,
                readable,
                writable,
            })?;
        }

        Ok(())
    }

    fn poller_modify(&self, socket: Socket, key: usize, readable: bool, writable: bool) -> Result<(), Error> {
        // If this errors, we retry the operation as an add instead. This is
        // done because epoll is weird.
        if let Err(e) = self.poller.modify(socket, Event {
            key,
            readable,
            writable,
        }) {
            tracing::debug!("failed to modify interest for socket key {}, retrying as an add: {}", key, e);
            self.poller.add(socket, Event {
                key,
                readable,
                writable,
            })?;
        }

        Ok(())
    }

    fn notify_curl_of_timeout(&self) -> Result<(), Error> {
        self.multi.action(curl_sys::CURL_SOCKET_TIMEOUT, &curl::multi::Events::new())?;

        Ok(())
    }
}

struct PollerWaker(Arc<Poller>);

impl ArcWake for PollerWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let _ = arc_self.0.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static_assertions::assert_impl_all!(Handle: Send, Sync);
    static_assertions::assert_impl_all!(Message: Send);
}

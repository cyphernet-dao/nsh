use cyphernet::{ed25519, x25519};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::RawFd;
use std::time::Duration;

use netservices::{ListenerEvent, SessionEvent};
use reactor::{Error, Resource};

use crate::{Session, Transport};

pub type Accept = netservices::NetAccept<Session>;
pub type Action = reactor::Action<Accept, Transport>;

pub type Ecdh = x25519::PrivateKey;
pub type Auth = ed25519::PrivateKey;

pub trait Delegate: Send {
    fn accept(&self, connection: TcpStream) -> Session;
    fn new_client(&mut self, id: RawFd, key: ed25519::PublicKey) -> Vec<Action>;
    fn input(&mut self, id: RawFd, data: Vec<u8>) -> Vec<Action>;
}

pub struct Server<D: Delegate> {
    outbox: HashMap<RawFd, VecDeque<Vec<u8>>>,
    action_queue: VecDeque<Action>,
    delegate: D,
}

impl<D: Delegate> Server<D> {
    pub fn with(listen: &impl ToSocketAddrs, delegate: D) -> io::Result<Self> {
        let mut action_queue = VecDeque::new();
        let listener = Accept::bind(listen)?;
        action_queue.push_back(Action::RegisterListener(listener));
        Ok(Self {
            outbox: empty!(),
            action_queue,
            delegate,
        })
    }
}

impl<D: Delegate> reactor::Handler for Server<D> {
    type Listener = Accept;
    type Transport = Transport;
    type Command = ();

    fn tick(&mut self, time: Duration) {
        log::trace!(target: "server", "reactor ticks at {time:?}");
    }

    fn handle_timer(&mut self) {
        log::trace!(target: "server", "Reactor receives a timer event");
    }

    fn handle_listener_event(
        &mut self,
        id: <Self::Listener as Resource>::Id,
        event: <Self::Listener as Resource>::Event,
        time: Duration,
    ) {
        log::trace!(target: "server", "Listener event on {id} at {time:?}");
        match event {
            ListenerEvent::Accepted(connection) => {
                let peer_addr = connection
                    .peer_addr()
                    .expect("unknown peer address on accepted connection");
                let local_addr = connection
                    .local_addr()
                    .expect("unknown local address on accepted connection");
                log::info!(target: "server", "Incoming connection from {peer_addr} on {local_addr}");
                let session = self.delegate.accept(connection);
                match Transport::accept(session) {
                    Ok(transport) => {
                        log::info!(target: "server", "Connection accepted, registering transport with reactor");
                        self.action_queue
                            .push_back(Action::RegisterTransport(transport));
                    }
                    Err(err) => {
                        log::info!(target: "server", "Error accepting incoming connection: {err}");
                    }
                }
            }
            ListenerEvent::Failure(err) => {
                log::error!(target: "server", "Error on listener {id}: {err}")
            }
        }
    }

    fn handle_transport_event(
        &mut self,
        id: <Self::Transport as Resource>::Id,
        event: <Self::Transport as Resource>::Event,
        time: Duration,
    ) {
        log::trace!(target: "server", "I/O on {id} at {time:?}");
        match event {
            SessionEvent::Established(artifact) => {
                let key = artifact.state.pk;
                let queue = self.outbox.remove(&id).unwrap_or_default();
                log::debug!(target: "server", "Connection with remote peer {key}@{id} successfully established; processing {} items from outbox", queue.len());
                self.action_queue.extend(self.delegate.new_client(id, key));
                self.action_queue
                    .extend(queue.into_iter().map(|msg| Action::Send(id, msg)))
            }
            SessionEvent::Data(data) => {
                log::trace!(target: "server", "Incoming data {data:?}");
                self.action_queue.extend(self.delegate.input(id, data));
            }
            SessionEvent::Terminated(err) => {
                log::error!(target: "server", "Connection with {id} is terminated due to an error: {err}");
                self.action_queue.push_back(Action::UnregisterTransport(id));
            }
        }
    }

    fn handle_command(&mut self, cmd: Self::Command) {
        log::debug!(target: "server", "Command {cmd:?} received");
    }

    fn handle_error(&mut self, err: Error<Self::Listener, Self::Transport>) {
        match err {
            Error::TransportDisconnect(_id, transport, _) => {
                log::warn!(target: "server", "Remote peer {transport} disconnected");
                return;
            }
            Error::WriteLogicError(id, msg) => {
                log::debug!(target: "server", "Remote peer {id} is not ready, putting message to outbox");
                self.outbox.entry(id).or_default().push_back(msg)
            }
            // All others are errors:
            ref err @ Error::ListenerUnknown(_)
            | ref err @ Error::TransportUnknown(_)
            | ref err @ Error::Poll(_) => {
                log::error!(target: "server", "Error: {err}");
            }
            ref err @ Error::ListenerDisconnect(id, _, _)
            | ref err @ Error::ListenerPollError(id, _) => {
                log::error!(target: "server", "Error: {err}");
                self.action_queue.push_back(Action::UnregisterListener(id));
            }
            ref err @ Error::WriteFailure(id, _) | ref err @ Error::TransportPollError(id, _) => {
                log::error!(target: "server", "Error: {err}");
                self.action_queue.push_back(Action::UnregisterTransport(id));
            }
        }
    }

    fn handover_listener(&mut self, listener: Self::Listener) {
        log::error!(target: "server", "Disconnected listener socket {}", listener.id());
        panic!("Disconnected listener socket {}", listener.id())
    }

    fn handover_transport(&mut self, transport: Self::Transport) {
        log::warn!(target: "server", "Remote peer {transport} disconnected");
    }
}

impl<D: Delegate> Iterator for Server<D> {
    type Item = Action;

    fn next(&mut self) -> Option<Self::Item> {
        self.action_queue.pop_front()
    }
}

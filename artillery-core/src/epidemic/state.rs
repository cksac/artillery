use crate::errors::*;
use super::cluster_config::ClusterConfig;
use uuid::Uuid;
use std::net::{SocketAddr};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::time::Duration;
use cuneiform_fields::prelude::*;
use super::membership::ArtilleryMemberList;
use crate::epidemic::member::{ArtilleryStateChange, ArtilleryMember, ArtilleryMemberState};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use serde::*;
use mio::{Events, Interest, Poll, Token};
use std::io;
use mio::net::UdpSocket;
use std::collections::hash_map::Entry;
use std::str::FromStr;
use failure::_core::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::Arc;
use std::ops::DerefMut;
use failure::_core::ops::Deref;
use crate::epidemic::constants::CONST_MTU;

pub type ArtilleryClusterEvent = (Vec<ArtilleryMember>, ArtilleryMemberEvent);
pub type WaitList = HashMap<SocketAddr, Vec<SocketAddr>>;

#[derive(Debug)]
pub enum ArtilleryMemberEvent {
    MemberJoined(ArtilleryMember),
    MemberWentUp(ArtilleryMember),
    MemberSuspectedDown(ArtilleryMember),
    MemberWentDown(ArtilleryMember),
    MemberLeft(ArtilleryMember),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArtilleryMessage {
    sender: Uuid,
    cluster_key: Vec<u8>,
    request: Request,
    state_changes: Vec<ArtilleryStateChange>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct EncSocketAddr(SocketAddr);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
enum Request {
    Ping,
    Ack,
    PingRequest(EncSocketAddr),
    AckHost(ArtilleryMember),
}

#[derive(Debug, Clone)]
pub struct TargetedRequest {
    request: Request,
    target: SocketAddr,
}

#[derive(Clone)]
pub enum ArtilleryClusterRequest {
    AddSeed(SocketAddr),
    Respond(SocketAddr, ArtilleryMessage),
    React(TargetedRequest),
    LeaveCluster,
    Exit(Sender<()>),
}

const UDP_SERVER: Token = Token(0);

pub struct ArtilleryState {
    host_key: Uuid,
    config: ClusterConfig,
    members: ArtilleryMemberList,
    seed_queue: Vec<SocketAddr>,
    pending_responses: Vec<(DateTime<Utc>, SocketAddr, Vec<ArtilleryStateChange>)>,
    state_changes: Vec<ArtilleryStateChange>,
    wait_list: WaitList,
    server_socket: UdpSocket,
    request_tx: ArchPadding<Sender<ArtilleryClusterRequest>>,
    event_tx: ArchPadding<Sender<ArtilleryClusterEvent>>,
    running: AtomicBool,
}

pub type ClusterReactor = (Poll, ArtilleryState);

impl ArtilleryState {
    pub fn new(host_key: Uuid,
           config: ClusterConfig,
           event_tx: Sender<ArtilleryClusterEvent>,
           internal_tx: Sender<ArtilleryClusterRequest>) -> Result<(Poll, ArtilleryState)> {
        let mut poll: Poll = Poll::new()?;

        let interests = Interest::READABLE.add(Interest::WRITABLE);
        let mut server_socket = UdpSocket::bind(config.listen_addr)?;
        poll.registry()
            .register(&mut server_socket, UDP_SERVER, interests)?;

        let me = ArtilleryMember::current(host_key.clone());

        let state = ArtilleryState {
            host_key,
            config,
            members: ArtilleryMemberList::new(me.clone()),
            seed_queue: Vec::new(),
            pending_responses: Vec::new(),
            state_changes: vec![ArtilleryStateChange::new(me)],
            wait_list: HashMap::new(),
            server_socket,
            request_tx: ArchPadding::new(internal_tx),
            event_tx: ArchPadding::new(event_tx),
            running: AtomicBool::new(true),
        };

        Ok((poll, state))
    }

    pub(crate) fn event_loop(receiver: &mut Receiver<ArtilleryClusterRequest>, mut poll: Poll, mut state: ArtilleryState) -> Result<()> {
        let mut events = Events::with_capacity(1);
        let mut buf = [0_u8; CONST_MTU];

        let mut start = Instant::now();
        let timeout = Duration::from_millis(state.config.ping_interval.num_milliseconds() as u64);

        debug!("Starting Event Loop");
        // Our event loop.
        loop {
            let elapsed = start.elapsed();

            dbg!(elapsed);
            dbg!(timeout);
            if elapsed >= timeout {
                debug!("Seeds are enqueued!");
                state.enqueue_seed_nodes();
                state.enqueue_random_ping();
                start = Instant::now();
            }

            if !state.running.load(Ordering::SeqCst) {
                debug!("Stopping artillery epidemic evloop");
                break;
            }

            // Poll to check if we have events waiting for us.
            if let Some(remaining) = timeout.checked_sub(elapsed) {
                poll.poll(&mut events, Some(remaining))?;
            }

            // Process our own events that are submitted to event loop
            // Aka outbound events
            while let Ok(msg) = receiver.try_recv() {
                let exit_tx = state.process_internal_request(msg);

                if let Some(exit_tx) = exit_tx {
                    state.running.swap(false, Ordering::SeqCst);
                    exit_tx.send(()).unwrap();
                }
            }

            // Process inbound events
            for event in events.iter() {
                match event.token() {
                    UDP_SERVER => loop {
                        match state.server_socket.recv_from(&mut buf) {
                            Ok((packet_size, source_address)) => {
                                let message = serde_json::from_slice(&buf[..packet_size])?;
                                state.request_tx.send(ArtilleryClusterRequest::Respond(source_address, message))?;
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // If we get a `WouldBlock` error we know our socket
                                // has no more packets queued, so we can return to
                                // polling and wait for some more.
                                break;
                            }
                            Err(e) => {
                                // If it was any other kind of error, something went
                                // wrong and we terminate with an error.
                                bail!(
                                    ArtilleryError::UnexpectedError,
                                    format!(
                                        "Unexpected error occured in event loop: {}",
                                        e.to_string()
                                    )
                                )
                            }
                        }
                    },
                    _ => {
                        warn!("Got event for unexpected token: {:?}", event);
                    }
                }
            }
        }

        info!("Exiting...");
        Ok(())
    }

    fn process_request(&mut self, request: TargetedRequest) {
        use Request::*;

        let timeout = Utc::now() + self.config.ping_timeout;
        let should_add_pending = request.request == Ping;
        let message = build_message(&self.host_key,
                                    &self.config.cluster_key,
                                    request.request,
                                    self.state_changes.clone(),
                                    self.config.network_mtu);

        if should_add_pending {
            self.pending_responses.push((timeout, request.target.clone(), message.state_changes.clone()));
        }

        let encoded = serde_json::to_string(&message).unwrap();

        assert!(encoded.len() < self.config.network_mtu);

        let mut buf = encoded.as_bytes();
        self.server_socket.send_to(&mut buf, request.target).unwrap();
    }

    fn enqueue_seed_nodes(&self) {
        for seed_node in &self.seed_queue {
            self.request_tx.send(ArtilleryClusterRequest::React(TargetedRequest {
                request: Request::Ping,
                target: seed_node.clone(),
            })).unwrap();
        }
    }

    fn enqueue_random_ping(&mut self) {
        if let Some(member) = self.members.next_random_member() {
            self.request_tx.send(ArtilleryClusterRequest::React(TargetedRequest {
                request: Request::Ping,
                target: member.remote_host().unwrap(),
            })).unwrap();
        }
    }

    fn prune_timed_out_responses(&mut self) {
        let now = Utc::now();

        let (remaining, expired): (Vec<_>, Vec<_>) = self.pending_responses
            .iter()
            .cloned()
            .partition(| &(t, _, _) | t < now);

        let expired_hosts: HashSet<SocketAddr> = expired
            .iter()
            .map(| &(_, a, _) | a)
            .collect();

        self.pending_responses = remaining;

        let (suspect, down) = self.members.time_out_nodes(expired_hosts);

        enqueue_state_change(&mut self.state_changes, &down);
        enqueue_state_change(&mut self.state_changes, &suspect);

        for member in suspect {
            self.send_ping_requests(&member);
            self.send_member_event(ArtilleryMemberEvent::MemberSuspectedDown(member.clone()));
        }

        for member in down {
            self.send_member_event(ArtilleryMemberEvent::MemberWentDown(member.clone()));
        }
    }

    fn send_ping_requests(&self, target: &ArtilleryMember) {
        if let Some(target_host) = target.remote_host() {
            for relay in self.members.hosts_for_indirect_ping(self.config.ping_request_host_count, &target_host) {
                self.request_tx.send(ArtilleryClusterRequest::React(TargetedRequest {
                    request: Request::PingRequest(EncSocketAddr::from_addr(&target_host)),
                    target: relay,
                })).unwrap();
            }
        }
    }

    fn process_internal_request(&mut self, message: ArtilleryClusterRequest) -> Option<Sender<()>> {
        use ArtilleryClusterRequest::*;

        match message {
            AddSeed(addr) => self.seed_queue.push(addr),
            Respond(src_addr, message) => self.respond_to_message(src_addr, message),
            React(request) => {
                self.prune_timed_out_responses();
                self.process_request(request);
            },
            LeaveCluster => {
                let myself = self.members.leave();
                enqueue_state_change(&mut self.state_changes, &[myself]);
            },
            Exit(tx) => return Some(tx),
        };

        None
    }

    fn respond_to_message(&mut self, src_addr: SocketAddr, message: ArtilleryMessage) {
        use Request::*;

        if message.cluster_key != self.config.cluster_key {
            error!("Mismatching cluster keys, ignoring message");
        }
        else {
            self.apply_state_changes(message.state_changes, src_addr);
            remove_potential_seed(&mut self.seed_queue, src_addr);

            self.ensure_node_is_member(src_addr, message.sender);

            let response = match message.request {
                Ping => Some(TargetedRequest { request: Ack, target: src_addr }),
                Ack => {
                    self.ack_response(src_addr);
                    self.mark_node_alive(src_addr);
                    None
                },
                PingRequest(dest_addr) => {
                    let EncSocketAddr(dest_addr) = dest_addr;
                    add_to_wait_list(&mut self.wait_list, &dest_addr, &src_addr);
                    Some(TargetedRequest { request: Ping, target: dest_addr })
                },
                AckHost(member) => {
                    self.ack_response(member.remote_host().unwrap());
                    self.mark_node_alive(member.remote_host().unwrap());
                    None
                }
            };

            match response {
                Some(response) => self.request_tx.send(
                    ArtilleryClusterRequest::React(response)).unwrap(),
                None => (),
            };
        }
    }

    fn ack_response(&mut self, src_addr: SocketAddr) {
        let mut to_remove = Vec::new();

        for &(ref t, ref addr, ref state_changes) in self.pending_responses.iter() {
            if src_addr != *addr {
                continue;
            }

            to_remove.push((t.clone(), addr.clone(), state_changes.clone()));

            self.state_changes
                .retain(|os| !state_changes.iter().any(| is | is.member().host_key() == os.member().host_key()))
        }

        self.pending_responses.retain(|op| !to_remove.iter().any(|ip| ip == op));
    }

    fn ensure_node_is_member(&mut self, src_addr: SocketAddr, sender: Uuid) {
        if self.members.has_member(&src_addr) {
            return;
        }

        let new_member = ArtilleryMember::new(sender, src_addr, 0, ArtilleryMemberState::Alive);

        self.members.add_member(new_member.clone());
        enqueue_state_change(&mut self.state_changes, &[new_member.clone()]);
        self.send_member_event(ArtilleryMemberEvent::MemberJoined(new_member));
    }

    fn send_member_event(&self, event: ArtilleryMemberEvent) {
        use ArtilleryMemberEvent::*;

        match event {
            MemberJoined(_) => {},
            MemberWentUp(ref m) => assert_eq!(m.state(), ArtilleryMemberState::Alive),
            MemberWentDown(ref m) => assert_eq!(m.state(), ArtilleryMemberState::Down),
            MemberSuspectedDown(ref m) => assert_eq!(m.state(), ArtilleryMemberState::Suspect),
            MemberLeft(ref m) => assert_eq!(m.state(), ArtilleryMemberState::Left),
        };

        self.event_tx.send((self.members.available_nodes(), event)).unwrap();
    }

    fn apply_state_changes(&mut self, state_changes: Vec<ArtilleryStateChange>, from: SocketAddr) {
        let (new, changed) = self.members.apply_state_changes(state_changes, &from);

        enqueue_state_change(&mut self.state_changes, &new);
        enqueue_state_change(&mut self.state_changes, &changed);

        for member in new {
            self.send_member_event(ArtilleryMemberEvent::MemberJoined(member));
        }

        for member in changed {
            self.send_member_event(determine_member_event(member));
        }
    }

    fn mark_node_alive(&mut self, src_addr: SocketAddr) {
        if let Some(member) = self.members.mark_node_alive(&src_addr) {
            match self.wait_list.get_mut(&src_addr) {
                Some(mut wait_list) => {
                    for remote in wait_list.iter() {
                        self.request_tx.send(ArtilleryClusterRequest::React(TargetedRequest {
                            request: Request::AckHost(member.clone()),
                            target: *remote
                        })).unwrap();
                    }

                    wait_list.clear();
                },
                None => ()
            };

            enqueue_state_change(&mut self.state_changes, &[member.clone()]);
            self.send_member_event(ArtilleryMemberEvent::MemberWentUp(member.clone()));
        }
    }
}

fn build_message(sender: &Uuid,
                 cluster_key: &Vec<u8>,
                 request: Request,
                 state_changes: Vec<ArtilleryStateChange>,
                 network_mtu: usize) -> ArtilleryMessage {
    let mut message = ArtilleryMessage {
        sender: sender.clone(),
        cluster_key: cluster_key.clone(),
        request: request.clone(),
        state_changes: Vec::new(),
    };

    for i in 0..state_changes.len() + 1 {
        message = ArtilleryMessage {
            sender: sender.clone(),
            cluster_key: cluster_key.clone(),
            request: request.clone(),
            state_changes: (&state_changes[..i]).iter().cloned().collect(),
        };

        let encoded = serde_json::to_string(&message).unwrap();
        if encoded.len() >= network_mtu {
            return message;
        }
    }

    message
}

fn add_to_wait_list(wait_list: &mut WaitList, wait_addr: &SocketAddr, notify_addr: &SocketAddr) {
    match wait_list.entry(*wait_addr) {
        Entry::Occupied(mut entry) => { entry.get_mut().push(notify_addr.clone()); },
        Entry::Vacant(entry) => { entry.insert(vec![notify_addr.clone()]); }
    };
}

fn remove_potential_seed(seed_queue: &mut Vec<SocketAddr>, src_addr: SocketAddr) {
    seed_queue.retain(|&addr| addr != src_addr)
}

fn determine_member_event(member: ArtilleryMember) -> ArtilleryMemberEvent {
    use ArtilleryMemberState::*;
    use ArtilleryMemberEvent::*;

    match member.state() {
        Alive => MemberWentUp(member),
        Suspect => MemberSuspectedDown(member),
        Down => MemberWentDown(member),
        Left => MemberLeft(member),
    }
}

fn enqueue_state_change(state_changes: &mut Vec<ArtilleryStateChange>, members: &[ArtilleryMember]) {
    for member in members {
        for state_change in state_changes.iter_mut() {
            if state_change.member().host_key() == member.host_key() {
                state_change.update(member.clone());
                return;
            }
        }

        state_changes.push(ArtilleryStateChange::new(member.clone()));
    }
}

impl EncSocketAddr {
    fn from_addr(addr: &SocketAddr) -> Self {
        EncSocketAddr(addr.clone())
    }
}
#![feature(iter_advance_by)]
#![feature(never_type)]
#![feature(lazy_cell)]
#![feature(trait_alias)]
#![feature(impl_trait_in_assoc_type)]
#![feature(array_windows)]
#![feature(iter_collect_into)]
#![feature(let_chains)]
#![feature(entry_insert)]
#![feature(iter_next_chunk)]
#![feature(if_let_guard)]
#![feature(map_try_insert)]
#![feature(macro_metavar_expr)]
#![feature(slice_take)]

use {
    self::api::{chat::Chat, MissingTopic},
    crate::api::Handler as _,
    anyhow::Context as _,
    chain_api::{ChatStakeEvent, EnvConfig, Mnemonic, NodeKeys, NodeVec, StakeEvents},
    chat_spec::{rpcs, ChatError, ChatName, Identity, Profile, Request, Topic, REPLICATION_FACTOR},
    codec::{Codec, Reminder, ReminderOwned},
    dashmap::{mapref::entry::Entry, DashMap},
    dht::Route,
    futures::channel::mpsc,
    handlers::Handler,
    libp2p::{
        core::{multiaddr, muxing::StreamMuxerBox, upgrade::Version},
        futures::{self, channel::oneshot, future::Either, SinkExt, StreamExt},
        swarm::{NetworkBehaviour, SwarmEvent},
        Multiaddr, PeerId,
    },
    onion::{key_share, EncryptedStream, PathId},
    opfusk::ToPeerId,
    rand_core::OsRng,
    rpc::CallId,
    std::{
        collections::HashMap, future::Future, io, net::Ipv4Addr, ops::DerefMut, sync::Arc,
        task::Poll, time::Duration,
    },
    tokio::sync::RwLock,
    topology_wrapper::BuildWrapped,
};

mod api;
mod db;
mod reputation;
#[cfg(test)]
mod tests;

type Context = &'static OwnedContext;
type SE = libp2p::swarm::SwarmEvent<<Behaviour as NetworkBehaviour>::ToSwarm>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let (node_config, chain_config) =
        config::combine_errors(NodeConfig::from_env(), EnvConfig::from_env())?;
    let keys = NodeKeys::from_mnemonic(&node_config.mnemonic);
    let (node_list, stake_events) = chain_config.connect_chat(&keys, true).await?;
    reputation::Rep::report_at_background(keys.identity());

    Server::new(node_config, keys, node_list, stake_events).await?.await;

    Ok(())
}

config::env_config! {
    struct NodeConfig {
        /// The port to listen on and publish to chain
        port: u16,
        /// The port to listen on for websocket connections, clients expect `port + 1`
        ws_port: u16,
        /// The mnemonic to derive keys from
        mnemonic: Mnemonic,
        /// Idle connection is dropped after ms of inactivity
        idle_timeout: u64,
        /// Rpc request is dropped after ms of delay
        rpc_timeout: u64,
    }
}

enum RpcRespChannel {
    Oneshot(oneshot::Sender<rpc::Result<Vec<u8>>>),
    Mpsc(mpsc::Sender<(PeerId, rpc::Result<Vec<u8>>)>),
}

impl RpcRespChannel {
    fn send(self, peer: PeerId, msg: rpc::Result<Vec<u8>>) {
        match self {
            Self::Oneshot(s) => _ = s.send(msg),
            Self::Mpsc(mut s) => _ = s.try_send((peer, msg)),
        }
    }
}

struct Server {
    swarm: libp2p::swarm::Swarm<Behaviour>,
    context: Context,
    request_events: mpsc::Receiver<RequestEvent>,
    clients: futures::stream::SelectAll<Stream>,
    client_router: handlers::Router<api::RouterContext>,
    server_router: handlers::Router<api::RouterContext>,
    stake_events: StakeEvents<ChatStakeEvent>,
    pending_rpcs: HashMap<CallId, RpcRespChannel>,
}

impl Server {
    async fn new(
        config: NodeConfig,
        keys: NodeKeys,
        node_list: NodeVec,
        stake_events: StakeEvents<ChatStakeEvent>,
    ) -> anyhow::Result<Self> {
        use libp2p::core::Transport;

        let NodeConfig { port, ws_port, idle_timeout, .. } = config;

        let (sender, receiver) = topology_wrapper::channel();

        let sender_clone = sender.clone();
        let mut swarm = libp2p::Swarm::new(
            libp2p::tcp::tokio::Transport::default()
                .upgrade(Version::V1)
                .authenticate(opfusk::Config::new(OsRng, keys.sign))
                .multiplex(libp2p::yamux::Config::default())
                .or_transport(
                    libp2p::websocket::WsConfig::new(libp2p::tcp::tokio::Transport::default())
                        .upgrade(Version::V1)
                        .authenticate(opfusk::Config::new(OsRng, keys.sign))
                        .multiplex(libp2p::yamux::Config::default()),
                )
                .map(move |option, _| match option {
                    Either::Left((peer, stream)) => (
                        peer,
                        StreamMuxerBox::new(topology_wrapper::muxer::new(
                            stream,
                            sender_clone.clone(),
                        )),
                    ),
                    Either::Right((peer, stream)) => (
                        peer,
                        StreamMuxerBox::new(topology_wrapper::muxer::new(stream, sender_clone)),
                    ),
                })
                .boxed(),
            Behaviour {
                key_share: key_share::Behaviour::new(keys.enc.public_key())
                    .include_in_vis(sender.clone()),
                onion: onion::Config::new(keys.enc.into(), keys.sign.to_peer_id())
                    .max_streams(10)
                    .keep_alive_interval(Duration::from_secs(100))
                    .build()
                    .include_in_vis(sender.clone()),
                dht: dht::Behaviour::new(chain_api::filter_incoming),
                rpc: rpc::Config::new()
                    .request_timeout(Duration::from_millis(config.rpc_timeout))
                    .build()
                    .include_in_vis(sender.clone()),
                report: topology_wrapper::report::new(receiver),
            },
            keys.sign.to_peer_id(),
            libp2p::swarm::Config::with_tokio_executor()
                .with_idle_connection_timeout(Duration::from_millis(idle_timeout)),
        );

        swarm
            .listen_on(
                Multiaddr::empty()
                    .with(multiaddr::Protocol::Ip4(Ipv4Addr::UNSPECIFIED))
                    .with(multiaddr::Protocol::Tcp(port)),
            )
            .context("starting to listen for peers")?;

        swarm
            .listen_on(
                Multiaddr::empty()
                    .with(multiaddr::Protocol::Ip4(Ipv4Addr::UNSPECIFIED))
                    .with(multiaddr::Protocol::Tcp(ws_port))
                    .with(multiaddr::Protocol::Ws("/".into())),
            )
            .context("starting to isten for clients")?;

        let node_data =
            node_list.into_iter().map(|(id, addr)| Route::new(id, chain_api::unpack_addr(addr)));
        swarm.behaviour_mut().dht.table.bulk_insert(node_data);

        let (request_event_sink, request_events) = mpsc::channel(100);

        Ok(Self {
            context: Box::leak(Box::new(OwnedContext {
                profiles: Default::default(),
                online: Default::default(),
                chats: Default::default(),
                request_event_sink,
                local_peer_id: *swarm.local_peer_id(),
            })),
            swarm,
            request_events,
            clients: Default::default(),
            stake_events,
            client_router: handlers::router!(
                api::chat => {
                    rpcs::CREATE_CHAT => create.repl();
                    rpcs::ADD_MEMBER => add_member.repl().restore();
                    rpcs::KICK_MEMBER => kick_member.repl().restore();
                    rpcs::SEND_MESSAGE => send_message.repl().restore();
                    rpcs::FETCH_MESSAGES => fetch_messages.restore();
                    rpcs::FETCH_MEMBERS => fetch_members.restore();
                };
                api::profile => {
                    rpcs::CREATE_PROFILE => create.repl();
                    rpcs::SEND_MAIL => send_mail.repl().restore();
                    rpcs::READ_MAIL => read_mail.repl().restore();
                    rpcs::INSERT_TO_VAULT => insert_to_vault.repl().restore();
                    rpcs::REMOVE_FROM_VAULT => remove_from_vault.repl().restore();
                    rpcs::FETCH_PROFILE => fetch_keys.restore();
                    rpcs::FETCH_VAULT => fetch_vault.restore();
                    rpcs::FETCH_VAULT_KEY => fetch_vault_key.restore();
                };
            ),
            server_router: handlers::router!(
                api::chat => {
                    rpcs::CREATE_CHAT => create;
                    rpcs::ADD_MEMBER => add_member.restore();
                    rpcs::KICK_MEMBER => kick_member.restore();
                    rpcs::SEND_BLOCK => handle_message_block
                        .rated(rate_map! { BlockNotExpected => 10, BlockUnexpectedMessages => 100, Outdated => 5 })
                        .restore().no_resp();
                    rpcs::FETCH_CHAT_DATA => fetch_chat_data.rated(rate_map!(20));
                    rpcs::SEND_MESSAGE => send_message.restore();
                    rpcs::VOTE_BLOCK => vote
                        .rated(rate_map! { NoReplicator => 50, NotFound => 5, AlreadyVoted => 30 })
                        .restore().no_resp();
                };
                api::profile => {
                    rpcs::CREATE_PROFILE => create;
                    rpcs::SEND_MAIL => send_mail.restore();
                    rpcs::READ_MAIL => read_mail.restore();
                    rpcs::INSERT_TO_VAULT => insert_to_vault.restore();
                    rpcs::REMOVE_FROM_VAULT => remove_from_vault.restore();
                    rpcs::FETCH_PROFILE_FULL => fetch_full.rated(rate_map!(20));
                };
            ),
            pending_rpcs: Default::default(),
        })
    }

    fn swarm_event(&mut self, event: SE) {
        match event {
            SwarmEvent::Behaviour(BehaviourEvent::Rpc(rpc::Event::Request(peer, id, body))) => {
                let Some((prefix, topic, body)) =
                    <(u8, Topic, Reminder)>::decode(&mut body.as_slice())
                else {
                    log::warn!("received invalid internal rpc request");
                    return;
                };

                let Some(missing_topic) = self.validate_topic(Some(topic)) else {
                    log::warn!("received request with invalid topic: {:?}", topic);
                    return;
                };

                self.server_router.handle(api::State {
                    req: Request { prefix, id, topic: Some(topic), body },
                    swarm: &mut self.swarm,
                    location: OnlineLocation::Remote(peer),
                    context: self.context,
                    missing_topic,
                });
            }
            SwarmEvent::Behaviour(BehaviourEvent::Rpc(rpc::Event::Response(peer, id, body))) => {
                let Some(resp) = self.pending_rpcs.remove(&id) else {
                    log::warn!("received response to unknown rpc {:?} {:?}", id, body);
                    return;
                };
                resp.send(peer, body);
            }
            SwarmEvent::Behaviour(BehaviourEvent::Onion(onion::Event::InboundStream(
                inner,
                id,
            ))) => {
                self.clients.push(Stream::new(id, inner));
            }
            SwarmEvent::Behaviour(_ev) => {}
            e => log::debug!("{e:?}"),
        }
    }

    fn client_message(&mut self, (id, req): (PathId, Vec<u8>)) {
        let Some(req) = chat_spec::Request::decode(&mut req.as_slice()) else {
            log::warn!("failed to decode client request: {:?}", req);
            return;
        };

        let Some(missing_topic) = self.validate_topic(req.topic) else {
            log::warn!("client sent message with invalid topic: {:?}", req.topic);
            return;
        };

        log::debug!("received message from client: {:?} {:?}", req.id, req.prefix);

        match req.prefix {
            rpcs::SUBSCRIBE => self.handle_subscribe(id, req, missing_topic),
            rpcs::UNSUBSCRIBE => self.handle_unsubscribe(id, req, missing_topic),
            _ => self.client_router.handle(api::State {
                req,
                swarm: &mut self.swarm,
                location: OnlineLocation::Local(id),
                context: self.context,
                missing_topic,
            }),
        }
    }

    fn handle_subscribe(
        &mut self,
        id: PathId,
        req: chat_spec::Request,
        missing_topic: Option<MissingTopic>,
    ) {
        let client = self.clients.iter_mut().find(|c| c.id == id).expect("no you don't");
        if let Some(topic) = req.topic
            && missing_topic.is_none()
        {
            log::debug!("client subscribed to topic: {:?}", topic);
            match topic {
                Topic::Profile(identity) => client.profile_sub = Some((identity, req.id)),
                Topic::Chat(chat) => _ = client.subscriptions.insert(chat, req.id),
            }
            _ = client.inner.write((req.id, Ok::<(), ChatError>(())));
        } else {
            _ = client.inner.write((req.id, Err::<(), ChatError>(ChatError::NotFound)));
        }
    }

    fn handle_unsubscribe(
        &mut self,
        id: PathId,
        req: chat_spec::Request,
        missing_topic: Option<MissingTopic>,
    ) {
        let client = self.clients.iter_mut().find(|c| c.id == id).expect("no you don't");
        if let Some(topic) = req.topic
            && missing_topic.is_none()
        {
            log::debug!("client unsubscribed from topic: {:?}", topic);
            match topic {
                Topic::Profile(_) => client.profile_sub = None,
                Topic::Chat(chat) => _ = client.subscriptions.remove(&chat),
            }
        }
    }

    fn validate_topic(&mut self, topic: Option<Topic>) -> Option<Option<MissingTopic>> {
        let Some(topic) = topic else {
            return Some(None);
        };

        let us = *self.swarm.local_peer_id();
        if self
            .swarm
            .behaviour_mut()
            .dht
            .table
            .closest(topic.as_bytes())
            .take(REPLICATION_FACTOR.get() + 1)
            .all(|r| r.peer_id() != us)
        {
            return None;
        }

        Some(match topic {
            Topic::Profile(prf) if !self.context.profiles.contains_key(&prf) => {
                Some(MissingTopic::Profile(prf))
            }
            Topic::Chat(name) if let Entry::Vacant(v) = self.context.chats.entry(name) => {
                Some(MissingTopic::Chat {
                    name,
                    lock: v.insert(Default::default()).clone().try_write_owned().unwrap(),
                })
            }
            _ => None,
        })
    }

    fn handle_chat_event(&mut self, chat: ChatName, event: Vec<u8>) {
        for client in self.clients.iter_mut() {
            let Some(call) = client.subscriptions.get(&chat) else {
                continue;
            };

            _ = client.inner.write((call, Reminder(&event)));
        }
    }

    fn handle_profile_event(
        &mut self,
        identity: Identity,
        event: Vec<u8>,
        success_resp: oneshot::Sender<()>,
    ) {
        let Some((id, client)) = self
            .clients
            .iter_mut()
            .find_map(|c| (c.profile_sub?.0 == identity).then_some((c.profile_sub?.1, c)))
        else {
            return;
        };

        if client.inner.write((id, ReminderOwned(event))).is_none() {
            return;
        }

        _ = success_resp.send(());
    }

    fn handle_rpc(
        &mut self,
        peer: PeerId,
        msg: Vec<u8>,
        resp: Option<oneshot::Sender<rpc::Result<Vec<u8>>>>,
    ) {
        let call_id =
            match self.swarm.behaviour_mut().rpc.request(peer, msg.as_slice(), resp.is_none()) {
                Ok(call_id) => call_id,
                Err(e) => {
                    if let Some(resp) = resp {
                        _ = resp.send(Err(e));
                    }
                    return;
                }
            };

        if let Some(resp) = resp {
            self.pending_rpcs.insert(call_id, RpcRespChannel::Oneshot(resp));
        }
    }

    fn handle_repl_rpc(
        &mut self,
        topic: Topic,
        msg: Vec<u8>,
        mut resp: Option<mpsc::Sender<(PeerId, rpc::Result<Vec<u8>>)>>,
    ) {
        let us = *self.swarm.local_peer_id();
        let beh = self.swarm.behaviour_mut();
        for peer in beh
            .dht
            .table
            .closest(topic.as_bytes())
            .take(REPLICATION_FACTOR.get() + 1)
            .map(Route::peer_id)
            .filter(|p| *p != us)
        {
            let call_id = match beh.rpc.request(peer, msg.as_slice(), resp.is_none()) {
                Ok(call_id) => call_id,
                Err(e) => {
                    if let Some(resp) = &mut resp {
                        _ = resp.try_send((peer, Err(e)));
                    } else {
                        log::warn!("failed to send rpc to {:?}: {}", peer, e);
                    }
                    continue;
                }
            };

            if let Some(resp) = &resp {
                self.pending_rpcs.insert(call_id, RpcRespChannel::Mpsc(resp.clone()));
            }
        }
    }

    fn server_router_response(
        &mut self,
        (res, (loc, id)): (handlers::Response, (OnlineLocation, CallId)),
    ) {
        let OnlineLocation::Remote(peer) = loc else { return };

        match res {
            handlers::Response::Success(b) | handlers::Response::Failure(b) => {
                self.swarm.behaviour_mut().rpc.respond(peer, id, b)
            }
            handlers::Response::DontRespond(_) => {}
        };
    }

    fn client_router_response(
        &mut self,
        (res, (loc, id)): (handlers::Response, (OnlineLocation, CallId)),
    ) {
        let OnlineLocation::Local(client) = loc else { return };

        match res {
            handlers::Response::Success(pl) | handlers::Response::Failure(pl) => {
                let Some(client) = self.clients.iter_mut().find(|c| c.id == client) else {
                    log::warn!("client did not wait for respnse");
                    return;
                };
                if client.inner.write((id, ReminderOwned(pl))).is_none() {
                    log::warn!("client cant keep up with own requests");
                    client.inner.close();
                };
            }
            handlers::Response::DontRespond(_) => {}
        }
    }

    fn stake_event(&mut self, event: ChatStakeEvent) {
        match event {
            ChatStakeEvent::Voted { source, target } => reputation::Rep::get().vote(source, target),
            ev => chain_api::stake_event(self, ev),
        }
    }
}

impl Future for Server {
    type Output = !;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        use component_utils::field as f;

        let log_message = |_: &mut Self, e| log::warn!("failed to read client message: {e}");
        let log_stake = |_: &mut Self, e| log::warn!("failed to read stake event: {e}");

        while component_utils::Selector::new(self.deref_mut(), cx)
            .try_stream(f!(mut clients), Self::client_message, log_message)
            .stream(f!(mut swarm), Self::swarm_event)
            .try_stream(f!(mut stake_events), Self::stake_event, log_stake)
            .stream(f!(mut client_router), Self::client_router_response)
            .stream(f!(mut server_router), Self::server_router_response)
            .stream(f!(mut request_events), |s, event| match event {
                RequestEvent::Profile(identity, event, resp) => {
                    s.handle_profile_event(identity, event, resp)
                }
                RequestEvent::Chat(topic, event) => s.handle_chat_event(topic, event),
                RequestEvent::Rpc(peer, msg, resp) => s.handle_rpc(peer, msg, resp),
                RequestEvent::ReplRpc(topic, msg, resp) => s.handle_repl_rpc(topic, msg, resp),
            })
            .done()
        {}

        Poll::Pending
    }
}

impl AsMut<dht::Behaviour> for Server {
    fn as_mut(&mut self) -> &mut dht::Behaviour {
        &mut self.swarm.behaviour_mut().dht
    }
}

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    onion: topology_wrapper::Behaviour<onion::Behaviour>,
    dht: dht::Behaviour,
    rpc: topology_wrapper::Behaviour<rpc::Behaviour>,
    report: topology_wrapper::report::Behaviour,
    key_share: topology_wrapper::Behaviour<key_share::Behaviour>,
}

enum InnerStream {
    Normal(EncryptedStream),
    #[cfg(test)]
    Test(mpsc::Receiver<Vec<u8>>, Option<mpsc::Sender<Vec<u8>>>),
}

impl InnerStream {
    #[must_use = "write could have failed"]
    pub fn write<'a>(&mut self, data: impl Codec<'a>) -> Option<()> {
        match self {
            Self::Normal(s) => s.write(data),
            #[cfg(test)]
            Self::Test(_, s) => s.as_mut()?.try_send(data.to_bytes()).ok(),
        }
    }

    pub fn close(&mut self) {
        match self {
            Self::Normal(s) => s.close(),
            #[cfg(test)]
            Self::Test(_, s) => _ = s.take(),
        }
    }
}

pub struct Stream {
    id: PathId,
    profile_sub: Option<(Identity, CallId)>,
    subscriptions: HashMap<ChatName, CallId>,
    inner: InnerStream,
}

impl Stream {
    fn new(id: PathId, inner: EncryptedStream) -> Self {
        Self {
            id,
            profile_sub: Default::default(),
            subscriptions: Default::default(),
            inner: InnerStream::Normal(inner),
        }
    }

    #[cfg(test)]
    fn new_test() -> [Self; 2] {
        let (input_s, input_r) = mpsc::channel(100);
        let (output_s, output_r) = mpsc::channel(100);
        let id = PathId::new();
        [(input_r, output_s), (output_r, input_s)].map(|(a, b)| Self {
            id,
            profile_sub: Default::default(),
            subscriptions: Default::default(),
            inner: InnerStream::Test(a, Some(b)),
        })
    }
}

impl libp2p::futures::Stream for Stream {
    type Item = io::Result<(PathId, Vec<u8>)>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match &mut self.inner {
            InnerStream::Normal(n) => n.poll_next_unpin(cx).map_ok(|v| (self.id, v)),
            #[cfg(test)]
            InnerStream::Test(t, _) => t.poll_next_unpin(cx).map(|v| v.map(|v| Ok((self.id, v)))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnlineLocation {
    Local(PathId),
    Remote(PeerId),
}

enum RequestEvent {
    Profile(Identity, Vec<u8>, oneshot::Sender<()>),
    Chat(ChatName, Vec<u8>),
    Rpc(PeerId, Vec<u8>, Option<oneshot::Sender<rpc::Result<Vec<u8>>>>),
    ReplRpc(Topic, Vec<u8>, Option<mpsc::Sender<(PeerId, rpc::Result<Vec<u8>>)>>),
}

// TODO: switch to lmdb
// TODO: remove recovery locks, ommit response instead
pub struct OwnedContext {
    profiles: DashMap<Identity, Profile>,
    online: DashMap<Identity, OnlineLocation>,
    chats: DashMap<ChatName, Arc<RwLock<Chat>>>,
    request_event_sink: mpsc::Sender<RequestEvent>,
    local_peer_id: PeerId,
}

fn rpc<'a>(prefix: u8, topic: impl Into<Topic>, msg: impl Codec<'a>) -> Vec<u8> {
    (prefix, topic.into(), msg).to_bytes()
}

impl OwnedContext {
    async fn push_chat_event(&self, topic: ChatName, msg: chat_spec::ChatEvent<'_>) {
        _ = self.request_event_sink.clone().send(RequestEvent::Chat(topic, msg.to_bytes())).await;
    }

    async fn push_profile_event(&self, for_who: [u8; 32], mail: &[u8]) -> bool {
        let (sender, recv) = oneshot::channel();
        let ev = RequestEvent::Profile(for_who, mail.to_vec(), sender);
        _ = self.request_event_sink.clone().send(ev).await;
        recv.await.is_ok()
    }

    async fn repl_rpc_no_resp<'a>(&self, topic: impl Into<Topic>, prefix: u8, msg: impl Codec<'a>) {
        let topic = topic.into();
        let ev = RequestEvent::ReplRpc(topic, rpc(prefix, topic, msg), None);
        _ = self.request_event_sink.clone().send(ev).await;
    }

    async fn repl_rpc<'a>(
        &self,
        topic: impl Into<Topic>,
        prefix: u8,
        msg: impl Codec<'a>,
    ) -> impl futures::Stream<Item = (PeerId, rpc::Result<Vec<u8>>)> {
        let topic = topic.into();
        let (sender, recv) = mpsc::channel(REPLICATION_FACTOR.get());
        let ev = RequestEvent::ReplRpc(topic, rpc(prefix, topic, msg), Some(sender));
        _ = self.request_event_sink.clone().send(ev).await;
        recv
    }

    async fn send_rpc<'a>(
        &self,
        topic: impl Into<Topic>,
        peer: PeerId,
        send_mail: u8,
        mail: impl Codec<'a>,
    ) -> rpc::Result<Vec<u8>> {
        let topic = topic.into();
        let (sender, recv) = oneshot::channel();
        let ev = RequestEvent::Rpc(peer, rpc(send_mail, topic, mail), Some(sender));
        _ = self.request_event_sink.clone().send(ev).await;
        recv.await.unwrap_or(Err(rpc::Error::Timeout.into()))
    }
}

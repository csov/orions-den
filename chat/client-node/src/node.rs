use {
    crate::{
        chain::{chain_node, min_nodes},
        timeout, RawOnionRequest, RawResponse, RawSateliteRequest, RawStorageRequest,
        RequestChannel, RequestInit, Requests, SubscriptionInit, SubscriptionMessage, UserKeys,
        Vault,
    },
    anyhow::Context,
    chat_spec::*,
    codec::{Codec, Reminder},
    component_utils::{FindAndRemove, PacketWriter},
    crypto::{
        enc,
        proof::{Nonce, Proof},
    },
    dht::Route,
    libp2p::{
        core::upgrade::Version,
        futures::{
            channel::{mpsc, oneshot},
            stream::FuturesUnordered,
            StreamExt,
        },
        swarm::{NetworkBehaviour, SwarmEvent},
        *,
    },
    onion::{EncryptedStream, PathId},
    opfusk::ToPeerId,
    rand::{rngs::OsRng, seq::IteratorRandom},
    std::{
        collections::{BTreeMap, HashMap, HashSet, VecDeque},
        convert::Infallible,
        future::Future,
        io,
        ops::DerefMut,
        pin,
        task::Poll,
        time::Duration,
    },
};

type StreamNegotiation = impl Future<Output = io::Result<()>>;

pub struct Node {
    swarm: Swarm<Behaviour>,
    subscriptions: futures::stream::SelectAll<Subscription>,
    pending_chat_requests: HashMap<CallId, oneshot::Sender<RawResponse>>,
    _pending_rpc_requests: HashMap<CallId, oneshot::Sender<RawResponse>>,
    pending_topic_search: HashMap<PathId, Vec<RequestInit>>,
    pending_streams: VecDeque<(PeerId, CallId, oneshot::Sender<libp2p::Stream>)>,
    negotiated_streams: FuturesUnordered<StreamNegotiation>,
    requests: RequestChannel,
}

impl Node {
    pub async fn new(
        keys: UserKeys,
        mut wboot_phase: impl FnMut(BootPhase),
    ) -> anyhow::Result<(Self, Vault, Requests, Nonce, Nonce)> {
        macro_rules! set_state { ($($t:tt)*) => {_ = wboot_phase(BootPhase::$($t)*)}; }

        set_state!(FetchNodesAndProfile);

        let (mut request_dispatch, commands) = Requests::new();
        let chain_api = chain_node(keys.name).await?;
        let node_request = chain_api.list_chat_nodes();
        let satelite_request = chain_api.list_satelite_nodes();
        let profile_request = chain_api.get_profile_by_name(username_to_raw(keys.name));
        let (node_data, satelite_data, profile_hash) =
            futures::try_join!(node_request, satelite_request, profile_request)?;
        let profile_hash = profile_hash.context("profile not found")?;
        let profile = keys.to_identity();

        anyhow::ensure!(
            profile_hash.sign == profile.sign && profile_hash.enc == profile.enc,
            "profile hash does not match our account"
        );

        set_state!(InitiateConnection);

        let mut swarm = libp2p::Swarm::new(
            libp2p::websocket_websys::Transport::default()
                .upgrade(Version::V1)
                .authenticate(opfusk::Config::new(OsRng, keys.sign))
                .multiplex(libp2p::yamux::Config::default())
                .boxed(),
            Behaviour {
                onion: onion::Config::default()
                    .keep_alive_interval(Duration::from_secs(100))
                    .build(),
                ..Default::default()
            },
            keys.sign.to_peer_id(),
            libp2p::swarm::Config::with_wasm_executor()
                .with_idle_connection_timeout(std::time::Duration::from_secs(10)),
        );

        let node_count = node_data.len();
        let tolerance = 0;
        set_state!(CollecringKeys(
            node_count - swarm.behaviour_mut().key_share.keys.len() - tolerance
        ));

        let nodes = node_data.into_iter().map(|(id, addr)| {
            let addr = chain_api::unpack_addr_offset(addr, 1);
            Route::new(id, addr.with(multiaddr::Protocol::Ws("/".into())))
        });
        swarm.behaviour_mut().chat_dht.table.bulk_insert(nodes);

        let satelites = satelite_data.into_iter().map(|(id, addr)| {
            let addr = chain_api::unpack_addr_offset(addr, 1);
            Route::new(id, addr.with(multiaddr::Protocol::Ws("/".into())))
        });
        swarm.behaviour_mut().satelite_dht.table.bulk_insert(satelites);

        let routes =
            swarm.behaviour_mut().chat_dht.table.iter().map(Route::peer_id).collect::<Vec<_>>();
        for route in routes {
            _ = swarm.dial(route);
        }

        _ = timeout(
            async {
                loop {
                    match swarm.select_next_some().await {
                        SwarmEvent::Behaviour(BehaviourEvent::KeyShare(..)) => {
                            let remining =
                                node_count - swarm.behaviour_mut().key_share.keys.len() - tolerance;
                            set_state!(CollecringKeys(remining));
                            if remining == 0 {
                                break;
                            }
                        }
                        e => log::debug!("{:?}", e),
                    }
                }
            },
            Duration::from_secs(10),
        )
        .await;

        let nodes = &swarm.behaviour_mut().key_share.keys;
        anyhow::ensure!(
            nodes.len() >= min_nodes(),
            "not enough nodes in network, needed {}, got {}",
            min_nodes(),
            nodes.len(),
        );

        let members = swarm
            .behaviour_mut()
            .chat_dht
            .table
            .closest(profile_hash.sign.as_ref())
            .take(REPLICATION_FACTOR.get() + 1);

        set_state!(ProfileOpen);
        let pick = members.choose(&mut rand::thread_rng()).unwrap().peer_id();
        let route = pick_route(&swarm.behaviour_mut().key_share.keys, pick);
        let pid = swarm.behaviour_mut().onion.open_path(route);
        let ((mut profile_stream, ..), profile_stream_id, profile_stream_peer) = loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(BehaviourEvent::Onion(onion::Event::OutboundStream(
                    stream,
                    id,
                ))) if id == pid => break (stream.context("opening profile route")?, id, pick),
                e => log::debug!("{:?}", e),
            }
        };

        set_state!(VaultLoad);
        let (mut vault_nonce, mail_action, vault) = match request_dispatch
            .dispatch_direct::<(Nonce, Nonce, BTreeMap<crypto::Hash, Vec<u8>>)>(
                &mut profile_stream,
                rpcs::FETCH_VAULT,
                Topic::Profile(profile_hash.sign),
                (),
            )
            .await
        {
            Ok((vn, m, v)) => (vn + 1, m + 1, v),
            Err(e) => {
                log::debug!("cannot access vault: {e} {:?}", profile_hash.sign);
                Default::default()
            }
        };

        log::debug!("{:?}", vault);

        let vault = if vault.is_empty() && vault_nonce == 0 {
            set_state!(ProfileCreate);
            let proof = Proof::new(&keys.sign, &mut vault_nonce, crypto::Hash::default(), OsRng);
            request_dispatch
                .dispatch_direct(
                    &mut profile_stream,
                    rpcs::CREATE_PROFILE,
                    Topic::Profile(profile_hash.sign),
                    &(proof, "", keys.enc.public_key()),
                )
                .await
                .context("creating account")?;

            Vault::default()
        } else {
            Vault::deserialize(vault, keys.vault)
        };
        let _ = vault.theme.apply();

        set_state!(ChatSearch);

        let mut profile_sub = Subscription {
            id: profile_stream_id,
            peer_id: profile_stream_peer,
            topics: [Topic::Profile(profile_hash.sign)].into(),
            subscriptions: Default::default(),
            stream: profile_stream,
        };

        let mut topology = HashMap::<PeerId, HashSet<ChatName>>::new();
        let iter = vault.chats.keys().copied().flat_map(|c| {
            swarm
                .behaviour_mut()
                .chat_dht
                .table
                .closest(c.as_bytes())
                .take(REPLICATION_FACTOR.get() + 1)
                .map(move |peer| (peer.peer_id(), c))
                .collect::<Vec<_>>()
        });
        for (peer, chat) in iter {
            if peer == profile_stream_peer {
                profile_sub.topics.push(chat.into());
                continue;
            }

            topology.entry(peer).or_default().insert(chat);
        }

        let mut topology = topology.into_iter().collect::<Vec<_>>();
        topology.sort_by_key(|(_, v)| v.len());
        let mut to_connect = vec![];
        let mut seen = HashSet::new();
        while seen.len() < vault.chats.len() {
            let (peer, mut chats) = topology.pop().unwrap();
            chats.retain(|&c| seen.insert(c));
            if chats.is_empty() {
                continue;
            }
            to_connect.push((peer, chats));
        }

        let mut awaiting = to_connect
            .into_iter()
            .map(|(pick, set)| {
                let route = pick_route(&swarm.behaviour_mut().key_share.keys, pick);
                let pid = swarm.behaviour_mut().onion.open_path(route);
                (pid, pick, set)
            })
            .collect::<Vec<_>>();

        let mut subscriptions = futures::stream::SelectAll::new();
        subscriptions.push(profile_sub);
        while !awaiting.is_empty() {
            let ((stream, got_peer_id), subs, peer_id, id) = loop {
                match swarm.select_next_some().await {
                    SwarmEvent::Behaviour(BehaviourEvent::Onion(onion::Event::OutboundStream(
                        stream,
                        id,
                    ))) => {
                        if let Some((.., peer_id, subs)) =
                            awaiting.find_and_remove(|&(i, ..)| i == id)
                        {
                            break (
                                stream.context("opening chat subscription route")?,
                                subs,
                                peer_id,
                                id,
                            );
                        }
                    }
                    e => log::debug!("{:?}", e),
                }
            };
            debug_assert!(peer_id == got_peer_id);

            subscriptions.push(Subscription {
                id,
                peer_id,
                topics: subs.into_iter().map(Topic::Chat).collect(),
                subscriptions: Default::default(),
                stream,
            });
        }

        set_state!(ChatRun);

        Ok((
            Self {
                swarm,
                subscriptions,
                pending_chat_requests: Default::default(),
                pending_topic_search: Default::default(),
                _pending_rpc_requests: Default::default(),
                pending_streams: Default::default(),
                negotiated_streams: Default::default(),
                requests: commands,
            },
            vault,
            request_dispatch,
            vault_nonce,
            mail_action.max(1),
        ))
    }

    fn handle_topic_search(&mut self, command: RequestInit) {
        let search_key = command.topic();
        if let Some((_, l)) = self
            .pending_topic_search
            .iter_mut()
            .find(|(_, v)| v.iter().any(|c| c.topic() == search_key))
        {
            log::debug!("search already in progress");
            l.push(command);
            return;
        }

        let peers = self
            .swarm
            .behaviour_mut()
            .chat_dht
            .table
            .closest(search_key.as_bytes())
            .take(REPLICATION_FACTOR.get() + 1)
            .map(Route::peer_id)
            .collect::<Vec<_>>();

        if let Some(sub) = self.subscriptions.iter_mut().find(|s| peers.contains(&s.peer_id)) {
            log::debug!("shortcut topic found");
            sub.topics.push(search_key);
            self.command(command);
            return;
        }

        let Some(pick) = peers.into_iter().choose(&mut rand::thread_rng()) else {
            log::error!("search response does not contain any peers");
            return;
        };

        let path = pick_route(&self.swarm.behaviour().key_share.keys, pick);
        let pid = self.swarm.behaviour_mut().onion.open_path(path);
        self.pending_topic_search.insert(pid, vec![command]);
    }

    fn onion_request(&mut self, req: RawOnionRequest) {
        let Some(sub) = self
            .subscriptions
            .iter_mut()
            .find(|s| req.topic.as_ref().map_or(true, |t| s.topics.contains(t)))
        else {
            self.handle_topic_search(RequestInit::OnionRequest(req));
            return;
        };

        let request = chat_spec::Request {
            prefix: req.prefix,
            id: req.id,
            topic: req.topic,
            body: Reminder(&req.payload),
        };

        sub.stream.write(request).unwrap();
        self.pending_chat_requests.insert(req.id, req.response);
        log::debug!("request sent, {:?}", req.id);
    }

    fn subscription_request(&mut self, sub: SubscriptionInit) {
        log::info!("Subscribing to {:?}", sub.topic);
        let Some(subs) = self.subscriptions.iter_mut().find(|s| s.topics.contains(&sub.topic))
        else {
            self.handle_topic_search(RequestInit::Subscription(sub));
            return;
        };

        log::info!("Creating Subsctiption request to {:?}", sub.topic);
        let request = chat_spec::Request {
            prefix: rpcs::SUBSCRIBE,
            id: sub.id,
            topic: Some(sub.topic),
            body: Reminder(&[]),
        };

        subs.stream.write(request).unwrap();
        subs.subscriptions.insert(sub.id, sub.channel);
        log::debug!("subscription request sent, {:?}", sub.id);
    }

    fn command(&mut self, command: RequestInit) {
        match command {
            RequestInit::OnionRequest(req) => self.onion_request(req),
            RequestInit::Subscription(sub) => self.subscription_request(sub),
            RequestInit::EndSubscription(topic) => self.subscription_end(topic),
            RequestInit::StorageRequest(req) => self.storage_request(req),
            RequestInit::SateliteRequest(req) => self.satelite_request(req),
        }
    }

    fn satelite_request(&mut self, _req: RawSateliteRequest) {
        // let beh = self.swarm.behaviour_mut();

        // let request = storage_spec::Request { prefix: req.prefix, body: Reminder(&req.payload) };
        // let res = beh.rpc.request(req.identity.to_peer_id(), request.to_bytes(), false);
        // let Ok(id) = res.inspect_err(|e| log::error!("cannot send storage request: {e}")) else {
        //     return;
        // };

        // self.pending_rpc_requests.insert(id, req.response);
    }

    fn storage_request(&mut self, _req: RawStorageRequest) {
        // let beh = self.swarm.behaviour_mut();
        // beh.storage_dht.table.insert(Route::new(req.identity, unpack_addr(req.addr)));

        // let request = storage_spec::Request { prefix: req.prefix, body: Reminder(&req.payload) };
        // let ignore_resp = req.response.is_ok();
        // let peer = req.identity.to_peer_id();
        // let res = beh.rpc.request(peer, request.to_bytes(), ignore_resp);
        // let Ok(id) = res.inspect_err(|e| log::error!("cannot send storage request: {e}")) else {
        //     return;
        // };

        // if req.response.is_ok() {
        //     beh.streaming.create_stream(peer);
        // }

        // match req.response {
        //     Ok(resp) => _ = self.pending_streams.push_front((peer, id, resp)),
        //     Err(resp) => _ = self.pending_rpc_requests.insert(id, resp),
        // }
    }

    fn subscription_end(&mut self, topic: Topic) {
        let Some(sub) = self.subscriptions.iter_mut().find(|s| s.topics.contains(&topic)) else {
            log::error!("cannot find subscription to end");
            return;
        };

        let request = chat_spec::Request {
            prefix: rpcs::UNSUBSCRIBE,
            id: CallId::new(),
            topic: Some(topic),
            body: Reminder(&[]),
        };

        sub.stream.write(request).unwrap();
        self.subscriptions
            .iter_mut()
            .find_map(|s| {
                let index = s.topics.iter().position(|t| t == &topic)?;
                s.topics.swap_remove(index);
                Some(())
            })
            .expect("channel to exist");
    }

    fn swarm_event(&mut self, event: SE) {
        match event {
            SwarmEvent::Behaviour(BehaviourEvent::Onion(onion::Event::OutboundStream(
                stream,
                id,
            ))) => {
                if let Some(req) = self.pending_topic_search.remove(&id) {
                    let Ok((stream, peer_id)) = stream else { return };

                    self.subscriptions.push(Subscription {
                        id,
                        peer_id,
                        topics: [req[0].topic().to_owned()].into(),
                        subscriptions: Default::default(),
                        stream,
                    });
                    req.into_iter().for_each(|r| self.command(r));
                }
            }
            // SwarmEvent::Behaviour(BehaviourEvent::Rpc(rpc::Event::Response(_, cid, body))) => {
            //     let Ok(body) = body.inspect_err(|e| log::error!("rpc response error: {e}")) else {
            //         return;
            //     };

            //     if let Some(sender) = self.pending_rpc_requests.remove(&cid) {
            //         if let Err(e) = sender.send(body) {
            //             log::error!("cannot send satelite response (no longer expected): {e:?}");
            //         }
            //     }
            // }
            SwarmEvent::Behaviour(BehaviourEvent::Streaming(streaming::Event::OutgoingStream(
                peer,
                stream,
            ))) => {
                let Ok(mut stream) = stream.inspect_err(|e| log::error!("stream error: {e}"))
                else {
                    return;
                };

                let Some(pos) = self.pending_streams.iter().rposition(|(p, ..)| *p == peer) else {
                    log::error!("stream response not expected form {:?}", peer);
                    return;
                };

                let (_, id, sender) = self.pending_streams.remove(pos).unwrap();
                self.negotiated_streams.push(async move {
                    let mut writer = PacketWriter::new(10);
                    writer.write_packet(id).unwrap();
                    writer.flush(&mut stream).await?;
                    let map = |_| io::Error::other("we no longer expect the tream");
                    sender.send(stream).map_err(map)?;
                    Ok(())
                });
            }
            e => log::debug!("{:?}", e),
        }
    }

    fn subscription_response(&mut self, (id, request): (PathId, io::Result<Vec<u8>>)) {
        let Ok(msg) = request.inspect_err(|e| log::error!("chat subscription error: {e}")) else {
            return;
        };

        let Some((cid, Reminder(content))) = <_>::decode(&mut &*msg) else {
            log::error!("invalid chat subscription response");
            return;
        };

        if let Some(channel) = self.pending_chat_requests.remove(&cid) {
            log::debug!("response recieved, {:?}", cid);
            _ = channel.send(content.to_owned());
            return;
        }

        if let Some(channel) = self
            .subscriptions
            .iter_mut()
            .find(|s| s.id == id)
            .and_then(|s| s.subscriptions.get_mut(&cid))
        {
            if channel.try_send(content.to_owned()).is_err() {
                self.subscriptions
                    .iter_mut()
                    .find_map(|s| s.subscriptions.remove(&cid))
                    .expect("channel to exist");
            }
            return;
        }

        log::error!("request does not exits even though we recieived it {:?}", cid);
    }

    pub fn pending_topic_search(&self) -> &HashMap<PathId, Vec<RequestInit>> {
        &self.pending_topic_search
    }
}

impl Future for Node {
    type Output = Result<Infallible, ()>;

    fn poll(
        mut self: pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Infallible, ()>> {
        use component_utils::field as f;

        let negot_fail = |_: &mut _, e| log::error!("stream negotiation failed: {e}");

        while component_utils::Selector::new(self.deref_mut(), cx)
            .stream(f!(mut requests), Self::command)
            .stream(f!(mut subscriptions), Self::subscription_response)
            .stream(f!(mut swarm), Self::swarm_event)
            .try_stream(f!(mut negotiated_streams), |_, _| {}, negot_fail)
            .done()
        {}

        Poll::Pending
    }
}

fn pick_route(
    nodes: &HashMap<PeerId, enc::PublicKey>,
    target: PeerId,
) -> [(onion::PublicKey, PeerId); 3] {
    assert!(nodes.len() >= 2);
    let mut rng = rand::thread_rng();
    let mut picked = nodes
        .iter()
        .filter(|(p, _)| **p != target)
        .map(|(p, ud)| (*ud, *p))
        .choose_multiple(&mut rng, 2);
    picked.insert(0, (*nodes.get(&target).unwrap(), target));
    picked.try_into().unwrap()
}

#[allow(deprecated)]
type SE = libp2p::swarm::SwarmEvent<<Behaviour as NetworkBehaviour>::ToSwarm>;

struct Subscription {
    id: PathId,
    peer_id: PeerId,
    topics: Vec<Topic>,
    subscriptions: HashMap<CallId, mpsc::Sender<SubscriptionMessage>>,
    stream: EncryptedStream,
}

impl futures::Stream for Subscription {
    type Item = (PathId, <EncryptedStream as futures::Stream>::Item);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx).map(|opt| opt.map(|v| (self.id, v)))
    }
}

#[derive(libp2p::swarm::NetworkBehaviour, Default)]
struct Behaviour {
    onion: onion::Behaviour,
    key_share: onion::key_share::Behaviour,
    chat_dht: dht::Behaviour,
    satelite_dht: dht::Behaviour,
    storage_dht: dht::Behaviour,
    streaming: streaming::Behaviour,
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[repr(u8)]
pub enum BootPhase {
    #[error("fetching nodes and profile from chain...")]
    FetchNodesAndProfile,
    #[error("initiating orion connection...")]
    InitiateConnection,
    #[error("collecting server keys... ({0} left)")]
    CollecringKeys(usize),
    #[error("opening route to profile...")]
    ProfileOpen,
    #[error("loading vault...")]
    VaultLoad,
    #[error("creating new profile...")]
    ProfileCreate,
    #[error("searching chats...")]
    ChatSearch,
    #[error("ready")]
    ChatRun,
}

impl BootPhase {
    pub fn discriminant(&self) -> u8 {
        // SAFETY: Because `Self` is marked `repr(u8)`, its layout is a `repr(C)` `union`
        // between `repr(C)` structs, each of which has the `u8` discriminant as its first
        // field, so we can read the discriminant without offsetting the pointer.
        unsafe { *<*const _>::from(self).cast::<u8>() }
    }
}

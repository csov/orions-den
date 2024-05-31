pub use {
    chain_types::runtime_types::{
        pallet_chat_subs::pallet::Subscription,
        pallet_node_staker::pallet::{
            Event as ChatStakeEvent, Event2 as SateliteStakeEvent, NodeAddress, Stake as ChatStake,
            Stake2 as SateliteStake,
        },
        pallet_user_manager::pallet::Profile,
    },
    subxt_signer::{
        bip39::Mnemonic,
        sr25519::{Keypair, Signature},
    },
};
use {
    chain_types::{polkadot, Hash},
    codec::Codec,
    crypto::{
        enc,
        rand_core::{OsRng, SeedableRng},
        sign,
    },
    futures::{SinkExt, StreamExt, TryStreamExt},
    libp2p::{multiaddr, Multiaddr, PeerId},
    rand_chacha::ChaChaRng,
    std::{
        net::{IpAddr, SocketAddr},
        str::FromStr,
        sync::Arc,
    },
    subxt::{
        blocks::Block,
        config::ParamsFor,
        storage::{address::Yes, StorageAddress},
        tx::{TxPayload, TxProgress},
        Error, OnlineClient, PolkadotConfig as Config,
    },
    subxt_signer::SecretUri,
};

pub type Balance = u128;
pub type AccountId = <Config as subxt::Config>::AccountId;
pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type Nonce = u64;
pub type RawUserName = [u8; USER_NAME_CAP];
pub type StakeEvents<E> = futures::channel::mpsc::Receiver<Result<E>>;
pub type NodeIdentity = crypto::Hash;
pub type NodeVec = Vec<(NodeIdentity, SocketAddr)>;

pub const USER_NAME_CAP: usize = 32;

pub async fn wait_for_in_block(
    mut progress: TxProgress<Config, OnlineClient<Config>>,
    finalized: bool,
) -> Result<Hash> {
    use subxt::{error::TransactionError as TE, tx::TxStatus as TS};

    while let Some(event) = progress.next().await {
        return match event? {
            TS::InBestBlock(b) if !finalized => Ok(b.wait_for_success().await?.extrinsic_hash()),
            TS::InFinalizedBlock(b) => Ok(b.wait_for_success().await?.extrinsic_hash()),
            TS::Error { message } => Err(Error::Transaction(TE::Error(message))),
            TS::Invalid { message } => Err(Error::Transaction(TE::Invalid(message))),
            TS::Dropped { message } => Err(Error::Transaction(TE::Dropped(message))),
            _ => continue,
        };
    }

    log::error!("tx stream ended without result");
    Err(subxt::Error::Unknown(vec![]))
}

pub fn format_balance(amount: Balance) -> String {
    format!("{:.2} BORN", amount as f64 / 1_000_000_000_000.0)
}

#[derive(Clone)]
pub struct Client {
    signer: Keypair,
    client: OnlineClient<Config>,
}

impl Client {
    pub async fn list_chat_nodes(&self) -> Result<NodeVec> {
        self.list_nodes(chain_types::storage().chat_staker().addresses_iter()).await
    }

    pub async fn list_satelite_nodes(&self) -> Result<NodeVec> {
        self.list_nodes(chain_types::storage().satelite_staker().addresses_iter()).await
    }

    async fn list_nodes<SA>(&self, tx: SA) -> Result<NodeVec>
    where
        SA: StorageAddress<IsIterable = Yes, Target = NodeAddress> + 'static,
    {
        let latest = self.client.storage().at_latest().await?;
        let map_ok = |kv: subxt::storage::StorageKeyValuePair<SA>| {
            let mut slc = &kv.key_bytes[48..];
            let key = parity_scale_codec::Decode::decode(&mut slc).unwrap();
            debug_assert!(slc.is_empty());
            (key, kv.value.into())
        };
        latest.iter(tx).await?.map_ok(map_ok).try_collect().await
    }

    pub async fn without_signer(url: &str) -> Result<Self> {
        let client = OnlineClient::<Config>::from_url(url).await?;
        let signer = Keypair::from_seed(crypto::SharedSecret::default()).unwrap();
        Ok(Self { signer, client })
    }

    pub async fn subscribe(&self, identity: NodeIdentity, amount: Balance) -> Result<()> {
        let call = polkadot::tx().chat_subs().subscribe(identity, amount);
        self.handle(call, self.get_nonce().await?).await
    }

    pub fn account_id(&self) -> AccountId {
        self.signer.public_key().to_account_id()
    }

    pub async fn get_balance(&self) -> Result<Option<Balance>> {
        let q = chain_types::storage().system().account(self.account_id());
        Ok(self.client.storage().at_latest().await?.fetch(&q).await?.map(|b| b.data.free))
    }

    pub async fn get_nonce(&self) -> Result<u64> {
        let account = self.account_id();
        self.client.blocks().at_latest().await?.account_nonce(&account).await
    }

    pub async fn transfere(&self, dest: AccountId, amount: Balance, nonce: Nonce) -> Result<()> {
        let transaction = polkadot::tx().balances().transfer_keep_alive(dest.into(), amount);
        self.handle(transaction, nonce).await
    }

    pub async fn join(
        &self,
        data: NodeIdentity,
        enc: crypto::Hash,
        addr: NodeAddress,
        nonce: Nonce,
    ) -> Result<()> {
        let tx = chain_types::tx().chat_staker().join(data, enc, addr, 0);
        self.handle(tx, nonce).await
    }

    pub async fn register(&self, name: RawUserName, data: Profile, nonce: Nonce) -> Result<()> {
        let tx = chain_types::tx().user_manager().register_with_name(data, name);
        self.handle(tx, nonce).await
    }

    pub async fn get_profile_by_name(&self, name: RawUserName) -> Result<Option<Profile>> {
        let latest = self.client.storage().at_latest().await?;
        let tx = chain_types::storage().user_manager().username_to_owner(name);
        let Some(account_id) = latest.fetch(&tx).await? else { return Ok(None) };
        latest.fetch(&chain_types::storage().user_manager().identities(account_id)).await
    }

    pub async fn joined(&self, node_dientity: NodeIdentity) -> Result<bool> {
        let latest = self.client.storage().at_latest().await?;
        let q = chain_types::storage().chat_staker().addresses(node_dientity);
        latest.fetch(&q).await.map(|o| o.is_some())
    }

    pub async fn user_exists(&self, name: RawUserName) -> Result<bool> {
        let latest = self.client.storage().at_latest().await?;
        let tx = chain_types::storage().user_manager().username_to_owner(name);
        latest.fetch(&tx).await.map(|o| o.is_some())
    }

    pub async fn get_username(&self, id: crypto::Hash) -> Result<Option<RawUserName>> {
        let latest = self.client.storage().at_latest().await?;
        latest.fetch(&chain_types::storage().user_manager().identity_to_username(id)).await
    }

    /* ## Server */

    pub async fn with_signer(url: &str, signer: Keypair) -> Result<Self> {
        let client = OnlineClient::<Config>::from_url(url).await?;
        Ok(Self { signer, client })
    }

    pub async fn get_subscription(&self, identity: NodeIdentity) -> Result<Option<Subscription>> {
        let q = chain_types::storage().chat_subs().subscriptions(identity);
        self.client.storage().at_latest().await?.fetch(&q).await
    }

    async fn node_event_stream<E: 'static>(
        &self,
        pallet_name: &'static str,
        unwrap: fn(chain_types::Event) -> Option<E>,
    ) -> Result<impl futures::Stream<Item = Result<E>>, Error> {
        let then = move |block: Result<Block<_, _>>| async move {
            let iter = block?
                .events()
                .await?
                .iter()
                .filter(move |e| e.as_ref().map_or(true, |e| e.pallet_name() == pallet_name))
                .map(|e| e.and_then(|e| e.as_root_event::<chain_types::Event>()))
                .filter_map(move |e| match e {
                    Ok(e) => unwrap(e).map(Ok),
                    Err(e) => Some(Err(e)),
                });
            Ok::<_, Error>(futures::stream::iter(iter))
        };
        let sub = self.client.blocks().subscribe_finalized().await?;
        Ok(sub.then(then).try_flatten())
    }

    pub async fn vote(&self, me: NodeIdentity, target: NodeIdentity, nonce: Nonce) -> Result<()> {
        let tx = chain_types::tx().chat_staker().vote(me, target);
        self.handle(tx, nonce).await
    }

    pub async fn reclaim(&self, me: NodeIdentity, nonce: Nonce) -> Result<()> {
        self.handle(chain_types::tx().chat_staker().reclaim(me), nonce).await
    }

    pub async fn vote_if_possible(&self, source: NodeIdentity, target: NodeIdentity) -> Result<()> {
        let latest = self.client.storage().at_latest().await?;

        let vote_q = chain_types::storage().chat_staker().votes(target);
        if latest.fetch(&vote_q).await?.is_some_and(|v| v.contains(&source)) {
            return Ok(());
        }

        let block_num = self.client.blocks().at_latest().await?.number();
        let stake_q = chain_types::storage().chat_staker().stakes(target);
        let Some(stake) = latest.fetch(&stake_q).await? else {
            return Ok(());
        };
        if stake.protected_until > block_num {
            return Ok(());
        }

        let nonce = self.get_nonce().await?;
        self.vote(source, target, nonce).await
    }

    async fn handle(&self, call: impl TxPayload, nonce: Nonce) -> Result<()> {
        let mut data = ParamsFor::<Config>::default();
        data.2 .0 = Some(nonce);
        let signed = self.client.tx().create_signed(&call, &self.signer, data).await?;
        let progress = signed.submit_and_watch().await?;
        wait_for_in_block(progress, true).await.map(drop)
    }
}

#[derive(Clone, Codec)]
pub struct NodeKeys {
    pub enc: enc::Keypair,
    pub sign: sign::Keypair,
}

impl NodeKeys {
    pub fn identity(&self) -> NodeIdentity {
        self.sign.identity()
    }

    pub fn enc_hash(&self) -> crypto::Hash {
        crypto::hash::new(self.enc.public_key())
    }

    pub fn from_mnemonic(mnemonic: &Mnemonic) -> Self {
        let seed = crypto::hash::new(mnemonic.to_seed(""));
        let mut rng = ChaChaRng::from_seed(seed);
        Self { enc: enc::Keypair::new(&mut rng), sign: sign::Keypair::new(rng) }
    }

    pub fn random() -> Self {
        let mut rng = OsRng;
        Self { enc: enc::Keypair::new(&mut rng), sign: sign::Keypair::new(rng) }
    }
}

#[derive(clap::Parser, Clone)]
pub struct EnvConfig {
    /// account which pays the stake for the node
    pub mnemonic: Mnemonic,
    /// chain nodes to connect to, its a comma separated list for redundancy, this a rpc url
    /// like `wss://polkadot.api.onfinality.io/public-ws`
    chain_nodes: Vec<String>,
}

impl EnvConfig {
    pub async fn client(&self) -> anyhow::Result<Client> {
        let account = Keypair::from_phrase(&self.mnemonic, None)?;

        for node in self.chain_nodes.iter() {
            let Ok(client) = Client::with_signer(node, account.clone())
                .await
                .inspect_err(|e| log::warn!("connecting chain client: {e:#}"))
            else {
                continue;
            };
            return Ok(client);
        }
        anyhow::bail!("no nodes were valid for connection");
    }

    pub async fn connect_satelite(
        self,
    ) -> anyhow::Result<(NodeVec, StakeEvents<SateliteStakeEvent>)> {
        let tx = chain_types::storage().satelite_staker().addresses_iter();
        self.connect(tx, "SateliteStaker", unwrap_satelite_staker).await
    }

    pub async fn connect_chat(self) -> anyhow::Result<(NodeVec, StakeEvents<ChatStakeEvent>)> {
        let tx = chain_types::storage().chat_staker().addresses_iter();
        self.connect(tx, "ChatStaker", unwrap_chat_staker).await
    }

    async fn connect<SA, E>(
        self,
        tx: SA,
        pallet_name: &'static str,
        unwrap: fn(chain_types::Event) -> Option<E>,
    ) -> anyhow::Result<(NodeVec, StakeEvents<E>)>
    where
        SA: StorageAddress<IsIterable = Yes, Target = NodeAddress> + 'static + Clone,
        E: 'static + Send,
    {
        let EnvConfig { chain_nodes, mnemonic } = self;
        let (mut chain_events_tx, stake_events) = futures::channel::mpsc::channel(0);
        let account = Keypair::from_phrase(&mnemonic, None)?;

        let mut others = chain_nodes.into_iter();

        let (node_list, client) = 'a: {
            for node in others.by_ref() {
                let Ok(client) = Client::with_signer(&node, account.clone())
                    .await
                    .inspect_err(|e| log::warn!("connecting chain client: {e:#}"))
                else {
                    continue;
                };
                let Ok(node_list) = client
                    .list_nodes(tx.clone())
                    .await
                    .inspect_err(|e| log::warn!("getting chat list: {e:#}"))
                else {
                    continue;
                };
                break 'a (node_list, client);
            }
            anyhow::bail!("failed to fetch node list");
        };

        let mut stream = client.node_event_stream(pallet_name, unwrap).await;
        let fut = async move {
            loop {
                if let Ok(mut stream) = stream {
                    let mut stream = std::pin::pin!(stream);
                    while let Some(event) = stream.next().await {
                        _ = chain_events_tx.send(event).await;
                    }
                }

                let Some(next) = others.next() else {
                    log::error!("failed to reconnect to chain");
                    std::process::exit(1);
                };

                let Ok(client) = Client::with_signer(&next, account.clone()).await else {
                    stream = Err(Error::Other("failed to reconnect to chain".into()));
                    continue;
                };

                stream = client.node_event_stream(pallet_name, unwrap).await;
            }
        };

        #[cfg(feature = "native")]
        tokio::spawn(fut);
        #[cfg(feature = "web")]
        wasm_bindgen_futures::spawn_local(fut);

        log::info!("entered the network with {} nodes", node_list.len());

        Ok((node_list, stake_events))
    }
}

pub fn stake_event(s: &mut impl AsMut<dht::Behaviour>, event: impl Into<ChatStakeEvent>) {
    let dht = s.as_mut();
    match event.into() {
        ChatStakeEvent::Joined { identity, addr }
        | ChatStakeEvent::AddrChanged { identity, addr } => {
            dht.table.write().insert(dht::Route::new(identity, unpack_addr(addr)));
        }
        ChatStakeEvent::Reclaimed { identity } => _ = dht.table.write().remove(identity),
        ChatStakeEvent::Voted { .. } => {}
    }
}

pub fn unpack_addr_offset(addr: SocketAddr, port_offset: u16) -> Multiaddr {
    unpack_addr((addr.ip(), addr.port() + port_offset))
}

pub fn unpack_addr(addr: impl Into<SocketAddr>) -> Multiaddr {
    let addr = addr.into();
    Multiaddr::empty()
        .with(match addr.ip() {
            IpAddr::V4(ip) => multiaddr::Protocol::Ip4(ip),
            IpAddr::V6(ip) => multiaddr::Protocol::Ip6(ip),
        })
        .with(multiaddr::Protocol::Tcp(addr.port()))
}

pub fn filter_incoming(
    _table: &mut dht::RoutingTable,
    _peer: PeerId,
    _local_addr: &Multiaddr,
    _: &Multiaddr,
) -> Result<(), libp2p::swarm::ConnectionDenied> {
    // if local_addr.iter().any(|p| matches!(p, multiaddr::Protocol::Ws(_))) {
    //     return Ok(());
    // }

    // if table.get(peer).is_some() {
    //     return Ok(());
    // }

    // Err(libp2p::swarm::ConnectionDenied::new("not registered as a node"))

    // FIXME: we need filtering but this also need access to config to deterimne what the port
    // means, for now we ignore this
    Ok(())
}

#[derive(Clone)]
pub struct ClapSecretUri(pub Arc<SecretUri>);

impl FromStr for ClapSecretUri {
    type Err = <SecretUri as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Arc::new(s.parse()?)))
    }
}

#[derive(Clone)]
pub struct ClapNodeIdentity(pub NodeIdentity);

impl FromStr for ClapNodeIdentity {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut arr = NodeIdentity::default();
        hex::decode_to_slice(s, &mut arr)?;
        Ok(Self(arr))
    }
}

impl std::fmt::Display for ClapNodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

fn unwrap_chat_staker(e: chain_types::Event) -> Option<ChatStakeEvent> {
    match e {
        chain_types::Event::ChatStaker(e) => Some(e),
        _ => None,
    }
}

fn unwrap_satelite_staker(e: chain_types::Event) -> Option<SateliteStakeEvent> {
    match e {
        chain_types::Event::SateliteStaker(e) => Some(e),
        _ => None,
    }
}

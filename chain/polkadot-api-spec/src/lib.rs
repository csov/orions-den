use {
    anyhow::Context,
    chain_api_spec::{NodeEvent, NodeId, NodeVec, Result},
    chain_types::{
        polkadot::runtime_types::node_template_runtime::RuntimeEvent,
        runtime_types::pallet_node_staker::pallet::Event as ChatStakeEvent, Hash,
    },
    futures::{StreamExt, TryStreamExt},
    std::str::FromStr,
    subxt::{
        blocks::Block,
        tx::{TxPayload, TxProgress},
        Error, OnlineClient, PolkadotConfig as Config,
    },
    subxt_signer::{bip39::Mnemonic, sr25519::Keypair},
};

chain_api_spec::export_api_spec!(Client);

async fn wait_for_in_block(
    mut progress: TxProgress<Config, OnlineClient<Config>>,
) -> Result<Hash, subxt::Error> {
    use subxt::{error::TransactionError as TE, tx::TxStatus as TS};

    while let Some(event) = progress.next().await {
        return match event? {
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

#[derive(Clone)]
pub struct Client {
    signer: Keypair,
    client: OnlineClient<Config>,
}

impl Client {
    async fn new() -> Result<Self> {
        let url = std::env::var("POLKADOT_RPC_URL").unwrap_or("ws://localhost:9944".into());
        let mnemonic =
            std::env::var("POLKADOT_MNEMONIC").context("wallet mnemonic for automated voting")?;
        let mnemonic = Mnemonic::from_str(&mnemonic).context("invalid mnemonic")?;
        let client = OnlineClient::<Config>::from_url(url).await?;
        Ok(Self { signer: Keypair::from_phrase(&mnemonic, None)?, client })
    }

    async fn get_sub(&self, identity: NodeId) -> Result<Option<chain_api_spec::Subscription>> {
        let q = chain_types::storage().chat_subs().subscriptions(identity);
        self.client.storage().at_latest().await?.fetch(&q).await.map_err(Into::into).map(|s| {
            s.map(|s| chain_api_spec::Subscription {
                topic: s.owner.0,
                amount: s.funds,
                timetsmp: s.created_at,
            })
        })
    }

    async fn list_nodes(&self) -> Result<NodeVec> {
        let latest = self.client.storage().at_latest().await?;
        let tx = chain_types::storage().satelite_staker().addresses_iter();
        latest
            .iter(tx)
            .await?
            .map_ok(|kv: subxt::storage::StorageKeyValuePair<_>| {
                let mut slc = &kv.key_bytes[48..];
                let key = parity_scale_codec::Decode::decode(&mut slc).unwrap();
                debug_assert!(slc.is_empty());
                (key, kv.value.into())
            })
            .try_collect()
            .await
            .map_err(Into::into)
    }

    async fn open_node_event_stream(
        &self,
    ) -> Result<impl futures::Stream<Item = Result<NodeEvent>>> {
        let then = move |block: Result<Block<_, _>, subxt::Error>| async move {
            let iter = block?
                .events()
                .await?
                .iter()
                .filter(move |e| e.as_ref().map_or(true, |e| e.pallet_name() == "chatStaker"))
                .map(|e| e.and_then(|e| e.as_root_event::<chain_types::Event>()))
                .filter_map(move |e| match e {
                    Ok(e) => match e {
                        RuntimeEvent::ChatStaker(e) => Some(Ok(match e {
                            ChatStakeEvent::Joined { identity, addr } => {
                                NodeEvent::Join { node: identity, addr: addr.into() }
                            }
                            ChatStakeEvent::AddrChanged { identity, addr } => {
                                NodeEvent::AddrChanged { node: identity, addr: addr.into() }
                            }
                            ChatStakeEvent::Reclaimed { identity } => {
                                NodeEvent::Left { node: identity }
                            }
                            ChatStakeEvent::Voted { .. } => return None,
                        })),
                        _ => None,
                    },
                    Err(e) => Some(Err(e.into())),
                });
            Ok::<_, Error>(futures::stream::iter(iter))
        };
        let sub = self.client.blocks().subscribe_finalized().await?;
        Ok(sub.then(then).try_flatten())
    }

    async fn vote(&self, me: NodeId, target: NodeId) -> Result<()> {
        let tx = chain_types::tx().chat_staker().vote(me, target);
        self.handle(tx).await
    }

    async fn vote_if_possible(&self, source: NodeId, target: NodeId) -> Result<()> {
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

        self.vote(source, target).await
    }

    async fn handle(&self, call: impl TxPayload) -> Result<()> {
        let signed =
            self.client.tx().create_signed(&call, &self.signer, Default::default()).await?;
        let progress = signed.submit_and_watch().await?;
        wait_for_in_block(progress).await.map(drop).map_err(Into::into)
    }
}

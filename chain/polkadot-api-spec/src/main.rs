use {
    chain_api_spec::{NodeEvent, NodeVec, Response},
    chain_types::{
        polkadot::runtime_types::node_template_runtime::RuntimeEvent,
        runtime_types::pallet_node_staker::pallet::Event as ChatStakeEvent, Hash,
    },
    clap::Parser,
    codec::{Decode as _, Encode},
    futures::{StreamExt, TryStreamExt},
    std::io::Write,
    subxt::{tx::TxProgress, Error, OnlineClient, PolkadotConfig as Config},
    subxt_signer::{bip39::Mnemonic, sr25519::Keypair},
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();

    let client = OnlineClient::<Config>::from_url(args.polkadot_rpc_url).await.unwrap();
    let signer = Keypair::from_phrase(&args.polkadot_mnemonic, None).unwrap();

    let (requests_sd, mut requests_rc) = tokio::sync::mpsc::channel::<chain_api_spec::Request>(100);

    std::thread::spawn(move || {
        futures::executor::block_on(async move {
            let mut buf = vec![];
            loop {
                use std::io::Read;

                {
                    let mut stdin = std::io::stdin().lock();

                    let mut len_buf = [0u8; 4];
                    stdin.read_exact(&mut len_buf).unwrap();
                    let len = u32::from_le_bytes(len_buf) as usize;

                    buf.resize(len, 0);
                    stdin.read_exact(&mut buf).unwrap();
                }

                let request = chain_api_spec::Request::decode_exact(&buf).unwrap();
                requests_sd.send(request).await.unwrap();
            }
        })
    });

    let (responses_sd, mut responses_rc) =
        tokio::sync::mpsc::channel::<chain_api_spec::Response>(100);
    std::thread::spawn(move || {
        futures::executor::block_on(async move {
            let mut buffer = vec![];
            while let Some(resp) = responses_rc.recv().await {
                let mut stdout = std::io::stdout().lock();
                resp.encode(&mut buffer).unwrap();
                stdout.write_all(&(buffer.len() as u32).to_le_bytes()).unwrap();
                stdout.write_all(&buffer).unwrap();
                stdout.flush().unwrap();
                buffer.clear();
            }
        })
    });

    let rsd = responses_sd.clone();
    let clt = client.clone();
    tokio::spawn(async move {
        clt.blocks()
            .subscribe_finalized()
            .await
            .unwrap()
            .then(move |block| async move {
                let iter = block
                    .unwrap()
                    .events()
                    .await
                    .unwrap()
                    .iter()
                    .filter(move |e| e.as_ref().map_or(true, |e| e.pallet_name() == "chatStaker"))
                    .map(|e| e.and_then(|e| e.as_root_event::<chain_types::Event>()))
                    .filter_map(move |e| match e.unwrap() {
                        RuntimeEvent::ChatStaker(e) => Some(match e {
                            ChatStakeEvent::Joined { identity, addr } => {
                                NodeEvent::Join { node: identity, addr: addr.into() }
                            }
                            ChatStakeEvent::AddrChanged { identity, addr } => {
                                NodeEvent::AddrChanged { node: identity, addr: addr.into() }
                            }
                            ChatStakeEvent::Reclaimed { identity } => {
                                NodeEvent::Left { node: identity }
                            }
                            ChatStakeEvent::Voted { source, target } => {
                                NodeEvent::Voted { source, target }
                            }
                        }),
                        _ => None,
                    });
                futures::stream::iter(iter)
            })
            .flatten()
            .map(Response::Event)
            .for_each(|e| async { rsd.send(e).await.unwrap() })
            .await;
    });

    while let Some(request) = requests_rc.recv().await {
        let sender = responses_sd.clone();
        let client = client.clone();
        let signer = signer.clone();
        tokio::spawn(async move {
            let body = match request.body {
                chain_api_spec::RequestBody::GetSub { topic } => {
                    let q = chain_types::storage().chat_subs().subscriptions(topic);
                    client
                        .storage()
                        .at_latest()
                        .await
                        .unwrap()
                        .fetch(&q)
                        .await
                        .unwrap()
                        .map(|s| chain_api_spec::Subscription {
                            topic: s.owner.0,
                            amount: s.funds,
                            timetsmp: s.created_at,
                        })
                        .to_bytes()
                }
                chain_api_spec::RequestBody::VoteIfPossible { source, target } => 'a: {
                    let latest = client.storage().at_latest().await.unwrap();

                    let vote_q = chain_types::storage().chat_staker().votes(target);
                    if latest.fetch(&vote_q).await.unwrap().is_some_and(|v| v.contains(&source)) {
                        break 'a vec![];
                    }

                    let block_num = client.blocks().at_latest().await.unwrap().number();
                    let stake_q = chain_types::storage().chat_staker().stakes(target);
                    let Some(stake) = latest.fetch(&stake_q).await.unwrap() else {
                        break 'a vec![];
                    };
                    if stake.protected_until > block_num {
                        break 'a vec![];
                    }

                    let call = chain_types::tx().chat_staker().vote(source, target);
                    let signed = client
                        .tx()
                        .create_signed(&call, &signer, Default::default())
                        .await
                        .unwrap();
                    let progress = signed.submit_and_watch().await.unwrap();
                    wait_for_in_block(progress).await.map(drop).unwrap().to_bytes()
                }
                chain_api_spec::RequestBody::ListNodes => {
                    let latest = client.storage().at_latest().await.unwrap();
                    let tx = chain_types::storage().chat_staker().addresses_iter();
                    latest
                        .iter(tx)
                        .await
                        .unwrap()
                        .map_ok(|kv: subxt::storage::StorageKeyValuePair<_>| {
                            let mut slc = &kv.key_bytes[48..];
                            let key = parity_scale_codec::Decode::decode(&mut slc).unwrap();
                            debug_assert!(slc.is_empty());
                            (key, kv.value.into())
                        })
                        .try_collect::<NodeVec>()
                        .await
                        .unwrap()
                        .to_bytes()
                }
            };
            sender.send(chain_api_spec::Response::Body { id: request.id, body }).await.unwrap();
        });
    }

    panic!("whaaaaaat");
}

#[derive(Parser)]
struct Args {
    #[clap(env)]
    polkadot_mnemonic: Mnemonic,
    #[clap(env, default_value = "ws://localhost:9944")]
    polkadot_rpc_url: String,
}

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

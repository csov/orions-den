#![feature(slice_take)]
#![feature(btree_extract_if)]
#![feature(macro_metavar_expr)]
#![feature(let_chains)]
#![feature(never_type)]
#![feature(type_alias_impl_trait)]

#[macro_export]
macro_rules! wasm_if {
    ($wasm:stmt $(, $not_wasm:stmt)?) => {
        #[cfg(target_arch = "wasm32")]
        $wasm;
        $(
            #[cfg(not(target_arch = "wasm32"))]
            $not_wasm;
        )?
    };
}

use {
    anyhow::Context,
    argon2::Argon2,
    chain_api::{Nonce, Profile},
    chat_spec::*,
    codec::{Codec, DecodeOwned, Encode},
    crypto::{enc, rand_core::CryptoRngCore, sign},
    libp2p::futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    onion::SharedSecret,
    rand::{rngs::OsRng, SeedableRng},
    std::{array, future::Future, io, marker::PhantomData, sync::Arc, time::Duration},
    storage_spec::ClientError,
};
pub use {
    chain::*,
    node::*,
    requests::*,
    vault::{Vault, *},
};
pub type SubscriptionMessage = Vec<u8>;
pub type RawResponse = Vec<u8>;
pub type MessageContent = String;

mod api;

mod chain;
mod node;
mod requests;
mod vault;

pub fn encode_direct_chat_name(name: UserName) -> String {
    format!("{}{}", name, " ".repeat(32))
}

pub trait RecoverMail {
    fn recover_mail(self) -> anyhow::Result<()>;
}

impl RecoverMail for anyhow::Result<()> {
    fn recover_mail(self) -> anyhow::Result<()> {
        self.or_else(|e| {
            e.root_cause()
                .downcast_ref::<ChatError>()
                .and_then(|e| match e {
                    ChatError::SentDirectly => Some(Ok(())),
                    _ => None,
                })
                .ok_or(e)?
        })
    }
}

pub trait DowncastNonce<O> {
    fn downcast_nonce(self) -> anyhow::Result<Result<O, Nonce>>;
}

impl<O> DowncastNonce<O> for anyhow::Result<O> {
    fn downcast_nonce(self) -> anyhow::Result<Result<O, Nonce>> {
        self.map(Ok).or_else(|e| {
            e.root_cause()
                .downcast_ref::<ClientError>()
                .and_then(|e| match e {
                    ClientError::InvalidNonce(n) => Some(Err(*n)),
                    _ => None,
                })
                .ok_or(e)
        })
    }
}

//pub struct RawStorageRequest {
//    pub prefix: u8,
//    pub payload: Vec<u8>,
//    pub identity: NodeId,
//    pub addr: SocketAddr,
//    pub response: Result<oneshot::Sender<libp2p::Stream>, oneshot::Sender<RawResponse>>,
//}
//
//pub struct RawSateliteRequest {
//    pub prefix: u8,
//    pub payload: Vec<u8>,
//    pub identity: NodeId,
//    pub response: oneshot::Sender<RawResponse>,
//}

#[derive(Clone)]
pub struct UserKeys {
    pub name: UserName,
    pub sign: sign::Keypair,
    pub enc: enc::Keypair,
    pub vault: crypto::SharedSecret,
    pub hot_wallet: crypto::SharedSecret,
    pub chain_url: Arc<str>,
}

impl UserKeys {
    pub fn new(name: UserName, password: &str, chain_url: &str) -> Self {
        let mut bytes = [0; 32];

        Argon2::default()
            .hash_password_into(password.as_bytes(), &username_to_raw(name), &mut bytes)
            .unwrap();
        let mut entropy = rand_chacha::ChaChaRng::from_seed(bytes);

        Self::new_from_entropy(name, &mut entropy, chain_url)
    }

    pub fn from_mnemonic(name: UserName, mnemonic: chain_api::Mnemonic, chain_url: &str) -> Self {
        let seed = mnemonic.to_seed("");
        let seed = array::from_fn(|i| seed[i] ^ seed[i + 32]);
        let mut entropy = rand_chacha::ChaChaRng::from_seed(seed);
        Self::new_from_entropy(name, &mut entropy, chain_url)
    }

    pub fn new_from_entropy(
        name: UserName,
        entropy: &mut impl CryptoRngCore,
        chain_url: &str,
    ) -> Self {
        let sign = sign::Keypair::new(&mut *entropy);
        let enc = enc::Keypair::new(&mut *entropy);
        let vault = crypto::new_secret(&mut *entropy);
        let hot_wallet = crypto::new_secret(&mut *entropy);
        Self { name, sign, enc, vault, hot_wallet, chain_url: chain_url.into() }
    }

    pub fn identity(&self) -> Identity {
        self.sign.identity()
    }

    pub fn to_identity(&self) -> Profile {
        Profile { sign: self.sign.identity(), enc: crypto::hash::new(self.enc.public_key()) }
    }

    pub fn chain_client(&self) -> impl Future<Output = chain_api::Result<chain::Client>> {
        let kp = chain_api::Keypair::from_seed(self.vault).expect("right amount of seed");
        let chain_url = self.chain_url.clone();
        async move { chain_api::Client::with_signer(&chain_url, kp).await }
    }

    pub async fn register(&self) -> anyhow::Result<()> {
        let client = self.chain_client().await.context("connecting to chain")?;

        let username = username_to_raw(self.name);

        if client.user_exists(username).await.context("checking if username is free")? {
            anyhow::bail!("user with this name already exists");
        }

        let nonce = client.get_nonce().await.context("fetching nonce")?;
        client.register(username, self.to_identity(), nonce).await.context("registering")?;

        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
pub async fn timeout<F: Future>(duration: Duration, f: F) -> Option<F::Output> {
    use {
        std::task::Poll,
        web_sys::{
            wasm_bindgen::{closure::Closure, JsCast},
            window,
        },
    };

    let mut fut = std::pin::pin!(f);
    let mut callback = None::<(Closure<dyn FnMut()>, i32)>;
    let until = instant::Instant::now() + duration;
    std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = fut.as_mut().poll(cx) {
            if let Some((_cl, handle)) = callback.take() {
                window().unwrap().clear_timeout_with_handle(handle);
            }

            return Poll::Ready(Some(v));
        }

        if until < instant::Instant::now() {
            return Poll::Ready(None);
        }

        if callback.is_none() {
            let waker = cx.waker().clone();
            let handler = Closure::once(move || waker.wake());
            let handle = window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    handler.as_ref().unchecked_ref(),
                    duration.as_millis() as i32,
                )
                .unwrap();
            callback = Some((handler, handle));
        }

        Poll::Pending
    })
    .await
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn timeout<F: Future>(duration: Duration, f: F) -> Option<F::Output> {
    tokio::time::timeout(duration, f).await.ok()
}

pub async fn send_request<R: DecodeOwned>(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    id: Prefix,
    topic: impl Into<Topic>,
    req: impl Encode,
) -> anyhow::Result<R> {
    timeout(
        Duration::from_secs(5),
        send_request_low::<Result<R, ChatError>>(stream, id, topic, req),
    )
    .await
    .context("timeout")??
    .map_err(Into::into)
}

pub async fn send_request_low<R: DecodeOwned>(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    id: Prefix,
    topic: impl Into<Topic>,
    req: impl Encode,
) -> io::Result<R> {
    let topic = topic.into();
    let len = req.encoded_len();
    let header = RequestHeader {
        prefix: id,
        call_id: [0; 4],
        topic: topic.compress(),
        len: (len as u32).to_be_bytes(),
    };
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&req.to_bytes()).await?;
    stream.flush().await?;

    let mut header = [0; std::mem::size_of::<ResponseHeader>()];
    stream.read_exact(&mut header).await?;
    let header = ResponseHeader::from_array(header);
    let mut buf = vec![0; header.get_len()];
    stream.read_exact(&mut buf).await?;
    R::decode_exact(&buf).ok_or(io::ErrorKind::InvalidData.into())
}

pub fn spawn(fut: impl Future + Send + 'static) {
    let task = async move { _ = fut.await };
    wasm_if!(wasm_bindgen_futures::spawn_local(task), tokio::spawn(task));
}

#[derive(Codec)]
pub struct Encrypted<T>(Vec<u8>, PhantomData<T>);

impl<T> Encrypted<T> {
    pub fn new(data: T, secret: SharedSecret) -> Self
    where
        T: Encode,
    {
        Self(encrypt(data.to_bytes(), secret), PhantomData)
    }

    pub fn decrypt(&mut self, secret: SharedSecret) -> Option<T>
    where
        T: DecodeOwned,
    {
        let data = crypto::decrypt(&mut self.0, secret)?;
        T::decode_exact(&data)
    }
}

pub fn encrypt(mut data: Vec<u8>, secret: SharedSecret) -> Vec<u8> {
    let tag = crypto::encrypt(&mut data, secret, OsRng);
    data.extend(tag);
    data
}

pub fn decrypt(mut data: Vec<u8>, secret: SharedSecret) -> Result<Vec<u8>, DecryptError> {
    match crypto::decrypt(&mut data, secret) {
        Some(slc) => {
            let len = slc.len();
            data.truncate(len);
            Ok(data)
        }
        None => Err(DecryptError(data)),
    }
}

#[derive(Debug)]
pub struct DecryptError(Vec<u8>);

impl std::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to decrypt data: {:?}", self.0)
    }
}

impl std::error::Error for DecryptError {}

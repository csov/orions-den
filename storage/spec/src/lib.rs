#![feature(impl_trait_in_assoc_type)]
#![feature(slice_take)]
#![feature(macro_metavar_expr)]
#![feature(array_windows)]
#![feature(slice_flatten)]
#![feature(portable_simd)]
#![allow(internal_features)]
#![feature(ptr_internals)]
#![feature(trait_alias)]

pub use berkleamp_welch::{DecodeError, RebuildError, ResourcesError, Share as ReconstructPiece};
use {
    arrayvec::ArrayVec,
    codec::{Codec, Encode, Reminder},
    crypto::{
        proof::{Nonce, Proof},
        sign::Signature,
    },
    std::{fmt::Write as _, net::SocketAddr},
};

pub mod protocol;
pub mod uri;
pub mod rpcs {
    macro_rules! rpcs {
        ($($name:ident;)*) => { $( pub const $name: u8 = ${index(0)}; )* };
    }

    rpcs! {
        // client to node
        STORE_FILE;
        READ_FILE;

        // satelite to node
        GET_PIECE_PROOF;

        // node to satellite
        REGISTER_NODE;
        HEARTBEAT;
        GET_GC_META;

        // client to satellite
        REGISTER_CLIENT;
        ALLOCATE_FILE;
        DELETE_FILE;
        ALLOCAET_BANDWIDTH;
        GET_FILE_HOLDERS;
    }
}

pub const PIECE_SIZE: usize = 1024;
pub const DATA_PIECES: usize = if cfg!(debug_assertions) { 4 } else { 16 };
pub const PARITY_PIECES: usize = if cfg!(debug_assertions) { 4 } else { 16 };
pub const MAX_PIECES: usize = DATA_PIECES + PARITY_PIECES;

pub type ReconstructBundle<'data> = [ReconstructPiece<'data>; DATA_PIECES];
pub type Piece = [u8; PIECE_SIZE];
pub type Data = [u8; DATA_PIECES * PIECE_SIZE];
pub type Parity = [u8; PARITY_PIECES * PIECE_SIZE];
pub type NodeIdentity = crypto::Hash;
pub type UserIdentity = crypto::Hash;
pub type FileId = crypto::Hash;
pub type NodeId = u16;
pub type FreeSpace = u64;
pub type Holders = [NodeId; MAX_PIECES];
pub type ExpandedHolders = [(NodeIdentity, SocketAddr); MAX_PIECES];
pub type ClientResult<T, E = ClientError> = Result<T, E>;
pub type NodeResult<T, E = NodeError> = Result<T, E>;

#[derive(Codec, thiserror::Error, Debug)]
pub enum ClientError {
    #[error("not enough nodes to store the file")]
    NotEnoughtNodes,
    #[error("internal error, actual message is logged")]
    InternalError,
    #[error("not allowed")]
    NotAllowed,
    #[error("already registered")]
    AlreadyRegistered,
    #[error("invalid proof")]
    InvalidProof,
    #[error("you won the lottery, now try again")]
    YouWonTheLottery,
    #[error("not found")]
    NotFound,
    #[error("invalid nonce, expected: {0}")]
    InvalidNonce(Nonce),
    #[error("not registered")]
    NotRegistered,
    #[error("channel closed")]
    ChannelClosed,
    #[error("timeout")]
    Timeout,
    #[error("invalid response")]
    InvalidResponse,
}

impl From<anyhow::Error> for ClientError {
    fn from(err: anyhow::Error) -> Self {
        log::error!("{:#?}", err);
        ClientError::InternalError
    }
}

#[derive(Codec, thiserror::Error, Debug)]
pub enum NodeError {
    #[error("invalid proof")]
    InvalidProof,
    #[error("already registered")]
    AlreadyRegistered,
    #[error("not registered")]
    NotRegistered,
    #[error("invalid nonce, expected: {0}")]
    InvalidNonce(u64),
    #[error("store thrown unexpected error, actual message is logged")]
    StoreError,
    #[error("invalid address, expected ip followed by port in the multiaddr format")]
    InvalidAddress,
}

impl From<anyhow::Error> for NodeError {
    fn from(err: anyhow::Error) -> Self {
        log::error!("lmdb error: {:?}", err);
        NodeError::StoreError
    }
}

#[repr(packed)]
#[derive(Clone, Copy, Codec)]
pub struct Address {
    pub id: FileId,
    pub size: u64,
}

impl Address {
    pub fn to_file_name(&self) -> String {
        // FIXME: use base64
        struct HexBuffer(String);

        impl codec::Buffer for HexBuffer {
            fn push(&mut self, byte: u8) -> Option<()> {
                write!(self.0, "{:02x}", byte).ok()
            }
        }

        impl AsMut<[u8]> for HexBuffer {
            fn as_mut(&mut self) -> &mut [u8] {
                unimplemented!()
            }
        }

        let mut hasher = HexBuffer(String::with_capacity(self.encoded_len() * 2));
        self.encode(&mut hasher).unwrap();
        hasher.0
    }
}

#[repr(packed)]
#[derive(Clone, Copy, Codec)]
pub struct StoreContext {
    pub address: Address,
    pub dest: NodeIdentity,
}

#[repr(packed)]
#[derive(Clone, Copy, Codec)]
pub struct BandwidthContext {
    pub dest: NodeIdentity,
    pub issuer: UserIdentity,
    pub amount: u64,
}

#[derive(Codec, Clone, Copy)]
pub struct CompactBandwidthUse {
    pub signature: Signature,
    pub amount: u64,
}

#[derive(Codec, Clone, Copy)]
pub struct BandwidthUse {
    pub proof: Proof<BandwidthContext>,
    pub amount: u64,
}
impl BandwidthUse {
    pub fn is_exhaused(&self) -> bool {
        self.amount == self.proof.context.amount
    }
}

#[derive(Codec, Clone, Copy)]
pub struct File {
    pub address: Address,
    pub holders: ExpandedHolders,
}

#[repr(packed)]
#[derive(Clone, Copy, Codec)]
pub struct FileMeta {
    pub holders: Holders,
    pub owner: UserIdentity,
}

pub struct Encoding {
    inner: berkleamp_welch::Fec,
    buffer: Vec<u8>,
}

impl Default for Encoding {
    fn default() -> Self {
        Self { inner: berkleamp_welch::Fec::new(DATA_PIECES, PARITY_PIECES), buffer: vec![] }
    }
}

impl Encoding {
    pub fn encode(&self, data: &Data, parity: &mut Parity) {
        self.inner.encode(data, parity).unwrap();
    }

    pub fn reconstruct(
        &mut self,
        shards: &mut ReconstructBundle,
    ) -> Result<(), berkleamp_welch::RebuildError> {
        self.inner.rebuild(shards, &mut self.buffer).map(drop)
    }

    pub fn find_cheaters(
        &mut self,
        shards: &mut ArrayVec<ReconstructPiece, MAX_PIECES>,
    ) -> Result<(), berkleamp_welch::DecodeError> {
        self.inner.decode(shards, true, &mut self.buffer).map(drop)
    }
}

#[derive(Codec)]
pub struct Request<'a> {
    pub prefix: u8,
    pub body: Reminder<'a>,
}

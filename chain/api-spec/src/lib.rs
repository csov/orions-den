#![feature(macro_metavar_expr)]

pub use bip39::Mnemonic;
use {
    bip39::{rand::SeedableRng, rand_core::OsRng},
    crypto::{enc, sign},
    rand_chacha::ChaChaRng,
    std::{net::SocketAddr, path::Path, pin::Pin},
};

pub type Topic = [u8; 32];
/// seconds from unix epoch
pub type Timestamp = u64;
pub type Balance = u128;
pub type NodeId = [u8; 32];

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;
pub type Task<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;
pub type NodeEventStream = Pin<Box<dyn futures::Stream<Item = Result<NodeEvent>> + Send>>;
pub type NodeVec = Vec<(NodeId, SocketAddr)>;

pub type InitFn = fn(*mut ()) -> Task<Result<()>>;
pub type CloneFn = fn(*mut (), *mut ());
pub type DropFn = fn(*mut ());

pub const ONION_CLIENT_VTABLE: &'static [u8] = b"__ONION_CLIENT_VTABLE";

macro_rules! impl_vtable {
    (
        #[client($client:ident)]
        $vis:vis trait $name:ident {$(
            #[ret_converter($converter:ident)]
            fn $fn:ident(&self $(,$arg:ident: $arg_ty:ty)*) -> $ret:ty;
        )*}
    ) => {
        $vis struct $name {
            pub id: &'static str,
            $(pub $fn: fn(*mut (), $($arg: $arg_ty),*) -> $ret,)*
            pub init: InitFn,
            pub clone: CloneFn,
            pub drop: DropFn,
            pub size: usize,
            pub align: usize,
        }

        impl $client {$(
            $vis fn $fn(&self $(,$arg: $arg_ty)*) -> $ret {
                (self.vtable.$fn)(self.obj, $($arg),*)
            }

        )*}

        #[macro_export]
        macro_rules! export_api_spec {
            ($$name:ty) => {
                #[no_mangle]
                pub static __ONION_CLIENT_VTABLE: $$crate::$name = $$crate::$name {
                    id: env!("CARGO_PKG_NAME"),
                    $($fn: |obj, $($arg),*| unsafe {
                        $$crate::$converter(<$$name>::$fn(&*(obj as *mut $$name), $($arg),*)) },)*
                    init: |obj| Box::pin($crate::AlwaisSend(async move { unsafe { *(obj as *mut $$name) = <$$name>::new().await?; Ok(()) } })),
                    clone: |obj, new_obj| unsafe { *(new_obj as *mut $$name) = Clone::clone(&*(obj as *mut $$name)); },
                    drop: |obj| unsafe { std::ptr::drop_in_place(obj as *mut $$name) },
                    size: std::mem::size_of::<$$crate::$name>(),
                    align: std::mem::align_of::<$$crate::$name>(),
                };
            }
        }
    }
}

impl_vtable! {
    #[client(Client)]
    pub trait ClientVTable {
        #[ret_converter(convert_future)]
        fn get_sub(&self, topic: Topic) -> Task<Result<Option<Subscription>>>;
        #[ret_converter(convert_stream)]
        fn open_node_event_stream(&self) -> Task<Result<NodeEventStream>>;
        #[ret_converter(convert_future)]
        fn vote_if_possible(&self, source: NodeId, target: NodeId) -> Task<Result<()>>;
        #[ret_converter(convert_future)]
        fn list_nodes(&self) -> Task<Result<NodeVec>>;
    }
}

pub struct AlwaisSend<T>(pub T);

unsafe impl<T> Send for AlwaisSend<T> {}

impl<T: std::future::Future> std::future::Future for AlwaisSend<T> {
    type Output = T::Output;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|s| &mut s.0).poll(cx) }
    }
}

pub fn convert_future<T>(fut: impl std::future::Future<Output = T> + 'static + Send) -> Task<T> {
    Box::pin(fut)
}

pub fn convert_stream(
    stream: impl std::future::Future<
            Output = Result<impl futures::Stream<Item = Result<NodeEvent>> + 'static + Send>,
        >
        + 'static
        + Send,
) -> Task<Result<NodeEventStream>> {
    use futures::FutureExt;
    stream.map(|s| s.map(|s| Box::pin(s) as _)).boxed()
}

impl ClientVTable {
    fn layout(&self) -> std::alloc::Layout {
        unsafe { std::alloc::Layout::from_size_align_unchecked(self.size, self.align) }
    }
}

pub struct Client {
    _dynlib: libloading::Library,
    vtable: ClientVTable,
    obj: *mut (),
}

impl Client {
    pub async unsafe fn new(dynlib: &Path) -> Result<Self> {
        let _dynlib = libloading::Library::new(dynlib)?;

        let vtable = std::ptr::read(*_dynlib.get::<*mut ClientVTable>(ONION_CLIENT_VTABLE)?);
        let obj = std::alloc::alloc(vtable.layout()) as *mut ();
        (vtable.init)(obj).await?;

        Ok(Self { _dynlib, vtable, obj })
    }

    pub fn id(&self) -> &'static str {
        self.vtable.id
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            (self.vtable.drop)(self.obj);
            std::alloc::dealloc(self.obj as *mut u8, self.vtable.layout());
        }
    }
}

#[derive(Clone)]
pub struct NodeKeys {
    pub enc: enc::Keypair,
    pub sign: sign::Keypair,
}

impl NodeKeys {
    pub fn identity(&self) -> NodeId {
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

pub struct Subscription {
    pub topic: Topic,
    pub amount: Balance,
    pub timetsmp: Timestamp,
}

pub enum NodeEvent {
    Join { node: NodeId, addr: SocketAddr },
    AddrChanged { node: NodeId, addr: SocketAddr },
    Left { node: NodeId },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Dummy {}

    impl Dummy {
        async fn new() -> Result<Self> {
            unimplemented!()
        }

        async fn get_sub(&self, _: Topic) -> Result<Option<Subscription>> {
            unimplemented!()
        }

        async fn open_node_event_stream(
            &self,
        ) -> Result<impl futures::Stream<Item = Result<NodeEvent>> + 'static + Send> {
            Ok(futures::stream::empty())
        }

        async fn vote_if_possible(&self, _: NodeId, _: NodeId) -> Result<()> {
            unimplemented!()
        }

        async fn list_nodes(&self) -> Result<NodeVec> {
            unimplemented!()
        }
    }

    export_api_spec!(Dummy);
}

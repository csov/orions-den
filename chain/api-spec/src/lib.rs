#![feature(if_let_guard)]
#![feature(macro_metavar_expr)]
#![feature(slice_take)]

pub use bip39::Mnemonic;
use {
    codec::{Codec, Decode, Encode},
    futures::{SinkExt, StreamExt},
    std::{
        collections::HashMap,
        io::{self, Read, Write},
        net::SocketAddr,
        path::Path,
        sync::{Arc, Mutex},
    },
};

pub type Topic = [u8; 32];
/// seconds from unix epoch
pub type Timestamp = u64;
pub type Balance = u128;
pub type NodeId = [u8; 32];

pub type NodeVec = Vec<(NodeId, SocketAddr)>;
pub type NodeEventStream = futures::channel::mpsc::Receiver<NodeEvent>;

struct CoreRequest {
    send_to: futures::channel::oneshot::Sender<Vec<u8>>,
    req: Request,
}

#[derive(Clone)]
pub struct Client {
    requests: futures::channel::mpsc::Sender<CoreRequest>,
}

impl Client {
    pub fn new(path: &Path) -> io::Result<(Self, futures::channel::mpsc::Receiver<NodeEvent>)> {
        let mut child = std::process::Command::new(path)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;

        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (req_sd, mut req_rd) = futures::channel::mpsc::channel::<CoreRequest>(100);

        let requests = Arc::new(Mutex::new(HashMap::new()));
        let (mut event_sd, event_rd) = futures::channel::mpsc::channel::<NodeEvent>(100);

        let rqs = requests.clone();
        std::thread::spawn(move || {
            let _ch = child;
            let f = futures::executor::block_on(async move {
                let mut buf = vec![];
                while let Some(core) = req_rd.next().await {
                    rqs.lock().unwrap().insert(core.req.id, core.send_to);
                    core.req.encode(&mut buf).unwrap();
                    stdin.write_all(&(buf.len() as u32).to_le_bytes())?;
                    stdin.write_all(&buf)?;
                    stdin.flush()?;
                }
                io::Result::Ok(())
            });

            f.unwrap();
        });

        std::thread::spawn(move || {
            let mut buf = vec![];
            let mut len_buf = [0u8; 4];
            let a: io::Result<()> = futures::executor::block_on(async {
                loop {
                    eprintln!("fff {:?}", "fa");
                    stdout.read_exact(&mut len_buf)?;
                    let len = u32::from_le_bytes(len_buf) as usize;

                    eprintln!("fff {:?}", len);

                    buf.resize(len, 0);
                    stdout.read_exact(&mut buf)?;

                    eprintln!("fff {:?}", buf);

                    match Response::decode_exact(&buf).ok_or(io::ErrorKind::InvalidData)? {
                        Response::Body { id, body } => {
                            eprintln!("{:?}", body);
                            _ = requests
                                .lock()
                                .unwrap()
                                .remove(&id)
                                .map(|ch| ch.send(body))
                                .unwrap();
                        }
                        Response::Event(e) => _ = event_sd.send(e).await,
                    }
                }
            });
            println!("{buf:?} {len_buf:?}");
            a.unwrap();
        });

        Ok((Self { requests: req_sd }, event_rd))
    }

    pub async fn get_sub(&mut self, topic: Topic) -> io::Result<Option<Subscription>> {
        self.make_req(RequestBody::GetSub { topic }).await
    }

    pub async fn vote_if_possible(&mut self, source: NodeId, target: NodeId) -> io::Result<()> {
        self.make_req(RequestBody::VoteIfPossible { source, target }).await
    }

    pub async fn list_nodes(&mut self) -> io::Result<NodeVec> {
        self.make_req(RequestBody::ListNodes).await
    }

    async fn make_req<T: for<'a> Decode<'a>>(&mut self, body: RequestBody) -> io::Result<T> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.requests
            .send(CoreRequest { send_to: tx, req: Request::new(body) })
            .await
            .map_err(|_| io::ErrorKind::ConnectionAborted)?;
        rx.await
            .map_err(|_| io::ErrorKind::Interrupted)
            .and_then(|v| <_>::decode_exact(&v).ok_or(io::ErrorKind::InvalidData))
            .map_err(Into::into)
    }
}

#[derive(Codec)]
pub struct Request {
    pub id: usize,
    pub body: RequestBody,
}
impl Request {
    fn new(body: RequestBody) -> Self {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Self { id, body }
    }
}

#[derive(Codec)]
pub enum RequestBody {
    GetSub { topic: Topic },
    VoteIfPossible { source: NodeId, target: NodeId },
    ListNodes,
}

#[derive(Codec, Debug)]
pub enum Response {
    Body { id: usize, body: Vec<u8> },
    Event(NodeEvent),
}

#[derive(Codec)]
pub struct Subscription {
    pub topic: Topic,
    pub amount: Balance,
    pub timetsmp: Timestamp,
}

#[derive(Codec, Debug)]
pub enum NodeEvent {
    Join { node: NodeId, addr: SocketAddr },
    AddrChanged { node: NodeId, addr: SocketAddr },
    Left { node: NodeId },
    Voted { source: NodeId, target: NodeId },
}

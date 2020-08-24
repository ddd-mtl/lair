//! Abstraction over unix domain sockets / windows named pipes

use crate::internal::wire::*;
use crate::*;

use futures::{future::FutureExt, sink::SinkExt, stream::StreamExt};
use ghost_actor::dependencies::tracing;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(not(windows))]
mod unix_ipc;
#[cfg(not(windows))]
use unix_ipc::*;

#[cfg(windows)]
mod win_ipc;
#[cfg(windows)]
use win_ipc::*;

ghost_actor::ghost_chan! {
    /// Low-level send api..
    pub chan LowLevelWireApi<LairError> {
        /// Send LairWire message somewhere.
        fn low_level_send(msg: LairWire) -> ();
    }
}

/// Low-level send api Sender.
type LowLevelWireSender = futures::channel::mpsc::Sender<LowLevelWireApi>;

/// Low-level send api Receiver.
type LowLevelWireReceiver = futures::channel::mpsc::Receiver<LowLevelWireApi>;

/// IpcRespond
type IpcRespond = ghost_actor::GhostRespond<IpcWireApiHandlerResult<LairWire>>;

/// IpcSender
pub type IpcSender = futures::channel::mpsc::Sender<IpcWireApi>;

/// IpcReceiver
pub type IpcReceiver = futures::channel::mpsc::Receiver<IpcWireApi>;

/// IncomingIpcReceiver
pub type IncomingIpcSender =
    futures::channel::mpsc::Sender<(KillSwitch, IpcSender, IpcReceiver)>;

/// IncomingIpcReceiver
pub type IncomingIpcReceiver =
    futures::channel::mpsc::Receiver<(KillSwitch, IpcSender, IpcReceiver)>;

fn err_spawn<F>(hint: &'static str, f: F)
where
    F: std::future::Future<Output = LairResult<()>> + 'static + Send,
{
    tokio::task::spawn(async move {
        match f.await {
            Ok(_) => tracing::debug!("FUTURE {} ENDED Ok!!!", hint),
            Err(e) => tracing::warn!("FUTURE {} ENDED Err: {:?}", hint, e),
        }
    });
}

fn spawn_low_level_write_half(
    kill_switch: KillSwitch,
    mut write_half: IpcWrite,
) -> LairResult<LowLevelWireSender> {
    let (s, mut r) = futures::channel::mpsc::channel(10);

    err_spawn("ll-write", async move {
        while let Some(msg) = r.next().await {
            match msg {
                LowLevelWireApi::LowLevelSend { respond, msg, .. } => {
                    tracing::trace!("ll write {:?}", msg);
                    let r: LairResult<()> = async {
                        let msg = msg.encode()?;
                        write_half
                            .write_all(&msg)
                            .await
                            .map_err(LairError::other)?;
                        Ok(())
                    }
                    .await;
                    respond.respond(Ok(async move { r }.boxed().into()));
                }
            }

            if !kill_switch.cont() {
                break;
            }
        }
        LairResult::<()>::Ok(())
    });

    Ok(s)
}

fn spawn_low_level_read_half(
    kill_switch: KillSwitch,
    mut read_half: IpcRead,
) -> LairResult<LowLevelWireReceiver> {
    let (s, r) = futures::channel::mpsc::channel(10);

    err_spawn("ll-read", async move {
        let mut pending_data = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = read_half
                .read(&mut buffer)
                .await
                .map_err(LairError::other)?;
            pending_data.extend_from_slice(&buffer[..read]);
            while let Ok(size) = LairWire::peek_size(&pending_data) {
                if pending_data.len() < size {
                    break;
                }
                let msg = LairWire::decode(&pending_data)?;
                tracing::trace!("ll read {:?}", msg);
                let _ = pending_data.drain(..size);
                s.low_level_send(msg).await?;
            }
            if !kill_switch.cont() {
                break;
            }
        }
        LairResult::<()>::Ok(())
    });

    Ok(r)
}

/// Establish an outgoing client ipc connection to a lair server.
pub async fn spawn_ipc_connection(
    config: Arc<Config>,
) -> LairResult<(KillSwitch, IpcSender, IpcReceiver)> {
    let (read_half, write_half) = ipc_connect(config).await?;
    spawn_connection_pair(read_half, write_half)
}

/// Spawn/bind a new ipc listener connection awaiting incomming clients.
pub async fn spawn_bind_ipc(
    config: Arc<Config>,
) -> LairResult<(KillSwitch, IncomingIpcReceiver)> {
    let kill_switch = KillSwitch::new();
    let (in_send, in_recv) = futures::channel::mpsc::channel(10);

    let srv = IpcServer::bind(config)?;

    err_spawn(
        "srv-bind",
        srv_main_bind_task(kill_switch.clone(), srv, in_send),
    );

    Ok((kill_switch, in_recv))
}

async fn srv_main_bind_task(
    kill_switch: KillSwitch,
    mut srv: IpcServer,
    mut in_send: IncomingIpcSender,
) -> LairResult<()> {
    loop {
        if let Ok((read_half, write_half)) = srv.accept().await {
            let (con_kill_switch, send, recv) =
                spawn_connection_pair(read_half, write_half)?;

            in_send
                .send((con_kill_switch, send, recv))
                .await
                .map_err(LairError::other)?;
        }
        if !kill_switch.cont() {
            break;
        }
    }
    Ok(())
}

fn spawn_connection_pair(
    read_half: IpcRead,
    write_half: IpcWrite,
) -> LairResult<(KillSwitch, IpcSender, IpcReceiver)> {
    let respond_track = RespondTrack::new();
    let kill_switch = KillSwitch::new();

    let writer = spawn_low_level_write_half(kill_switch.clone(), write_half)?;
    let reader = spawn_low_level_read_half(kill_switch.clone(), read_half)?;

    let (outgoing_msg_send, outgoing_msg_recv) =
        futures::channel::mpsc::channel(10);
    let (incoming_msg_send, incoming_msg_recv) =
        futures::channel::mpsc::channel(10);

    err_spawn(
        "con-write",
        spawn_write_task(
            respond_track.clone(),
            kill_switch.clone(),
            outgoing_msg_recv,
            writer.clone(),
        ),
    );
    err_spawn(
        "con-read",
        spawn_read_task(
            respond_track,
            kill_switch.clone(),
            incoming_msg_send,
            reader,
            writer,
        ),
    );

    Ok((kill_switch, outgoing_msg_send, incoming_msg_recv))
}

async fn spawn_write_task(
    respond_track: RespondTrack,
    kill_switch: KillSwitch,
    mut outgoing_msg_recv: IpcReceiver,
    writer: LowLevelWireSender,
) -> LairResult<()> {
    while let Some(msg) = outgoing_msg_recv.next().await {
        match msg {
            IpcWireApi::Request { respond, msg, .. } => {
                respond_track.register(msg.get_msg_id(), respond).await;
                writer.low_level_send(msg).await?;
            }
        }
        if !kill_switch.cont() {
            break;
        }
    }
    Ok(())
}

async fn spawn_read_task(
    respond_track: RespondTrack,
    kill_switch: KillSwitch,
    incoming_msg_send: IpcSender,
    mut reader: LowLevelWireReceiver,
    writer: LowLevelWireSender,
) -> LairResult<()> {
    while let Some(msg) = reader.next().await {
        match msg {
            LowLevelWireApi::LowLevelSend { respond, msg, .. } => {
                // respond right away, so it can start processing the
                // next message.
                respond.respond(Ok(async move { Ok(()) }.boxed().into()));

                if msg.is_req() {
                    let fut = incoming_msg_send.request(msg);
                    let writer_clone = writer.clone();
                    err_spawn("req-mini", async move {
                        if let Ok(res) = fut.await {
                            let _ = writer_clone.low_level_send(res).await;
                        }
                        LairResult::<()>::Ok(())
                    });
                } else {
                    respond_track.respond(msg).await;
                }
            }
        }
        if !kill_switch.cont() {
            break;
        }
    }
    Ok(())
}

/// If any of these are dropped, they all say we should stop looping.
#[derive(Clone)]
pub struct KillSwitch(Arc<std::sync::atomic::AtomicBool>);

impl Drop for KillSwitch {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Relaxed)
    }
}

impl KillSwitch {
    /// Create a new kill switch
    pub fn new() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicBool::new(true)))
    }

    /// Should we continue?
    pub fn cont(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for KillSwitch {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct RespondTrack(Arc<tokio::sync::Mutex<HashMap<u64, IpcRespond>>>);

impl RespondTrack {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::Mutex::new(HashMap::new())))
    }

    pub async fn register(&self, msg_id: u64, respond: IpcRespond) {
        let mut lock = self.0.lock().await;
        lock.insert(msg_id, respond);
    }

    pub async fn respond(&self, msg: LairWire) {
        let mut lock = self.0.lock().await;
        let msg_id = msg.get_msg_id();
        if let Some(respond) = lock.remove(&msg_id) {
            respond.respond(Ok(async move { Ok(msg) }.boxed().into()));
        }
    }
}

ghost_actor::ghost_chan! {
    /// Ipc wire api for both incoming api requsets and outgoing event requests.
    pub chan IpcWireApi<LairError> {
        /// Make an Ipc request.
        fn request(msg: LairWire) -> LairWire;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(threaded_scheduler)]
    async fn test_ipc_raw_wire() -> LairResult<()> {
        let tmpdir = tempfile::tempdir().unwrap();

        let config = Config::builder().set_root_path(tmpdir.path()).build();

        let (srv_kill, mut srv_recv) = spawn_bind_ipc(config.clone()).await?;

        let srv_task_kill = srv_kill.clone();
        err_spawn("test-outer", async move {
            while let Some((con_kill, con_send, mut con_recv)) =
                srv_recv.next().await
            {
                err_spawn("test-inner", async move {
                    println!("GOT CONNECTION!!");
                    let r = con_send
                        .request(LairWire::ToCliRequestUnlockPassphrase {
                            msg_id: 0,
                        })
                        .await
                        .unwrap();
                    println!("passphrase req RESPONSE: {:?}", r);
                    match r {
                        LairWire::ToLairRequestUnlockPassphraseResponse {
                            passphrase,
                            ..
                        } => {
                            assert_eq!("test-passphrase", &passphrase);
                        }
                        _ => panic!("unexpected: {:?}", r),
                    }
                    println!("DONE WITH PASSPHRASE LOOP\n\n");
                    while let Some(msg) = con_recv.next().await {
                        println!("GOT MESSAGE!!: {:?}", msg);
                        match msg {
                            IpcWireApi::Request { respond, msg, .. } => {
                                println!("GOT MESSAGE!!: {:?}", msg);
                                if let LairWire::ToLairLairGetLastEntryIndex {
                                    msg_id,
                                } = msg
                                {
                                    respond.respond(Ok(async move {
                                        Ok(LairWire::ToCliLairGetLastEntryIndexResponse {
                                            msg_id,
                                            last_keystore_index: 42.into(),
                                        })
                                    }.boxed().into()));
                                }
                            }
                        }
                        if !con_kill.cont() {
                            break;
                        }
                    }
                    LairResult::<()>::Ok(())
                });
                if !srv_task_kill.cont() {
                    break;
                }
            }
            LairResult::<()>::Ok(())
        });

        let (cli_kill, cli_send, mut cli_recv) =
            spawn_ipc_connection(config).await?;

        match cli_recv.next().await.unwrap() {
            IpcWireApi::Request { respond, msg, .. } => {
                println!("GOT: {:?}", msg);
                match msg {
                    LairWire::ToCliRequestUnlockPassphrase { msg_id } => {
                        respond.respond(Ok(async move {
                            Ok(LairWire::ToLairRequestUnlockPassphraseResponse {
                                msg_id,
                                passphrase: "test-passphrase".to_string(),
                            })
                        }
                        .boxed()
                        .into()));
                    }
                    _ => panic!("unexpected: {:?}", msg),
                }
            }
        }

        let res = cli_send
            .request(LairWire::ToLairLairGetLastEntryIndex { msg_id: 0 })
            .await
            .unwrap();
        println!("GOT: {:?}", res);

        match res {
            LairWire::ToCliLairGetLastEntryIndexResponse {
                last_keystore_index,
                ..
            } => {
                assert_eq!(42, last_keystore_index.0);
            }
            _ => panic!("unexpected: {:?}", res),
        }

        drop(cli_kill);
        drop(srv_kill);
        drop(tmpdir);

        Ok(())
    }
}

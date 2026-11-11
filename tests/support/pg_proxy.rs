//! 테스트 전용 PostgreSQL wire fault. 전용 loopback 서버의 COMMIT 완료만 확인하고 ACK를 유실한다.
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::Notify,
    task::JoinHandle,
};
pub struct CommitProxy {
    pub port: u16,
    pub armed: Arc<AtomicBool>,
    pub committed: Arc<Notify>,
    #[allow(dead_code)]
    pub release: Arc<Notify>,
    task: JoinHandle<()>,
}
impl Drop for CommitProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl CommitProxy {
    pub async fn new() -> Self {
        Self::assemble(false).await
    }
    #[allow(dead_code)]
    pub async fn migration_stall() -> Self {
        Self::assemble(true).await
    }
    async fn assemble(migration: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let armed = Arc::new(AtomicBool::new(false));
        let committed = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let r = release.clone();
        let a = armed.clone();
        let n = committed.clone();
        let task = tokio::spawn(async move {
            let mut jobs = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result=listener.accept()=>{let (socket,_)=result.unwrap();if jobs.len()>=8 {drop(socket);continue;}let a=a.clone();let n=n.clone();let r=r.clone();jobs.spawn(async move{let _=forward(socket,a,n,migration,r).await;});},
                    _=jobs.join_next(),if !jobs.is_empty()=>{},
                }
            }
        });
        Self {
            port,
            armed,
            committed,
            release,
            task,
        }
    }
}
async fn message(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let tag = reader.read_u8().await?;
    let size = reader.read_u32().await? as usize;
    if !(4..=2 * 1024 * 1024).contains(&size) {
        return Err(io::Error::other("bounded test PG frame required"));
    }
    let mut bytes = vec![0; size + 1];
    bytes[0] = tag;
    bytes[1..5].copy_from_slice(&(size as u32).to_be_bytes());
    reader.read_exact(&mut bytes[5..]).await?;
    Ok(bytes)
}
async fn write_message(writer: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes).await?;
    writer.flush().await
}
async fn forward(
    mut client: TcpStream,
    armed: Arc<AtomicBool>,
    committed: Arc<Notify>,
    migration: bool,
    release: Arc<Notify>,
) -> io::Result<()> {
    let mut server = TcpStream::connect("127.0.0.1:64603").await?;
    let len = client.read_u32().await? as usize;
    if !(8..=65536).contains(&len) {
        return Err(io::Error::other("bounded startup required"));
    }
    let mut startup = vec![0; len];
    startup[..4].copy_from_slice(&(len as u32).to_be_bytes());
    client.read_exact(&mut startup[4..]).await?;
    write_message(&mut server, &startup).await?;
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let drop_ack = Arc::new(AtomicBool::new(false));
    let gate = drop_ack.clone();
    let frontend = async {
        loop {
            let bytes = message(&mut cr).await?;
            let target = if migration {
                bytes[0] == b'P'
                    && bytes
                        .windows(b"pg_advisory_xact_lock".len())
                        .any(|s| s == b"pg_advisory_xact_lock")
            } else {
                bytes[0] == b'Q' && &bytes[5..] == b"COMMIT\0"
            };
            if target && armed.swap(false, Ordering::SeqCst) {
                gate.store(true, Ordering::SeqCst);
            }
            write_message(&mut sw, &bytes).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let backend = async {
        loop {
            let bytes = message(&mut sr).await?;
            if drop_ack.load(Ordering::SeqCst) {
                if migration {
                    committed.notify_one();
                    release.notified().await;
                    drop_ack.store(false, Ordering::SeqCst);
                    write_message(&mut cw, &bytes).await?;
                } else if bytes[0] == b'C' && &bytes[5..] == b"COMMIT\0" {
                    committed.notify_one();
                    return Ok::<(), io::Error>(());
                }
            } else {
                write_message(&mut cw, &bytes).await?;
            }
        }
    };
    tokio::select! {result=frontend=>result,result=backend=>result}
}

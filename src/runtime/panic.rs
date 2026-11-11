//! panic payload/소스 위치는 운영 로그에 기록하지 않는다. sink 포화에도 기다리거나 task를 생성하지 않는다.
use std::{
    os::fd::AsFd,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
static PANICS: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn counts() -> (u64, u64) {
    (
        PANICS.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
    )
}

pub fn install_redacting_hook() {
    use std::os::unix::fs::FileTypeExt;
    enum Sink {
        Pipe(std::fs::File),
        Socket(std::os::unix::net::UnixStream),
    }
    let sink = (|| {
        let fd = std::io::stderr().as_fd().try_clone_to_owned().ok()?;
        let kind = std::fs::File::from(fd.try_clone().ok()?)
            .metadata()
            .ok()?
            .file_type();
        if kind.is_socket() {
            let socket = std::os::unix::net::UnixStream::from(fd);
            socket.set_nonblocking(true).ok()?;
            Some(Sink::Socket(socket))
        } else {
            let pipe = tokio::net::unix::pipe::Sender::from_owned_fd(fd).ok()?;
            Some(Sink::Pipe(std::fs::File::from(
                pipe.into_nonblocking_fd().ok()?,
            )))
        }
    })();
    let sink = Mutex::new(sink);
    std::panic::set_hook(Box::new(move |_| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        crate::telemetry::add(crate::telemetry::Metric::Panics, 1);
        let line = b"{\"alarm\":\"runtime_panic\"}\n";
        let sent = sink
            .try_lock()
            .ok()
            .and_then(|mut guard| match guard.as_mut()? {
                Sink::Pipe(pipe) => {
                    use std::io::Write;
                    pipe.write(line).ok()
                }
                Sink::Socket(socket) => {
                    use std::io::Write;
                    socket.write(line).ok()
                }
            });
        if sent != Some(line.len()) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }));
}

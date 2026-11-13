//! The local socket a background session listens on: a socket file on Unix, a named pipe on
//! Windows.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadBuf};

pub(crate) use interprocess::local_socket::tokio::Listener;
pub(crate) use interprocess::local_socket::traits::tokio::Listener as Accept;

/// The longest socket path every Unix takes (macOS allows 104 bytes, the terminator included).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

/// Where the background session `id` listens. On Windows a pipe name; on Unix a file in the
/// run directory, or in a directory of its own under the temporary directory when the path
/// would be too long for a socket.
pub(crate) fn name_for(home: &Path, id: &str) -> io::Result<String> {
    #[cfg(windows)]
    {
        let _ = home;
        Ok(format!(r"\\.\pipe\azazel-{id}"))
    }
    #[cfg(unix)]
    {
        let file = crate::running::run_dir(home).join(format!("{id}.sock"));
        if file.as_os_str().len() <= MAX_SOCKET_PATH {
            return Ok(file.to_string_lossy().into_owned());
        }
        let dir = std::env::temp_dir().join(format!("azazel-{id}"));
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir)?;
        Ok(dir.join("s.sock").to_string_lossy().into_owned())
    }
}

fn name(socket: &str) -> io::Result<Name<'_>> {
    socket.to_fs_name::<GenericFilePath>()
}

/// Starts listening on `socket`. A socket file left by a process that died is replaced.
pub(crate) fn listen(socket: &str) -> io::Result<Listener> {
    ListenerOptions::new()
        .name(name(socket)?)
        .try_overwrite(true)
        .create_tokio()
}

/// One end of a connection, for a task of its own: the stream is shared by the task that reads
/// and the task that writes.
pub(crate) struct Half(Arc<Stream>);

impl AsyncRead for Half {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut &*self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Half {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut &*self.0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut &*self.0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut &*self.0).poll_shutdown(cx)
    }
}

/// The reading and the writing end of a connection.
pub(crate) fn halves(stream: Stream) -> (BufReader<Half>, Half) {
    let stream = Arc::new(stream);
    (BufReader::new(Half(stream.clone())), Half(stream))
}

/// Connects to the background session listening on `socket`.
pub(crate) async fn connect(socket: &str) -> io::Result<(BufReader<Half>, Half)> {
    Ok(halves(Stream::connect(name(socket)?).await?))
}

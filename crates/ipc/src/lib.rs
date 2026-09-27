//! Local, same-machine IPC between `aissh-mcp`, the desktop app and `aisshd`.
//!
//! The transport is a Unix domain socket on Unix and a named pipe on Windows.
//! Both are restricted to the account that owns the endpoint:
//!
//! - Unix: the socket file is mode `0600`, and every accepted connection has its
//!   peer UID compared against the daemon's effective UID.
//! - Windows: the pipe is created with `FILE_FLAG_FIRST_PIPE_INSTANCE` (so a
//!   second daemon cannot silently take over a name in use) and
//!   `PIPE_REJECT_REMOTE_CLIENTS`, and every accepted connection has the client's
//!   token user SID compared against the daemon's own.
//!
//! Everything above the transport — framing, request dispatch — is shared and
//! lives in `aissh-protocol`; [`Stream`] only has to be `AsyncRead + AsyncWrite`.

use std::io;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

/// A bound endpoint that hands out one [`Stream`] per accepted client.
pub struct Listener {
    inner: platform::Listener,
}

/// One connected IPC client.
pub struct Stream {
    inner: platform::Stream,
}

impl Listener {
    /// Binds `endpoint`, taking it over if a previous daemon died without
    /// releasing it.
    ///
    /// Returns [`io::ErrorKind::AddrInUse`] when a live daemon already owns the
    /// endpoint, so a second daemon refuses to start instead of splitting
    /// clients between two processes.
    pub async fn bind(endpoint: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: platform::Listener::bind(endpoint).await?,
        })
    }

    /// Waits for the next client.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        Ok(Stream {
            inner: self.inner.accept().await?,
        })
    }

    /// Applies the endpoint's owner-only restriction.
    ///
    /// A Unix socket file is subject to the umask, so its mode has to be set
    /// after binding; a Windows pipe name carries no filesystem permissions, and
    /// the client's identity is checked per connection instead.
    pub fn harden(&self) -> io::Result<()> {
        self.inner.harden()
    }

    /// Releases endpoint state that outlives the process. Only a Unix socket
    /// file needs this; a Windows pipe disappears with its last handle.
    pub fn cleanup(&self) {
        self.inner.cleanup();
    }

    /// The endpoint this listener is bound to, for logs.
    pub fn endpoint(&self) -> &Path {
        self.inner.endpoint()
    }
}

impl Stream {
    /// Connects to a daemon listening on `endpoint`.
    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: platform::Stream::connect(endpoint).await?,
        })
    }

    /// Whether the peer runs as the same user as this process.
    ///
    /// Fails rather than returning `false` when the peer cannot be inspected, so
    /// a daemon that cannot prove who connected refuses the connection.
    pub fn same_user(&self) -> io::Result<bool> {
        self.inner.same_user()
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A unique endpoint per test run, so parallel test binaries never share one.
    fn endpoint(label: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        #[cfg(unix)]
        {
            std::env::temp_dir().join(format!("aissh-ipc-{label}-{suffix}.sock"))
        }
        #[cfg(windows)]
        {
            std::path::PathBuf::from(format!(r"\\.\pipe\aissh-ipc-{label}-{suffix}"))
        }
    }

    #[tokio::test]
    async fn carries_bytes_in_both_directions() {
        let endpoint = endpoint("round-trip");
        let mut listener = Listener::bind(&endpoint).await.unwrap();
        let client_endpoint = endpoint.clone();

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut request = [0u8; 5];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"hello");
            stream.write_all(b"world").await.unwrap();
            stream.flush().await.unwrap();
        });

        let mut client = Stream::connect(&client_endpoint).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.flush().await.unwrap();
        let mut response = [0u8; 5];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"world");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn reports_a_connected_client_as_the_same_user() {
        let endpoint = endpoint("same-user");
        let mut listener = Listener::bind(&endpoint).await.unwrap();
        let client_endpoint = endpoint.clone();

        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            stream.same_user().unwrap()
        });

        let _client = Stream::connect(&client_endpoint).await.unwrap();
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn refuses_to_bind_an_endpoint_a_live_daemon_owns() {
        let endpoint = endpoint("in-use");
        let _listener = Listener::bind(&endpoint).await.unwrap();

        let error = match Listener::bind(&endpoint).await {
            Ok(_) => panic!("binding an endpoint a live daemon owns must fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(error.to_string().contains("already running"));
    }

    /// A daemon that crashed leaves a socket file behind; the next one has to be
    /// able to take the endpoint over rather than refuse to start forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn takes_over_an_endpoint_left_by_a_dead_daemon() {
        let endpoint = endpoint("stale");
        {
            let listener = Listener::bind(&endpoint).await.unwrap();
            listener.cleanup();
        }
        assert!(endpoint.exists());

        let listener = Listener::bind(&endpoint).await.unwrap();
        listener.cleanup();
    }

    #[tokio::test]
    async fn accepts_a_second_client_after_the_first_disconnects() {
        let endpoint = endpoint("sequential");
        let mut listener = Listener::bind(&endpoint).await.unwrap();
        let client_endpoint = endpoint.clone();

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let mut stream = listener.accept().await.unwrap();
                stream.write_all(b"ok").await.unwrap();
                stream.flush().await.unwrap();
            }
        });

        for _ in 0..2 {
            let mut client = Stream::connect(&client_endpoint).await.unwrap();
            let mut response = [0u8; 2];
            client.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"ok");
        }

        server.await.unwrap();
    }
}

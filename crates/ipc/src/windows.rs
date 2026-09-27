use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_PIPE_BUSY: i32 = 231;

/// How long a client keeps retrying while the server swaps instances.
const CONNECT_ATTEMPTS: usize = 40;

pub struct Listener {
    endpoint: PathBuf,
    pending: NamedPipeServer,
}

/// The server and client halves are distinct tokio types that both speak the
/// same protocol, so the reader and writer are the only thing that differs.
pub enum Stream {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

fn instance(endpoint: &Path, first: bool) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    // A name that is already taken must fail rather than attach to the pipe a
    // running daemon owns, and a pipe must never accept a client from another
    // machine over SMB.
    options.first_pipe_instance(first);
    options.reject_remote_clients(true);
    options.create(endpoint)
}

impl Listener {
    pub async fn bind(endpoint: &Path) -> io::Result<Self> {
        match instance(endpoint, true) {
            Ok(pending) => Ok(Self {
                endpoint: endpoint.to_path_buf(),
                pending,
            }),
            // Creating the first instance of a name another process already owns
            // is reported as access denied rather than as a name collision.
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("aisshd is already running at {}", endpoint.display()),
            )),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("cannot create daemon pipe {}: {error}", endpoint.display()),
            )),
        }
    }

    pub async fn accept(&mut self) -> io::Result<Stream> {
        self.pending.connect().await?;
        // A pipe only accepts a client while an instance is waiting, so the next
        // one is created before this connection is handed off.
        let next = instance(&self.endpoint, false)?;
        let connected = std::mem::replace(&mut self.pending, next);
        Ok(Stream::Server(connected))
    }

    /// A pipe name carries no filesystem permissions; the client's token is
    /// checked on every connection instead.
    pub fn harden(&self) -> io::Result<()> {
        Ok(())
    }

    /// A pipe is destroyed with its last handle, so there is nothing to release.
    pub fn cleanup(&self) {}

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
}

impl Stream {
    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        let mut delay = Duration::from_millis(2);
        for _ in 0..CONNECT_ATTEMPTS {
            match ClientOptions::new().open(endpoint) {
                Ok(client) => return Ok(Self::Client(client)),
                // Every instance was busy while the server replaced the one it
                // just handed out, which clears by itself.
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(50));
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("named pipe {} stayed busy", endpoint.display()),
        ))
    }

    pub fn same_user(&self) -> io::Result<bool> {
        let handle = match self {
            Self::Server(pipe) => pipe.as_raw_handle(),
            Self::Client(pipe) => pipe.as_raw_handle(),
        };
        client_user_sid(handle as HANDLE).and_then(|client| {
            // The pseudo-handle GetCurrentProcess returns must not be closed.
            process_user_sid(unsafe { GetCurrentProcess() }).map(|daemon| client == daemon)
        })
    }
}

/// A SID is a self-contained byte structure, so two SIDs name the same account
/// exactly when their bytes match.
fn client_user_sid(pipe: HANDLE) -> io::Result<Vec<u8>> {
    let mut pid = 0u32;
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        // A process this one may not open cannot be proven to belong to the same
        // user, so the caller rejects it rather than guessing.
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    process_user_sid(process.0)
}

fn process_user_sid(process: HANDLE) -> io::Result<Vec<u8>> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut needed = 0u32;
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; needed as usize];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    // TOKEN_USER points at a SID inside the same buffer, so the SID is copied out
    // before the buffer is dropped.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let length = unsafe { GetLengthSid(user.User.Sid) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::slice::from_raw_parts(user.User.Sid.cast::<u8>(), length) }.to_vec())
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Server(pipe) => std::pin::Pin::new(pipe).poll_read(cx, buf),
            Self::Client(pipe) => std::pin::Pin::new(pipe).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Server(pipe) => std::pin::Pin::new(pipe).poll_write(cx, buf),
            Self::Client(pipe) => std::pin::Pin::new(pipe).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Server(pipe) => std::pin::Pin::new(pipe).poll_flush(cx),
            Self::Client(pipe) => std::pin::Pin::new(pipe).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Server(pipe) => std::pin::Pin::new(pipe).poll_shutdown(cx),
            Self::Client(pipe) => std::pin::Pin::new(pipe).poll_shutdown(cx),
        }
    }
}

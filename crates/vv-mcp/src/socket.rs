//! Attach mode: the editor serves MCP on a local socket, and
//! `vv-app mcp --attach` pipes a client's stdio to it. Only the user can open
//! the socket; there is no network port.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::McpHandle;

/// Where the editors put their sockets, one per process.
pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(runtime) => PathBuf::from(runtime).join("venturi"),
        None => std::env::temp_dir().join(format!(
            "venturi-{}",
            std::env::var("USER").unwrap_or_default()
        )),
    }
}

fn socket_name(pid: u32) -> String {
    format!("mcp-{pid}.sock")
}

fn pid_of(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("mcp-")?
        .strip_suffix(".sock")?
        .parse()
        .ok()
}

/// The sockets in `dir` with an editor behind them, by pid. The others are
/// left over by crashed editors and get removed.
#[cfg(unix)]
pub fn live_sockets(dir: &Path) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut live: Vec<(u32, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|path| Some((pid_of(&path)?, path)))
        .filter(|(_, path)| {
            let alive = std::os::unix::net::UnixStream::connect(path).is_ok();
            if !alive {
                let _ = std::fs::remove_file(path);
            }
            alive
        })
        .collect();
    live.sort();
    live
}

/// MCP served on a Unix socket until dropped; every connection is a client
/// session feeding the same `McpHandle`.
pub struct SocketServer {
    path: PathBuf,
    clients: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SocketServer {
    /// Serves on `socket_dir()`, named after this process.
    #[cfg(unix)]
    pub fn start(handle: McpHandle) -> std::io::Result<Self> {
        let dir = socket_dir();
        std::fs::create_dir_all(&dir)?;
        restrict(&dir, 0o700)?;
        live_sockets(&dir);
        Self::start_at(handle, dir.join(socket_name(std::process::id())))
    }

    #[cfg(unix)]
    pub fn start_at(handle: McpHandle, path: PathBuf) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        restrict(&path, 0o600)?;
        listener.set_nonblocking(true)?;
        let clients = Arc::new(AtomicUsize::new(0));
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let thread = std::thread::spawn({
            let clients = clients.clone();
            move || runtime.block_on(accept(listener, handle, clients, stop))
        });
        Ok(Self {
            path,
            clients,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    #[cfg(not(unix))]
    pub fn start(_handle: McpHandle) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "attaching to the editor is not supported on this system yet",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// MCP clients connected right now.
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::Relaxed)
    }
}

impl Drop for SocketServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(unix)]
async fn accept(
    listener: std::os::unix::net::UnixListener,
    handle: McpHandle,
    clients: Arc<AtomicUsize>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    use rmcp::ServiceExt;
    let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
        return;
    };
    loop {
        let stream = tokio::select! {
            _ = &mut stop => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
        };
        let server = crate::VenturiServer::new(handle.clone());
        let (clients, handle) = (clients.clone(), handle.clone());
        clients.fetch_add(1, Ordering::Relaxed);
        handle.notify();
        tokio::spawn(async move {
            if let Ok(service) = server.serve(stream).await {
                let _ = service.waiting().await;
            }
            clients.fetch_sub(1, Ordering::Relaxed);
            handle.notify();
        });
    }
}

/// `vv-app mcp --attach [--pid N]`: connects stdin/stdout to the socket of
/// a running editor, until either side closes.
#[cfg(unix)]
pub fn bridge_stdio(pid: Option<u32>) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let live = live_sockets(&socket_dir());
    let path = match (pid, live.as_slice()) {
        (Some(pid), _) => live
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, path)| path.clone())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("no Venturi window with pid {pid} is serving MCP"),
                )
            })?,
        (None, [(_, path)]) => path.clone(),
        (None, []) => {
            return Err(Error::new(
                ErrorKind::NotFound,
                "no Venturi window is serving MCP: enable it in Settings > Integrations, \
                 or start Venturi with --mcp",
            ));
        }
        (None, several) => {
            let pids: Vec<String> = several.iter().map(|(pid, _)| pid.to_string()).collect();
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "several Venturi windows serve MCP (pids {}): choose one with --pid",
                    pids.join(", ")
                ),
            ));
        }
    };
    let stream = std::os::unix::net::UnixStream::connect(&path)?;
    let mut to_socket = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_socket);
        let _ = to_socket.shutdown(std::net::Shutdown::Write);
    });
    let mut from_socket = stream;
    let mut stdout = std::io::stdout().lock();
    std::io::copy(&mut from_socket, &mut stdout)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn bridge_stdio(_pid: Option<u32>) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "attaching to the editor is not supported on this system yet",
    ))
}

#[cfg(test)]
#[path = "tests/socket.rs"]
mod tests;

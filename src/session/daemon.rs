//! Local process boundary for managed sessions. One length-prefixed JSON request
//! per connection; Attach and Watch turn that connection into a bounded stream.
//! Control and feedback use separate connections from terminal output.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::registry::{
    Feedback, History, Lifecycle, Output, ReceiveError, Registry, Sequenced, Snapshot, Spawn,
    Status, Subscription,
};
use super::Dims;

pub const VERSION: u32 = 1;
const MAX_REQUEST: usize = 128 * 1024;
const MAX_RESPONSE: usize = 128 * 1024 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Ping,
    Create(Spawn),
    List,
    Attach {
        id: String,
    },
    Watch {
        id: String,
    },
    History {
        id: String,
        sequence: u64,
        offset: usize,
        limit: usize,
    },
    Input {
        id: String,
        bytes: Vec<u8>,
    },
    Resize {
        id: String,
        dims: Dims,
    },
    Stop {
        id: String,
    },
    Forget {
        id: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    Ok,
    Error(String),
    Status(Status),
    Sessions(Vec<Status>),
    Snapshot(Box<Snapshot>),
    History(History),
    Watching { status: Status, sequence: u64 },
    Output(Sequenced<Output>),
    Feedback(Sequenced<Feedback>),
    ResyncRequired,
}

/// Socket directory is private even when the surrounding RADAR_HOME is shared.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("run").join("sessions.sock")
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    _lock: File,
    registry: Arc<Registry>,
    stopping: Arc<AtomicBool>,
}

impl Server {
    /// flock serializes startup and stale socket cleanup. Never unlink a live
    /// daemon's socket or stop it merely because its protocol version differs.
    pub fn bind(home: &Path) -> Result<Self> {
        let path = socket_path(home);
        let directory = path.parent().unwrap();
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("sessions.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("session daemon is already running");
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path).context("bind session socket")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path,
            _lock: lock,
            registry: Arc::new(Registry::default()),
            stopping: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn run(self) -> Result<()> {
        let mut workers = Vec::new();
        while !self.stopping.load(Ordering::Acquire) {
            workers.retain(|worker: &std::thread::JoinHandle<()>| !worker.is_finished());
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Local clients can still accidentally flood the daemon.
                    if workers.len() >= 128 {
                        drop(stream);
                        continue;
                    }
                    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
                    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
                    let registry = self.registry.clone();
                    let stopping = self.stopping.clone();
                    workers.push(std::thread::spawn(move || {
                        let mut stream = stream;
                        if let Err(error) = serve(&mut stream, registry, stopping) {
                            let _ = write_frame(
                                &mut stream,
                                &Response::Error(error.to_string()),
                                MAX_RESPONSE,
                            );
                        }
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.registry.stop_all();
        for worker in workers {
            let _ = worker.join();
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.registry.stop_all();
        let _ = fs::remove_file(&self.path);
    }
}

fn serve(
    stream: &mut UnixStream,
    registry: Arc<Registry>,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    let request: Request = read_frame(stream, MAX_REQUEST)?;
    if request.version != VERSION {
        bail!(
            "unsupported session protocol version {} (expected {VERSION})",
            request.version
        );
    }
    if stopping.load(Ordering::Acquire) {
        bail!("session daemon is shutting down");
    }
    let response = match request.command {
        Command::Ping => Response::Hello { version: VERSION },
        Command::Create(spec) => Response::Status(registry.create(spec)?.status()),
        Command::List => Response::Sessions(registry.list()),
        Command::History {
            id,
            sequence,
            offset,
            limit,
        } => Response::History(registry.get(&id)?.history(sequence, offset, limit)?),
        Command::Input { id, bytes } => {
            registry.get(&id)?.input(bytes)?;
            Response::Ok
        }
        Command::Resize { id, dims } => {
            registry.get(&id)?.resize(dims)?;
            Response::Ok
        }
        Command::Stop { id } => {
            registry.get(&id)?.stop();
            Response::Ok
        }
        Command::Forget { id } => {
            registry.forget(&id)?;
            Response::Ok
        }
        Command::Shutdown => {
            registry.stop_all();
            stopping.store(true, Ordering::Release);
            Response::Ok
        }
        Command::Attach { id } => {
            let (snapshot, subscription) = registry.get(&id)?.attach();
            let closed = snapshot.status.stream_closed;
            write_frame(
                stream,
                &Response::Snapshot(Box::new(snapshot)),
                MAX_RESPONSE,
            )?;
            if closed {
                return Ok(());
            }
            return stream_events(stream, subscription, stopping, Response::Output, |event| {
                matches!(event, Output::Closed)
            });
        }
        Command::Watch { id } => {
            let (status, sequence, subscription) = registry.get(&id)?.watch();
            let mut closed = status.stream_closed;
            let mut ended = !matches!(status.lifecycle, Lifecycle::Running);
            write_frame(
                stream,
                &Response::Watching { status, sequence },
                MAX_RESPONSE,
            )?;
            if closed && ended {
                return Ok(());
            }
            return stream_events(
                stream,
                subscription,
                stopping,
                Response::Feedback,
                move |event| {
                    match event {
                        Feedback::StreamClosed => closed = true,
                        Feedback::Lifecycle(lifecycle) => {
                            ended = !matches!(lifecycle, Lifecycle::Running)
                        }
                        _ => {}
                    }
                    closed && ended
                },
            );
        }
    };
    write_frame(stream, &response, MAX_RESPONSE)
}

fn stream_events<T>(
    stream: &mut UnixStream,
    subscription: Subscription<T>,
    stopping: Arc<AtomicBool>,
    wrap: impl Fn(Sequenced<T>) -> Response,
    mut finished: impl FnMut(&T) -> bool,
) -> Result<()> {
    while !stopping.load(Ordering::Acquire) {
        match subscription.try_recv() {
            Ok(event) => {
                let complete = finished(&event.event);
                write_frame(stream, &wrap(event), MAX_RESPONSE)?;
                if complete {
                    return Ok(());
                }
            }
            Err(ReceiveError::Empty) => {
                // Watch sockets must release their subscription on an idle
                // disconnect as well as on the next event.
                let mut byte = [0_u8];
                let n = unsafe {
                    libc::recv(
                        stream.as_raw_fd(),
                        byte.as_mut_ptr().cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                if n == 0 {
                    return Ok(());
                }
                if n > 0 {
                    bail!("stream connections are read-only after attachment");
                }
                let error = std::io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) {
                    return Err(error.into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(ReceiveError::ResyncRequired) => {
                write_frame(stream, &Response::ResyncRequired, MAX_RESPONSE)?;
                return Ok(());
            }
            Err(ReceiveError::Closed) => return Ok(()),
        }
    }
    Ok(())
}

/// A disconnected socket, truncated frame, sequence gap, or ResyncRequired all
/// require a new attachment. Never continue an old stream after any of them.
pub struct Client {
    stream: UnixStream,
    next_sequence: Option<u64>,
    failed: bool,
}

impl Client {
    pub fn connect(home: &Path, command: Command) -> Result<Self> {
        let mut stream = UnixStream::connect(socket_path(home))?;
        stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
        write_frame(
            &mut stream,
            &Request {
                version: VERSION,
                command,
            },
            MAX_REQUEST,
        )?;
        Ok(Self {
            stream,
            next_sequence: None,
            failed: false,
        })
    }

    pub fn receive(&mut self) -> Result<Response> {
        if self.failed {
            bail!("connection requires a fresh attachment");
        }
        let response = match self.receive_checked() {
            Ok(response) => response,
            Err(error) => {
                self.failed = true;
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                return Err(error);
            }
        };
        if matches!(response, Response::ResyncRequired) {
            self.failed = true;
        }
        Ok(response)
    }

    fn receive_checked(&mut self) -> Result<Response> {
        let response: Response = read_frame(&mut self.stream, MAX_RESPONSE)?;
        let sequence = match &response {
            Response::Error(message) => bail!("{message}"),
            Response::Snapshot(snapshot) => {
                self.next_sequence = Some(snapshot.sequence + 1);
                None
            }
            Response::Watching { sequence, .. } => {
                self.next_sequence = Some(sequence + 1);
                None
            }
            Response::Output(event) => Some(event.sequence),
            Response::Feedback(event) => Some(event.sequence),
            _ => None,
        };
        if let Some(sequence) = sequence {
            if self.next_sequence != Some(sequence) {
                bail!("session stream sequence gap; attach again");
            }
            self.next_sequence = Some(sequence + 1);
        }
        Ok(response)
    }

    pub fn request(home: &Path, command: Command) -> Result<Response> {
        Self::connect(home, command)?.receive()
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize, limit: usize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > limit {
        bail!("session frame exceeds {limit} bytes");
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}

fn read_frame<T: DeserializeOwned>(reader: &mut impl Read, limit: usize) -> Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        bail!("session frame exceeds {limit} bytes");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_gap_poisons_client_until_a_new_attachment() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut client = Client {
            stream: reader,
            next_sequence: Some(10),
            failed: false,
        };
        write_frame(
            &mut writer,
            &Response::Output(Sequenced {
                sequence: 11,
                event: Output::Bytes(vec![b'x']),
            }),
            MAX_RESPONSE,
        )
        .unwrap();
        assert!(client
            .receive()
            .unwrap_err()
            .to_string()
            .contains("sequence gap"));
        assert!(client
            .receive()
            .unwrap_err()
            .to_string()
            .contains("fresh attachment"));
    }

    #[test]
    fn truncated_frame_poisons_client_instead_of_reusing_partial_data() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut client = Client {
            stream: reader,
            next_sequence: None,
            failed: false,
        };
        writer.write_all(&100_u32.to_be_bytes()).unwrap();
        writer.write_all(b"partial").unwrap();
        drop(writer);
        assert!(client.receive().is_err());
        assert!(client
            .receive()
            .unwrap_err()
            .to_string()
            .contains("fresh attachment"));
    }

    #[test]
    fn incompatible_protocol_is_rejected_before_executing_a_command() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        write_frame(
            &mut client,
            &Request {
                version: VERSION + 1,
                command: Command::Shutdown,
            },
            MAX_REQUEST,
        )
        .unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let error =
            serve(&mut server, Arc::new(Registry::default()), stopping.clone()).unwrap_err();
        assert!(error.to_string().contains("unsupported session protocol"));
        assert!(!stopping.load(Ordering::Acquire));
    }

    #[test]
    fn shutdown_closes_registry_before_acknowledging() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        write_frame(
            &mut client,
            &Request {
                version: VERSION,
                command: Command::Shutdown,
            },
            MAX_REQUEST,
        )
        .unwrap();
        let registry = Arc::new(Registry::default());
        serve(
            &mut server,
            registry.clone(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(matches!(
            read_frame::<Response>(&mut client, MAX_RESPONSE).unwrap(),
            Response::Ok
        ));
        let result = registry.create(Spawn {
            id: "late".into(),
            argv: vec!["/bin/true".into()],
            cwd: std::env::current_dir().unwrap(),
            dims: Dims { cols: 80, rows: 24 },
            env: Vec::new(),
            env_remove: Vec::new(),
        });
        assert!(result.is_err());
        assert!(registry.list().is_empty());
    }
}

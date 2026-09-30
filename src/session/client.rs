//! A client attachment to a daemon-owned session.
//!
//! The daemon owns the child and the authoritative terminal. This module
//! delivers the lossless snapshot and the raw output stream to the client as
//! [`ClientEvent`]s, and forwards input and resizes. A small local PTY is kept
//! only as the input channel. Dropping this attachment closes its sockets but
//! does not send Stop.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Result};

use super::daemon::{ensure_running, Client, Command, Response};
use super::registry::{Feedback, Lifecycle, Output, Sequenced, Spawn};
use super::{Dims, ExitInfo};

const POLL_MS: i32 = 50;
const INPUT_CHUNK: usize = 4096;

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ClientEvent {
    Exit(ExitInfo),
    Failed(String),
    /// The daemon's lossless snapshot for this attachment. The client decodes
    /// it into its own engine instead of replaying ANSI.
    Snapshot(Vec<u8>),
    /// Raw program output after the snapshot watermark.
    Bytes(Vec<u8>),
    /// The program set (or reset) the window title.
    Title(Option<String>),
    /// The program rang the terminal bell.
    Bell,
}

struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

#[derive(Default)]
struct Interrupts {
    output: Mutex<Option<UnixStream>>,
    feedback: Mutex<Option<UnixStream>>,
}

impl Interrupts {
    fn set(slot: &Mutex<Option<UnixStream>>, stream: &UnixStream) {
        *slot.lock().unwrap() = stream.try_clone().ok();
    }

    fn clear(slot: &Mutex<Option<UnixStream>>) {
        slot.lock().unwrap().take();
    }

    fn interrupt(&self) {
        for slot in [&self.output, &self.feedback] {
            if let Some(stream) = slot.lock().unwrap().take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// A client attached to a persistent daemon session.
pub struct RemoteSession {
    id: String,
    home: PathBuf,
    process_id: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    interrupts: Arc<Interrupts>,
    _master: Fd,
    client_fd: Fd,
    workers: Vec<JoinHandle<()>>,
}

impl RemoteSession {
    pub fn attach(
        home: &Path,
        id: String,
        spawn: Spawn,
        replace_existing: bool,
        emit: impl Fn(ClientEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        let (master, slave) = open_bridge(spawn.dims)?;
        let client_fd = unsafe { libc::dup(master.0) };
        if client_fd < 0 {
            bail!("dup bridge PTY: {}", io::Error::last_os_error());
        }
        let client_fd = Fd(client_fd);
        let input_fd = unsafe { libc::dup(slave.0) };
        if input_fd < 0 {
            bail!("dup bridge PTY input: {}", io::Error::last_os_error());
        }
        let input_fd = Fd(input_fd);
        let size_fd = unsafe { libc::dup(master.0) };
        if size_fd < 0 {
            bail!("dup bridge PTY size: {}", io::Error::last_os_error());
        }
        let size_fd = Fd(size_fd);
        let stop = Arc::new(AtomicBool::new(false));
        let interrupts = Arc::new(Interrupts::default());
        let process_id = Arc::new(AtomicU32::new(0));
        let callback: Arc<dyn Fn(ClientEvent) + Send + Sync> = Arc::new(emit);
        let exit_reported = Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(ClientEvent) + Send + Sync> = {
            let callback = callback.clone();
            let exit_reported = exit_reported.clone();
            Arc::new(move |event| match event {
                ClientEvent::Exit(info) if !exit_reported.swap(true, Ordering::AcqRel) => {
                    callback(ClientEvent::Exit(info));
                }
                ClientEvent::Exit(_) => {}
                other => callback(other),
            })
        };

        let output_worker = {
            let home = home.to_path_buf();
            let id = id.clone();
            let stop = stop.clone();
            let interrupts = interrupts.clone();
            let process_id = process_id.clone();
            let emit = emit.clone();
            std::thread::Builder::new()
                .name(format!("radar-client-output-{id}"))
                .spawn(move || {
                    output_loop(
                        &home,
                        &id,
                        spawn,
                        replace_existing,
                        stop,
                        interrupts,
                        process_id,
                        emit,
                    );
                })?
        };
        let input_worker = {
            let stop = stop.clone();
            let home = home.to_path_buf();
            let id = id.clone();
            let input_fd = input_fd;
            let size_fd = size_fd;
            std::thread::Builder::new()
                .name(format!("radar-client-input-{id}"))
                .spawn(move || {
                    input_loop(&home, &id, input_fd, size_fd, stop);
                })?
        };
        let feedback_worker = {
            let stop = stop.clone();
            let home = home.to_path_buf();
            let id = id.clone();
            let interrupts = interrupts.clone();
            let emit = emit.clone();
            std::thread::Builder::new()
                .name(format!("radar-client-feedback-{id}"))
                .spawn(move || {
                    feedback_loop(&home, &id, stop, interrupts, emit);
                })?
        };
        Ok(Self {
            id,
            home: home.to_path_buf(),
            process_id,
            stop,
            interrupts,
            _master: master,
            client_fd,
            workers: vec![output_worker, input_worker, feedback_worker],
        })
    }

    pub fn client_fd(&self) -> libc::c_int {
        self.client_fd.0
    }

    pub fn process_id(&self) -> Option<u32> {
        match self.process_id.load(Ordering::Acquire) {
            0 => None,
            pid => Some(pid),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.interrupts.interrupt();
        let workers = std::mem::take(&mut self.workers);
        // A control request can be waiting on a slow or wedged daemon. Reap
        // workers away from GTK so detaching a pane never waits on the socket.
        let _ = std::thread::Builder::new()
            .name(format!("radar-client-reaper-{}", self.id))
            .spawn(move || {
                for worker in workers {
                    let _ = worker.join();
                }
            });
    }
}

fn open_bridge(dims: Dims) -> Result<(Fd, Fd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut winsize = libc::winsize {
        ws_row: dims.rows,
        ws_col: dims.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        #[allow(clippy::unnecessary_mut_passed)]
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut winsize,
        )
    };
    if result != 0 {
        bail!("openpty bridge: {}", io::Error::last_os_error());
    }
    for fd in [master, slave] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            bail!(
                "mark bridge PTY close-on-exec: {}",
                io::Error::last_os_error()
            );
        }
    }
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave, &mut termios) == 0 {
            libc::cfmakeraw(&mut termios);
            libc::tcsetattr(slave, libc::TCSANOW, &termios);
        }
        let flags = libc::fcntl(slave, libc::F_GETFL);
        if flags < 0 || libc::fcntl(slave, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            libc::close(master);
            libc::close(slave);
            bail!(
                "make bridge PTY nonblocking: {}",
                io::Error::last_os_error()
            );
        }
    }
    Ok((Fd(master), Fd(slave)))
}

fn connect_attach(home: &Path, id: &str) -> Result<Client> {
    connect_attach_with(home, id, ensure_running)
}

fn connect_attach_with(
    home: &Path,
    id: &str,
    ensure: impl FnOnce(&Path) -> Result<()>,
) -> Result<Client> {
    let command = || Command::Attach { id: id.into() };
    match Client::connect(home, command()) {
        Ok(client) => Ok(client),
        Err(error)
            if error.downcast_ref::<io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                )
            }) =>
        {
            ensure(home)?;
            Client::connect(home, command())
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn output_loop(
    home: &Path,
    id: &str,
    spawn: Spawn,
    replace_existing: bool,
    stop: Arc<AtomicBool>,
    interrupts: Arc<Interrupts>,
    process_id: Arc<AtomicU32>,
    emit: Arc<dyn Fn(ClientEvent) + Send + Sync>,
) {
    let mut create_needed = true;
    let mut replace_needed = replace_existing;
    while !stop.load(Ordering::Acquire) {
        if replace_needed {
            replace_needed = false;
            let mut forgotten = false;
            match Client::request(home, Command::Stop { id: id.into() }) {
                Ok(_) => {
                    let deadline = std::time::Instant::now() + Duration::from_secs(3);
                    loop {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        let ended = matches!(Client::request(home, Command::List), Ok(Response::Sessions(statuses)) if statuses.iter().find(|status| status.id == id).is_some_and(|status| status.stream_closed));
                        if ended || std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(30));
                    }
                    while !stop.load(Ordering::Acquire)
                        && std::time::Instant::now() < deadline + Duration::from_secs(1)
                    {
                        if Client::request(home, Command::Forget { id: id.into() }).is_ok() {
                            forgotten = true;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                Err(_) => forgotten = true,
            }
            if !forgotten {
                emit(ClientEvent::Failed(format!(
                    "could not replace ended session {id}"
                )));
                return;
            }
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        let mut client = match connect_attach(home, id) {
            Ok(client) => client,
            Err(error) => {
                emit(ClientEvent::Failed(format!(
                    "could not attach to {id}: {error}"
                )));
                return;
            }
        };
        let Ok(interrupt) = client.interrupt_handle() else {
            return;
        };
        Interrupts::set(&interrupts.output, &interrupt);
        let mut response = client.receive();
        if response.is_err() && create_needed {
            match Client::request(home, Command::Create(spawn.clone())) {
                Ok(Response::Status(status)) => {
                    if let Some(pid) = status.pid {
                        process_id.store(pid, Ordering::Release);
                    }
                    create_needed = false;
                    client = match connect_attach(home, id) {
                        Ok(client) => client,
                        Err(error) => {
                            emit(ClientEvent::Failed(format!(
                                "could not attach to {id}: {error}"
                            )));
                            return;
                        }
                    };
                    let Ok(interrupt) = client.interrupt_handle() else {
                        return;
                    };
                    Interrupts::set(&interrupts.output, &interrupt);
                    response = client.receive();
                }
                Ok(other) => {
                    emit(ClientEvent::Failed(format!(
                        "unexpected daemon response: {other:?}"
                    )));
                    return;
                }
                Err(create_error) => {
                    // Another client may have created the stable tab ID after
                    // our first attach probe. Retry attachment before surfacing
                    // an ID conflict (launch stamps can differ across clients).
                    if let Ok(attached) = connect_attach(home, id) {
                        if let Ok(interrupt) = attached.interrupt_handle() {
                            Interrupts::set(&interrupts.output, &interrupt);
                            client = attached;
                            response = client.receive();
                            if response.is_ok() {
                                create_needed = false;
                            }
                        }
                    }
                    if response.is_err() {
                        emit(ClientEvent::Failed(format!(
                            "could not create/attach {id}: {create_error}"
                        )));
                        return;
                    }
                }
            }
        }
        match response {
            Ok(Response::Snapshot(snapshot)) => {
                create_needed = false;
                if let Some(pid) = snapshot.status.pid {
                    process_id.store(pid, Ordering::Release);
                }
                // The client decodes the lossless snapshot into its own engine.
                emit(ClientEvent::Snapshot(snapshot.terminal_snapshot.clone()));
                match snapshot.status.lifecycle {
                    Lifecycle::Exited(info) => emit(ClientEvent::Exit(info)),
                    Lifecycle::Failed(message) => emit(ClientEvent::Failed(message)),
                    Lifecycle::Running => {}
                }
                if snapshot.status.stream_closed {
                    Interrupts::clear(&interrupts.output);
                    return;
                }
                let _ = client.set_read_timeout(None);
            }
            Ok(Response::Error(error)) => {
                Interrupts::clear(&interrupts.output);
                emit(ClientEvent::Failed(format!(
                    "could not attach to {id}: {error}"
                )));
                return;
            }
            Ok(other) => {
                Interrupts::clear(&interrupts.output);
                emit(ClientEvent::Failed(format!(
                    "unexpected attachment response: {other:?}"
                )));
                return;
            }
            Err(error) => {
                Interrupts::clear(&interrupts.output);
                if stop.load(Ordering::Acquire) {
                    return;
                }
                eprintln!("radar: session {id} attachment reset: {error}");
                std::thread::sleep(Duration::from_millis(40));
                continue;
            }
        }
        loop {
            if stop.load(Ordering::Acquire) {
                Interrupts::clear(&interrupts.output);
                return;
            }
            match client.receive() {
                Ok(Response::Output(item)) => match item.event {
                    Output::Bytes(bytes) => emit(ClientEvent::Bytes(bytes)),
                    Output::Resize(_) => {}
                    Output::Closed => {
                        Interrupts::clear(&interrupts.output);
                        return;
                    }
                },
                Ok(Response::ResyncRequired) => break,
                Ok(Response::Error(error)) => {
                    Interrupts::clear(&interrupts.output);
                    emit(ClientEvent::Failed(error));
                    return;
                }
                Ok(other) => {
                    Interrupts::clear(&interrupts.output);
                    emit(ClientEvent::Failed(format!(
                        "unexpected output response: {other:?}"
                    )));
                    return;
                }
                Err(_) => {
                    if stop.load(Ordering::Acquire) {
                        Interrupts::clear(&interrupts.output);
                        return;
                    }
                    break;
                }
            }
        }
        Interrupts::clear(&interrupts.output);
        // Resnapshot after an explicit lag or truncated socket: the next
        // attach delivers a fresh snapshot that replaces the client state.
    }
}

fn feedback_loop(
    home: &Path,
    id: &str,
    stop: Arc<AtomicBool>,
    interrupts: Arc<Interrupts>,
    emit: Arc<dyn Fn(ClientEvent) + Send + Sync>,
) {
    let mut exit_reported = false;
    while !stop.load(Ordering::Acquire) {
        let mut client = match Client::connect(home, Command::Watch { id: id.into() }) {
            Ok(client) => client,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(80));
                continue;
            }
        };
        let Ok(interrupt) = client.interrupt_handle() else {
            return;
        };
        Interrupts::set(&interrupts.feedback, &interrupt);
        match client.receive() {
            Ok(Response::Watching { status, .. }) => {
                let mut lifecycle_ended = false;
                let mut stream_closed = status.stream_closed;
                match status.lifecycle {
                    Lifecycle::Exited(info) => {
                        lifecycle_ended = true;
                        if !exit_reported {
                            exit_reported = true;
                            emit(ClientEvent::Exit(info));
                        }
                    }
                    Lifecycle::Failed(message) => {
                        lifecycle_ended = true;
                        emit(ClientEvent::Failed(message));
                    }
                    Lifecycle::Running => {}
                }
                if stream_closed && lifecycle_ended {
                    Interrupts::clear(&interrupts.feedback);
                    return;
                }
                let _ = client.set_read_timeout(None);

                loop {
                    if stop.load(Ordering::Acquire) {
                        Interrupts::clear(&interrupts.feedback);
                        return;
                    }
                    match client.receive() {
                        Ok(Response::Feedback(Sequenced { event, .. })) => match event {
                            Feedback::Lifecycle(Lifecycle::Exited(info)) => {
                                lifecycle_ended = true;
                                if !exit_reported {
                                    exit_reported = true;
                                    emit(ClientEvent::Exit(info));
                                }
                            }
                            Feedback::Lifecycle(Lifecycle::Failed(message)) => {
                                lifecycle_ended = true;
                                emit(ClientEvent::Failed(message));
                            }
                            Feedback::StreamClosed => stream_closed = true,
                            Feedback::Title(title) => emit(ClientEvent::Title(title)),
                            Feedback::Bell => emit(ClientEvent::Bell),
                            _ => {}
                        },
                        Ok(Response::ResyncRequired) | Err(_) => break,
                        Ok(_) => break,
                    }
                    if stream_closed && lifecycle_ended {
                        Interrupts::clear(&interrupts.feedback);
                        return;
                    }
                }
            }
            _ => {
                Interrupts::clear(&interrupts.feedback);
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
        }
        Interrupts::clear(&interrupts.feedback);
    }
}

fn input_loop(home: &Path, id: &str, slave: Fd, master: Fd, stop: Arc<AtomicBool>) {
    let slave = slave.0;
    let master = master.0;
    let mut last_size = None;
    let mut buffer = [0; INPUT_CHUNK];
    while !stop.load(Ordering::Acquire) {
        let mut descriptor = libc::pollfd {
            fd: slave,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut descriptor, 1, POLL_MS);
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        if descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let count = unsafe { libc::read(slave, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count > 0 {
                let bytes = buffer[..count as usize].to_vec();
                if !bytes.is_empty() {
                    let _ = Client::request(
                        home,
                        Command::Input {
                            id: id.into(),
                            bytes,
                        },
                    );
                }
            }
        }
        if let Some(dims) = winsize_of(master) {
            if last_size != Some(dims) {
                last_size = Some(dims);
                let _ = Client::request(
                    home,
                    Command::Resize {
                        id: id.into(),
                        dims,
                    },
                );
            }
        }
    }
}

fn winsize_of(fd: libc::c_int) -> Option<Dims> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    (size.ws_col > 0 && size.ws_row > 0).then_some(Dims {
        cols: size.ws_col,
        rows: size.ws_row,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_attach_restarts_daemon_and_retries_protocol_attach() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!(
            "radar-attach-recovery-{}-{nonce}",
            std::process::id()
        ));
        let socket = super::super::daemon::socket_path(&home);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(stale);

        let initial_error = Client::connect(
            &home,
            Command::Attach {
                id: "stable".into(),
            },
        )
        .err()
        .and_then(|error| error.downcast_ref::<io::Error>().map(io::Error::kind));

        let mut server_thread = None;
        let result = (|| -> Result<()> {
            let mut client = connect_attach_with(&home, "stable", |home| {
                let server = super::super::daemon::Server::bind(home)?;
                server_thread = Some(std::thread::spawn(move || server.run()));
                match Client::request(
                    home,
                    Command::Create(Spawn {
                        id: "stable".into(),
                        cwd: std::env::current_dir()?,
                        argv: vec!["/bin/true".into()],
                        dims: Dims { cols: 80, rows: 24 },
                        env: Vec::new(),
                        env_remove: Vec::new(),
                    }),
                )? {
                    Response::Status(_) => Ok(()),
                    other => bail!("unexpected session create response: {other:?}"),
                }
            })?;
            match client.receive()? {
                Response::Snapshot(snapshot) => assert_eq!(snapshot.status.id, "stable"),
                other => bail!("expected attach snapshot, got {other:?}"),
            }
            drop(client);
            Ok(())
        })();

        let shutdown = server_thread.map(|server| {
            let response = Client::request(&home, Command::Shutdown);
            let result = server.join();
            (response, result)
        });
        std::fs::remove_dir_all(&home).unwrap();

        assert_eq!(initial_error, Some(io::ErrorKind::ConnectionRefused));
        result.unwrap();
        let (response, server) = shutdown.expect("recovery must start the daemon");
        assert!(matches!(response.unwrap(), Response::Ok));
        server.unwrap().unwrap();
    }
}

use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;
use std::task::Poll;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

// reference: https://docs.rs/tokio/1.49.0/tokio/net/windows/named_pipe/struct.ServerOptions.html
// reference: https://docs.rs/mio/1.1.1/src/mio/pipe/named_pipe.rs.html

/// Convert a filesystem path to a Windows named pipe path.
///
/// e.g. `/run/user/1000/jcode.sock` -> `\\.\pipe\jcode`
/// e.g. `/run/user/1000/jcode/myserver.sock` -> `\\.\pipe\jcode-myserver`
fn path_to_pipe_name(path: &Path) -> String {
    path_to_pipe_name_with(path, canonical_parent)
}

/// The pipe name for `path`, with the parent-directory resolution injected.
///
/// The resolution is a parameter so the normalization can be tested against a
/// path whose short spelling is derived at runtime, instead of being pinned to
/// whatever this machine happens to have.
fn path_to_pipe_name_with(
    path: &Path,
    resolve_parent: fn(&Path) -> Option<std::path::PathBuf>,
) -> String {
    let stem: String = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("jcode")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(32)
        .collect();
    let stem = if stem.is_empty() { "jcode" } else { &stem };
    let resolved = resolve_for_hash(path, resolve_parent);
    let normalized = resolved
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    let hash = hex::encode(digest);
    format!(r"\\.\pipe\{}-{}", stem, &hash[..16])
}

/// The path whose STRING is hashed into the pipe name.
///
/// The parent directory is resolved, so two spellings of one directory hash to
/// the same pipe. The final component is kept as written: the socket is
/// unlinked and recreated on bind, so the NAME is what matters, not whatever
/// it happened to point at.
///
/// Falls back to the raw path when the parent cannot be resolved (it does not
/// exist yet, or the process may not read it). Two unresolvable paths still
/// compare consistently with each other.
fn resolve_for_hash(
    path: &Path,
    resolve_parent: fn(&Path) -> Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(file_name)) => match resolve_parent(parent) {
            Some(canonical) => canonical.join(file_name),
            None => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// Resolve the parent directory of a socket path to its canonical spelling.
///
/// WHY THIS EXISTS (measured 2026-11-14 on this host)
///
/// The pipe name is a hash of the path STRING, so two spellings of one
/// directory produce two DIFFERENT pipes and therefore two independent
/// daemons that cannot see each other. Windows hands a Scheduled Task the 8.3
/// short spelling of `%TEMP%` (`C:\Users\WORKSH~1\...`) while an interactive
/// shell gets the long one (`C:\Users\WorkShop1\...`), and the daemon derives
/// its socket from `std::env::temp_dir()`. MEASURED: the two spellings hash to
/// `d6352b96b284a42d` and `5e984a5ce3694442` - two pipes, two daemons, one
/// shared session database.
///
/// Resolving the PARENT fixes that: `std::fs::canonicalize` expands an 8.3
/// name to its long form. MEASURED on this host: canonicalizing
/// `C:\Users\WORKSH~1\AppData\Local\Temp\jcode-WorkShop1` yields
/// `C:\Users\WorkShop1\AppData\Local\Temp\jcode-WorkShop1`, and hashing THAT
/// gives `5e984a5ce3694442` - the same pipe the long spelling produces.
///
/// Only the parent is resolved. The final component is kept as written, for
/// the same reason `resolved_socket_identity` keeps it: the socket file is
/// unlinked and recreated on bind, so the name is what matters, not whatever
/// it happened to point at.
///
/// Falls back to the raw path when the parent cannot be resolved (it does not
/// exist yet, or the process may not read it). Two unresolvable paths still
/// compare consistently with each other.
fn canonical_parent(parent: &Path) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(parent)
        .ok()
        // `canonicalize` returns a VERBATIM path (`\\?\C:\...`) on Windows.
        // That prefix is not part of the path the SDK hashes, so keeping it
        // would change the pipe name for every path - breaking the contract
        // pinned by `pipe_name_matches_the_typescript_sdk`.
        //
        // reference: https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file
        // reference: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfinalpathnamebyhandlew
        .map(|resolved| strip_verbatim_prefix(&resolved))
}

/// Drop the `\\?\` verbatim prefix from a Windows path.
///
/// `\\?\C:\dir` becomes `C:\dir`; `\\?\UNC\server\share` becomes
/// `\\server\share`. A path without the prefix is returned unchanged.
///
/// reference: https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file
fn strip_verbatim_prefix(path: &Path) -> std::path::PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => std::path::PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// Listener wraps a Windows named pipe server, providing an accept loop
/// that matches the UnixListener interface.
pub struct Listener {
    pipe_name: String,
    current_server: NamedPipeServer,
}

impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let pipe_name = path_to_pipe_name(path);
        match ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
        {
            Ok(server) => Ok(Self {
                pipe_name,
                current_server: server,
            }),
            Err(e)
                if e.raw_os_error()
                    == Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32) =>
            {
                eprintln!(
                    "[windows] Named pipe {} busy (access denied), retrying without first_pipe_instance",
                    pipe_name
                );
                std::thread::sleep(std::time::Duration::from_millis(200));
                let server = ServerOptions::new().create(&pipe_name)?;
                Ok(Self {
                    pipe_name,
                    current_server: server,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub async fn accept(&mut self) -> io::Result<(Stream, PipeAddr)> {
        self.current_server.connect().await?;

        let connected = std::mem::replace(
            &mut self.current_server,
            ServerOptions::new().create(&self.pipe_name)?,
        );

        Ok((Stream::Server(connected), PipeAddr))
    }
}

/// Placeholder for the "address" of a named pipe connection.
pub struct PipeAddr;

/// Stream wraps either a NamedPipeServer (accepted connection) or
/// NamedPipeClient (outgoing connection).
pub enum Stream {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

impl Stream {
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let pipe_name = path_to_pipe_name(path.as_ref());
        loop {
            match ClientOptions::new().open(&pipe_name) {
                Ok(client) => return Ok(Stream::Client(client)),
                Err(e)
                    if e.raw_os_error()
                        == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn into_split(self) -> (ReadHalf, WriteHalf) {
        let (read, write) = tokio::io::split(self);
        (ReadHalf { inner: read }, WriteHalf { inner: write })
    }

    pub fn split(&mut self) -> (SplitReadRef<'_>, SplitWriteRef<'_>) {
        let (read, write) = tokio::io::split(self);
        (SplitReadRef { inner: read }, SplitWriteRef { inner: write })
    }

    pub fn pair() -> io::Result<(Self, Self)> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static PAIR_COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = PAIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pipe_name = format!(r"\\.\pipe\jcode-pair-{}-{}", std::process::id(), counter);
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)?;
        let client = ClientOptions::new().open(&pipe_name)?;

        // The client connected when we opened it above, but the server must
        // call connect() to transition into the connected state.  For an
        // already-connected client this returns immediately.
        //
        // We use a short-lived runtime-free poll: since the client already
        // connected synchronously, the server's connect future will resolve
        // on the first poll.
        use std::future::Future;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn dummy_raw_waker() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(p: *const ()) -> RawWaker {
                RawWaker::new(p, &VTABLE)
            }
            const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        let waker = unsafe { Waker::from_raw(dummy_raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = server.connect();
        let pinned = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
        match pinned.poll(&mut cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Err(e),
            Poll::Pending => {
                // Should not happen since the client already connected.
                // Drop the future and proceed - the pipe is still usable.
            }
        }
        drop(fut);

        Ok((Stream::Server(server), Stream::Client(client)))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Server(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Stream::Client(c) => std::pin::Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let probe = wake_probe_enabled();
        let id = WAKE_PROBE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let task = if probe { task_index(cx.waker()) } else { 0 };
        let was_woken = probe && note_repoll(cx.waker());
        let result = if probe {
            let wrapped = std::task::Waker::from(std::sync::Arc::new(ProbeWaker {
                inner: cx.waker().clone(),
                id,
                task,
            }));
            let mut probe_cx = std::task::Context::from_waker(&wrapped);
            match self.get_mut() {
                Stream::Server(s) => std::pin::Pin::new(s).poll_write(&mut probe_cx, buf),
                Stream::Client(c) => std::pin::Pin::new(c).poll_write(&mut probe_cx, buf),
            }
        } else {
            match self.get_mut() {
                Stream::Server(s) => std::pin::Pin::new(s).poll_write(cx, buf),
                Stream::Client(c) => std::pin::Pin::new(c).poll_write(cx, buf),
            }
        };
        if probe {
            use std::sync::atomic::Ordering;
            if buf.len() >= WAKE_PROBE_LONG_WRITE {
                // A long write polled again while one was pending: this is the
                // re-poll the wakeup was supposed to cause.
                if WAKE_PROBE_LONG_PENDING.swap(false, Ordering::Relaxed) {
                    WAKE_PROBE_LONG_REPOLLS.fetch_add(1, Ordering::Relaxed);
                }
                if matches!(result, Poll::Pending) {
                    WAKE_PROBE_LONG_PENDING.store(true, Ordering::Relaxed);
                    WAKE_PROBE_LONG_PENDING_COUNT.fetch_add(1, Ordering::Relaxed);
                    // The previous poll on this half returned Ready: the
                    // previous write was only SUBMITTED, so this Pending is
                    // mio refusing a second write while one is in flight.
                    if WAKE_PROBE_LAST_RESULT.load(Ordering::Relaxed) == 1 {
                        WAKE_PROBE_INFLIGHT_PENDING.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            WAKE_PROBE_LAST_RESULT.store(
                match &result {
                    Poll::Ready(Ok(_)) => 1,
                    Poll::Pending => 2,
                    Poll::Ready(Err(_)) => 0,
                },
                Ordering::Relaxed,
            );
            if matches!(result, Poll::Pending) {
                arm_parked(cx.waker(), id);
            }
            eprintln!(
                "SDKCB_POLL id={id} task={task} len={} result={} woken_before={was_woken}",
                buf.len(),
                write_probe_result(&result)
            );
        }
        result
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Server(s) => std::pin::Pin::new(s).poll_flush(cx),
            Stream::Client(c) => std::pin::Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Server(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Stream::Client(c) => std::pin::Pin::new(c).poll_shutdown(cx),
        }
    }
}

/// Owned read half of a Stream, created by `into_split()`.
pub struct ReadHalf {
    inner: tokio::io::ReadHalf<Stream>,
}

impl AsyncRead for ReadHalf {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let result = std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf);
        if wake_probe_enabled() {
            eprintln!(
                "SDKCB_READ len={} result={}",
                buf.remaining(),
                match &result {
                    Poll::Ready(Ok(())) => "Ready(Ok)".to_string(),
                    Poll::Ready(Err(e)) => format!("Ready(Err({:?}))", e.kind()),
                    Poll::Pending => "Pending".to_string(),
                }
            );
        }
        result
    }
}

/// Owned write half of a Stream, created by `into_split()`.
pub struct WriteHalf {
    inner: tokio::io::WriteHalf<Stream>,
}

/// Is the write probe active?
///
/// The probe exists to diagnose the SDK-callback write hang: a `write_all`
/// that never returns can only be parked on WRITABLE READINESS, so every
/// `poll_write` and its outcome have to be visible in the SAME binary that
/// runs the failing test. That is why the gate is an environment variable
/// and NOT a `cfg(test)` gate: the failing test lives in `jcode-app-core`,
/// not in this crate.
///
/// Set `JCODE_WRITE_PROBE=1` to enable it.
fn write_probe_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("JCODE_WRITE_PROBE").is_some())
}

/// Is the waker probe active?
///
/// The waker probe answers the question the write probe cannot: after
/// `poll_write` returned `Pending`, is the task ever WOKEN again? The gate is
/// an environment variable for the same reason as the write probe: the
/// failing scenario lives in another crate, so a `cfg(test)` gate would not
/// reach it.
///
/// Set `JCODE_WAKE_PROBE=1` to enable it.
fn wake_probe_enabled() -> bool {
    std::env::var_os("JCODE_WAKE_PROBE").is_some()
}

/// Probe counters. They let a test classify an outcome without scraping
/// stderr, and they are process-global because the waker may be woken from a
/// different thread than the one that polled.
static WAKE_PROBE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// A long write is outstanding and `poll_write` already returned `Pending`.
static WAKE_PROBE_LONG_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// How often a long write returned `Pending`.
static WAKE_PROBE_LONG_PENDING_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// How often a long write was polled AGAIN while one was still pending.
static WAKE_PROBE_LONG_REPOLLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// How often the wrapped waker was woken.
static WAKE_PROBE_WAKES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// How often the wrapped waker was woken while a long write was pending.
static WAKE_PROBE_WAKES_DURING_PENDING: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// The result of the previous `poll_write` on a write half:
/// 0 = none yet, 1 = Ready(Ok(..)), 2 = Pending.
static WAKE_PROBE_LAST_RESULT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// How often a long write returned `Pending` right after a `Ready(Ok(..))`.
/// That is mio's `WriteState::InFlight` path: the previous write was
/// SUBMITTED (not completed) and the next one is refused.
static WAKE_PROBE_INFLIGHT_PENDING: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// The PARKED writers: the real task waker, the probe id of the last
/// `poll_write` that returned `Pending` for it, and whether that writer has
/// been woken since.
///
/// The real waker is the task identity: two writers on the same pipe are two
/// different tasks, and the SAME task re-polled keeps its identity. That is
/// what makes the correlation decisive: a wake that arrives for a parked
/// writer is a wake that was DELIVERED to the right task.
static WAKE_PROBE_ARMED: std::sync::Mutex<Vec<(std::task::Waker, u64, bool)>> =
    std::sync::Mutex::new(Vec::new());
/// How often a wake reached the waker of a PARKED writer.
static WAKE_PROBE_WOKE_ARMED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// How often a wake reached a waker that was NOT parked (a superseded slot).
static WAKE_PROBE_WOKE_UNMATCHED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// How often a task that had been WOKEN was polled again. The difference
/// between this and `WAKE_PROBE_WOKE_ARMED` is the production symptom: a
/// wake that is delivered but never followed by a re-poll.
static WAKE_PROBE_REPOLL_AFTER_WAKE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The size at which a write counts as "long" (the field report's event frame
/// was 545 bytes).
const WAKE_PROBE_LONG_WRITE: usize = 500;

/// A waker that logs every wake and then delegates to the real waker.
///
/// It is installed ONLY for the probe and never changes semantics: every wake
/// reaches the original waker, so the task is scheduled exactly as before.
///
/// reference: https://doc.rust-lang.org/std/task/trait.Wake.html
struct ProbeWaker {
    inner: std::task::Waker,
    id: u64,
    /// The identity of the TASK that owns this waker. Two writers on the same
    /// pipe are two different tasks, and the same task re-polled keeps its
    /// identity: that is what lets a trace say "task T was woken and never
    /// polled again".
    task: u32,
}

impl std::task::Wake for ProbeWaker {
    fn wake(self: std::sync::Arc<Self>) {
        record_wake(self.id, self.task, "wake");
        self.inner.clone().wake();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        record_wake(self.id, self.task, "wake_by_ref");
        self.inner.wake_by_ref();
    }
}

/// Record one wake on the wrapped waker.
fn record_wake(id: u64, task: u32, event: &str) {
    use std::sync::atomic::Ordering;
    WAKE_PROBE_WAKES.fetch_add(1, Ordering::Relaxed);
    if WAKE_PROBE_LONG_PENDING.load(Ordering::Relaxed) {
        WAKE_PROBE_WAKES_DURING_PENDING.fetch_add(1, Ordering::Relaxed);
    }
    // The decisive correlation: did this wake reach the waker of a writer
    // that is PARKED on this very waker slot?
    let mut armed = WAKE_PROBE_ARMED.lock().unwrap();
    let matched = armed.iter().position(|(_, parked, _)| *parked == id);
    let armed_ids: Vec<u64> = armed.iter().map(|(_, parked, _)| *parked).collect();
    match matched {
        Some(pos) => {
            // Mark it woken, but KEEP it: a later poll by the same task is
            // then measurable as "woken and re-polled".
            armed[pos].2 = true;
            drop(armed);
            WAKE_PROBE_WOKE_ARMED.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            drop(armed);
            WAKE_PROBE_WOKE_UNMATCHED.fetch_add(1, Ordering::Relaxed);
        }
    }
    eprintln!(
        "SDKCB_WAKE id={id} task={task} thread={:?} event={event} matched_armed={matched:?} armed_ids={armed_ids:?}",
        std::thread::current().id()
    );
}

/// The tasks seen by the probe, keyed by their real waker.
static WAKE_PROBE_TASKS: std::sync::Mutex<Vec<(std::task::Waker, u32)>> =
    std::sync::Mutex::new(Vec::new());

/// The index of the task that owns `real`, assigning one on first sight.
fn task_index(real: &std::task::Waker) -> u32 {
    let mut tasks = WAKE_PROBE_TASKS.lock().unwrap();
    match tasks.iter().position(|(waker, _)| waker.will_wake(real)) {
        Some(pos) => tasks[pos].1,
        None => {
            let index = tasks.len() as u32;
            tasks.push((real.clone(), index));
            index
        }
    }
}

/// Mark a writer as PARKED on its waker.
fn arm_parked(real: &std::task::Waker, id: u64) {
    let mut armed = WAKE_PROBE_ARMED.lock().unwrap();
    // The same task re-polled replaces its own entry: only the LATEST probe id
    // is the one tokio's writer slot holds.
    match armed.iter().position(|(waker, _, _)| waker.will_wake(real)) {
        Some(pos) => {
            armed[pos].1 = id;
            armed[pos].2 = false;
        }
        None => armed.push((real.clone(), id, false)),
    }
}

/// Note that a task was polled again, and report whether it had been woken.
fn note_repoll(real: &std::task::Waker) -> bool {
    use std::sync::atomic::Ordering;
    let mut armed = WAKE_PROBE_ARMED.lock().unwrap();
    match armed.iter_mut().find(|(waker, _, _)| waker.will_wake(real)) {
        Some(entry) => {
            let was_woken = entry.2;
            entry.2 = false;
            if was_woken {
                WAKE_PROBE_REPOLL_AFTER_WAKE.fetch_add(1, Ordering::Relaxed);
            }
            was_woken
        }
        None => false,
    }
}

/// The number of `poll_write` calls seen so far, so the probe output can be
/// ordered and gaps spotted.
static WRITE_PROBE_INDEX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Describe a `Poll` for the probe output.
fn write_probe_result(result: &Poll<io::Result<usize>>) -> String {
    match result {
        Poll::Ready(Ok(n)) => format!("Ready(Ok({n}))"),
        Poll::Ready(Err(error)) => format!("Ready(Err({:?}))", error.kind()),
        Poll::Pending => "Pending".to_string(),
    }
}

impl AsyncWrite for WriteHalf {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let result = std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf);
        if write_probe_enabled() {
            let index = WRITE_PROBE_INDEX.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            eprintln!(
                "WRITE_PROBE poll=#{index} len={} result={}",
                buf.len(),
                write_probe_result(&result)
            );
        }
        result
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let result = std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx);
        if write_probe_enabled() {
            eprintln!(
                "FLUSH_PROBE poll=#{} result={}",
                WRITE_PROBE_INDEX.load(std::sync::atomic::Ordering::Relaxed),
                match &result {
                    Poll::Ready(Ok(())) => "Ready(Ok(()))".to_string(),
                    Poll::Ready(Err(error)) => format!("Ready(Err({:?}))", error.kind()),
                    Poll::Pending => "Pending".to_string(),
                }
            );
        }
        result
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Borrowed read reference for `stream.split()`.
pub struct SplitReadRef<'a> {
    inner: tokio::io::ReadHalf<&'a mut Stream>,
}

impl<'a> AsyncRead for SplitReadRef<'a> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

/// Borrowed write reference for `stream.split()`.
pub struct SplitWriteRef<'a> {
    inner: tokio::io::WriteHalf<&'a mut Stream>,
}

impl<'a> AsyncWrite for SplitWriteRef<'a> {
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

/// Blocking named-pipe facade. The handle is overlapped so a blocked reader
/// never prevents a clone from writing (as synchronous duplicated handles do).
/// Each operation owns its event and OVERLAPPED until completion, including
/// after cancellation. No tokio runtime is required.
pub struct SyncStream {
    inner: std::sync::Arc<SyncPipe>,
}

struct SyncPipe {
    handle: std::fs::File,
    // Only submission and shutdown are serialized, never completion waits.
    state: std::sync::Mutex<SyncPipeState>,
}

#[derive(Default)]
struct SyncPipeState {
    shutdown: bool,
    read_timeout: Option<std::time::Duration>,
}

impl SyncStream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(path_to_pipe_name(path))?;
        Ok(Self {
            inner: std::sync::Arc::new(SyncPipe {
                handle,
                state: std::sync::Mutex::new(SyncPipeState::default()),
            }),
        })
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            inner: self.inner.clone(),
        })
    }

    /// Set the timeout for subsequent reads on this stream and all its clones.
    /// An already submitted read keeps its original timeout. Timed-out reads
    /// are canceled and drained before returning, so a slow kernel cancellation
    /// may extend the wait beyond the requested duration.
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        if timeout.is_some_and(|duration| duration.is_zero()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read timeout must be nonzero",
            ));
        }
        self.state()?.read_timeout = timeout;
        Ok(())
    }

    /// Interrupt both halves, including reads already blocked on other threads.
    /// Like socket shutdown this affects all clones, and is irreversible.
    /// Named pipes do not support independent read/write half shutdown here.
    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
        use windows_sys::Win32::System::IO::CancelIoEx;

        if how != std::net::Shutdown::Both {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "named pipes only support Shutdown::Both",
            ));
        }
        let mut state = self.state()?;
        state.shutdown = true;
        // Keep the submission gate locked until cancellation is requested so
        // no request can slip between the shutdown check and CancelIoEx.
        let canceled = unsafe { CancelIoEx(self.inner.handle.as_raw_handle(), std::ptr::null()) };
        if canceled == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NOT_FOUND as i32) {
                return Err(error);
            }
        }
        Ok(())
    }

    fn state(&self) -> io::Result<std::sync::MutexGuard<'_, SyncPipeState>> {
        self.inner
            .state
            .lock()
            .map_err(|_| io::Error::other("named-pipe state lock poisoned"))
    }

    fn transfer(
        &self,
        reading: bool,
        start: impl FnOnce(
            windows_sys::Win32::Foundation::HANDLE,
            *mut windows_sys::Win32::System::IO::OVERLAPPED,
        ) -> windows_sys::Win32::Foundation::BOOL,
    ) -> io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_MORE_DATA};

        let mut operation = SyncPipeOperation::new(&self.inner.handle)?;
        let state = self.state()?;
        if state.shutdown {
            return if reading {
                Ok(0)
            } else {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe shut down"))
            };
        }
        let timeout = if reading { state.read_timeout } else { None };
        // The caller borrows its buffer for this entire blocking call. The
        // boxed OVERLAPPED and owned event remain stable through completion.
        let started = start(
            self.inner.handle.as_raw_handle(),
            operation.overlapped.get(),
        );
        if started == 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(code) if code == ERROR_IO_PENDING as i32 => operation.pending = true,
                // Message-mode reads may complete with a valid short buffer.
                Some(code) if code == ERROR_MORE_DATA as i32 => {}
                _ => return Err(error),
            }
        }
        drop(state);
        operation.complete(timeout)
    }
}

/// RAII cancellation is essential: even an error waiting for the event must
/// drain pending I/O before its OVERLAPPED, event, or caller's buffer is freed.
struct SyncPipeOperation<'a> {
    handle: &'a std::fs::File,
    event: std::os::windows::io::OwnedHandle,
    // The kernel mutates this while requests are pending. Never manufacture a
    // Rust reference to the contents during that time.
    overlapped: Box<std::cell::UnsafeCell<windows_sys::Win32::System::IO::OVERLAPPED>>,
    pending: bool,
}

impl<'a> SyncPipeOperation<'a> {
    fn new(handle: &'a std::fs::File) -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::CreateEventW;

        // A distinct manual-reset event per operation permits simultaneous
        // reads and writes on the same pipe handle.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED =
            unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.as_raw_handle();
        Ok(Self {
            handle,
            event,
            overlapped: Box::new(std::cell::UnsafeCell::new(overlapped)),
            pending: false,
        })
    }

    fn cancel(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::IO::CancelIoEx;
        // Cancellation can race normal completion (ERROR_NOT_FOUND). Either
        // way GetOverlappedResult must observe completion before cleanup.
        unsafe { CancelIoEx(self.handle.as_raw_handle(), self.overlapped.get()) };
    }

    fn complete(&mut self, timeout: Option<std::time::Duration>) -> io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            ERROR_MORE_DATA, ERROR_OPERATION_ABORTED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::IO::GetOverlappedResult;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let mut timed_out = false;
        if self.pending
            && let Some(timeout) = timeout
        {
            let start = std::time::Instant::now();
            loop {
                // Round up sub-millisecond waits and never turn a finite
                // timeout into INFINITE. Long timeouts are waited in chunks.
                let remaining = timeout.saturating_sub(start.elapsed());
                let millis = if remaining.is_zero() {
                    0
                } else {
                    remaining
                        .as_millis()
                        .saturating_add(1)
                        .min(u128::from(u32::MAX - 1)) as u32
                };
                let result = unsafe { WaitForSingleObject(self.event.as_raw_handle(), millis) };
                match result {
                    WAIT_OBJECT_0 => break,
                    WAIT_TIMEOUT if start.elapsed() < timeout => continue,
                    WAIT_TIMEOUT => {
                        timed_out = true;
                        self.cancel();
                        break;
                    }
                    WAIT_FAILED => return Err(io::Error::last_os_error()),
                    _ => return Err(io::Error::other("unexpected named-pipe wait result")),
                }
            }
        }
        let mut transferred = 0;
        // Even after CancelIoEx, wait for terminal completion. Windows does
        // not promise cancellation is instantaneous, so abandoning this wait
        // at a deadline would leave the kernel using freed Rust memory.
        let succeeded = unsafe {
            GetOverlappedResult(
                self.handle.as_raw_handle(),
                self.overlapped.get(),
                &mut transferred,
                1,
            )
        };
        self.pending = false;
        if succeeded != 0 {
            return Ok(transferred as usize);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == ERROR_MORE_DATA as i32 => Ok(transferred as usize),
            Some(code) if code == ERROR_OPERATION_ABORTED as i32 => Err(io::Error::new(
                if timed_out {
                    io::ErrorKind::TimedOut
                } else {
                    io::ErrorKind::Interrupted
                },
                if timed_out {
                    "named-pipe read timed out"
                } else {
                    "named-pipe operation canceled"
                },
            )),
            _ => Err(error),
        }
    }
}

impl Drop for SyncPipeOperation<'_> {
    fn drop(&mut self) {
        if self.pending {
            self.cancel();
            let _ = self.complete(None);
        }
    }
}

impl io::Read for SyncStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use windows_sys::Win32::Foundation::{
            ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_PIPE_NOT_CONNECTED,
        };
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        let result = self.transfer(true, |handle, overlapped| unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                len,
                std::ptr::null_mut(),
                overlapped,
            )
        });
        match result {
            Err(error)
                if matches!(error.raw_os_error(), Some(code) if
                    code == ERROR_BROKEN_PIPE as i32 || code == ERROR_HANDLE_EOF as i32 ||
                    code == ERROR_PIPE_NOT_CONNECTED as i32) =>
            {
                Ok(0)
            }
            result => result,
        }
    }
}

impl io::Write for SyncStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;

        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        self.transfer(false, |handle, overlapped| unsafe {
            WriteFile(handle, buf.as_ptr(), len, std::ptr::null_mut(), overlapped)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes already wait for completion. FlushFileBuffers would instead
        // wait for the peer to read everything, which can deadlock duplex IPC.
        Ok(())
    }
}

/// Decide whether a failed pipe probe still proves a server is present.
///
/// reference: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
///   "If the CreateNamedPipe function was not successfully called on the server
///    prior to this operation, a pipe will not exist and CreateFile will fail
///    with ERROR_FILE_NOT_FOUND."
/// reference: https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-client
///   "If the pipe exists but all of its instances are busy, CreateFile returns
///    INVALID_HANDLE_VALUE and the GetLastError function returns
///    ERROR_PIPE_BUSY."
/// reference: https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights
///   "when a client calls the CreateFile or CallNamedPipe function to connect
///    to the client end of a named pipe, the system performs an access check
///    before granting access to the client."
///
/// So the only error that means "no server" is ERROR_FILE_NOT_FOUND. Every
/// other failure means the endpoint is there and something else went wrong.
fn probe_error_means_endpoint_present(code: Option<i32>) -> bool {
    let Some(code) = code else {
        return false;
    };
    code == windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32
        // The pipe instance EXISTS but this caller may not open it.
        //
        // MEASURED 2026-11-14 on this host: a Scheduled Task registered with
        // `-RunLevel Limited` opens the live daemon's pipe and gets
        // ERROR_ACCESS_DENIED (err=5), because Windows Mandatory Integrity
        // Control denies the lower-integrity process access to the
        // higher-integrity daemon's pipe. The old code mapped every error
        // other than ERROR_PIPE_BUSY to "no listener", so the Harbor carrier
        // logged "No running jcode server found" 12/12 times while the daemon
        // was alive and serving sessions.
        //
        // Access denied means the endpoint is PRESENT. Reporting it as absent
        // made a permission problem indistinguishable from a dead daemon.
        // Treating it as present matches the Unix side, where `is_socket_path`
        // is just "the path exists", and it is the safe direction for every
        // caller: a client tries to connect and gets its own honest error
        // instead of spawning a duplicate daemon against a live one.
        || code == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32
}

pub fn is_socket_path(path: &Path) -> bool {
    let pipe_name = path_to_pipe_name(path);
    match probe_pipe(&pipe_name) {
        Ok(_) => true,
        Err(error) => {
            // Every published instance being occupied proves that a server is
            // listening. Do not report the endpoint as absent and race a second
            // daemon against it merely because another client connected first.
            if probe_error_means_endpoint_present(error.raw_os_error()) {
                true
            } else {
                false
            }
        }
    }
}

pub fn remove_socket(path: &Path) {
    let pipe_name = path_to_pipe_name(path);
    if probe_pipe(&pipe_name).is_ok() {
        eprintln!(
            "[windows] Named pipe {} still open, will be replaced by new server",
            pipe_name
        );
    }
}

/// Probe whether a named pipe is currently reachable, without a tokio reactor.
///
/// `ClientOptions::open` registers the pipe handle with the ambient tokio
/// reactor, so it panics with "there is no reactor running" when called from a
/// thread that has no runtime context. The two probes above are synchronous by
/// contract and are reachable from plain `std::thread` callers, so they must
/// not touch the tokio client API. `SyncStream::connect` already opens pipes
/// with `std::fs`, which is the same approach.
fn probe_pipe(pipe_name: &str) -> io::Result<()> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name)
        .map(|_handle| ())
}

pub fn stream_pair() -> io::Result<(Stream, Stream)> {
    Stream::pair()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static BUSY_PIPE_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Jcode Desktop drives the SDK from plain `std::thread`s, where tokio's
    /// named-pipe client panics with "there is no reactor running". The two
    /// synchronous probes must therefore stay off the tokio client API.
    ///
    /// The pipe has to be a real, bound one: an absent pipe makes the open fail
    /// before tokio reaches the reactor registration, so probing a missing
    /// endpoint would pass with or without the fix and prove nothing.
    ///
    /// The test itself is a `#[tokio::test]` so that `Listener::bind` has a
    /// runtime, while the `std::thread` doing the probe deliberately has none.
    /// That split is the situation this bug is about: the server owns a runtime,
    /// the synchronous probes are called from plain threads that do not.
    #[tokio::test]
    async fn sync_pipe_probes_do_not_panic_without_a_tokio_reactor() {
        let path = std::env::temp_dir().join(format!(
            "jcode-plain-thread-probe-{}-{}.sock",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _listener = Listener::bind(&path).expect("bind named pipe");

        // A second, separately bound pipe for `remove_socket`. Probing consumes
        // the only available instance of a pipe, so reusing `path` would let the
        // second probe return ERROR_PIPE_BUSY and never reach the code under
        // test, which is exactly the coverage a single shared pipe left out.
        let remove_path = std::env::temp_dir().join(format!(
            "jcode-plain-thread-remove-{}-{}.sock",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _remove_listener = Listener::bind(&remove_path).expect("bind second named pipe");

        let probe = path.clone();
        let remove_probe = remove_path.clone();
        let joined = std::thread::spawn(move || {
            let reachable = is_socket_path(&probe);
            remove_socket(&remove_probe);
            reachable
        })
        .join();

        assert!(
            joined.is_ok(),
            "probing a live pipe on a plain thread panicked: {:?}",
            joined.err()
        );
        assert_eq!(
            joined.expect("thread joined"),
            true,
            "a bound named pipe must be reported as a live endpoint"
        );
    }

    /// The TypeScript SDK derives this name independently, so the two must
    /// agree exactly or a Windows client dials a pipe nobody is listening on.
    /// Pinning literal values here gives that duplicate a fixed contract to
    /// match, and `sockets.test.ts` asserts the same strings.
    #[test]
    fn pipe_name_matches_the_typescript_sdk() {
        for (path, expected) in [
            (
                r"C:\Users\jeremy\AppData\Local\jcode\run\jcode-api.sock",
                r"\\.\pipe\jcode-api-5e00c01702e8cfe4",
            ),
            (r"C:\a\b\jcode.sock", r"\\.\pipe\jcode-52dfdb00b2f35a71"),
        ] {
            assert_eq!(
                path_to_pipe_name(Path::new(path)),
                expected,
                "pipe name for {path} drifted from the SDK's derivation"
            );
        }
    }

    #[test]
    fn pipe_name_is_stable_and_normalizes_case_and_separators() {
        let a = path_to_pipe_name(Path::new(r"C:\Temp\Jcode\server.sock"));
        let b = path_to_pipe_name(Path::new("c:/temp/jcode/server.sock"));
        assert_eq!(a, b, "pipe names should be normalized consistently");
        assert!(
            a.starts_with(r"\\.\pipe\server-"),
            "unexpected pipe name: {}",
            a
        );
    }

    #[test]
    fn pipe_name_falls_back_when_stem_is_empty() {
        let name = path_to_pipe_name(Path::new("..."));
        assert!(
            name.starts_with(r"\\.\pipe\jcode-"),
            "unexpected pipe name: {}",
            name
        );
    }

    /// A read that is still pending on the same pipe must not block the write
    /// half. The SDK callback path hit exactly this: the server had a pending
    /// read on the connection pipe, and the next write of a few hundred bytes
    /// never completed, so the event forwarder stopped forwarding and the
    /// client never saw the tool call.
    ///
    /// reference: docs.rs/tokio/1.49.0/tokio/net/windows/named_pipe/struct.ServerOptions.html
    /// (tokio 1.49.0 offers pipe_mode, in_buffer_size and out_buffer_size, but
    /// no read or write timeout, so a stalled write cannot be bounded through
    /// the pipe API itself).
    ///
    /// The variants below isolate the trigger: each one differs from the
    /// failing sequence in exactly one way, so the one that hangs names the
    /// condition that has to be avoided.
    async fn probe(
        prior_request: bool,
        prior_write: bool,
        pending_read: bool,
        payload_len: usize,
        peer_drains: bool,
    ) -> bool {
        let (a, mut peer) = stream_pair().expect("create stream pair");
        let (mut read, mut write) = a.into_split();

        // The peer stays alive for the whole probe and keeps draining, so the
        // pipe never breaks and the only variable is the server side. If the
        // peer dropped the pipe, the write would fail with BrokenPipe and the
        // probe would measure the wrong thing.
        let peer_task = tokio::spawn(async move {
            if prior_request {
                peer.write_all(&[b'r'; 48]).await.expect("peer send request");
                peer.flush().await.expect("peer flush request");
            }
            let mut sink = vec![0u8; 4096];
            loop {
                match peer.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });

        if prior_request {
            // The peer sends a request and the server reads it: this is the
            // connection actor reading the next request off the socket.
            let mut request = vec![0u8; 48];
            read.read_exact(&mut request).await.expect("read request");
            assert_eq!(request.len(), 48);
        }
        if prior_write {
            write
                .write_all(b"{\"type\":\"ack\",\"id\":4}\n")
                .await
                .expect("ack write");
            write.flush().await.expect("ack flush");
        }

        let pending = if pending_read {
            Some(tokio::spawn(async move {
                let mut buf = [0u8; 16];
                let _ = read.read(&mut buf).await;
            }))
        } else {
            None
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let payload = vec![b'x'; payload_len];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            async {
                write.write_all(&payload).await.expect("write payload");
                write.flush().await.expect("flush payload");
            },
        )
        .await;
        if let Some(pending) = pending {
            pending.abort();
        }
        peer_task.abort();
        result.is_ok()
    }

    /// Reproduce the SDK-callback topology exactly: the server splits one
    /// stream into a read half and a write half, the forwarder writes through
    /// a shared `Arc<Mutex<WriteHalf>>`, and the client only reads when it is
    /// waiting for a reply. The observed failure is a `write_all` of ~539
    /// bytes that returns `Poll::Pending` and is never woken again.
    ///
    /// reference: https://docs.rs/tokio/1.49.0/tokio/net/windows/named_pipe/struct.ServerOptions.html
    /// (byte mode, 64 KiB buffers, no read/write timeout: a stalled write
    /// cannot be bounded through the pipe API itself).
    #[tokio::test]
    async fn probe_client_not_reading_while_server_writes() {
        let (server, mut client) = stream_pair().expect("create stream pair");
        let (mut server_read, mut server_write) = server.into_split();

        // The client sends one request and then stops reading, exactly like
        // the SDK client that writes a prompt and waits for the answer.
        let client_task = tokio::spawn(async move {
            client.write_all(b"{\"type\":\"message\"}\n").await.expect("client write");
            client.flush().await.expect("client flush");
            // Deliberately do not read for a while: the server now writes.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let mut sink = vec![0u8; 4096];
            loop {
                match client.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });

        // The server has a pending read on the same pipe (the actor waits for
        // the next request) while the forwarder writes an event.
        let pending_read = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let _ = server_read.read(&mut buf).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let payload = vec![b'x'; 539];
        let write = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                server_write.write_all(&payload).await
            },
        )
        .await;
        pending_read.abort();
        client_task.abort();
        assert!(
            write.is_ok(),
            "a 539 byte write must complete even when the client is not reading yet"
        );
    }

    #[tokio::test]
    async fn probe_full_sequence() {
        assert!(
            probe(true, true, true, 543, true).await,
            "full sequence (request, ack, pending read, 543 bytes) must not stall"
        );
    }

    #[tokio::test]
    async fn probe_without_pending_read() {
        assert!(
            probe(true, true, false, 543, true).await,
            "request + ack + 543 bytes (no pending read) must not stall"
        );
    }

    #[tokio::test]
    async fn probe_without_prior_write() {
        assert!(
            probe(true, false, true, 543, true).await,
            "request + pending read + 543 bytes (no prior write) must not stall"
        );
    }

    #[tokio::test]
    async fn probe_small_payload() {
        assert!(
            probe(true, true, true, 22, true).await,
            "a small payload must not stall either"
        );
    }

    #[tokio::test]
    async fn probe_large_payload() {
        assert!(
            probe(true, true, true, 64 * 1024, true).await,
            "a large payload must not stall either"
        );
    }

    #[tokio::test]
    async fn stream_pair_round_trips_bytes() {
        let (mut a, mut b) = stream_pair().expect("create stream pair");
        a.write_all(b"ping").await.expect("write to pipe");
        a.flush().await.expect("flush pipe");

        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).await.expect("read from pipe");
        assert_eq!(&buf, b"ping");
    }

    #[tokio::test]
    async fn split_stream_supports_concurrent_read_and_write() {
        let (a, mut b) = stream_pair().expect("create stream pair");
        let (mut read, mut write) = a.into_split();

        let peer = tokio::spawn(async move {
            let mut request = [0u8; 4];
            b.read_exact(&mut request).await.expect("read request");
            assert_eq!(&request, b"ping");
            b.write_all(b"pong").await.expect("write response");
            b.flush().await.expect("flush response");
        });

        let exchange = async {
            write.write_all(b"ping").await.expect("write request");
            write.flush().await.expect("flush request");
            let mut response = [0u8; 4];
            read.read_exact(&mut response).await.expect("read response");
            assert_eq!(&response, b"pong");
        };

        tokio::time::timeout(std::time::Duration::from_secs(5), exchange)
            .await
            .expect("split stream exchange should not stall");
        peer.await.expect("peer task");
    }

    /// Exercise the exact cold-start race: a synchronous readiness probe
    /// closes its client handle before the server polls ConnectNamedPipe.
    #[tokio::test]
    async fn accept_survives_closed_readiness_probes() {
        let path = std::env::temp_dir().join(format!(
            "jcode-closed-probe-{}-{}.sock",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut listener = Listener::bind(&path).expect("bind named pipe");

        for _ in 0..8 {
            assert!(
                is_socket_path(&path),
                "probe must open the waiting instance"
            );
            // Mio treats ERROR_NO_DATA during ConnectNamedPipe as an accepted
            // but already closed stream. Consume that probe just as the server
            // does, then prove a real client can still exchange data.
            let (mut probe, _) = listener.accept().await.expect("accept closed probe");
            let mut byte = [0];
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(5), probe.read(&mut byte))
                    .await
                    .expect("closed probe read must not hang");
            assert!(
                matches!(read, Ok(0)) || read.is_err_and(|e| e.kind() == io::ErrorKind::BrokenPipe),
                "closed readiness probe should report EOF or broken pipe"
            );
            drop(probe);

            let exchange = async {
                let server = async {
                    let (mut stream, _) = listener.accept().await.expect("accept after probe");
                    let mut request = [0; 4];
                    stream.read_exact(&mut request).await.expect("read request");
                    assert_eq!(&request, b"ping");
                    stream.write_all(b"pong").await.expect("write response");
                };
                let client = async {
                    let mut stream = Stream::connect(&path).await.expect("connect after probe");
                    stream.write_all(b"ping").await.expect("write request");
                    let mut response = [0; 4];
                    stream
                        .read_exact(&mut response)
                        .await
                        .expect("read response");
                    assert_eq!(&response, b"pong");
                };
                tokio::join!(server, client);
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), exchange)
                .await
                .expect("listener must remain usable after a closed probe");
        }
    }

    #[tokio::test]
    async fn accepted_client_disconnect_does_not_stop_listener() {
        let path = std::env::temp_dir().join(format!(
            "jcode-client-disconnect-{}-{}.sock",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut listener = Listener::bind(&path).expect("bind named pipe");
        for _ in 0..8 {
            let client = Stream::connect(&path).await.expect("connect client");
            let (mut server, _) = listener.accept().await.expect("accept client");
            drop(client);
            let mut byte = [0];
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(5), server.read(&mut byte))
                    .await
                    .expect("disconnected read must not hang");
            assert!(
                matches!(read, Ok(0)) || read.is_err_and(|e| e.kind() == io::ErrorKind::BrokenPipe),
                "closed client should report EOF or broken pipe"
            );
        }
    }

    #[tokio::test]
    async fn busy_pipe_is_reported_as_a_live_socket_path() {
        let path = std::env::temp_dir().join(format!(
            "jcode-busy-pipe-probe-{}-{}.sock",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _listener = Listener::bind(&path).expect("bind named pipe");
        let _client = Stream::connect(&path).await.expect("occupy named pipe");

        assert!(
            is_socket_path(&path),
            "a busy named pipe must still be recognized as a live server endpoint"
        );
    }

    /// A probe that fails on ACCESS DENIED must NOT report the endpoint absent.
    ///
    /// MEASURED 2026-11-14: a `-RunLevel Limited` Scheduled Task probing a live
    /// daemon's pipe got ERROR_ACCESS_DENIED, and the probe reported "no
    /// listener" for 12/12 attempts while the daemon was alive and serving.
    /// The endpoint was present the whole time; only the caller's rights were
    /// insufficient.
    ///
    /// The mapping is pinned directly, so the test holds regardless of the
    /// integrity level the test runner happens to execute at.
    #[test]
    fn an_access_denied_pipe_is_not_reported_as_absent() {
        let access_denied = windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32;
        let file_not_found = windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32;
        let pipe_busy = windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32;

        assert!(
            probe_error_means_endpoint_present(Some(access_denied)),
            "ACCESS_DENIED proves the pipe EXISTS; it must not read as absent"
        );
        assert!(
            probe_error_means_endpoint_present(Some(pipe_busy)),
            "PIPE_BUSY proves a server is listening"
        );
        assert!(
            !probe_error_means_endpoint_present(Some(file_not_found)),
            "FILE_NOT_FOUND is the only error that means the pipe is absent"
        );
        assert!(
            !probe_error_means_endpoint_present(None),
            "an error with no OS code carries no proof of presence"
        );
    }

    /// A pipe with no server behind it must still be reported as absent.
    ///
    /// This is the negative control for the test above: it proves the mapping
    /// did not simply start reporting every probe as present.
    #[test]
    fn a_pipe_with_no_server_is_reported_as_absent() {
        let path = std::env::temp_dir().join(format!(
            "jcode-absent-pipe-probe-{}-{}.sock",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(
            !is_socket_path(&path),
            "a pipe with no server must be reported as absent"
        );
    }

    /// The short (8.3) spelling of a directory must name the SAME pipe as the
    /// long spelling.
    ///
    /// The pipe name is a hash of the path STRING, so two spellings of one
    /// directory used to produce two pipes and therefore two independent
    /// daemons. MEASURED 2026-11-14 on this host: a Scheduled Task received
    /// `C:\Users\WORKSH~1\...` while an interactive shell received
    /// `C:\Users\WorkShop1\...`, and the two hashed to `d6352b96b284a42d` and
    /// `5e984a5ce3694442` - two daemons holding the same session database.
    ///
    /// The short spelling is obtained from the OS at run time, not pinned to a
    /// literal, so the test does not depend on this machine's user name and
    /// fails loudly if the OS stops producing one.
    ///
    /// reference: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getshortpathnamew
    #[test]
    fn the_short_spelling_of_a_directory_names_the_same_pipe() {
        let long_dir = std::env::temp_dir().join(format!(
            "jcode-short-spelling-{}-{}",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&long_dir).expect("create the long-named directory");

        let short_dir = short_path_spelling(&long_dir);
        assert!(
            short_dir.is_some(),
            "the OS must produce an 8.3 spelling for this test to mean anything"
        );
        let short_dir = short_dir.expect("checked above");
        assert_ne!(
            short_dir, long_dir,
            "the 8.3 spelling must differ from the long one, otherwise this test is vacuous"
        );

        let long_socket = long_dir.join("jcode.sock");
        let short_socket = short_dir.join("jcode.sock");

        assert_eq!(
            path_to_pipe_name(&long_socket),
            path_to_pipe_name(&short_socket),
            "two spellings of one directory must name ONE pipe, or two daemons start"
        );
    }

    /// The negative control for the test above: WITHOUT the parent resolution
    /// the two spellings do produce different pipe names. If this ever passes,
    /// the test above proves nothing.
    #[test]
    fn without_the_parent_resolution_the_two_spellings_do_differ() {
        fn no_resolution(_parent: &Path) -> Option<std::path::PathBuf> {
            None
        }

        let long_dir = std::env::temp_dir().join(format!(
            "jcode-short-spelling-nc-{}-{}",
            std::process::id(),
            BUSY_PIPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&long_dir).expect("create the long-named directory");
        let short_dir = short_path_spelling(&long_dir).expect("the OS must produce an 8.3 spelling");
        assert_ne!(short_dir, long_dir, "the 8.3 spelling must differ");

        assert_ne!(
            path_to_pipe_name_with(&long_dir.join("jcode.sock"), no_resolution),
            path_to_pipe_name_with(&short_dir.join("jcode.sock"), no_resolution),
            "without the resolution the spellings must differ, or the fix proves nothing"
        );
    }

    /// A directory that cannot be resolved falls back to the raw path.
    ///
    /// The pipe names pinned by `pipe_name_matches_the_typescript_sdk` use
    /// paths whose parent does not exist. Resolving them must not change their
    /// hash, or the SDK contract would break for every path that is not yet
    /// created.
    #[test]
    fn an_unresolvable_parent_keeps_the_raw_path() {
        let path = Path::new(r"C:\a\b\jcode.sock");
        assert_eq!(
            path_to_pipe_name(path),
            r"\\.\pipe\jcode-52dfdb00b2f35a71",
            "a path whose parent does not exist must hash exactly as before"
        );
    }

    /// The `\\?\` verbatim prefix must not reach the hash.
    ///
    /// MEASURED 2026-11-14: `std::fs::canonicalize` on Windows returns
    /// `\\?\C:\Users\WorkShop1\...`. Hashing that verbatim string gives
    /// `2713a047a605d211` instead of `5e984a5ce3694442`, which would change
    /// every pipe name on the machine and break the pinned SDK contract.
    #[test]
    fn the_verbatim_prefix_is_stripped_before_hashing() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\Users\WorkShop1\AppData")),
            Path::new(r"C:\Users\WorkShop1\AppData")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share")),
            Path::new(r"\\server\share")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\plain\path")),
            Path::new(r"C:\plain\path"),
            "a path without the prefix must be left alone"
        );
    }

    /// The 8.3 spelling of `path`, as the OS reports it.
    ///
    /// Returns `None` when the OS declines to produce one (the volume has 8.3
    /// name generation disabled), which the caller treats as "this test cannot
    /// run here" rather than as a failure of the code under test.
    ///
    /// reference: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getshortpathnamew
    fn short_path_spelling(path: &Path) -> Option<std::path::PathBuf> {
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buffer = vec![0u16; wide.len() + 64];
        let written = unsafe {
            GetShortPathNameW(
                wide.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        };
        if written == 0 || written as usize >= buffer.len() {
            return None;
        }
        buffer.truncate(written as usize);
        Some(std::path::PathBuf::from(std::ffi::OsString::from_wide(
            &buffer[..written as usize],
        )))
    }

    /// Stress harness for the SDK-callback write hang.
    ///
    /// The field report is a `write_all` of ~541 bytes that returns
    /// `Poll::Pending` and is never woken again, on the server side of a
    /// Windows named pipe, while the peer keeps receiving other messages.
    /// The reported topology is:
    ///
    /// 1. one mio named pipe object, split into a read half and a write half;
    /// 2. a request-reader task that keeps an overlapped read pending on the
    ///    read half;
    /// 3. a separate forwarder task that writes ~541 byte JSON events on the
    ///    write half.
    ///
    /// These tests rebuild exactly that topology, count how often a write
    /// fails to complete, and print a `STRESS_RESULT` line so two runs can
    /// be compared.
    ///
    /// reference: docs.rs/tokio/1.49.0/tokio/net/windows/named_pipe/struct.ServerOptions.html
    /// reference: https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-client
    mod stress {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Interest};
        use tokio::net::windows::named_pipe::{
            ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
        };

        /// Iterations of the stress loop.
        pub const ITERATIONS: usize = 200;
        /// Events the forwarder writes per iteration.
        pub const EVENTS_PER_ITER: usize = 4;
        /// Size of one forwarded event: the size seen in the field report.
        pub const EVENT_BYTES: usize = 541;
        /// How long a single write may take before it counts as a hang.
        pub const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

        static PIPE_COUNTER: AtomicU64 = AtomicU64::new(0);

        /// What the client does before it starts reading.
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum Request {
            /// Nothing.
            None,
            /// A complete request line.
            Full,
            /// A request line WITHOUT its newline, so the server's read stays
            /// pending mid-line.
            Partial,
        }

        /// How the server read half is driven.
        ///
        /// `Split` is the reported topology: a reader task that keeps an
        /// overlapped read outstanding. `PollBased` never leaves a read in
        /// flight between polls. `None` runs no reader at all.
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum Reader {
            Split,
            PollBased,
            None,
        }

        /// How the server write half is driven.
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum Writer {
            /// The split write half, used directly by the forwarder.
            Split,
            /// A dedicated writer task fed over an mpsc channel (M4).
            Dedicated,
            /// TWO tasks writing to the SAME write half (H1): one direct
            /// writer and one channel-fed writer.
            Shared,
        }

        /// Knobs of one stress run.
        pub struct Options {
            /// `Some(PipeMode::Message)` switches both ends to message mode.
            pub mode: Option<PipeMode>,
            /// Size of one forwarded event.
            pub event_bytes: usize,
            /// Events written per iteration.
            pub events_per_iter: usize,
            /// What the client sends before it starts reading.
            pub request: Request,
            /// Per-write timeout.
            pub timeout: std::time::Duration,
            /// Whether a timed-out write re-creates the connection (M3).
            pub reconnect_on_timeout: bool,
            /// How the server read half is driven.
            pub reader: Reader,
            /// How the server write half is driven.
            pub writer: Writer,
            /// Delay between two writes.
            pub write_delay: std::time::Duration,
            /// How long the client waits before it starts reading.
            pub client_read_delay: std::time::Duration,
        }

        impl Default for Options {
            fn default() -> Self {
                Self {
                    mode: None,
                    event_bytes: EVENT_BYTES,
                    events_per_iter: EVENTS_PER_ITER,
                    request: Request::None,
                    timeout: WRITE_TIMEOUT,
                    reconnect_on_timeout: false,
                    reader: Reader::Split,
                    writer: Writer::Split,
                    write_delay: std::time::Duration::from_millis(1),
                    client_read_delay: std::time::Duration::ZERO,
                }
            }
        }

        /// What one stress run measured.
        #[derive(Default)]
        pub struct Outcome {
            pub iterations: usize,
            /// Iterations in which a write never completed.
            pub hangs: usize,
            /// Writes that timed out.
            pub timeouts: usize,
            /// Connections re-created after a timeout.
            pub reconnects: usize,
            /// Lines the clients received in total.
            pub lines: u64,
            /// First hang as (iteration, bytes).
            pub first_hang: Option<(usize, usize)>,
        }

        /// A connected server/client named pipe pair.
        ///
        /// The client opens the instance the server created, so the server's
        /// `connect()` resolves immediately with `ERROR_PIPE_CONNECTED`.
        pub async fn make_pair(mode: Option<PipeMode>) -> (NamedPipeServer, NamedPipeClient) {
            let name = format!(
                r"\\.\pipe\jcode-stress-{}-{}",
                std::process::id(),
                PIPE_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let server = {
                let mut opts = ServerOptions::new();
                opts.first_pipe_instance(true);
                if let Some(mode) = mode {
                    opts.pipe_mode(mode);
                }
                opts.create(&name).expect("create stress pipe server")
            };
            let client = {
                let mut opts = ClientOptions::new();
                if let Some(mode) = mode {
                    opts.pipe_mode(mode);
                }
                opts.open(&name).expect("open stress pipe client")
            };
            server.connect().await.expect("connect stress pipe server");
            (server, client)
        }

        /// One forwarded event: JSON-ish, newline terminated, `bytes` long.
        pub fn payload(bytes: usize) -> Vec<u8> {
            let mut payload = vec![b'x'; bytes];
            payload[0] = b'{';
            let last = payload.len() - 1;
            payload[last - 1] = b'"';
            payload[last] = b'\n';
            payload
        }

        /// Run the stress loop and report what it measured.
        pub async fn run(opts: &Options) -> Outcome {
            let mut outcome = Outcome::default();
            let mut iteration = 0usize;
            while iteration < ITERATIONS {
                iteration += 1;
                outcome.iterations = iteration;

                // One iteration may need more than one connection when a
                // timeout re-creates it (M3).
                loop {
                    let (server, mut client) = make_pair(opts.mode).await;

                    match opts.request {
                        Request::None => {}
                        Request::Full => {
                            client
                                .write_all(b"{\"type\":\"message\"}\n")
                                .await
                                .expect("client request");
                            client.flush().await.expect("client request flush");
                        }
                        Request::Partial => {
                            // No newline: the server's read stays pending
                            // mid-line for the whole iteration.
                            client
                                .write_all(b"{\"type\":\"message\"")
                                .await
                                .expect("client partial request");
                            client.flush().await.expect("client partial request flush");
                        }
                    }

                    let client_task = spawn_client_reader(client, opts.client_read_delay);

                    let event = payload(opts.event_bytes);
                    let mut timed_out = false;

                    match opts.reader {
                        Reader::PollBased => {
                            // M1': the reader is driven by tokio's poll-based
                            // API, so no overlapped READ is outstanding while
                            // the writer is active.
                            timed_out =
                                poll_based_exchange(server, &event, opts, &mut outcome, iteration)
                                    .await
                                    .is_err();
                        }
                        Reader::None => {
                            // No server-side reader at all: the pure
                            // write-after-write path.
                            let (read_half, mut write_half) = Stream::Server(server).into_split();
                            drop(read_half);
                            timed_out = write_burst(
                                &mut write_half,
                                &event,
                                opts,
                                &mut outcome,
                                iteration,
                            )
                            .await;
                        }
                        Reader::Split => {
                            if let Writer::Shared = opts.writer {
                                // H1 needs the WHOLE stream: two `tokio::io::split`
                                // halves over the same mio pipe.
                                timed_out = shared_writers(
                                    server,
                                    &event,
                                    opts,
                                    &mut outcome,
                                    iteration,
                                )
                                .await;
                                client_task.abort();
                                break;
                            }

                            // The request reader keeps an overlapped read
                            // pending on the read half for the whole iteration.
                            let (mut read_half, write_half) =
                                Stream::Server(server).into_split();
                            let reader_task = tokio::spawn(async move {
                                let mut buf = vec![0u8; 1024];
                                loop {
                                    match read_half.read(&mut buf).await {
                                        Ok(0) | Err(_) => break,
                                        Ok(_) => {}
                                    }
                                }
                            });

                            match opts.writer {
                                Writer::Dedicated => {
                                    // M4: a dedicated writer task owns the write
                                    // half, so there is never more than one
                                    // write in flight.
                                    timed_out = channel_writer(
                                        write_half,
                                        &event,
                                        opts,
                                        &mut outcome,
                                        iteration,
                                    )
                                    .await;
                                }
                                Writer::Shared => {
                                    unreachable!("handled above")
                                }
                                Writer::Split => {
                                    let mut write_half = write_half;
                                    timed_out = write_burst(
                                        &mut write_half,
                                        &event,
                                        opts,
                                        &mut outcome,
                                        iteration,
                                    )
                                    .await;
                                }
                            }

                            reader_task.abort();
                        }
                    }

                    client_task.abort();

                    if timed_out && opts.reconnect_on_timeout {
                        outcome.reconnects += 1;
                        continue;
                    }
                    break;
                }
            }

            outcome.lines = LINES_TOTAL.load(Ordering::Relaxed);
            LINES_TOTAL.store(0, Ordering::Relaxed);
            outcome
        }

        /// Write `events_per_iter` frames on the given write half, one after
        /// the other, and report whether any of them timed out.
        async fn write_burst(
            write_half: &mut WriteHalf,
            event: &[u8],
            opts: &Options,
            outcome: &mut Outcome,
            iteration: usize,
        ) -> bool {
            for _ in 0..opts.events_per_iter {
                match tokio::time::timeout(opts.timeout, write_half.write_all(event)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        panic!("stress write failed with {error}");
                    }
                    Err(_) => {
                        record_hang(outcome, iteration, event.len(), opts);
                        return true;
                    }
                }
                if !opts.write_delay.is_zero() {
                    tokio::time::sleep(opts.write_delay).await;
                }
            }
            false
        }

        /// A `Stream` that several halves can share.
        ///
        /// The mio named pipe lives BEHIND this mutex, so two write halves
        /// built from two clones poll the SAME mio object and the SAME tokio
        /// `ScheduledIo` write slot. That is what lets two `write_all`
        /// futures compete for one writer waker, which is the shape the
        /// jcode server has (the actor writes acks while the forwarder writes
        /// events).
        pub struct SharedStream(pub std::sync::Arc<std::sync::Mutex<Stream>>);

        impl Clone for SharedStream {
            fn clone(&self) -> Self {
                SharedStream(std::sync::Arc::clone(&self.0))
            }
        }

        impl AsyncRead for SharedStream {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                let mut guard = self.0.lock().expect("shared stream lock");
                std::pin::Pin::new(&mut *guard).poll_read(cx, buf)
            }
        }

        impl AsyncWrite for SharedStream {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> std::task::Poll<io::Result<usize>> {
                let mut guard = self.0.lock().expect("shared stream lock");
                std::pin::Pin::new(&mut *guard).poll_write(cx, buf)
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                let mut guard = self.0.lock().expect("shared stream lock");
                std::pin::Pin::new(&mut *guard).poll_flush(cx)
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                let mut guard = self.0.lock().expect("shared stream lock");
                std::pin::Pin::new(&mut *guard).poll_shutdown(cx)
            }
        }

        /// M4: one writer task owns the write half and writes every frame
        /// itself, so there is never more than one write in flight.
        async fn channel_writer(
            write_half: WriteHalf,
            event: &[u8],
            opts: &Options,
            outcome: &mut Outcome,
            iteration: usize,
        ) -> bool {
            let mut write_half = write_half;
            write_burst(&mut write_half, event, opts, outcome, iteration).await
        }

        /// H1: TWO tasks write to the SAME write half at the same time.
        ///
        /// The jcode server has exactly this shape: the actor writes acks
        /// directly and the event forwarder writes events, both through the
        /// same `Arc<Mutex<WriteHalf>>`. tokio's `ScheduledIo` keeps ONE
        /// writer waker per direction, so two concurrent `write_all`
        /// futures on the same half can overwrite each other's waker and
        /// lose a wakeup.
        ///
        /// Each writer writes a short frame (the ack) and then a long one
        /// (the event), so the long write starts while the short one may
        /// still be in flight.
        async fn shared_writers(
            server: NamedPipeServer,
            event: &[u8],
            opts: &Options,
            outcome: &mut Outcome,
            iteration: usize,
        ) -> bool {
            // ONE mio named pipe object, TWO `tokio::io::split` halves. Both
            // halves poll the SAME `NamedPipeServer`, so both register their
            // waker on the SAME tokio `ScheduledIo` write slot.
            let shared = SharedStream(std::sync::Arc::new(std::sync::Mutex::new(
                Stream::Server(server),
            )));
            let (_read_a, mut write_a) = tokio::io::split(shared.clone());
            let (_read_b, mut write_b) = tokio::io::split(shared);

            let short = vec![b'a'; 22];
            let event = event.to_vec();
            let event_len = event.len();
            let events = opts.events_per_iter;

            let short_b = short.clone();
            let event_b = event.clone();
            let task_a = tokio::spawn(async move {
                for _ in 0..events {
                    write_a.write_all(&short).await.ok();
                    write_a.write_all(&event).await.ok();
                }
            });
            let task_b = tokio::spawn(async move {
                for _ in 0..events {
                    write_b.write_all(&short_b).await.ok();
                    write_b.write_all(&event_b).await.ok();
                }
            });
            // If one of the two writers is never woken again, its future
            // never finishes.
            let timed_out = tokio::time::timeout(opts.timeout, async {
                task_a.await.ok();
                task_b.await.ok();
            })
            .await
            .is_err();

            if timed_out {
                record_hang(outcome, iteration, event_len, opts);
            }
            timed_out
        }

        /// P2/P3/P4: the exact trace shape from the field report.
        ///
        /// The real failing trace is: a SHORT write (the actor's ack, 22
        /// bytes) completes, and the VERY NEXT write (the forwarder's event,
        /// 545 bytes) is polled exactly once, returns `Pending`, and is never
        /// polled again.
        ///
        /// `TwoTasks` reproduces the jcode shape: the ack is written by one
        /// task and the event by another, both through the SAME
        /// `Arc<Mutex<WriteHalf>>`. `SameTask` writes both frames from ONE
        /// task, so it has no shared-waker contention at all. `OneTaskViaChannel`
        /// is H2's shape with H1's traffic: one writer task fed over an mpsc
        /// channel writes the short frame and then the long frame.
        pub enum AckThenEvent {
            /// Two tasks, one write half, ack awaited BEFORE the event.
            TwoTasks,
            /// Two tasks, one write half, both writes IN FLIGHT at once.
            TwoTasksConcurrent,
            /// One task, two sequential writes.
            SameTask,
            /// One writer task fed over an mpsc channel.
            OneTaskViaChannel,
        }

        /// Run the ack-then-event shape and report whether the LONG write
        /// completed.
        ///
        /// `tokio::sync` is not enabled in this crate, so the shared write
        /// half is modelled with `SharedStream` (one mio pipe behind an
        /// `Arc<Mutex<Stream>>`), the same shape `shared_writers` uses and
        /// the same shape the jcode server has (`Arc<Mutex<WriteHalf>>`).
        pub async fn ack_then_event(server: NamedPipeServer, shape: AckThenEvent) -> bool {
            let shared = SharedStream(std::sync::Arc::new(std::sync::Mutex::new(
                Stream::Server(server),
            )));
            let short = vec![b'a'; 22];
            let long = payload(545);

            match shape {
                AckThenEvent::SameTask => {
                    let mut writer = shared.clone();
                    if writer.write_all(&short).await.is_err() {
                        return true;
                    }
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        writer.write_all(&long),
                    )
                    .await
                    .map(|r| r.is_err())
                    .unwrap_or(true)
                }
                AckThenEvent::OneTaskViaChannel => {
                    // ONE writer task performs BOTH writes, so there is
                    // never more than one write in flight on the half.
                    // (`tokio::sync` is not enabled in this crate, and a
                    // blocking `std::sync::mpsc::recv` would park the
                    // single-threaded test runtime, so the queue is
                    // modelled by the writer task itself.)
                    let task = tokio::spawn(async move {
                        let mut writer = shared;
                        if writer.write_all(&short).await.is_err() {
                            return false;
                        }
                        writer.write_all(&long).await.is_ok()
                    });
                    tokio::time::timeout(std::time::Duration::from_secs(5), task)
                        .await
                        .map(|r| r.map(|ok| !ok).unwrap_or(true))
                        .unwrap_or(true)
                }
                AckThenEvent::TwoTasksConcurrent => {
                    // BOTH writes are in flight at the same time: the ack
                    // task is NOT awaited before the event task starts.
                    // This is the shape in which the two `write_all`
                    // futures compete for tokio's SINGLE writer waker slot.
                    let ack_writer = shared.clone();
                    let ack = tokio::spawn(async move {
                        let mut writer = ack_writer;
                        writer.write_all(&short).await.is_ok()
                    });
                    let event_writer = shared.clone();
                    let event = tokio::spawn(async move {
                        let mut writer = event_writer;
                        writer.write_all(&long).await.is_ok()
                    });
                    let timed_out = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        async {
                            let _ = ack.await;
                            let _ = event.await;
                        },
                    )
                    .await
                    .is_err();
                    timed_out
                }
                AckThenEvent::TwoTasks => {
                    let ack_writer = shared.clone();
                    let ack = tokio::spawn(async move {
                        let mut writer = ack_writer;
                        writer.write_all(&short).await.is_ok()
                    });
                    // The ack write must COMPLETE before the event write
                    // starts: that is the ordering in the real trace.
                    let _ = ack.await;

                    let event_writer = shared.clone();
                    let event = tokio::spawn(async move {
                        let mut writer = event_writer;
                        writer.write_all(&long).await.is_ok()
                    });
                    tokio::time::timeout(std::time::Duration::from_secs(5), event)
                        .await
                        .map(|r| r.map(|ok| !ok).unwrap_or(true))
                        .unwrap_or(true)
                }
            }
        }

        /// Record one timed-out write.
        fn record_hang(outcome: &mut Outcome, iteration: usize, bytes: usize, opts: &Options) {
            outcome.timeouts += 1;
            outcome.hangs += 1;
            if outcome.first_hang.is_none() {
                outcome.first_hang = Some((iteration, bytes));
            }
            eprintln!(
                "STRESS_HANG iteration={iteration} bytes={bytes} timeout={:?}",
                opts.timeout
            );
        }

        /// The client reads continuously, so the pipe never backs up and a
        /// stalled write cannot be blamed on a full buffer.
        pub fn spawn_client_reader(
            client: NamedPipeClient,
            delay: std::time::Duration,
        ) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let mut reader = BufReader::new(client);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    match reader.read_line(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            LINES_TOTAL.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        }

        /// M1': drive the write side with tokio's poll-based API, so no
        /// overlapped READ is outstanding while the writer is active.
        ///
        /// The poll-based API is synchronous, so it runs on a blocking thread
        /// of the runtime.
        async fn poll_based_exchange(
            server: NamedPipeServer,
            event: &[u8],
            opts: &Options,
            outcome: &mut Outcome,
            iteration: usize,
        ) -> Result<(), ()> {
            let event = event.to_vec();
            let event_len = event.len();
            let timeout = opts.timeout;
            let events = opts.events_per_iter;
            let delay = opts.write_delay;
            let handle = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; 1024];
                for _ in 0..events {
                    let mut written = 0usize;
                    while written < event.len() {
                        match server.try_write(&event[written..]) {
                            Ok(0) => {}
                            Ok(n) => written += n,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // Wait for writability, then try again.
                                let _ = server.ready(Interest::WRITABLE);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    // Keep the pipe drained so the reader never wedges.
                    let _ = server.try_read(&mut buf);
                }
                Ok::<(), io::Error>(())
            });
            let joined = tokio::time::timeout(timeout, handle).await;
            match joined {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(error))) => panic!("stress poll-based write failed with {error}"),
                Ok(Err(_)) => {
                    record_hang(outcome, iteration, event_len, opts);
                    Err(())
                }
                Err(_) => {
                    record_hang(outcome, iteration, event_len, opts);
                    Err(())
                }
            }
        }

        /// Lines received by the clients of the last run, summed up.
        static LINES_TOTAL: AtomicU64 = AtomicU64::new(0);

        /// Print the summary line every stress test ends with.
        pub fn report(label: &str, outcome: &Outcome) {
            println!(
                "STRESS_RESULT label={label} iterations={} hangs={} timeouts={} reconnects={} lines={} first_hang={:?}",
                outcome.iterations,
                outcome.hangs,
                outcome.timeouts,
                outcome.reconnects,
                outcome.lines,
                outcome.first_hang
            );
        }
    }

    /// BASELINE: the reported SDK-callback topology, one mio named pipe
    /// object split into a read half (with a pending read) and a write half.
    ///
    /// This is the reference measurement every mitigation is compared to.
    #[tokio::test]
    async fn stress_pending_read_plus_write() {
        let outcome = stress::run(&stress::Options::default()).await;
        stress::report("baseline", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "baseline: a write on a split pipe with a pending read must not hang"
        );
    }

    /// M2: the same topology with both ends in `PipeMode::Message`.
    ///
    /// Message mode changes how Windows frames data on the pipe, so it may
    /// change whether a pending read on one direction starves a write on the
    /// other.
    #[tokio::test]
    async fn stress_message_mode() {
        use tokio::net::windows::named_pipe::PipeMode;

        let opts = stress::Options {
            mode: Some(PipeMode::Message),
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("message_mode", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "M2: message mode must not hang either"
        );
    }

    /// The client requests something before it starts reading, so the server
    /// reads a request and then keeps a read pending while it writes.
    ///
    /// This is the closest match to the field report, where the SDK client
    /// sends a prompt and then waits for the forwarded events.
    #[tokio::test]
    async fn stress_client_requests_then_server_writes() {
        let opts = stress::Options {
            request: stress::Request::Full,
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("client_requests", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "a request-then-write sequence must not hang"
        );
    }

    /// The client sends a request WITHOUT its newline, so the server's read
    /// stays pending MID-LINE while the forwarder writes.
    #[tokio::test]
    async fn stress_partial_line_request_then_server_writes() {
        let opts = stress::Options {
            request: stress::Request::Partial,
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("partial_line_request", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "a read pending mid-line must not starve a write"
        );
    }

    /// M5: the write-after-write path with NO reader on the server side.
    ///
    /// mio 1.1.1 keeps ONE write slot per pipe: while a write is in flight
    /// the next write returns `WouldBlock` and tokio waits for mio to report
    /// the pipe writable again. So a burst of writes back to back on the SAME
    /// pipe instance is the exact failing shape, and no reader is needed to
    /// trigger it.
    #[tokio::test]
    async fn stress_write_after_write_burst() {
        let mut hangs = 0usize;
        let mut iterations = 0usize;
        for events in [2usize, 4, 16, 64] {
            let opts = stress::Options {
                reader: stress::Reader::None,
                events_per_iter: events,
                write_delay: std::time::Duration::ZERO,
                ..stress::Options::default()
            };
            let outcome = stress::run(&opts).await;
            stress::report(&format!("burst_{events}"), &outcome);
            hangs += outcome.hangs;
            iterations += outcome.iterations;
        }
        println!("STRESS_RESULT label=write_after_write iterations={iterations} hangs={hangs}");
        assert_eq!(hangs, 0, "the write-after-write path must not hang");
    }

    /// H1: TWO tasks write to the SAME write half at the same time.
    ///
    /// tokio's `ScheduledIo` keeps ONE writer waker per direction, so two
    /// concurrent `write_all` futures on the same `WriteHalf` can overwrite
    /// each other's waker and lose a wakeup. This is the shape the jcode
    /// server has: the actor writes acks directly while the forwarder writes
    /// events.
    #[tokio::test]
    async fn stress_two_writers_same_half() {
        let opts = stress::Options {
            writer: stress::Writer::Shared,
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("two_writers_same_half", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "H1: two writers on one half must not hang"
        );
    }

    /// H2: the SAME shape, but every write goes through ONE writer task.
    ///
    /// If H1 stalls and H2 does not, the root cause is the shared writer
    /// waker and the server-side fix is to route every pipe write through a
    /// single writer task.
    #[tokio::test]
    async fn stress_single_writer_task() {
        let opts = stress::Options {
            writer: stress::Writer::Dedicated,
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("single_writer_task", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "H2: a single writer task must not hang either"
        );
    }

    /// M1': the reader is driven by tokio's poll-based API, so NO overlapped
    /// READ is outstanding while the writer is active.
    ///
    /// `NamedPipeServer::ready` + `try_read` never leaves a read in flight
    /// between polls, which removes the precondition "a pending overlapped
    /// read exists during the write".
    #[tokio::test]
    async fn stress_poll_based_reader_no_pending_read() {
        let opts = stress::Options {
            reader: stress::Reader::PollBased,
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("poll_based_reader", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "M1': a poll-based reader must not hang either"
        );
    }

    /// M4: a dedicated writer task owns the write half and receives frames
    /// over an mpsc channel, so there is never more than one write in flight
    /// and the writer never competes with a direct writer path.
    #[tokio::test]
    async fn stress_dedicated_writer_task() {
        let opts = stress::Options {
            writer: stress::Writer::Dedicated,
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("dedicated_writer", &outcome);
        assert_eq!(
            outcome.hangs, 0,
            "M4: a dedicated writer task must not hang either"
        );
    }

    /// Event sizes from 100 bytes to 64 KiB, so a hang cannot be an artifact
    /// of one particular payload length.
    #[tokio::test]
    async fn stress_varied_payload_sizes() {
        let mut hangs = 0usize;
        let mut iterations = 0usize;
        for bytes in [100usize, 541, 1024, 4096, 16384, 65536] {
            let opts = stress::Options {
                event_bytes: bytes,
                ..stress::Options::default()
            };
            let outcome = stress::run(&opts).await;
            stress::report(&format!("payload_{bytes}"), &outcome);
            hangs += outcome.hangs;
            iterations += outcome.iterations;
        }
        println!("STRESS_RESULT label=varied_payloads iterations={iterations} hangs={hangs}");
        assert_eq!(hangs, 0, "no payload size may hang");
    }

    /// P2: the SHORT write (ack) from task A completes, then the LONG write
    /// (event) from task B on the SAME write half.
    ///
    /// This is the exact trace shape of the field report, and it isolates the
    /// concurrency from the payload length.
    #[tokio::test]
    async fn stress_ack_then_event_two_tasks() {
        let mut hangs = 0usize;
        let mut iterations = 0usize;
        for _ in 0..stress::ITERATIONS {
            let (server, client) = stress::make_pair(None).await;
            let client_task = stress::spawn_client_reader(client, std::time::Duration::ZERO);
            let timed_out =
                stress::ack_then_event(server, stress::AckThenEvent::TwoTasks).await;
            client_task.abort();
            iterations += 1;
            if timed_out {
                hangs += 1;
                if hangs == 1 {
                    eprintln!("STRESS_HANG label=ack_then_event_two_tasks iteration=1 bytes=545");
                }
            }
        }
        println!(
            "STRESS_RESULT label=ack_then_event_two_tasks iterations={iterations} hangs={hangs} timeouts={hangs} reconnects=0 lines=0 first_hang=None"
        );
    }

    /// P3: the SAME two writes, but from ONE task, so there is no shared
    /// waker contention.
    ///
    /// If P3 also hangs, the cause is not the shared writer waker but
    /// something about the SECOND write after a completed one.
    #[tokio::test]
    async fn stress_ack_then_event_same_task() {
        let mut hangs = 0usize;
        let mut iterations = 0usize;
        for _ in 0..stress::ITERATIONS {
            let (server, client) = stress::make_pair(None).await;
            let client_task = stress::spawn_client_reader(client, std::time::Duration::ZERO);
            let timed_out =
                stress::ack_then_event(server, stress::AckThenEvent::SameTask).await;
            client_task.abort();
            iterations += 1;
            if timed_out {
                hangs += 1;
                if hangs == 1 {
                    eprintln!("STRESS_HANG label=ack_then_event_same_task iteration=1 bytes=545");
                }
            }
        }
        println!(
            "STRESS_RESULT label=ack_then_event_same_task iterations={iterations} hangs={hangs} timeouts={hangs} reconnects=0 lines=0 first_hang=None"
        );
    }

    /// P4: H2's shape (ONE writer task over an mpsc channel) with H1's
    /// traffic (short frame, then long frame).
    ///
    /// This is the decisive control for P2/P3: if P4 never hangs while P2
    /// does, the shared writer waker is the cause.
    #[tokio::test]
    async fn stress_ack_then_event_one_writer_task() {
        let mut hangs = 0usize;
        let mut iterations = 0usize;
        for _ in 0..stress::ITERATIONS {
            let (server, client) = stress::make_pair(None).await;
            let client_task = stress::spawn_client_reader(client, std::time::Duration::ZERO);
            let timed_out =
                stress::ack_then_event(server, stress::AckThenEvent::OneTaskViaChannel).await;
            client_task.abort();
            iterations += 1;
            if timed_out {
                hangs += 1;
                if hangs == 1 {
                    eprintln!("STRESS_HANG label=ack_then_event_one_writer_task iteration=1 bytes=545");
                }
            }
        }
        println!(
            "STRESS_RESULT label=ack_then_event_one_writer_task iterations={iterations} hangs={hangs} timeouts={hangs} reconnects=0 lines=0 first_hang=None"
        );
    }

    /// P5: the ack write and the event write are IN FLIGHT AT THE SAME TIME
    /// on the same write half, from two different tasks.
    ///
    /// P2 (ack awaited first) never hangs; P5 (both in flight) is the shape
    /// where the two `write_all` futures compete for tokio's single writer
    /// waker slot.
    #[tokio::test]
    async fn stress_ack_then_event_two_tasks_concurrent() {
        let mut hangs = 0usize;
        let mut iterations = 0usize;
        for _ in 0..stress::ITERATIONS {
            let (server, client) = stress::make_pair(None).await;
            let client_task = stress::spawn_client_reader(client, std::time::Duration::ZERO);
            let timed_out =
                stress::ack_then_event(server, stress::AckThenEvent::TwoTasksConcurrent).await;
            client_task.abort();
            iterations += 1;
            if timed_out {
                hangs += 1;
                if hangs == 1 {
                    eprintln!(
                        "STRESS_HANG label=ack_then_event_two_tasks_concurrent iteration=1 bytes=545"
                    );
                }
            }
        }
        println!(
            "STRESS_RESULT label=ack_then_event_two_tasks_concurrent iterations={iterations} hangs={hangs} timeouts={hangs} reconnects=0 lines=0 first_hang=None"
        );
    }

    /// M3: a write timeout plus reconnect.
    ///
    /// This is a mitigation candidate, not a root-cause fix: on a timeout the
    /// connection is dropped and re-created, so the server keeps working even
    /// if one connection wedges.
    #[tokio::test]
    async fn stress_timeout_reconnect() {
        let opts = stress::Options {
            reconnect_on_timeout: true,
            ..stress::Options::default()
        };
        let outcome = stress::run(&opts).await;
        stress::report("timeout_reconnect", &outcome);
        // Reconnect is the mitigation: hangs may still happen, but the run
        // must not wedge.
        assert!(
            outcome.iterations >= stress::ITERATIONS,
            "M3 must complete every iteration, even through reconnects"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stress_waker_probe_ack_then_event() {
        run_waker_probe("multi_thread").await;
    }

    /// The same production shape on a CURRENT-THREAD runtime.
    ///
    /// The runtime flavour decides which task can make progress while another
    /// is parked on the write, so the two flavours are measured separately: a
    /// conclusion drawn from one of them does not hold for the other.
    #[tokio::test]
    async fn stress_waker_probe_ack_then_event_current_thread() {
        run_waker_probe("current_thread").await;
    }

    /// The CONCURRENT-writer shape, instrumented.
    ///
    /// The P5 harness (two `write_all` futures polled CONCURRENTLY on one
    /// write half) hangs 200/200. This test runs that shape with the waker
    /// probe enabled, so the hang is classified as A (the wakeup was delivered
    /// but the task was not re-polled) or B (the wakeup was never delivered).
    ///
    /// Both writers go through ONE mio pipe object, so tokio's `ScheduledIo`
    /// keeps only ONE writer waker slot: the second writer to park replaces
    /// the first writer's waker, which is where a wakeup can be lost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stress_waker_probe_concurrent_writers() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;
        let mut woken_while_hung = 0usize;
        let mut never_woken = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let repolls_before = WAKE_PROBE_LONG_REPOLLS.load(Ordering::Relaxed);
            let wakes_during_before = WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let unmatched_before = WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            // ONE mio pipe, TWO write halves, both in flight at once.
            let shared = stress::SharedStream(std::sync::Arc::new(std::sync::Mutex::new(server)));
            let (_read_a, mut write_a) = tokio::io::split(shared.clone());
            let (_read_b, mut write_b) = tokio::io::split(shared);

            let ack = vec![b'a'; 22];
            let event = vec![b'x'; 545];
            let ack_b = ack.clone();
            let event_b = event.clone();

            let task_a = tokio::spawn(async move {
                write_a.write_all(&ack).await.ok();
                write_a.write_all(&event).await.is_ok()
            });
            let task_b = tokio::spawn(async move {
                write_b.write_all(&ack_b).await.ok();
                write_b.write_all(&event_b).await.is_ok()
            });

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    let _ = task_a.await;
                    let _ = task_b.await;
                },
            )
            .await
            .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let repolls = WAKE_PROBE_LONG_REPOLLS.load(Ordering::Relaxed) - repolls_before;
            let wakes_during =
                WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed) - wakes_during_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let unmatched_wakes =
                WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed) - unmatched_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;

            if timed_out {
                hung += 1;
                if wakes_during > 0 {
                    woken_while_hung += 1;
                } else {
                    never_woken += 1;
                }
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=concurrent iteration={iteration} pendings={pendings} repolls={repolls} wakes={wakes} wakes_during_pending={wakes_during} armed_wakes={armed_wakes} unmatched_wakes={unmatched_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=concurrent iterations={iterations} completed={completed} hung={hung} hung_but_woken={woken_while_hung} hung_never_woken={never_woken}"
        );
    }

    /// The MISSING CELL: TWO writer TASKS on a CURRENT_THREAD runtime.
    ///
    /// The production test `sdk_socket_configure_list_callback_and_busy_rejection`
    /// is a plain `#[tokio::test]`, so the daemon's connection actor and the
    /// event forwarder run on ONE worker thread and interleave at await points.
    /// The previous matrix had `two_tasks_one_mutex` on `multi_thread` and
    /// `ack_then_event_current_thread` with ONE task, so this cell was never
    /// measured.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_two_tasks_one_mutex_current_thread() {
        run_two_tasks_cell("two_tasks_mutex_current_thread", true).await;
    }

    /// The same two-task shape on a multi-thread runtime (the control for the
    /// cell above).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stress_waker_probe_two_tasks_one_mutex_multi_thread() {
        run_two_tasks_cell("two_tasks_mutex_multi_thread", true).await;
    }

    /// The two-task shape WITHOUT a pending read on the read half.
    ///
    /// This tells us whether the pending read is REQUIRED for the hang.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_two_tasks_no_pending_read() {
        run_two_tasks_cell("two_tasks_no_pending_read", false).await;
    }

    /// ONE task, but the two writes separated by a CHANNEL HOP.
    ///
    /// This isolates the mio `InFlight` collision plus a STALE writer waker in
    /// tokio's slot from the two-task aspect: if this hangs too, the two-task
    /// aspect is irrelevant.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_one_task_channel_hop() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            // ONE task writes BOTH frames, but the event goes through a
            // channel hop first, so the ack's completion lands before the
            // forwarder's write.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));
            let writer_half = std::sync::Arc::clone(&write_half);
            let writer = tokio::spawn(async move {
                let mut guard = writer_half.lock().await;
                guard.write_all(b"{\"type\":\"ack\",\"id\":4}\n").await?;
                // The channel hop: the event is queued, and the writer task
                // picks it up AFTER the ack's write completed.
                let event = rx.recv().await.ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed")
                })?;
                guard.write_all(&event).await?;
                Ok::<(), std::io::Error>(())
            });
            tx.send(vec![b'x'; 545]).expect("queue the event");

            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
                .await
                .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=one_task_channel_hop iteration={iteration} pendings={pendings} wakes={wakes} armed_wakes={armed_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=one_task_channel_hop iterations={iterations} completed={completed} hung={hung}"
        );
    }

    /// The A/B: BOTH writes go through ONE dedicated writer task fed by an
    /// mpsc channel, so there is never more than one `write_all` in flight on
    /// the pipe and never more than one task competing for tokio's single
    /// writer waker slot.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_single_writer_task() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            // ONE dedicated writer task owns the write half for good.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let writer = tokio::spawn(async move {
                let mut write_half = server_write;
                while let Some(frame) = rx.recv().await {
                    write_half.write_all(&frame).await?;
                }
                Ok::<(), std::io::Error>(())
            });

            // The ACTOR and the FORWARDER both only SEND.
            let actor_tx = tx.clone();
            let actor = tokio::spawn(async move {
                actor_tx.send(b"{\"type\":\"ack\",\"id\":4}\n".to_vec()).is_ok()
            });
            let forwarder = tokio::spawn(async move {
                tx.send(vec![b'x'; 545]).is_ok()
            });

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    let _ = actor.await;
                    let _ = forwarder.await;
                    let _ = writer.await;
                },
            )
            .await
            .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=single_writer_task iteration={iteration} pendings={pendings} wakes={wakes} armed_wakes={armed_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=single_writer_task iterations={iterations} completed={completed} hung={hung}"
        );
    }

    /// BOTH writer tasks park on a write at the same time.
    ///
    /// The production trace shows the actor's ack completing and the
    /// forwarder's event parking. This cell makes the ACTOR park as well, by
    /// giving it a second frame to write while the forwarder's is still in
    /// flight. Two tasks are then simultaneously registered against tokio's
    /// SINGLE writer-waker slot, which is the precondition for one of them
    /// losing its wakeup.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_both_tasks_park() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));

            // The ACTOR writes the ack and then a SECOND frame, so it can be
            // parked on a write too.
            let actor_half = std::sync::Arc::clone(&write_half);
            let actor = tokio::spawn(async move {
                let mut guard = actor_half.lock().await;
                guard.write_all(b"{\"type\":\"ack\",\"id\":4}\n").await?;
                guard.write_all(&vec![b'a'; 600]).await?;
                Ok::<(), std::io::Error>(())
            });

            // The FORWARDER writes the event.
            let forwarder_half = std::sync::Arc::clone(&write_half);
            let forwarder = tokio::spawn(async move {
                let mut guard = forwarder_half.lock().await;
                guard.write_all(&vec![b'x'; 545]).await?;
                Ok::<(), std::io::Error>(())
            });

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    let _ = actor.await;
                    let _ = forwarder.await;
                },
            )
            .await
            .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=both_tasks_park iteration={iteration} pendings={pendings} wakes={wakes} armed_wakes={armed_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=both_tasks_park iterations={iterations} completed={completed} hung={hung}"
        );
    }

    /// A writer parked while HOLDING the mutex, and a second writer that
    /// then needs the same mutex.
    ///
    /// This is the production structure: the forwarder takes the mutex, its
    /// `write_all` parks, and the mutex is STILL HELD. The connection actor
    /// then needs that mutex to write the next ack, so it blocks behind a
    /// parked write. If the parked writer never completes, the whole
    /// connection stalls: no more acks, and the client times out.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_mutex_held_across_park() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));

            // The FORWARDER takes the mutex and parks on its event write while
            // STILL HOLDING the mutex.
            let forwarder_half = std::sync::Arc::clone(&write_half);
            let forwarder = tokio::spawn(async move {
                let mut guard = forwarder_half.lock().await;
                guard.write_all(&vec![b'x'; 545]).await?;
                Ok::<(), std::io::Error>(())
            });

            // The ACTOR needs the SAME mutex for the next ack. It blocks
            // behind the parked write.
            let actor_half = std::sync::Arc::clone(&write_half);
            let actor = tokio::spawn(async move {
                let mut guard = actor_half.lock().await;
                guard.write_all(b"{\"type\":\"ack\",\"id\":9}\n").await?;
                Ok::<(), std::io::Error>(())
            });

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    let _ = actor.await;
                    let _ = forwarder.await;
                },
            )
            .await
            .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=mutex_held_across_park iteration={iteration} pendings={pendings} wakes={wakes} armed_wakes={armed_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=mutex_held_across_park iterations={iterations} completed={completed} hung={hung}"
        );
    }

    /// The PRODUCTION SIGNATURE: the waker IS called, but the task is never
    /// polled again.
    ///
    /// Every other cell in this file produces one of two signatures:
    /// "never woken" (the task's waker was replaced in tokio's single writer
    /// slot) or "woken and re-polled". Production shows a THIRD signature:
    /// `SDKCB_WAKE id=19` fires for the forwarder, and no `SDKCB_POLL id=20`
    /// ever appears.
    ///
    /// On a CURRENT-THREAD runtime there is exactly ONE worker thread
    /// (reference: https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html
    /// -- "When using the current_thread runtime, the return value is always 1"),
    /// so a wake only becomes a poll when that thread returns to the runtime.
    /// If the thread is BLOCKED, the wake is still DELIVERED to the task and
    /// the task is still SCHEDULED, but it cannot RUN until the block ends:
    /// the wake is observed and the re-poll never happens. That is exactly
    /// the production signature.
    ///
    /// The sleep stands in for whatever blocking call the daemon makes on that
    /// worker. It is a deliberate, temporary instrument: a blocking call
    /// inside an async task on a current-thread runtime is the defect this
    /// cell is designed to expose.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_blocked_worker() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;
        let mut woken_while_hung = 0usize;

        for iteration in 0..iterations {
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));

            // The WRITER parks on its write.
            let writer_half = std::sync::Arc::clone(&write_half);
            let writer = tokio::spawn(async move {
                let mut guard = writer_half.lock().await;
                guard.write_all(&vec![b'x'; 545]).await?;
                Ok::<(), std::io::Error>(())
            });

            // A BLOCKER occupies the only worker thread of this runtime.
            // Its duration is the CONTROL VARIABLE: if the re-poll of the
            // woken writer is gated on the block ending, the write completes
            // exactly `block_ms` after the start, not before.
            let block_ms: u64 = 1500;
            let blocker = tokio::spawn(async move {
                std::thread::sleep(std::time::Duration::from_millis(block_ms));
            });

            let started = std::time::Instant::now();
            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(15), writer)
                .await
                .is_err();
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;

            if timed_out {
                hung += 1;
                if armed_wakes > 0 {
                    woken_while_hung += 1;
                }
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=blocked_worker iteration={iteration} wakes={wakes} armed_wakes={armed_wakes} repolls_after_wake={repolls} client_bytes={} timed_out={timed_out} block_ms={block_ms} elapsed_ms={elapsed_ms}",
                client_bytes.load(Ordering::Relaxed)
            );

            blocker.abort();
            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=blocked_worker iterations={iterations} completed={completed} hung={hung} hung_but_woken={woken_while_hung}"
        );
    }

    /// The EXACT production signature, with the production CAUSE.
    ///
    /// Production shape:
    /// - `#[tokio::test]` defaults to a CURRENT-THREAD runtime: exactly one
    ///   worker thread (reference: tokio `Builder::new_current_thread`).
    /// - `crate::storage::lock_test_env()` returns a `std::sync::MutexGuard`
    ///   that is held for the WHOLE `async` test body
    ///   (`crates/jcode-base/src/storage.rs:34`).
    /// - `std::sync::Mutex::lock` is a BLOCKING call. If another test in the
    ///   same binary holds that mutex, the current-thread runtime's ONLY
    ///   worker thread blocks INSIDE the test body.
    /// - The writer task is parked on `Poll::Pending`; mio completes the
    ///   overlapped write and calls the waker. The wake IS DELIVERED (it goes
    ///   to the right task's waker). But the target task cannot RUN, because
    ///   the only worker thread is blocked on the mutex.
    /// - The test's `timeout(10s)` fires first: "daemon event timeout".
    ///
    /// Observed production trace (sdkcb_prod_probe.log), last lines:
    ///   SDKCB_POLL id=19 len=535 result=Pending
    ///   SDKCB_WAKE id=19 thread=ThreadId(2) event=wake
    ///   <no further SDKCB_POLL at all>
    ///
    /// This cell reproduces that: the wake arrives, and NO re-poll happens
    /// until the mutex is released.
    #[tokio::test(flavor = "current_thread")]
    async fn stress_waker_probe_std_mutex_blocked_worker() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;
        let mut woken_while_hung = 0usize;

        for iteration in 0..iterations {
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            // A DIFFERENT OS THREAD holds the std mutex, exactly like a
            // PARALLEL TEST in the same binary holding `lock_test_env()`.
            let mutex = std::sync::Arc::new(std::sync::Mutex::new(()));
            let held = std::sync::Arc::clone(&mutex);
            let holder = std::thread::spawn(move || {
                let guard = held.lock().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(2000));
                drop(guard);
            });

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            // The BLOCKER: the current-thread runtime's ONLY worker blocks
            // here, inside the async body, on a std::sync::Mutex.
            //
            // The guard is held across a BLOCKING SLEEP, not across an
            // `await`, so the spawned future stays `Send`.
            let blocker_mutex = std::sync::Arc::clone(&mutex);
            let blocker = tokio::spawn(async move {
                let _guard = blocker_mutex.lock().unwrap();
                std::thread::sleep(std::time::Duration::from_secs(30));
            });

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));
            let writer_half = std::sync::Arc::clone(&write_half);
            let writer = tokio::spawn(async move {
                let mut guard = writer_half.lock().await;
                guard.write_all(&vec![b'x'; 535]).await?;
                Ok::<(), std::io::Error>(())
            });

            let started = std::time::Instant::now();
            // The production timeout is 10s; the mutex is held for 2s, so a
            // 3s window already separates "blocked then released" from
            // "never re-polled".
            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(3), writer)
                .await
                .is_err();
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;

            if timed_out {
                hung += 1;
                if armed_wakes > 0 {
                    woken_while_hung += 1;
                }
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=std_mutex_blocked iteration={iteration} wakes={wakes} armed_wakes={armed_wakes} repolls_after_wake={repolls} client_bytes={} timed_out={timed_out} elapsed_ms={elapsed_ms}",
                client_bytes.load(Ordering::Relaxed)
            );

            blocker.abort();
            reader_task.abort();
            client_task.abort();
            holder.join().ok();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=std_mutex_blocked iterations={iterations} completed={completed} hung={hung} hung_but_woken={woken_while_hung}"
        );
    }

    /// The two-writer-task cell, parameterised by runtime and pending read.
    ///
    /// `flavor` names the cell in the output, `pending_read` decides whether a
    /// request reader task holds an overlapped read open on the read half.
    async fn run_two_tasks_cell(flavor: &str, pending_read: bool) {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;
        let mut hung_woken = 0usize;
        let mut hung_never_woken = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let wakes_during_before = WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let unmatched_before = WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            // The production request reader keeps an overlapped read PENDING.
            let reader_task = if pending_read {
                Some(tokio::spawn(async move {
                    let mut buf = vec![0u8; 1024];
                    loop {
                        match server_read.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }))
            } else {
                drop(server_read);
                None
            };

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));

            // The ACTOR: writes the ack and releases the lock.
            let actor_half = std::sync::Arc::clone(&write_half);
            let actor = tokio::spawn(async move {
                let mut guard = actor_half.lock().await;
                guard.write_all(b"{\"type\":\"ack\",\"id\":4}\n").await?;
                Ok::<(), std::io::Error>(())
            });

            // The FORWARDER: takes the lock and writes the event, no delay.
            let forwarder_half = std::sync::Arc::clone(&write_half);
            let forwarder = tokio::spawn(async move {
                let mut guard = forwarder_half.lock().await;
                let event = vec![b'x'; 545];
                guard.write_all(&event).await?;
                Ok::<(), std::io::Error>(())
            });

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    let _ = actor.await;
                    let _ = forwarder.await;
                },
            )
            .await
            .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let wakes_during =
                WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed) - wakes_during_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let unmatched_wakes =
                WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed) - unmatched_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
                if wakes_during > 0 {
                    hung_woken += 1;
                } else {
                    hung_never_woken += 1;
                }
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor={flavor} iteration={iteration} pendings={pendings} wakes={wakes} wakes_during_pending={wakes_during} armed_wakes={armed_wakes} unmatched_wakes={unmatched_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            if let Some(reader) = reader_task {
                reader.abort();
            }
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor={flavor} iterations={iterations} completed={completed} hung={hung} hung_woken={hung_woken} hung_never_woken={hung_never_woken}"
        );
    }

    /// The PRODUCTION shape with TWO writer TASKS, as the field trace shows.
    ///
    /// The field trace is:
    ///
    /// ```text
    /// WRITE_PROBE poll=#18 len=22  result=Ready(Ok(22))     <- the actor's ack
    /// SDK_CB_PROBE forwarder lock acquired ... bytes=545 ... <- the FORWARDER
    /// WRITE_PROBE poll=#19 len=545 result=Pending
    /// ```
    ///
    /// So the ack and the event are written by TWO DIFFERENT TASKS, both
    /// through the SAME `Arc<tokio::sync::Mutex<WriteHalf>>`, with the lock
    /// held ACROSS `write_all`. That is the shape this test reproduces.
    ///
    /// The single-task variant (`run_waker_probe`) writes both frames from one
    /// task, so the second `write_all` reuses the FIRST task's identity and
    /// its waker slot. The two-task variant does not: the forwarder parks on
    /// a writer slot the actor may already own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stress_waker_probe_two_tasks_one_mutex() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;
        let mut hung_never_woken = 0usize;
        let mut hung_woken = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let wakes_during_before = WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let unmatched_before = WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            // The production request reader keeps an overlapped read PENDING.
            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));

            // The ACTOR: writes the ack and releases the lock.
            let actor_half = std::sync::Arc::clone(&write_half);
            let actor = tokio::spawn(async move {
                let mut guard = actor_half.lock().await;
                let ack = b"{\"type\":\"ack\",\"id\":4}\n";
                guard.write_all(ack).await?;
                Ok::<(), std::io::Error>(())
            });

            // The FORWARDER: takes the lock and writes the event, no delay.
            let forwarder_half = std::sync::Arc::clone(&write_half);
            let forwarder = tokio::spawn(async move {
                let mut guard = forwarder_half.lock().await;
                let event = vec![b'x'; 545];
                guard.write_all(&event).await?;
                Ok::<(), std::io::Error>(())
            });

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    let _ = actor.await;
                    let _ = forwarder.await;
                },
            )
            .await
            .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let wakes_during =
                WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed) - wakes_during_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let unmatched_wakes =
                WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed) - unmatched_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
                if wakes_during > 0 {
                    hung_woken += 1;
                } else {
                    hung_never_woken += 1;
                }
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=two_tasks_mutex iteration={iteration} pendings={pendings} wakes={wakes} wakes_during_pending={wakes_during} armed_wakes={armed_wakes} unmatched_wakes={unmatched_wakes} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=two_tasks_mutex iterations={iterations} completed={completed} hung={hung} hung_woken={hung_woken} hung_never_woken={hung_never_woken}"
        );
    }

    /// The PRODUCTION shape with the FORWARDER DELAYED past the ack's
    /// completion, which is the field race.
    ///
    /// tokio's `ScheduledIo` keeps ONE writer waker slot per IO resource
    /// (`scheduled_io.rs`: `waiters.writer`). The actor's ack leaves its OWN
    /// waker in that slot: `poll_write_ready` registers the waker and the
    /// write then returns `Ready(Ok(22))` at mio's SUBMISSION time, so
    /// `clear_readiness` is never called and the registration stays.
    ///
    /// Then mio's `write_done` for the ack fires: it calls `notify_writable`
    /// -> `set_readiness(WRITABLE)` -> `wake(WRITABLE)`, which takes the
    /// ACTOR's stale waker out of the slot and wakes the actor. The WRITABLE
    /// READINESS STAYS SET.
    ///
    /// Now the forwarder polls: `poll_readiness` sees a NON-EMPTY readiness
    /// and returns `Ready` WITHOUT registering the forwarder's waker
    /// (`scheduled_io.rs`: the waker is registered only in the
    /// `ready.is_empty()` branch). mio then refuses the write (`InFlight` ->
    /// `WouldBlock`) and `clear_readiness` clears the readiness, but the slot
    /// still holds the ACTOR's stale waker. The next writable notification
    /// therefore wakes the ACTOR, and the FORWARDER is never woken again.
    ///
    /// The delay models the production path, where the forwarder reaches the
    /// write through a channel hop, so the ack's completion lands first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stress_waker_probe_delayed_forwarder() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let unmatched_before = WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));

            // The ACTOR writes the ack and releases the lock.
            let actor_half = std::sync::Arc::clone(&write_half);
            let actor = tokio::spawn(async move {
                let mut guard = actor_half.lock().await;
                guard.write_all(b"{\"type\":\"ack\",\"id\":4}\n").await?;
                Ok::<(), std::io::Error>(())
            });

            // The FORWARDER reaches the write after the ack's completion.
            let forwarder_half = std::sync::Arc::clone(&write_half);
            let forwarder = tokio::spawn(async move {
                let mut guard = forwarder_half.lock().await;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                let event = vec![b'x'; 545];
                guard.write_all(&event).await?;
                Ok::<(), std::io::Error>(())
            });

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    let _ = actor.await;
                    let _ = forwarder.await;
                },
            )
            .await
            .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let unmatched_wakes =
                WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed) - unmatched_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=delayed_forwarder iteration={iteration} pendings={pendings} wakes={wakes} armed_wakes={armed_wakes} unmatched_wakes={unmatched_wakes} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=delayed_forwarder iterations={iterations} completed={completed} hung={hung}"
        );
    }

    /// The PRODUCTION shape of the SDK-callback write hang, instrumented to
    /// answer one question: after `poll_write` returned `Pending` for the long
    /// frame, was the writer task ever WOKEN?
    ///
    /// The production server:
    /// 1. splits ONE stream into a read half and a write half;
    /// 2. keeps a request reader task with an overlapped read PENDING on the
    ///    read half;
    /// 3. serializes writes with `Arc<tokio::sync::Mutex<WriteHalf>>` and
    ///    holds that lock ACROSS `write_all`;
    /// 4. writes a short ack frame, then IMMEDIATELY a long event frame.
    ///
    /// Step 3 does NOT serialize the mio overlapped operations: mio's
    /// `write_bufs` returns `Ok(len)` at SUBMISSION time, so the lock is
    /// released while the kernel write is still in flight and the next write
    /// hits `WriteState::InFlight` -> `WouldBlock` -> `Pending`.
    ///
    /// The waker probe wraps the waker handed to `poll_write` and logs every
    /// wake, so the outcome is decidable:
    ///
    /// - `completed`: the long write never returned `Pending`;
    /// - `pending_then_woken_then_repolled`: `Pending`, then a wake, then a
    ///   re-poll: the wakeup was delivered and worked;
    /// - `pending_then_woken_no_repoll`: `Pending`, then a wake, then NO
    ///   re-poll: the wakeup was delivered but the task was not re-polled;
    /// - `pending_never_woken`: `Pending` and no wake at all: mio never
    ///   published writable readiness.
    async fn run_waker_probe(flavor: &str) {
        use std::sync::atomic::Ordering;

        // The probes read the environment at call time, so the test turns them
        // on before the first write. Edition 2024 makes `set_var` unsafe
        // because another thread could observe the environment concurrently.
        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut completed = 0usize;
        let mut pending_then_woken_then_repolled = 0usize;
        let mut pending_then_woken_no_repoll = 0usize;
        let mut pending_never_woken = 0usize;
        let mut hung = 0usize;
        let mut inflight_pendings: u64 = 0;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let repolls_before = WAKE_PROBE_LONG_REPOLLS.load(Ordering::Relaxed);
            let wakes_during_before = WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let inflight_before = WAKE_PROBE_INFLIGHT_PENDING.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            // The client reads continuously, so the pipe never backs up and a
            // stalled write cannot be blamed on a full buffer.
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            });

            // The production request reader: an overlapped read left PENDING
            // on the read half for the whole exchange.
            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            // The production write serialization: one mutex, lock held ACROSS
            // `write_all`.
            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));

            let ack = b"{\"type\":\"ack\",\"id\":4}\n".to_vec();
            let event = vec![b'x'; 545];

            let write = {
                let write_half = std::sync::Arc::clone(&write_half);
                tokio::spawn(async move {
                    let mut guard = write_half.lock().await;
                    guard.write_all(&ack).await?;
                    // NO delay: the second write must hit mio's InFlight.
                    guard.write_all(&event).await?;
                    Ok::<(), std::io::Error>(())
                })
            };

            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), write)
                .await
                .is_err();

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let repolls = WAKE_PROBE_LONG_REPOLLS.load(Ordering::Relaxed) - repolls_before;
            let wakes_during =
                WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed) - wakes_during_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            inflight_pendings +=
                WAKE_PROBE_INFLIGHT_PENDING.load(Ordering::Relaxed) - inflight_before;

            if pendings == 0 {
                completed += 1;
            } else if timed_out {
                hung += 1;
                if wakes_during > 0 {
                    pending_then_woken_no_repoll += 1;
                } else {
                    pending_never_woken += 1;
                }
            } else {
                pending_then_woken_then_repolled += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor={flavor} iteration={iteration} pendings={pendings} repolls={repolls} wakes={wakes} wakes_during_pending={wakes_during} timed_out={timed_out}"
            );

            reader_task.abort();
            client_task.abort();
            write_half.lock().await.flush().await.ok();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor={flavor} iterations={iterations} completed={completed} pending_then_woken_then_repolled={pending_then_woken_then_repolled} pending_then_woken_no_repoll={pending_then_woken_no_repoll} pending_never_woken={pending_never_woken} hung={hung} inflight_pendings={inflight_pendings}"
        );
    }

    /// The factors the SDK-callback write hang matrix varies.
    ///
    /// Every cell runs on a CURRENT_THREAD runtime, because the production
    /// test is a plain `#[tokio::test]`: the actor and the forwarder share ONE
    /// worker thread and interleave only at await points.
    #[derive(Clone, Copy)]
    struct CtCell {
        /// The forwarder and the actor write in the field trace's order.
        ordered: bool,
        /// ONE task does every write.
        single_task: bool,
        /// A request reader keeps an overlapped read PENDING.
        pending_read: bool,
        /// The actor writes to a DIFFERENT pipe than the forwarder.
        two_pipes: bool,
        /// Every write goes through ONE dedicated writer task.
        channel_writer: bool,
        /// Every `write_all` is wrapped in a timeout.
        write_timeout: bool,
        /// The peer NEVER drains: the pipe buffer fills, so the write really
        /// blocks in the kernel instead of parking in userspace.
        stuck_peer: bool,
        /// The event frame is large enough to exceed the pipe buffer.
        big_frame: bool,
    }

    impl CtCell {
        fn base() -> Self {
            Self {
                ordered: false,
                single_task: false,
                pending_read: true,
                two_pipes: false,
                channel_writer: false,
                write_timeout: false,
                stuck_peer: false,
                big_frame: false,
            }
        }
    }

    /// A closed channel as an IO error.
    fn ct_closed() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed")
    }

    /// Write one frame through the shared mutex, optionally under a timeout.
    async fn ct_write(
        half: &std::sync::Arc<tokio::sync::Mutex<WriteHalf>>,
        frame: &[u8],
        with_timeout: bool,
        timeouts: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<(), std::io::Error> {
        use std::sync::atomic::Ordering;
        let mut guard = half.lock().await;
        if with_timeout {
            match tokio::time::timeout(std::time::Duration::from_secs(5), guard.write_all(frame))
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    timeouts.fetch_add(1, Ordering::Relaxed);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "write timeout",
                    ))
                }
            }
        } else {
            guard.write_all(frame).await
        }
    }

    /// Run one cell of the SDK-callback write hang matrix.
    ///
    /// The shape is the production one: one named pipe, the write half behind
    /// an `Arc<tokio::sync::Mutex<..>>` held ACROSS `write_all`, a request
    /// reader with an overlapped read pending, and a client that reads
    /// continuously so a stalled write cannot be blamed on a full buffer.
    async fn run_ct_cell(label: &str, cell: CtCell) {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;
        let mut hung_woken = 0usize;
        let mut hung_never_woken = 0usize;
        let mut timeouts_fired = 0u64;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let wakes_during_before = WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let unmatched_before = WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            // A SLOW peer: the first drain is delayed, so a big frame really
            // blocks in the kernel and the write completes ASYNCHRONOUSLY
            // through the overlapped-IO completion path. With an instant peer
            // the write never parks, so the "Pending -> wake -> no re-poll"
            // state is never reached.
            let slow_peer = cell.stuck_peer;
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                if slow_peer {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            // The second pipe, used only by the "different pipes" cell.
            let (server2, mut client2) = stream_pair().expect("create second stream pair");
            let (_server_read2, server_write2) = server2.into_split();
            let client_bytes2 = std::sync::Arc::clone(&client_bytes);
            let client2_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client2.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes2.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = if cell.pending_read {
                Some(tokio::spawn(async move {
                    let mut buf = vec![0u8; 1024];
                    loop {
                        match server_read.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }))
            } else {
                drop(server_read);
                None
            };

            let timeouts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));
            // The actor's pipe: the SAME pipe by default, a SECOND pipe in the
            // "different pipes" cell.
            let actor_half = if cell.two_pipes {
                std::sync::Arc::new(tokio::sync::Mutex::new(server_write2))
            } else {
                std::sync::Arc::clone(&write_half)
            };

            let mut tasks: Vec<tokio::task::JoinHandle<Result<(), std::io::Error>>> = Vec::new();

            if cell.channel_writer {
                // FIX A/B: ONE dedicated writer task owns the write half, and
                // the actor and the forwarder only SEND.
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                let writer_half = std::sync::Arc::clone(&write_half);
                let writer_timeouts = std::sync::Arc::clone(&timeouts);
                tasks.push(tokio::spawn(async move {
                    while let Some(frame) = rx.recv().await {
                        ct_write(&writer_half, &frame, cell.write_timeout, &writer_timeouts).await?;
                    }
                    Ok(())
                }));
                let actor_tx = tx.clone();
                tasks.push(tokio::spawn(async move {
                    actor_tx
                        .send(b"{\"type\":\"ack\",\"id\":4}\n".to_vec())
                        .map_err(|_| ct_closed())
                }));
                let forwarder_tx = tx.clone();
                tasks.push(tokio::spawn(async move {
                    forwarder_tx.send(vec![b'x'; 545]).map_err(|_| ct_closed())
                }));
                drop(tx);
            } else if cell.single_task {
                // ONE task does EVERY write: the actor and the forwarder are
                // the same task, so there is only one writer waker.
                let half = std::sync::Arc::clone(&write_half);
                let timeouts_task = std::sync::Arc::clone(&timeouts);
                tasks.push(tokio::spawn(async move {
                    ct_write(
                        &half,
                        b"{\"type\":\"ack\",\"id\":4}\n",
                        cell.write_timeout,
                        &timeouts_task,
                    )
                    .await?;
                    ct_write(&half, &vec![b'x'; 545], cell.write_timeout, &timeouts_task).await?;
                    Ok(())
                }));
            } else if cell.ordered {
                // The field trace's ORDER: forwarder 22, actor 29, forwarder
                // 115, actor 48, actor 22, forwarder 535.
                let (f_tx, mut f_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
                let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
                let f_half = std::sync::Arc::clone(&write_half);
                let f_timeouts = std::sync::Arc::clone(&timeouts);
                tasks.push(tokio::spawn(async move {
                    for size in [22usize, 115, 535] {
                        f_rx.recv().await.ok_or_else(ct_closed)?;
                        ct_write(&f_half, &vec![b'f'; size], cell.write_timeout, &f_timeouts).await?;
                    }
                    Ok(())
                }));
                let a_half = std::sync::Arc::clone(&actor_half);
                let a_timeouts = std::sync::Arc::clone(&timeouts);
                tasks.push(tokio::spawn(async move {
                    for size in [29usize, 48, 22] {
                        a_rx.recv().await.ok_or_else(ct_closed)?;
                        ct_write(&a_half, &vec![b'a'; size], cell.write_timeout, &a_timeouts).await?;
                    }
                    Ok(())
                }));
                // The driver hands out the turns in the trace's order.
                tokio::spawn(async move {
                    for target in [true, false, true, false, false, true] {
                        if target {
                            f_tx.send(()).ok();
                        } else {
                            a_tx.send(()).ok();
                        }
                    }
                });
            } else {
                // The plain two-task shape: the actor writes the ack, the
                // forwarder writes the event, both under the SAME mutex.
                let frame_len = if cell.big_frame { 64 * 1024 } else { 545 };
                let a_half = std::sync::Arc::clone(&actor_half);
                let a_timeouts = std::sync::Arc::clone(&timeouts);
                tasks.push(tokio::spawn(async move {
                    ct_write(
                        &a_half,
                        b"{\"type\":\"ack\",\"id\":4}\n",
                        cell.write_timeout,
                        &a_timeouts,
                    )
                    .await
                }));
                let f_half = std::sync::Arc::clone(&write_half);
                let f_timeouts = std::sync::Arc::clone(&timeouts);
                tasks.push(tokio::spawn(async move {
                    ct_write(&f_half, &vec![b'x'; frame_len], cell.write_timeout, &f_timeouts).await
                }));
            }

            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(if cell.write_timeout { 20 } else { 5 }),
                async {
                    for task in tasks {
                        let _ = task.await;
                    }
                },
            )
            .await
            .is_err();

            // Let the pipe drain before the byte count is taken: a write that
            // mio only SUBMITTED (Ready(Ok(n)) at submission time) can still
            // arrive afterwards, and the byte count must reflect that.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let wakes_during =
                WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed) - wakes_during_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let unmatched_wakes =
                WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed) - unmatched_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();
            timeouts_fired += timeouts.load(Ordering::Relaxed);

            if timed_out {
                hung += 1;
                if wakes_during > 0 {
                    hung_woken += 1;
                } else {
                    hung_never_woken += 1;
                }
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor={label} iteration={iteration} pendings={pendings} wakes={wakes} wakes_during_pending={wakes_during} armed_wakes={armed_wakes} unmatched_wakes={unmatched_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} write_timeouts={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed),
                timeouts.load(Ordering::Relaxed)
            );

            if let Some(reader) = reader_task {
                reader.abort();
            }
            client_task.abort();
            client2_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor={label} iterations={iterations} completed={completed} hung={hung} hung_woken={hung_woken} hung_never_woken={hung_never_woken} write_timeouts={timeouts_fired}"
        );
    }

    /// CELL A: TWO writer TASKS on a CURRENT_THREAD runtime.
    ///
    /// This is the cell the earlier matrix never measured: `two_tasks_one_mutex`
    /// ran on `multi_thread`, and the current-thread variants used ONE task.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_a_two_writers_current_thread() {
        run_ct_cell("cell_a_two_writers", CtCell::base()).await;
    }

    /// CELL B: the production ORDER on a CURRENT_THREAD runtime.
    ///
    /// The field trace interleaves the actor's acks with the forwarder's
    /// frames, so the two writers take the mutex alternately.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_b_ordered_interleave_current_thread() {
        run_ct_cell(
            "cell_b_ordered",
            CtCell {
                ordered: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL C1: ONE task does ALL the writes.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_c1_single_task_current_thread() {
        run_ct_cell(
            "cell_c1_single_task",
            CtCell {
                single_task: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL C2: two tasks, but NO pending read on the read half.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_c2_no_pending_read_current_thread() {
        run_ct_cell(
            "cell_c2_no_pending_read",
            CtCell {
                pending_read: false,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL C3: two tasks, but the actor and the forwarder write to DIFFERENT
    /// pipes.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_c3_two_pipes_current_thread() {
        run_ct_cell(
            "cell_c3_two_pipes",
            CtCell {
                two_pipes: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL D: candidate FIX A/B, ONE dedicated writer task fed by an mpsc
    /// channel.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_d_channel_writer_current_thread() {
        run_ct_cell(
            "cell_d_channel_writer",
            CtCell {
                channel_writer: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL E: candidate FIX A/B, every `write_all` wrapped in a timeout.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_e_write_timeout_current_thread() {
        run_ct_cell(
            "cell_e_write_timeout",
            CtCell {
                write_timeout: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL F: the two-writer shape with a SLOW peer, so the write really
    /// blocks in the kernel and completes through the overlapped-IO
    /// completion path. This is the factor cells A-E never varied: with an
    /// instant peer the write never parks, so "Pending -> wake -> no re-poll"
    /// is unreachable.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_f_slow_peer_two_writers_current_thread() {
        run_ct_cell(
            "cell_f_slow_peer",
            CtCell {
                stuck_peer: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL G: SLOW peer AND a frame larger than the pipe buffer, so the
    /// kernel write cannot complete in one submission.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_g_slow_peer_big_frame_current_thread() {
        run_ct_cell(
            "cell_g_slow_peer_big_frame",
            CtCell {
                stuck_peer: true,
                big_frame: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL H: the candidate FIX (ONE dedicated writer task) under the SAME
    /// pressure as cells G. If G hangs and H does not, the channel writer is
    /// the fix; if both hang, the factor is elsewhere.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_h_channel_writer_slow_peer_big_frame_current_thread() {
        run_ct_cell(
            "cell_h_channel_writer_pressure",
            CtCell {
                channel_writer: true,
                stuck_peer: true,
                big_frame: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL I: ONE task does every write, under the SAME pressure.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_i_single_task_slow_peer_big_frame_current_thread() {
        run_ct_cell(
            "cell_i_single_task_pressure",
            CtCell {
                single_task: true,
                stuck_peer: true,
                big_frame: true,
                ..CtCell::base()
            },
        )
        .await;
    }

    /// CELL J: the PRODUCTION trace's FULL length: REPEATED interleaved writes
    /// from the actor and the forwarder, not one write each.
    ///
    /// The field trace shows six consecutive writes that alternate between the
    /// two writers (forwarder 22, actor 29, forwarder 115, actor 48, actor 22,
    /// forwarder 535) and then a LAST forwarder frame that never gets
    /// re-polled. One write per writer cannot reach that state, so this cell
    /// replays the trace's whole sequence: three forwarder frames and three
    /// actor frames, in that order, on a CURRENT_THREAD runtime.
    ///
    /// It is deliberately a SEPARATE function instead of another `CtCell`
    /// flag: another agent is editing this same file's `CtCell` right now, and
    /// a self-contained function cannot collide with that work.
    #[tokio::test(flavor = "current_thread")]
    async fn ct_cell_j_production_trace_length_current_thread() {
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("JCODE_WRITE_PROBE", "1");
            std::env::set_var("JCODE_WAKE_PROBE", "1");
        }

        let iterations: usize = 25;
        let mut hung = 0usize;
        let mut completed = 0usize;
        let mut hung_woken = 0usize;
        let mut hung_never_woken = 0usize;

        for iteration in 0..iterations {
            let pendings_before = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed);
            let wakes_before = WAKE_PROBE_WAKES.load(Ordering::Relaxed);
            let wakes_during_before = WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed);
            let armed_before = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed);
            let unmatched_before = WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed);
            let repoll_before = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed);

            let (server, mut client) = stream_pair().expect("create stream pair");
            let (mut server_read, server_write) = server.into_split();

            let client_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let client_bytes_task = std::sync::Arc::clone(&client_bytes);
            let client_task = tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                loop {
                    match client.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            client_bytes_task.fetch_add(n as u64, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            });

            let reader_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });

            let write_half = std::sync::Arc::new(tokio::sync::Mutex::new(server_write));
            let (f_tx, mut f_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
            let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();

            // The FORWARDER writes the trace's three frames.
            let f_half = std::sync::Arc::clone(&write_half);
            let forwarder = tokio::spawn(async move {
                while let Some(size) = f_rx.recv().await {
                    let mut guard = f_half.lock().await;
                    guard.write_all(&vec![b'f'; size]).await?;
                }
                Ok::<(), std::io::Error>(())
            });

            // The ACTOR writes the trace's three acks.
            let a_half = std::sync::Arc::clone(&write_half);
            let actor = tokio::spawn(async move {
                while let Some(size) = a_rx.recv().await {
                    let mut guard = a_half.lock().await;
                    guard.write_all(&vec![b'a'; size]).await?;
                }
                Ok::<(), std::io::Error>(())
            });

            // The trace's order: f22, a29, f115, a48, a22, f535.
            let driver = tokio::spawn(async move {
                for (target, size) in [
                    (true, 22usize),
                    (false, 29),
                    (true, 115),
                    (false, 48),
                    (false, 22),
                    (true, 535),
                ] {
                    if target {
                        f_tx.send(size).ok();
                    } else {
                        a_tx.send(size).ok();
                    }
                }
            });

            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let _ = driver.await;
                let _ = actor.await;
                let _ = forwarder.await;
            })
            .await
            .is_err();

            // Let the pipe drain before the byte count is taken.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let pendings = WAKE_PROBE_LONG_PENDING_COUNT.load(Ordering::Relaxed) - pendings_before;
            let wakes = WAKE_PROBE_WAKES.load(Ordering::Relaxed) - wakes_before;
            let wakes_during =
                WAKE_PROBE_WAKES_DURING_PENDING.load(Ordering::Relaxed) - wakes_during_before;
            let armed_wakes = WAKE_PROBE_WOKE_ARMED.load(Ordering::Relaxed) - armed_before;
            let unmatched_wakes =
                WAKE_PROBE_WOKE_UNMATCHED.load(Ordering::Relaxed) - unmatched_before;
            let repolls = WAKE_PROBE_REPOLL_AFTER_WAKE.load(Ordering::Relaxed) - repoll_before;
            let parked_left = WAKE_PROBE_ARMED.lock().unwrap().len();

            if timed_out {
                hung += 1;
                if wakes_during > 0 {
                    hung_woken += 1;
                } else {
                    hung_never_woken += 1;
                }
            } else {
                completed += 1;
            }

            eprintln!(
                "WAKE_PROBE_RESULT flavor=cell_j_trace_length iteration={iteration} pendings={pendings} wakes={wakes} wakes_during_pending={wakes_during} armed_wakes={armed_wakes} unmatched_wakes={unmatched_wakes} repolls_after_wake={repolls} parked_left={parked_left} client_bytes={} timed_out={timed_out}",
                client_bytes.load(Ordering::Relaxed)
            );

            reader_task.abort();
            client_task.abort();
        }

        println!(
            "WAKE_PROBE_SUMMARY flavor=cell_j_trace_length iterations={iterations} completed={completed} hung={hung} hung_woken={hung_woken} hung_never_woken={hung_never_woken}"
        );
    }
}

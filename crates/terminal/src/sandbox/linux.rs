//! Linux network-isolation backend (audit 4/28/35-39), compiled only on
//! `target_os = "linux"`.
//!
//! `NetworkIsolation::DenyAll` spawns install this pre-exec hook: after
//! fork, before exec, the child calls `unshare(CLONE_NEWNET)` and drops
//! into a FRESH network namespace. An empty netns is adequate: the new
//! namespace contains only loopback, and loopback is left DOWN by the
//! kernel — no bring-up is performed, so no TCP or UDP egress can leave
//! the child (a connect() to any address fails at the route/device layer).
//! No network setup code of any kind runs here.
//!
//! Fail-closed contract: if the kernel or the user-namespace policy
//! refuses the unshare (EPERM, EINVAL, ENOSYS, ...), the closure returns
//! the raw OS error, `spawn()` fails, and the supervisor surfaces a typed
//! permission refusal — the child NEVER runs unenforced and nothing is
//! logged as a warning-and-continue. std transports only the raw errno
//! out of the pre-exec child, so the caller classifies any DenyAll spawn
//! failure as the typed refusal (the OS message still names the cause).
//!
//! The closure runs in the single-threaded post-fork child and only calls
//! `unshare` and reads the errno — no allocation, no locks.

#![allow(unsafe_code)] // platform authority module: every unsafe
                       // block/function in this module carries a `// SAFETY:` justification and is
                       // enumerated by tests/static-authority.
use std::io;
use std::os::unix::process::CommandExt;

/// Test-only simulation of a kernel/user-namespace refusal: set before a
/// `DenyAll` spawn to prove the fail-closed path (typed refusal, no exec).
/// The pre-exec hook only reads this atomic — no allocation, no locks.
#[cfg(test)]
static FORCE_UNSHARE_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Force the next unshare pre-exec calls to fail with EPERM (tests).
#[cfg(test)]
pub(crate) fn force_unshare_failure_for_tests(force: bool) {
    FORCE_UNSHARE_FAILURE.store(force, std::sync::atomic::Ordering::SeqCst);
}

/// Install the deny-all network isolation pre-exec hook on `cmd`.
///
/// # Safety
///
/// The installed closure runs in the forked child between fork and exec:
/// it must not allocate, lock, or call anything but async-signal-safe
/// operations. It calls `libc::unshare(CLONE_NEWNET)` and reads the
/// errno — nothing else.
// SAFETY: the arguments were validated by the caller per this function's documented contract and the call has no additional aliasing or lifetime requirements.
pub(crate) unsafe fn apply_deny_all_isolation(cmd: &mut std::process::Command) {
    // SAFETY (of the pre_exec call): std requires the caller to uphold the
    // pre-exec restrictions; the closure below is allocation-free and
    // single-purpose (documented above).
    cmd.pre_exec(unshare_netns_pre_exec);
}

fn unshare_netns_pre_exec() -> io::Result<()> {
    #[cfg(test)]
    if FORCE_UNSHARE_FAILURE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    // SAFETY: unshare(2) takes no pointer arguments; the raw errno read
    // after failure is async-signal-safe.
    let ret = unsafe { libc::unshare(libc::CLONE_NEWNET) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// BrokerOnly backend (audit item 8): a dedicated network namespace whose ONLY
// reachable endpoint is the Faktor broker.
//
// `NetworkIsolation::BrokerOnly { endpoint }` spawns install a pre-exec hook
// that `setns()`es the child into a namespace prepared by
// [`BrokerOnlyInner::start`]. That namespace contains loopback (brought UP)
// and NOTHING else: no interfaces, no routes, so no external destination is
// reachable — a raw socket in the child cannot leave the sandbox.
//
// The broker endpoint itself stays reachable through a relay bridge across
// the namespace boundary:
//
// * the namespace side binds a TCP listener on the broker endpoint (a
//   loopback literal; the port is free by construction because the namespace
//   is fresh) and, per accepted connection, opens a per-connection AF_UNIX
//   stream to the host side (AF_UNIX is not network-namespaced, so the shared
//   0700 temp-dir path is visible on both sides) and writes a one-line
//   forwarding header;
// * the host side reads the header and relays the bytes to the configured
//   broker endpoint in the daemon's namespace (the real broker).
//
// The same bridge carries the reverse direction for the browser's DevTools
// control channel: `expose_loopback_port` binds a host-side 127.0.0.1
// listener, and each accepted host connection is relayed to the announced
// in-sandbox port through the ns-side listener (the ns side dials
// 127.0.0.1:<port>` INSIDE the namespace and pumps bytes).
//
// Fail-closed contract: `unshare(CLONE_NEWNET)` (or loopback bring-up, or
// listener binding) failure refuses the spawn typed BEFORE exec — the child
// never runs outside the namespace. `setns()` failure at exec is surfaced by
// the spawn path as the same typed permission refusal.
//
// Bounded everything: relay slots are capped (`MAX_RELAYS`), every listener
// loop polls a stop flag, and `Drop` shuts down live relays and releases the
// listeners — the namespace thread exits within one poll interval. Threads
// are deliberately not joined from `Drop` (it may run under supervisor
// locks); they are self-terminating and the stop flag is checked on every
// iteration, so no thread or namespace outlives the bridge by more than the
// poll bound.

mod broker_only {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Hard ceiling on concurrent relayed connections (across both
    /// directions). Past the ceiling a new connection is refused by closing
    /// it; the sandbox never accumulates unbounded relay state.
    pub(crate) const MAX_RELAYS: usize = 256;

    /// Listener poll interval. All accept loops are non-blocking and check
    /// the stop flag at least this often, so teardown is bounded.
    const POLL: Duration = Duration::from_millis(10);

    /// Longest forwarding header we accept (`"B\n"` or `"C <port>\n"`), plus
    /// a bound so a hostile peer can never feed us an unbounded header.
    const HEADER_MAX: usize = 16;

    /// Connect/read bound for the header and the in-namespace dial.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

    /// One relay endpoint (a TCP socket or an AF_UNIX socket).
    pub(crate) enum RelayEnd {
        Tcp(TcpStream),
        Unix(UnixStream),
    }

    impl RelayEnd {
        fn try_clone(&self) -> Option<RelayEnd> {
            match self {
                RelayEnd::Tcp(s) => s.try_clone().ok().map(RelayEnd::Tcp),
                RelayEnd::Unix(s) => s.try_clone().ok().map(RelayEnd::Unix),
            }
        }

        fn shutdown(&self) {
            match self {
                RelayEnd::Tcp(s) => {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
                RelayEnd::Unix(s) => {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }

    impl Read for RelayEnd {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self {
                RelayEnd::Tcp(s) => s.read(buf),
                RelayEnd::Unix(s) => s.read(buf),
            }
        }
    }

    impl Write for RelayEnd {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                RelayEnd::Tcp(s) => s.write(buf),
                RelayEnd::Unix(s) => s.write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self {
                RelayEnd::Tcp(s) => s.flush(),
                RelayEnd::Unix(s) => s.flush(),
            }
        }
    }

    /// Shared relay state: slots hold shutdown handles for live relays (so
    /// `Drop` can wake every pump), `active` bounds the live count.
    pub(crate) struct RelayState {
        slots: Mutex<Vec<Option<RelayEnd>>>,
        active: AtomicUsize,
        stop: AtomicBool,
    }

    impl RelayState {
        fn new() -> Arc<RelayState> {
            Arc::new(RelayState {
                slots: Mutex::new(Vec::new()),
                active: AtomicUsize::new(0),
                stop: AtomicBool::new(false),
            })
        }

        /// Admit one relay. `None` when the ceiling is reached or the bridge
        /// is stopping: the caller closes the connection.
        fn admit(self: &Arc<Self>, ends: &[&RelayEnd]) -> Option<RelayGuard> {
            if self.stop.load(Ordering::SeqCst) {
                return None;
            }
            let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            if self.active.load(Ordering::SeqCst) >= MAX_RELAYS {
                return None;
            }
            let mut indices = Vec::with_capacity(ends.len());
            for end in ends {
                let Some(clone) = end.try_clone() else {
                    for i in &indices {
                        slots[*i] = None;
                    }
                    return None;
                };
                let idx = match slots.iter().position(Option::is_none) {
                    Some(idx) => {
                        slots[idx] = Some(clone);
                        idx
                    }
                    None => {
                        slots.push(Some(clone));
                        slots.len() - 1
                    }
                };
                indices.push(idx);
            }
            self.active.fetch_add(1, Ordering::SeqCst);
            drop(slots);
            Some(RelayGuard {
                state: self.clone(),
                indices,
            })
        }

        /// Stop every live relay and the listeners (idempotent).
        pub(crate) fn stop(&self) {
            self.stop.store(true, Ordering::SeqCst);
            let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            for end in slots.iter().flatten() {
                end.shutdown();
            }
        }
    }

    /// One admitted relay slot set: cleared (and the live count released)
    /// when the relay finishes.
    struct RelayGuard {
        state: Arc<RelayState>,
        indices: Vec<usize>,
    }

    impl Drop for RelayGuard {
        fn drop(&mut self) {
            let mut slots = self.state.slots.lock().unwrap_or_else(|p| p.into_inner());
            for idx in &self.indices {
                slots[*idx] = None;
            }
            self.state.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// The namespace-side half of the bridge and its host-side listeners.
    pub(crate) struct Inner {
        /// 0700 temp dir shared by both namespace sides (AF_UNIX paths are
        /// not network-namespaced).
        _dir: tempfile::TempDir,
        ns_path: PathBuf,
        state: Arc<RelayState>,
        /// The namespace fd the pre-exec hook `setns()`es into (kept open for
        /// the whole bridge lifetime; the hook captures the raw fd).
        ns_fd: std::fs::File,
    }

    // SAFETY note for the raw-fd access: the fd is owned by `ns_fd` and kept
    // open for the bridge's whole lifetime; pre-exec callers only read it.
    impl Inner {
        pub(crate) fn start(endpoint: SocketAddr) -> std::io::Result<Inner> {
            if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "BrokerOnly endpoint must be a non-zero loopback address",
                ));
            }
            let dir = tempfile::Builder::new()
                .prefix("faktor-broker-only-")
                .tempdir()?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
            }
            let host_path = dir.path().join("host.sock");
            let ns_path = dir.path().join("ns.sock");
            let host_listener = UnixListener::bind(&host_path)?;
            host_listener.set_nonblocking(true)?;
            let state = RelayState::new();
            let (tx, rx) = std::sync::mpsc::channel();
            {
                let state = state.clone();
                let ns_path = ns_path.clone();
                std::thread::Builder::new()
                    .name("faktor-broker-only-ns".to_string())
                    .spawn(move || ns_thread(endpoint, ns_path, host_path, state, tx))?;
            }
            let ready = match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(ready) => ready,
                Err(_) => {
                    state.stop();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "BrokerOnly namespace did not become ready",
                    ));
                }
            };
            let ns_fd = match ready {
                Ok(ns_fd) => ns_fd,
                Err(e) => {
                    state.stop();
                    return Err(e);
                }
            };
            let host_state = state.clone();
            std::thread::Builder::new()
                .name("faktor-broker-only-host".to_string())
                .spawn(move || host_accept_loop(host_listener, endpoint, host_state))?;
            Ok(Inner {
                _dir: dir,
                ns_path,
                state,
                ns_fd,
            })
        }

        /// The namespace fd for the pre-exec `setns()` hook.
        pub(crate) fn ns_fd(&self) -> std::os::fd::RawFd {
            use std::os::fd::AsRawFd;
            self.ns_fd.as_raw_fd()
        }

        /// Expose an in-sandbox loopback port on a fresh host-side 127.0.0.1
        /// port (the DevTools control channel direction). Returns the host
        /// port; connections to it are relayed into the namespace.
        pub(crate) fn expose_loopback_port(&self, in_sandbox_port: u16) -> std::io::Result<u16> {
            if in_sandbox_port == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "port 0 cannot be exposed",
                ));
            }
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let host_port = listener.local_addr()?.port();
            listener.set_nonblocking(true)?;
            let ns_path = self.ns_path.clone();
            let state = self.state.clone();
            std::thread::Builder::new()
                .name("faktor-broker-only-expose".to_string())
                .spawn(move || expose_accept_loop(listener, in_sandbox_port, ns_path, state))?;
            Ok(host_port)
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            // Wakes every pump and every listener; the accept loops exit
            // within one `POLL` interval. No joins: `Drop` may run under
            // supervisor locks, and the loops are bounded and
            // self-terminating (see the module contract above).
            self.state.stop();
        }
    }

    /// The calling thread's tid (`SYS_gettid`), needed to address the
    /// per-thread namespace files under `/proc/self/task/<tid>/ns/`.
    fn current_tid() -> i32 {
        // SAFETY: gettid(2) takes no pointer arguments and cannot fail.
        unsafe { libc::syscall(libc::SYS_gettid) as i32 }
    }

    /// The namespace thread: unshare, bring loopback up, bind the broker
    /// listener and the ns-side bridge listener, hand the namespace fd to
    /// the host, then serve until stopped.
    fn ns_thread(
        endpoint: SocketAddr,
        ns_path: PathBuf,
        host_path: PathBuf,
        state: Arc<RelayState>,
        ready: std::sync::mpsc::Sender<std::io::Result<std::fs::File>>,
    ) {
        let setup = (|| -> std::io::Result<(TcpListener, UnixListener, std::fs::File)> {
            // SAFETY: unshare(2) takes no pointer arguments; failure is
            // reported through the raw errno (`last_os_error`).
            let ret = unsafe { libc::unshare(libc::CLONE_NEWNET) };
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
            bring_up_loopback()?;
            let broker = TcpListener::bind(endpoint)?;
            broker.set_nonblocking(true)?;
            let bridge = UnixListener::bind(&ns_path)?;
            bridge.set_nonblocking(true)?;
            // The namespace is PER-THREAD (`nsproxy`): only the calling
            // thread unshared. `/proc/self/ns/net` resolves through
            // `/proc/<tgid>` and names the THREAD-GROUP LEADER's namespace
            // (the daemon's host namespace), so it would silently hand out
            // the wrong fd. The per-thread path is the correct one.
            let ns_fd = std::fs::File::open(format!("/proc/self/task/{}/ns/net", current_tid()))?;
            Ok((broker, bridge, ns_fd))
        })();
        let (broker, bridge) = match setup {
            Ok((broker, bridge, ns_fd)) => {
                if ready.send(Ok(ns_fd)).is_err() {
                    return;
                }
                (broker, bridge)
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        loop {
            if state.stop.load(Ordering::SeqCst) {
                break;
            }
            let mut idle = true;
            match broker.accept() {
                Ok((stream, _)) => {
                    idle = false;
                    spawn_broker_relay(stream, host_path.clone(), state.clone());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            match bridge.accept() {
                Ok((stream, _)) => {
                    idle = false;
                    spawn_expose_relay(stream, state.clone());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if idle {
                std::thread::sleep(POLL);
            }
        }
    }

    /// Host accept loop: every namespace-originated connection carries a
    /// forwarding header naming its destination in the daemon namespace.
    fn host_accept_loop(listener: UnixListener, broker: SocketAddr, state: Arc<RelayState>) {
        loop {
            if state.stop.load(Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let state = state.clone();
                    std::thread::spawn(move || {
                        let Ok(header) = read_header(&stream) else {
                            return;
                        };
                        if header != "B" {
                            return;
                        }
                        let Ok(broker_stream) =
                            TcpStream::connect_timeout(&broker, CONNECT_TIMEOUT)
                        else {
                            return;
                        };
                        let ends = [RelayEnd::Tcp(broker_stream), RelayEnd::Unix(stream)];
                        if let Some(guard) = state.admit(&[&ends[0], &ends[1]]) {
                            let [a, b] = ends;
                            relay(a, b);
                            drop(guard);
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                }
                Err(_) => break,
            }
        }
    }

    /// Per-connection broker relay (namespace side): connect the host-side
    /// bridge path and forward.
    fn spawn_broker_relay(stream: TcpStream, host_path: PathBuf, state: Arc<RelayState>) {
        std::thread::spawn(move || {
            let Ok(mut to_host) = UnixStream::connect(&host_path) else {
                return;
            };
            if to_host.write_all(b"B\n").is_err() {
                return;
            }
            let ends = [RelayEnd::Tcp(stream), RelayEnd::Unix(to_host)];
            if let Some(guard) = state.admit(&[&ends[0], &ends[1]]) {
                let [a, b] = ends;
                relay(a, b);
                drop(guard);
            }
        });
    }

    /// Expose accept loop (host side): relay every host connection to the
    /// namespace-side listener with a `C <port>` header.
    fn expose_accept_loop(
        listener: TcpListener,
        in_sandbox_port: u16,
        ns_path: PathBuf,
        state: Arc<RelayState>,
    ) {
        loop {
            if state.stop.load(Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let ns_path = ns_path.clone();
                    let state = state.clone();
                    std::thread::spawn(move || {
                        let Ok(mut to_ns) = UnixStream::connect(&ns_path) else {
                            return;
                        };
                        if to_ns
                            .write_all(format!("C {in_sandbox_port}\n").as_bytes())
                            .is_err()
                        {
                            return;
                        }
                        let ends = [RelayEnd::Tcp(stream), RelayEnd::Unix(to_ns)];
                        if let Some(guard) = state.admit(&[&ends[0], &ends[1]]) {
                            let [a, b] = ends;
                            relay(a, b);
                            drop(guard);
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                }
                Err(_) => break,
            }
        }
    }

    /// Namespace-side expose relay: dial the requested loopback port INSIDE
    /// the namespace and pump.
    fn spawn_expose_relay(stream: UnixStream, state: Arc<RelayState>) {
        std::thread::spawn(move || {
            let Ok(header) = read_header(&stream) else {
                return;
            };
            let Some(port) = header
                .strip_prefix("C ")
                .and_then(|p| p.parse::<u16>().ok())
            else {
                return;
            };
            let Ok(in_ns) = TcpStream::connect_timeout(
                &SocketAddr::from(([127, 0, 0, 1], port)),
                CONNECT_TIMEOUT,
            ) else {
                return;
            };
            let ends = [RelayEnd::Tcp(in_ns), RelayEnd::Unix(stream)];
            if let Some(guard) = state.admit(&[&ends[0], &ends[1]]) {
                let [a, b] = ends;
                relay(a, b);
                drop(guard);
            }
        });
    }

    /// Read one bounded `\n`-terminated header, byte by byte (never
    /// over-reading into the relayed payload).
    fn read_header(stream: &UnixStream) -> std::io::Result<String> {
        let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));
        let mut out: Vec<u8> = Vec::with_capacity(HEADER_MAX);
        let mut byte = [0u8; 1];
        let mut reader = stream;
        for _ in 0..HEADER_MAX {
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == b'\n' {
                        return String::from_utf8(out)
                            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData));
                    }
                    out.push(byte[0]);
                }
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "relay header missing or over-long",
        ))
    }

    /// Pump bytes both ways until either side closes; each direction on its
    /// own thread, with the counterpart shutdown as soon as one direction
    /// ends so neither thread can outlive the connection.
    fn relay(mut a: RelayEnd, mut b: RelayEnd) {
        let (a2, b2) = (a.try_clone(), b.try_clone());
        let forward = std::thread::spawn(move || {
            if let (Some(mut a2), Some(mut b2)) = (a2, b2) {
                let _ = std::io::copy(&mut a2, &mut b2);
                b2.shutdown();
            }
        });
        let _ = std::io::copy(&mut b, &mut a);
        a.shutdown();
        let _ = forward.join();
    }

    /// `SIOCGIFFLAGS`/`SIOCSIFFLAGS` are `c_ulong` on every Linux libc while
    /// the `ioctl` request parameter is `Ioctl` (`c_ulong` on gnu, `c_int`
    /// on musl): normalize once, with the cast only where it is real.
    #[cfg(target_env = "musl")]
    const SIOCGIFFLAGS_REQ: libc::Ioctl = libc::SIOCGIFFLAGS as libc::Ioctl;
    #[cfg(not(target_env = "musl"))]
    const SIOCGIFFLAGS_REQ: libc::Ioctl = libc::SIOCGIFFLAGS;
    #[cfg(target_env = "musl")]
    const SIOCSIFFLAGS_REQ: libc::Ioctl = libc::SIOCSIFFLAGS as libc::Ioctl;
    #[cfg(not(target_env = "musl"))]
    const SIOCSIFFLAGS_REQ: libc::Ioctl = libc::SIOCSIFFLAGS;

    /// Bring `lo` UP inside the namespace. A fresh netns ships loopback DOWN;
    /// without this, even loopback `connect()`/`bind()` would fail. No other
    /// interface or route is touched — the namespace has no external route.
    fn bring_up_loopback() -> std::io::Result<()> {
        // SAFETY: socket(2) takes no pointers; the fd is closed below.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = (|| -> std::io::Result<()> {
            // SAFETY: `ifreq` is a plain C struct of integer/char arrays; all
            // zeroes is a valid initial state.
            let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
            let name = b"lo\0";
            // SAFETY: `name` is a 3-byte NUL-terminated literal and
            // `ifr_name` is at least IFNAMSIZ bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    name.as_ptr() as *const libc::c_char,
                    req.ifr_name.as_mut_ptr(),
                    name.len(),
                );
            }
            // SAFETY: SIOCGIFFLAGS reads/writes the ifru flags union member;
            // the fd is a valid AF_INET socket.
            let ret = unsafe { libc::ioctl(fd, SIOCGIFFLAGS_REQ, &mut req) };
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: SIOCGIFFLAGS just populated `ifru_flags`; the union
            // read/write is in-bounds for the initialized struct.
            unsafe {
                req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            }
            // SAFETY: SIOCSIFFLAGS consumes the same `ifreq`; fd still valid.
            let ret = unsafe { libc::ioctl(fd, SIOCSIFFLAGS_REQ, &req) };
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })();
        // SAFETY: `fd` came from socket(2) above and is owned here.
        unsafe {
            libc::close(fd);
        }
        result
    }

    /// Validate a BrokerOnly endpoint (loopback, non-zero port). Public to
    /// the crate so the gate can refuse an unenforceable endpoint typed
    /// before any process exists.
    pub(crate) fn validate_endpoint(endpoint: SocketAddr) -> std::io::Result<()> {
        if !endpoint.ip().is_loopback() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("BrokerOnly endpoint {endpoint} is not a loopback address"),
            ));
        }
        if endpoint.port() == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "BrokerOnly endpoint port 0 is not a broker endpoint",
            ));
        }
        Ok(())
    }
}

pub(crate) use broker_only::{validate_endpoint, Inner};

/// Install the BrokerOnly pre-exec hook on `cmd`: after fork, before exec,
/// the child `setns()`es into the bridge's network namespace. Any failure is
/// transported out of the forked child by std and refuses the spawn typed
/// (see `spawn_failure`) — the child never runs outside the namespace.
///
/// # Safety
///
/// The closure runs in the single-threaded post-fork child and only calls
/// `setns` with a raw fd that the caller keeps open (`Bridge` owns the ns
/// fd for its whole lifetime) and reads the errno — no allocation, no locks.
// SAFETY: the arguments were validated by the caller per this function's documented contract and the call has no additional aliasing or lifetime requirements.
pub(crate) fn install_broker_only_isolation(cmd: &mut std::process::Command, ns_fd: i32) {
    // SAFETY: `apply_broker_only_isolation` installs the allocation-free
    // setns pre-exec hook documented below; the caller keeps the namespace
    // fd open for the whole spawn (the bridge owns it).
    unsafe {
        apply_broker_only_isolation(cmd, ns_fd);
    }
}

unsafe fn apply_broker_only_isolation(cmd: &mut std::process::Command, ns_fd: i32) {
    // SAFETY (of the pre_exec call): std requires the caller to uphold the
    // pre-exec restrictions; the closure below is allocation-free and
    // single-purpose (documented above).
    cmd.pre_exec(move || {
        // SAFETY: setns(2) with CLONE_NEWNET takes the namespace fd the
        // caller kept open; failure is reported through the raw errno.
        let ret = unsafe { libc::setns(ns_fd, libc::CLONE_NEWNET) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    });
}

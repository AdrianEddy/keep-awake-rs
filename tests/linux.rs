//! Mock-bus tests for the Linux D-Bus legs: which backend is chosen, what the
//! services actually receive, that dropping a holder releases, that a bus with
//! nobody listening produces an `Err` rather than a holder that inhibits nothing,
//! and that a service which takes a call and never answers it can neither hang the
//! acquire nor hang the drop.
//!
//! Each test runs against its own pair of private `dbus-daemon` instances standing
//! in for the session and system buses, so it fully controls which services exist.
//! `dbus-daemon` therefore has to be on `PATH` — it is on every Linux desktop and
//! CI image, and these tests only build on Linux to begin with.
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, connection};
use zbus::object_server::Interface;
use zbus::zvariant::OwnedFd;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// Serializes the whole file. The code under test finds its buses through the
/// process-global `DBUS_*_BUS_ADDRESS` variables, so only one test may be
/// configuring or using them at a time.
fn serialized() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_path(suffix: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "keep-awake-test-{}-{n}.{suffix}",
        std::process::id()
    ))
}

/// A bus with no services and no activation, so a test only ever sees what it
/// registered itself.
const BUS_CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow own="*"/>
    <allow send_type="method_call"/>
    <allow send_type="signal"/>
    <allow send_type="method_return"/>
    <allow send_type="error"/>
    <allow receive_type="method_call"/>
    <allow receive_type="signal"/>
    <allow receive_type="method_return"/>
    <allow receive_type="error"/>
  </policy>
</busconfig>
"#;

/// A private `dbus-daemon`, killed when this value is dropped.
struct Bus {
    daemon: Child,
    config: PathBuf,
    address: String,
}

impl Bus {
    fn spawn() -> Bus {
        let config = unique_path("conf");
        std::fs::write(&config, BUS_CONFIG).expect("write bus config");
        let mut daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .arg("--print-address")
            .arg("--nofork")
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon must be on PATH to run the Linux tests");
        let mut address = String::new();
        BufReader::new(daemon.stdout.as_mut().expect("piped stdout"))
            .read_line(&mut address)
            .expect("dbus-daemon --print-address");
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        Bus {
            daemon,
            config,
            address,
        }
    }

    /// Registers `iface` at `path` under the well-known `name`. The returned
    /// connection has to stay alive for the service to stay on the bus.
    fn serve<I: Interface>(&self, name: &'static str, path: &'static str, iface: I) -> Connection {
        connection::Builder::address(self.address.as_str())
            .expect("bus address")
            .serve_at(path, iface)
            .expect("serve_at")
            .name(name)
            .expect("request name")
            .build()
            .expect("connect to the private bus")
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_file(&self.config);
    }
}

/// The private buses for one test, installed in the environment for as long as
/// this value lives.
struct Buses {
    _lock: MutexGuard<'static, ()>,
    session: Bus,
    system: Bus,
}

impl Buses {
    fn spawn() -> Buses {
        let lock = serialized();
        let session = Bus::spawn();
        let system = Bus::spawn();
        // SAFETY: `lock` is held, and it is the only way into this file, so no
        // other test thread is reading or writing the environment right now.
        unsafe {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &session.address);
            std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", &system.address);
        }
        Buses {
            _lock: lock,
            session,
            system,
        }
    }

    /// Points the session-bus variable at a socket nobody is listening on, which is
    /// how a headless or pre-session process sees the world.
    fn without_a_session_bus(self) -> Buses {
        // SAFETY: as in `spawn` — the file lock is held for as long as `self` is.
        unsafe {
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/keep-awake-has-no-session-bus",
            );
        }
        self
    }
}

/// What a mock service was asked to do, in order.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, call: String) {
        self.0.lock().expect("log").push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.0.lock().expect("log").clone()
    }

    /// Blocks until `call` shows up. A release that expects no reply has by design
    /// already returned to its caller by the time the service runs it.
    fn wait_for(&self, call: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.calls().iter().any(|seen| seen == call) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{call} never arrived; the service saw {:?}", self.calls());
    }
}

// ---------------------------------------------------------------------------
// mock services
// ---------------------------------------------------------------------------

struct MockSessionManager {
    log: Log,
    next_cookie: u32,
}

#[zbus::interface(name = "org.gnome.SessionManager")]
impl MockSessionManager {
    #[zbus(name = "Inhibit")]
    fn inhibit(&mut self, app_id: String, toplevel_xid: u32, reason: String, flags: u32) -> u32 {
        self.next_cookie += 1;
        self.log.push(format!(
            "Inhibit({app_id:?}, {toplevel_xid}, {reason:?}, {flags}) -> {}",
            self.next_cookie
        ));
        self.next_cookie
    }

    #[zbus(name = "Uninhibit")]
    fn uninhibit(&mut self, cookie: u32) {
        self.log.push(format!("Uninhibit({cookie})"));
    }
}

struct MockScreenSaver {
    log: Log,
    next_cookie: u32,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl MockScreenSaver {
    #[zbus(name = "Inhibit")]
    fn inhibit(&mut self, application_name: String, reason_for_inhibit: String) -> u32 {
        self.next_cookie += 1;
        self.log.push(format!(
            "Inhibit({application_name:?}, {reason_for_inhibit:?}) -> {}",
            self.next_cookie
        ));
        self.next_cookie
    }

    #[zbus(name = "UnInhibit")]
    fn un_inhibit(&mut self, cookie: u32) {
        self.log.push(format!("UnInhibit({cookie})"));
    }
}

struct MockLogind {
    log: Log,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl MockLogind {
    #[zbus(name = "Inhibit")]
    fn inhibit(
        &mut self,
        what: String,
        who: String,
        why: String,
        mode: String,
    ) -> zbus::fdo::Result<OwnedFd> {
        self.log
            .push(format!("Inhibit({what:?}, {who:?}, {why:?}, {mode:?})"));
        // Any descriptor will do: what is under test is that the caller receives
        // one and keeps it, not what logind attaches to it.
        let null = std::fs::File::open("/dev/null")
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(OwnedFd::from(std::os::fd::OwnedFd::from(null)))
    }
}

fn session_manager(bus: &Bus) -> (Connection, Log) {
    let log = Log::default();
    let conn = bus.serve(
        "org.gnome.SessionManager",
        "/org/gnome/SessionManager",
        MockSessionManager {
            log: log.clone(),
            next_cookie: 0,
        },
    );
    (conn, log)
}

fn screen_saver_at(bus: &Bus, path: &'static str) -> (Connection, Log) {
    let log = Log::default();
    let conn = bus.serve(
        "org.freedesktop.ScreenSaver",
        path,
        MockScreenSaver {
            log: log.clone(),
            next_cookie: 0,
        },
    );
    (conn, log)
}

fn logind(bus: &Bus) -> (Connection, Log) {
    let log = Log::default();
    let conn = bus.serve(
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        MockLogind { log: log.clone() },
    );
    (conn, log)
}

/// A screen saver that owns the name and answers, but refuses every request — the
/// shape of an implementation that is present but cannot do what was asked.
struct RefusingScreenSaver {
    log: Log,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl RefusingScreenSaver {
    #[zbus(name = "Inhibit")]
    fn inhibit(
        &self,
        application_name: String,
        reason_for_inhibit: String,
    ) -> zbus::fdo::Result<u32> {
        self.log.push(format!(
            "Inhibit({application_name:?}, {reason_for_inhibit:?}) -> refused"
        ));
        Err(zbus::fdo::Error::NotSupported("no".into()))
    }

    #[zbus(name = "UnInhibit")]
    fn un_inhibit(&self, cookie: u32) {
        self.log.push(format!("UnInhibit({cookie})"));
    }
}

/// A screen saver that hands out a cookie and then never answers anything again.
///
/// Both methods take `&self` and the hanging one is `async`, so the stuck call
/// parks a task instead of a thread and never holds the interface's write lock:
/// the wedge is confined to the caller under test.
struct WedgedScreenSaver {
    log: Log,
    next_cookie: AtomicU32,
    wedge_acquire: bool,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl WedgedScreenSaver {
    #[zbus(name = "Inhibit")]
    async fn inhibit(&self, application_name: String, reason_for_inhibit: String) -> u32 {
        let cookie = self.next_cookie.fetch_add(1, Ordering::Relaxed) + 1;
        self.log.push(format!(
            "Inhibit({application_name:?}, {reason_for_inhibit:?}) -> {cookie}"
        ));
        if self.wedge_acquire {
            std::future::pending::<()>().await;
        }
        cookie
    }

    #[zbus(name = "UnInhibit")]
    async fn un_inhibit(&self, cookie: u32) {
        self.log.push(format!("UnInhibit({cookie})"));
        std::future::pending::<()>().await
    }
}

fn refusing_screen_saver(bus: &Bus) -> (Connection, Log) {
    let log = Log::default();
    let conn = bus.serve(
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        RefusingScreenSaver { log: log.clone() },
    );
    (conn, log)
}

fn wedged_screen_saver(bus: &Bus, wedge_acquire: bool) -> (Connection, Log) {
    let log = Log::default();
    let conn = bus.serve(
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        WedgedScreenSaver {
            log: log.clone(),
            next_cookie: AtomicU32::new(0),
            wedge_acquire,
        },
    );
    (conn, log)
}

// ---------------------------------------------------------------------------
// display
// ---------------------------------------------------------------------------

#[test]
fn display_prefers_the_gnome_session_manager() {
    let buses = Buses::spawn();
    let (_gnome_conn, gnome) = session_manager(&buses.session);
    let (_saver_conn, saver) = screen_saver_at(&buses.session, "/org/freedesktop/ScreenSaver");

    let holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    // idle (8) | suspend (4)
    assert_eq!(gnome.calls(), [r#"Inhibit("app", 0, "why", 12) -> 1"#]);
    assert!(
        saver.calls().is_empty(),
        "the ScreenSaver leg should not run"
    );

    drop(holder);
    gnome.wait_for("Uninhibit(1)");
    assert_eq!(
        gnome.calls(),
        [r#"Inhibit("app", 0, "why", 12) -> 1"#, "Uninhibit(1)"]
    );
}

#[test]
fn display_falls_back_to_the_screen_saver() {
    let buses = Buses::spawn();
    let (_saver_conn, saver) = screen_saver_at(&buses.session, "/org/freedesktop/ScreenSaver");

    let holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    assert_eq!(saver.calls(), [r#"Inhibit("app", "why") -> 1"#]);

    drop(holder);
    saver.wait_for("UnInhibit(1)");
    assert_eq!(
        saver.calls(),
        [r#"Inhibit("app", "why") -> 1"#, "UnInhibit(1)"]
    );
}

#[test]
fn display_falls_back_to_the_legacy_screen_saver_path() {
    let buses = Buses::spawn();
    let (_saver_conn, saver) = screen_saver_at(&buses.session, "/ScreenSaver");

    let holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    assert_eq!(saver.calls(), [r#"Inhibit("app", "why") -> 1"#]);

    drop(holder);
    saver.wait_for("UnInhibit(1)");
    assert_eq!(
        saver.calls(),
        [r#"Inhibit("app", "why") -> 1"#, "UnInhibit(1)"]
    );
}

#[test]
fn display_falls_back_to_logind_when_nothing_owns_the_screen_saver() {
    // swayidle's world: a systemd session with no GNOME, no KDE and no owner for
    // `org.freedesktop.ScreenSaver` at all — but a logind that honours `idle`.
    let buses = Buses::spawn();
    let (_logind_conn, logind_log) = logind(&buses.system);

    let _holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    assert_eq!(
        logind_log.calls(),
        [r#"Inhibit("idle", "app", "why", "block")"#]
    );
}

#[test]
fn display_falls_back_to_logind_when_the_screen_saver_refuses() {
    let buses = Buses::spawn();
    let (_saver_conn, saver) = refusing_screen_saver(&buses.session);
    let (_logind_conn, logind_log) = logind(&buses.system);

    let _holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    // Only the specified path is served, so the legacy one errors before it is logged.
    assert_eq!(saver.calls(), [r#"Inhibit("app", "why") -> refused"#]);
    assert_eq!(
        logind_log.calls(),
        [r#"Inhibit("idle", "app", "why", "block")"#]
    );
}

#[test]
fn display_reaches_logind_without_a_session_bus() {
    let buses = Buses::spawn().without_a_session_bus();
    let (_logind_conn, logind_log) = logind(&buses.system);

    let _holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    assert_eq!(
        logind_log.calls(),
        [r#"Inhibit("idle", "app", "why", "block")"#]
    );
}

#[test]
fn display_errors_and_names_every_mechanism_when_nothing_answers() {
    let _buses = Buses::spawn();
    let error = keep_awake::inhibit_display("app", "why")
        .err()
        .expect("nothing is on either bus, so nothing can be inhibited")
        .to_string();
    for mechanism in [
        "org.freedesktop.ScreenSaver at /org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver at /ScreenSaver",
        "org.freedesktop.login1",
    ] {
        assert!(
            error.contains(mechanism),
            "{mechanism:?} missing from {error:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// system
// ---------------------------------------------------------------------------

#[test]
fn system_prefers_the_gnome_session_manager() {
    let buses = Buses::spawn();
    let (_gnome_conn, gnome) = session_manager(&buses.session);
    let (_logind_conn, logind_log) = logind(&buses.system);

    let holder = keep_awake::inhibit_system("app", "why").expect("inhibit_system");
    // suspend (4) only: a system inhibition does not mark the session non-idle.
    assert_eq!(gnome.calls(), [r#"Inhibit("app", 0, "why", 4) -> 1"#]);
    assert!(
        logind_log.calls().is_empty(),
        "the logind leg should not run"
    );

    drop(holder);
    gnome.wait_for("Uninhibit(1)");
    assert_eq!(
        gnome.calls(),
        [r#"Inhibit("app", 0, "why", 4) -> 1"#, "Uninhibit(1)"]
    );
}

#[test]
fn system_falls_back_to_logind() {
    let buses = Buses::spawn();
    let (_logind_conn, logind_log) = logind(&buses.system);

    let holder = keep_awake::inhibit_system("app", "why").expect("inhibit_system");
    assert_eq!(
        logind_log.calls(),
        [r#"Inhibit("idle", "app", "why", "block")"#]
    );
    // The lock lives on the returned descriptor, so releasing it is the holder's
    // drop closing that descriptor — there is no call for the mock to observe.
    drop(holder);
}

#[test]
fn system_ignores_a_screen_saver_that_cannot_keep_the_system_awake() {
    let buses = Buses::spawn();
    let (_saver_conn, saver) = screen_saver_at(&buses.session, "/org/freedesktop/ScreenSaver");
    let (_logind_conn, logind_log) = logind(&buses.system);

    let _holder = keep_awake::inhibit_system("app", "why").expect("inhibit_system");
    assert!(saver.calls().is_empty());
    assert_eq!(
        logind_log.calls(),
        [r#"Inhibit("idle", "app", "why", "block")"#]
    );
}

#[test]
fn system_errors_when_no_service_answers() {
    let _buses = Buses::spawn();
    let error = keep_awake::inhibit_system("app", "why")
        .err()
        .expect("nothing is on either bus, so nothing can be inhibited")
        .to_string();
    assert!(error.contains("org.freedesktop.login1"), "{error:?}");
}

// ---------------------------------------------------------------------------
// release and deadlines
// ---------------------------------------------------------------------------

#[test]
fn releasing_does_not_wait_for_the_service_to_answer() {
    let buses = Buses::spawn();
    let (_saver_conn, saver) = wedged_screen_saver(&buses.session, false);

    let holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    assert_eq!(saver.calls(), [r#"Inhibit("app", "why") -> 1"#]);

    // The service never answers `UnInhibit`. Dropping must not care: `Drop` runs on
    // whichever thread let the holder go, which in the caller this crate exists for
    // is the UI thread, possibly mid-unwind.
    let started = Instant::now();
    drop(holder);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "dropping the holder blocked for {elapsed:?}"
    );

    // The release is still genuinely sent, not skipped: no-reply, not no-call.
    saver.wait_for("UnInhibit(1)");
}

#[test]
fn a_wedged_screen_saver_does_not_wedge_the_acquire() {
    let buses = Buses::spawn();
    let (_saver_conn, saver) = wedged_screen_saver(&buses.session, true);
    let (_logind_conn, logind_log) = logind(&buses.system);

    // The screen saver takes the call and never answers it. zbus arms no reply
    // deadline of its own, so without the one this crate sets this would hang for
    // good instead of costing one timeout and moving on.
    let started = Instant::now();
    let _holder = keep_awake::inhibit_display("app", "why").expect("inhibit_display");
    let elapsed = started.elapsed();

    assert_eq!(saver.calls(), [r#"Inhibit("app", "why") -> 1"#]);
    assert!(
        elapsed >= Duration::from_secs(4),
        "returned in {elapsed:?}, so no reply was ever waited for"
    );
    assert!(elapsed < Duration::from_secs(30), "took {elapsed:?}");
    assert_eq!(
        logind_log.calls(),
        [r#"Inhibit("idle", "app", "why", "block")"#]
    );
}

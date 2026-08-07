//! Linux inhibition over D-Bus.
//!
//! Each kind tries its mechanisms in order and stops at the first one that
//! acknowledges:
//!
//! | Kind | 1 | 2 | 3 |
//! |---|---|---|---|
//! | display | `org.gnome.SessionManager.Inhibit` (idle + suspend) | `org.freedesktop.ScreenSaver.Inhibit` | `org.freedesktop.login1.Manager.Inhibit("idle", …, "block")` |
//! | system | `org.gnome.SessionManager.Inhibit` (suspend) | — | `org.freedesktop.login1.Manager.Inhibit("idle", …, "block")` |
//!
//! `org.freedesktop.ScreenSaver` is a display-only rung: it is defined to hold the
//! screensaver and the blanker off and says nothing about system sleep.
//!
//! logind is the last rung of both because it is the one mechanism a systemd box
//! always has. The pre-systemd `org.freedesktop.PowerManagement.Inhibit` is only
//! still exported by KDE, and `org.freedesktop.ScreenSaver` is not universal either:
//! kscreenlocker, gsd-screensaver-proxy and hypridle export it, but swayidle does
//! not export it at all — it speaks only logind — so on a wlroots session the
//! screensaver rung has no owner to answer it.
//!
//! What makes the logind lock effective is *not* logind. `IdleAction=` defaults to
//! `ignore` and `manager_dispatch_idle_action` returns immediately while it is, so a
//! stock logind never acts on an `idle` lock itself. The idle daemons poll the lock
//! instead: swayidle refuses to arm its timeouts while `Manager.BlockInhibited`
//! contains `idle`, hypridle does the same, and KDE's PowerDevil maps `idle` onto its
//! `ChangeScreenSettings` policy — blanking and DPMS. Taking the lock needs no
//! privilege; `org.freedesktop.login1.inhibit-block-idle` defaults to `allow_any=yes`.
//! The lock is owned by a file descriptor, so releasing it is a close.
//!
//! Every failure is reported, and a total failure names every mechanism that was
//! tried rather than only the last. A holder is only ever returned for an inhibition
//! that a service actually acknowledged, so a caller that keeps its own reference
//! count can trust it.

use std::time::Duration;

use zbus::Message;
use zbus::blocking::{Connection, connection};
use zbus::message::Flags;
use zbus::zvariant::OwnedFd;

/// A session-bus inhibition service whose bus name doubles as its interface name.
struct SessionService {
    name: &'static str,
    uninhibit: &'static str,
}

static GNOME_SESSION_MANAGER: SessionService = SessionService {
    name: "org.gnome.SessionManager",
    uninhibit: "Uninhibit",
};
const GNOME_SESSION_MANAGER_PATH: &str = "/org/gnome/SessionManager";

// const GNOME_INHIBIT_LOGOUT: u32 = 1;
// const GNOME_INHIBIT_SWITCH_USER: u32 = 2;
const GNOME_INHIBIT_SUSPEND_SESSION: u32 = 4;
const GNOME_INHIBIT_MARK_SESSION_IDLE: u32 = 8;

static SCREEN_SAVER: SessionService = SessionService {
    name: "org.freedesktop.ScreenSaver",
    uninhibit: "UnInhibit",
};
/// The interface is specified at the first path, and every implementation that
/// exports it at all — kscreenlocker, gsd-screensaver-proxy, hypridle — also exports
/// it at the second, a legacy alias kept for callers that only ever knew the short
/// form. Both are tried, in specification order.
const SCREEN_SAVER_PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];

const LOGIND_NAME: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";

/// The label the session bus itself gets in a failure report.
const SESSION_BUS: &str = "the session bus";

/// How long one D-Bus round trip may take.
///
/// zbus arms no deadline of its own: `connection::Builder`'s `method_timeout`
/// defaults to `None`, and `Connection::call_method` only wraps the reply in a
/// timeout when it is `Some`. The bus daemon does not time method calls out on the
/// client's behalf either, so without this a wedged or stopped gnome-session or
/// screen locker would hang the calling thread — in a GUI, the UI thread — for good.
const METHOD_TIMEOUT: Duration = Duration::from_secs(5);

fn session_bus() -> zbus::Result<Connection> {
    connection::Builder::session()?
        .method_timeout(METHOD_TIMEOUT)
        .build()
}

fn system_bus() -> zbus::Result<Connection> {
    connection::Builder::system()?
        .method_timeout(METHOD_TIMEOUT)
        .build()
}

/// Why each mechanism that was tried did not work, so that a total failure names all
/// of them instead of only the last.
#[derive(Debug, Default)]
struct Failures(Vec<String>);

impl Failures {
    fn push(&mut self, mechanism: impl std::fmt::Display, error: impl std::fmt::Display) {
        log::warn!("{mechanism} did not inhibit: {error}");
        self.0.push(format!("{mechanism}: {error}"));
    }

    /// Unwraps `result`, recording the failure and yielding `None` if there was one.
    fn keep<T>(
        &mut self,
        mechanism: impl std::fmt::Display,
        result: Result<T, impl std::fmt::Display>,
    ) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(e) => {
                self.push(mechanism, e);
                None
            }
        }
    }
}

impl std::fmt::Display for Failures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no inhibitor was granted: {}", self.0.join("; "))
    }
}

impl std::error::Error for Failures {}

pub fn inhibit_display(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    let mut failures = Failures::default();

    // A missing session bus is not fatal: the logind rung is on the system bus.
    if let Some(conn) = failures.keep(SESSION_BUS, session_bus()) {
        if name_has_owner(&conn, GNOME_SESSION_MANAGER.name)
            && let Some(holder) = failures.keep(
                GNOME_SESSION_MANAGER.name,
                gnome_inhibit(
                    &conn,
                    name,
                    reason,
                    GNOME_INHIBIT_MARK_SESSION_IDLE | GNOME_INHIBIT_SUSPEND_SESSION,
                ),
            )
        {
            return Ok(holder);
        }
        if let Some(holder) = screen_saver_inhibit(&conn, name, reason, &mut failures) {
            return Ok(holder);
        }
    }

    let holder = failures.keep(LOGIND_NAME, logind_inhibit(name, reason));
    holder.ok_or_else(|| failures.into())
}

pub fn inhibit_system(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    let mut failures = Failures::default();

    if let Some(conn) = failures.keep(SESSION_BUS, session_bus())
        && name_has_owner(&conn, GNOME_SESSION_MANAGER.name)
        && let Some(holder) = failures.keep(
            GNOME_SESSION_MANAGER.name,
            gnome_inhibit(&conn, name, reason, GNOME_INHIBIT_SUSPEND_SESSION),
        )
    {
        return Ok(holder);
    }

    let holder = failures.keep(LOGIND_NAME, logind_inhibit(name, reason));
    holder.ok_or_else(|| failures.into())
}

/// A live inhibition. Dropping it releases.
pub struct Holder {
    release: Release,
}

enum Release {
    /// A cookie handed out by a session-bus service. Released explicitly, on the
    /// same connection that took it out; that connection is held alongside because
    /// both services also drop an inhibition when its requester leaves the bus,
    /// which makes the drop of this field the backstop for the explicit call.
    Cookie {
        conn: Connection,
        service: &'static SessionService,
        path: &'static str,
        cookie: u32,
    },
    /// A logind inhibitor lock, which lives on the file descriptor logind returned:
    /// closing it — i.e. dropping this field — *is* the release.
    Fd(#[allow(dead_code)] OwnedFd),
}

fn name_has_owner(conn: &Connection, name: &str) -> bool {
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(name,),
    )
    .and_then(|reply| reply.body().deserialize())
    .unwrap_or(false)
}

fn gnome_inhibit(
    conn: &Connection,
    name: &str,
    reason: &str,
    flags: u32,
) -> Result<Holder, zbus::Error> {
    let reply = conn.call_method(
        Some(GNOME_SESSION_MANAGER.name),
        GNOME_SESSION_MANAGER_PATH,
        Some(GNOME_SESSION_MANAGER.name),
        "Inhibit",
        // Inhibit(s app_id, u toplevel_xid, s reason, u flags) -> u cookie. The xid
        // must be typed: a bare `0` is an `i32`, which would make the body signature
        // `sisu` and get the call rejected before it reached gnome-session.
        &(name, 0u32, reason, flags),
    )?;
    let cookie: u32 = reply.body().deserialize()?;
    Ok(Holder {
        release: Release::Cookie {
            conn: conn.clone(),
            service: &GNOME_SESSION_MANAGER,
            path: GNOME_SESSION_MANAGER_PATH,
            cookie,
        },
    })
}

/// Tries `org.freedesktop.ScreenSaver` at both of its paths, recording what each one
/// said. Any error moves on to the next path, not just "no such object": which error
/// a service returns for a path it does not export is not standardised across D-Bus
/// bindings, and mistaking a live implementation for a dead one costs the whole
/// inhibition, while retrying a genuinely broken service costs one round trip.
fn screen_saver_inhibit(
    conn: &Connection,
    name: &str,
    reason: &str,
    failures: &mut Failures,
) -> Option<Holder> {
    for path in SCREEN_SAVER_PATHS {
        let cookie = conn
            .call_method(
                Some(SCREEN_SAVER.name),
                path,
                Some(SCREEN_SAVER.name),
                "Inhibit",
                // Inhibit(s application_name, s reason_for_inhibit) -> u cookie
                &(name, reason),
            )
            .and_then(|reply| reply.body().deserialize::<u32>());
        if let Some(cookie) = failures.keep(format!("{} at {path}", SCREEN_SAVER.name), cookie) {
            return Some(Holder {
                release: Release::Cookie {
                    conn: conn.clone(),
                    service: &SCREEN_SAVER,
                    path,
                    cookie,
                },
            });
        }
    }
    None
}

fn logind_inhibit(name: &str, reason: &str) -> Result<Holder, zbus::Error> {
    let conn = system_bus()?;
    let reply = conn.call_method(
        Some(LOGIND_NAME),
        LOGIND_PATH,
        Some(LOGIND_MANAGER),
        "Inhibit",
        // Inhibit(s what, s who, s why, s mode) -> h fd. "block" refuses the idle
        // action outright, as opposed to "delay", which only postpones it.
        &("idle", name, reason, "block"),
    )?;
    let fd: OwnedFd = reply.body().deserialize()?;
    // The lock belongs to the descriptor, not to this connection, so there is
    // nothing to keep alive besides the descriptor itself.
    Ok(Holder {
        release: Release::Fd(fd),
    })
}

impl Drop for Holder {
    fn drop(&mut self) {
        match &self.release {
            Release::Cookie {
                conn,
                service,
                path,
                cookie,
            } => {
                if let Err(e) = send_uninhibit(conn, service, path, *cookie) {
                    log::error!(
                        "{}.{} could not be sent: {e}",
                        service.name,
                        service.uninhibit
                    );
                }
            }
            // `OwnedFd`'s own drop closes the descriptor, which is the release.
            Release::Fd(_) => {}
        }
    }
}

/// Sends the release without waiting for a reply.
///
/// `Drop` runs on whichever thread let the holder go — in a GUI that is the UI
/// thread, and it can be unwinding a panic — so it must not be able to block, and a
/// method call that waits for a reply can: see [`METHOD_TIMEOUT`]. `NoReplyExpected`
/// turns this into a one-way write. The daemon still delivers it and the service
/// still runs it; nothing here waits for the answer.
///
/// The release stays explicit rather than being left to the connection's own drop.
/// Both services key an inhibition to the bus name that took it out and release it
/// on `NameOwnerChanged`, so dropping `conn` is a real backstop — but only a
/// backstop, and it fires whenever the daemon gets round to it, whereas this fires
/// now and does not depend on `conn` being the last reference.
fn send_uninhibit(
    conn: &Connection,
    service: &SessionService,
    path: &str,
    cookie: u32,
) -> zbus::Result<()> {
    let mut release = Message::method_call(path, service.uninhibit)?
        .destination(service.name)?
        .interface(service.name)?
        .with_flags(Flags::NoReplyExpected)?;
    // The daemon overwrites this field anyway; setting it keeps the release
    // byte-for-byte the kind of message `Connection::call_method` would have built.
    if let Some(sender) = conn.unique_name() {
        release = release.sender(sender)?;
    }
    conn.send(&release.build(&(cookie,))?)
}

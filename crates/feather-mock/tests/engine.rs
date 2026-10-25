//! End-to-end tests: the real feather-core engine against the mock Discord server.

use feather_core::config::Endpoints;
use feather_core::engine::{self, Command, EngineConfig, EngineHandle, Event, HistoryKind, Session};
use feather_core::gateway::ConnState;
use feather_core::model::{GuildInfo, Id};
use std::process::{Child, Command as Proc, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "MTAwMA.mock.feather-test-token-not-a-secret";

struct Mock {
    child: Child,
    port: u16,
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn mock(extra: &[&str]) -> Mock {
    let port = free_port();
    let child = Proc::new(env!("CARGO_BIN_EXE_feather-mock"))
        .args(["--port", &port.to_string(), "--heartbeat-ms", "800", "--history", "120"])
        .args(extra)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t.elapsed() < Duration::from_secs(10), "mock did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Mock { child, port }
}

fn control(port: u16, action: &str) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(s, "POST /control/{action} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
}

/// Tests run in parallel threads but the endpoint override is process-global environment, so
/// setting it and reading it back must be one atomic step.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn start(port: u16, dir: &std::path::Path) -> EngineHandle {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("FEATHER_API_BASE", format!("http://127.0.0.1:{port}"));
    std::env::set_var("FEATHER_NO_KEYRING", "1");
    std::env::set_var("FEATHER_NO_NOTIFY", "1");
    feather_core::diag::init_logging(None);
    engine::spawn(
        EngineConfig {
            data_dir: dir.to_path_buf(),
            endpoints: Endpoints::from_env(),
            intents: Default::default(),
            disk_messages_per_channel: 2000,
            notifications: false,
            notify_all_dms: false,
        },
        Arc::new(|| {}),
    )
}

fn wait<T>(h: &EngineHandle, what: &str, mut f: impl FnMut(&Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match h.rx.recv_timeout(left) {
            Ok(e) => {
                seen.push(format!("{e:?}").chars().take(90).collect());
                if let Some(v) = f(&e) {
                    return v;
                }
            }
            Err(_) => panic!("timed out waiting for {what}; events seen meanwhile: {seen:#?}"),
        }
    }
}

/// Logs in and returns the first guild seen on the way (guild events may precede "connected").
fn login(h: &EngineHandle) -> Option<GuildInfo> {
    h.send(Command::Login { token: feather_core::creds::normalise(TOKEN).unwrap(), remember: false });
    // The engine may report the connection before or after the login result.
    let (mut logged_in, mut connected, mut guild) = (false, false, None);
    wait(h, "logged in and connected", |e| {
        logged_in |= matches!(e, Event::Session(Session::LoggedIn { .. }));
        connected |= matches!(e, Event::Conn(ConnState::Connected));
        if let Event::Guild(g) = e {
            guild.get_or_insert_with(|| g.clone());
        }
        (logged_in && connected).then_some(())
    });
    guild
}

const GENERAL: Id = Id(2_000_001);

#[test]
fn full_session_against_mock() {
    let m = mock(&[]);
    let dir = tempfile::tempdir().unwrap();
    let h = start(m.port, dir.path());
    wait(&h, "snapshot", |e| matches!(e, Event::Snapshot { .. }).then_some(()));
    let early = login(&h);

    // Guilds arrive with permission filtering applied (the "secret" channel is hidden).
    let g = match early {
        Some(g) => g,
        None => wait(&h, "guild", |e| if let Event::Guild(g) = e { Some(g.clone()) } else { None }),
    };
    assert!(g.channels.iter().any(|c| c.name.as_deref() == Some("general")));
    assert!(!g.channels.iter().any(|c| c.name.as_deref() == Some("secret")));

    // History: cache first (empty), then network.
    h.send(Command::OpenChannel { channel: GENERAL });
    let net = wait(&h, "network history", |e| match e {
        Event::History { channel, kind: HistoryKind::Latest, messages, from_cache: false, .. } if *channel == GENERAL => Some(messages.clone()),
        _ => None,
    });
    assert_eq!(net.len(), 50);
    assert!(net.windows(2).all(|w| w[0].id < w[1].id), "sorted oldest first");

    // Older page: rate limited twice, still succeeds.
    control(m.port, "ratelimit?n=2");
    h.send(Command::LoadOlder { channel: GENERAL, before: net[0].id });
    let older = wait(&h, "older history", |e| match e {
        Event::History { kind: HistoryKind::Older, messages, from_cache: false, .. } => Some(messages.clone()),
        _ => None,
    });
    assert_eq!(older.len(), 50);
    assert!(older.last().unwrap().id < net[0].id);

    // Send: arrives back through the gateway.
    h.send(Command::Send { channel: GENERAL, content: "hello **mock**".into(), reply_to: None, nonce: "n1".into(), files: vec![] });
    wait(&h, "own message", |e| matches!(e, Event::Message(m) if m.content == "hello **mock**").then_some(()));

    // Abrupt disconnect, a message sent while we're away, then resume with replay.
    control(m.port, "drop");
    wait(&h, "reconnecting", |e| matches!(e, Event::Conn(ConnState::Reconnecting { .. }) | Event::Conn(ConnState::Resuming)).then_some(()));
    control(m.port, "emit?content=while-away");
    wait(&h, "replayed message", |e| matches!(e, Event::Message(m) if m.content == "while-away").then_some(()));

    // Server-requested reconnect (op 7) and invalid session (op 9) both recover.
    wait(&h, "connected before op7", |e| matches!(e, Event::Conn(ConnState::Connected)).then_some(()));
    std::thread::sleep(Duration::from_millis(300));
    control(m.port, "reconnect");
    wait(&h, "reconnecting after op7", |e| matches!(e, Event::Conn(ConnState::Reconnecting { .. })).then_some(()));
    wait(&h, "connected after op7", |e| matches!(e, Event::Conn(ConnState::Connected)).then_some(()));
    std::thread::sleep(Duration::from_millis(300));
    control(m.port, "invalid-session");
    wait(&h, "re-identified", |e| matches!(e, Event::Session(Session::LoggedIn { .. })).then_some(()));

    // Heartbeats keep flowing (interval 800 ms in the mock).
    std::thread::sleep(Duration::from_millis(2000));
    h.send(Command::QueryStats);
    let st = wait(&h, "stats", |e| if let Event::Stats(s) = e { Some(s.clone()) } else { None });
    assert!(st.gw_heartbeats >= 2, "{st:?}");
    assert!(st.gw_resumes >= 1, "{st:?}");
    assert!(st.rest_rate_limited >= 2, "{st:?}");

    // Local search sees the cached messages.
    h.send(Command::Search { query: "mock".into(), channel: None, seq: 7 });
    let res = wait(&h, "search", |e| if let Event::SearchResults { seq: 7, results } = e { Some(results.clone()) } else { None });
    assert!(res.iter().any(|m| m.content.contains("hello")));

    h.shutdown(Duration::from_secs(3));

    // Warm start: guilds and messages come from the cache with no network.
    drop(m);
    let h = start(1, dir.path()); // port 1: nothing listens
    let (guilds, _) = wait(&h, "warm snapshot", |e| match e {
        Event::Snapshot { guilds, user, .. } => Some((guilds.len(), user.clone())),
        _ => None,
    });
    assert_eq!(guilds, 3);
    h.send(Command::OpenChannel { channel: GENERAL });
    let cached = wait(&h, "cached history", |e| match e {
        Event::History { kind: HistoryKind::Latest, messages, from_cache: true, .. } => Some(messages.clone()),
        _ => None,
    });
    assert_eq!(cached.len(), 50);
    assert!(cached.iter().any(|m| m.content == "while-away"));
    h.shutdown(Duration::from_secs(2));
}

#[test]
fn bad_token_and_corrupt_cache() {
    let m = mock(&[]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cache.db"), vec![0x42u8; 8192]).unwrap();
    let h = start(m.port, dir.path());
    wait(&h, "rebuild notice", |e| matches!(e, Event::Toast(t) if t.contains("rebuilt")).then_some(()));
    h.send(Command::Login { token: feather_core::creds::normalise("AAAA.BBBB.CCCC").unwrap(), remember: false });
    let reason = wait(&h, "rejected", |e| match e {
        Event::Session(Session::NeedsLogin { reason: Some(r) }) if r.contains("rejected") => Some(r.clone()),
        _ => None,
    });
    assert!(reason.contains("rejected"), "{reason}");
    h.shutdown(Duration::from_secs(2));
}

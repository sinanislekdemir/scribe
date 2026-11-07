use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::nextcloud::{Client, status_of};

/// How often an open editor checks whether someone wants to take the file.
pub const OWNER_POLL: Duration = Duration::from_secs(3);
/// How long the new opener waits for the old instance to release the lock.
pub const KICK_WAIT: Duration = Duration::from_secs(10);

const KICK_POLL: Duration = Duration::from_millis(300);
const LOCK_CONTENT_TYPE: &str = "application/json";
const LOCK_ATTEMPTS: usize = 5;

static INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct Identity {
    pub owner: String,
    pub host: String,
    pub pid: u32,
    pub user: String,
}

impl Identity {
    pub fn new(user: &str) -> Self {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let host = hostname();
        let sequence = INSTANCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let owner = format!(
            "{:08x}-{:016x}-{:016x}-{:04x}",
            pid,
            nanos,
            fnv1a(&host),
            sequence
        );
        Self {
            owner,
            host,
            pid,
            user: user.to_string(),
        }
    }

    fn lock_info(&self, remote: &str) -> LockInfo {
        LockInfo {
            version: 1,
            owner: self.owner.clone(),
            host: self.host.clone(),
            pid: self.pid,
            user: self.user.clone(),
            editor: format!("scribe/{}", env!("CARGO_PKG_VERSION")),
            file: remote.to_string(),
            acquired_at: now_secs(),
            state: LockState::Active,
            kicked_by: None,
        }
    }

    fn kicker(&self) -> Kicker {
        Kicker {
            owner: self.owner.clone(),
            host: self.host.clone(),
            pid: self.pid,
            user: self.user.clone(),
            requested_at: now_secs(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LockHandle {
    pub path: String,
    pub owner: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    Active,
    KickRequested,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockInfo {
    pub version: u32,
    pub owner: String,
    pub host: String,
    pub pid: u32,
    pub user: String,
    pub editor: String,
    pub file: String,
    pub acquired_at: u64,
    pub state: LockState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kicked_by: Option<Kicker>,
}

impl LockInfo {
    fn unreadable(remote: &str) -> Self {
        Self {
            version: 1,
            owner: "unreadable".to_string(),
            host: "unknown".to_string(),
            pid: 0,
            user: "unknown".to_string(),
            editor: "unknown".to_string(),
            file: remote.to_string(),
            acquired_at: 0,
            state: LockState::Active,
            kicked_by: None,
        }
    }

    pub fn age_secs(&self) -> Option<u64> {
        if self.acquired_at == 0 {
            return None;
        }
        now_secs().checked_sub(self.acquired_at)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Kicker {
    pub owner: String,
    pub host: String,
    pub pid: u32,
    pub user: String,
    pub requested_at: u64,
}

#[derive(Clone, Debug)]
pub struct HeldLock {
    pub info: LockInfo,
    pub etag: Option<String>,
    pub unreadable: Option<String>,
}

#[derive(Clone, Debug)]
pub enum LockEvent {
    Acquired { path: String, handle: LockHandle },
    Held { path: String, held: HeldLock },
    TimedOut { path: String },
    Kicked { path: String, by: Kicker },
    Lost { path: String, new_owner: LockInfo },
    Missing { path: String },
    Error { path: String, message: String },
}

#[allow(clippy::large_enum_variant)]
pub enum Acquire {
    Acquired { handle: LockHandle },
    Held(HeldLock),
}

#[allow(clippy::large_enum_variant)]
enum WaitOutcome {
    Acquired { handle: LockHandle },
    Held(HeldLock),
    TimedOut,
}

enum Watch {
    StillMine,
    KickRequested(Kicker),
    Lost(LockInfo),
    Missing,
}

/// The lock file that sits next to a remote ODT.
///
/// It must not start with a dot: some reverse-proxy configurations reject
/// hidden paths with 403 before the request reaches Nextcloud.
pub fn lock_path(remote: &str) -> String {
    let normalized = format!("/{}", remote.trim_matches('/'));
    if normalized == "/" {
        return "/scribe.lock".to_string();
    }
    match normalized.rsplit_once('/') {
        Some((dir, name)) if !dir.is_empty() => format!("{dir}/{name}.lock"),
        Some((_, name)) => format!("/{name}.lock"),
        None => format!("/{normalized}.lock"),
    }
}

pub fn probe(client: &Client, remote: &str) -> Result<Option<HeldLock>> {
    let Some((bytes, etag)) = client.get_optional(&lock_path(remote))? else {
        return Ok(None);
    };
    Ok(Some(parse_held(&bytes, remote, etag)))
}

fn parse_held(bytes: &[u8], remote: &str, etag: Option<String>) -> HeldLock {
    match serde_json::from_slice::<LockInfo>(bytes) {
        Ok(info) => HeldLock {
            info,
            etag,
            unreadable: None,
        },
        Err(error) => HeldLock {
            info: LockInfo::unreadable(remote),
            etag,
            unreadable: Some(error.to_string()),
        },
    }
}

/// Try to create the lock. Never steals an existing lock.
pub fn acquire(client: &Client, remote: &str, identity: &Identity) -> Result<Acquire> {
    for _ in 0..LOCK_ATTEMPTS {
        match probe(client, remote)? {
            None => {
                let info = identity.lock_info(remote);
                let bytes = serde_json::to_vec(&info)?;
                match client.put_raw(&lock_path(remote), &bytes, LOCK_CONTENT_TYPE, None, true) {
                    Ok(_) => {
                        return Ok(Acquire::Acquired {
                            handle: handle_for(remote, identity),
                        });
                    }
                    Err(error) if status_of(&error) == Some(412) => continue,
                    Err(error) => return Err(error),
                }
            }
            Some(held) => {
                if held.info.owner == identity.owner {
                    return Ok(Acquire::Acquired {
                        handle: handle_for(remote, identity),
                    });
                }
                return Ok(Acquire::Held(held));
            }
        }
    }
    bail!("could not acquire the lock after repeated races")
}

/// Force-take the lock, used only after the user confirms.
pub fn force_acquire(client: &Client, remote: &str, identity: &Identity) -> Result<Acquire> {
    for _ in 0..LOCK_ATTEMPTS {
        match probe(client, remote)? {
            None => {
                let info = identity.lock_info(remote);
                let bytes = serde_json::to_vec(&info)?;
                match client.put_raw(&lock_path(remote), &bytes, LOCK_CONTENT_TYPE, None, true) {
                    Ok(_) => {
                        return Ok(Acquire::Acquired {
                            handle: handle_for(remote, identity),
                        });
                    }
                    Err(error) if status_of(&error) == Some(412) => continue,
                    Err(error) => return Err(error),
                }
            }
            Some(held) => {
                let info = identity.lock_info(remote);
                let bytes = serde_json::to_vec(&info)?;
                match client.put_raw(
                    &lock_path(remote),
                    &bytes,
                    LOCK_CONTENT_TYPE,
                    held.etag.as_deref(),
                    false,
                ) {
                    Ok(_) => {
                        return Ok(Acquire::Acquired {
                            handle: handle_for(remote, identity),
                        });
                    }
                    Err(error) if status_of(&error) == Some(412) => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }
    bail!("could not force the lock after repeated races")
}

/// Delete the lock if it still belongs to us.
pub fn release(client: &Client, remote: &str, owner: &str) -> Result<bool> {
    let path = lock_path(remote);
    for _ in 0..LOCK_ATTEMPTS {
        match probe(client, remote)? {
            None => return Ok(true),
            Some(held) => {
                if held.info.owner != owner {
                    return Ok(false);
                }
                match client.delete(&path, held.etag.as_deref()) {
                    Ok(_) => return Ok(true),
                    Err(error) if status_of(&error) == Some(412) => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(false)
}

/// Ask the current owner to leave, then wait up to `KICK_WAIT` for the lock.
fn kick_and_wait(
    client: &Client,
    remote: &str,
    expected_owner: &str,
    identity: &Identity,
    wait: Duration,
) -> Result<WaitOutcome> {
    let deadline = Instant::now() + wait;
    loop {
        match probe(client, remote)? {
            None => {
                return match acquire(client, remote, identity)? {
                    Acquire::Acquired { handle } => Ok(WaitOutcome::Acquired { handle }),
                    Acquire::Held(held) => Ok(WaitOutcome::Held(held)),
                };
            }
            Some(mut held) => {
                if held.info.owner == identity.owner {
                    return Ok(WaitOutcome::Acquired {
                        handle: handle_for(remote, identity),
                    });
                }
                if held.info.owner != expected_owner {
                    return Ok(WaitOutcome::Held(held));
                }
                if held.info.state != LockState::KickRequested {
                    held.info.state = LockState::KickRequested;
                    held.info.kicked_by = Some(identity.kicker());
                    let bytes = serde_json::to_vec(&held.info)?;
                    match client.put_raw(
                        &lock_path(remote),
                        &bytes,
                        LOCK_CONTENT_TYPE,
                        held.etag.as_deref(),
                        false,
                    ) {
                        Ok(_) => {}
                        Err(error) if status_of(&error) == Some(412) => continue,
                        Err(error) => return Err(error),
                    }
                }
            }
        }

        if Instant::now() >= deadline {
            return match probe(client, remote)? {
                None => match acquire(client, remote, identity)? {
                    Acquire::Acquired { handle } => Ok(WaitOutcome::Acquired { handle }),
                    Acquire::Held(held) => Ok(WaitOutcome::Held(held)),
                },
                Some(held) if held.info.owner == identity.owner => Ok(WaitOutcome::Acquired {
                    handle: handle_for(remote, identity),
                }),
                Some(_) => Ok(WaitOutcome::TimedOut),
            };
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(KICK_POLL.min(remaining));
    }
}

fn check(client: &Client, handle: &LockHandle) -> Result<Watch> {
    match probe(client, &handle.path)? {
        None => Ok(Watch::Missing),
        Some(held) => {
            if held.info.owner != handle.owner {
                return Ok(Watch::Lost(held.info));
            }
            if held.info.state == LockState::KickRequested {
                return Ok(Watch::KickRequested(
                    held.info.kicked_by.clone().unwrap_or_else(|| Kicker {
                        owner: "unknown".to_string(),
                        host: "unknown".to_string(),
                        pid: 0,
                        user: "unknown".to_string(),
                        requested_at: 0,
                    }),
                ));
            }
            Ok(Watch::StillMine)
        }
    }
}

pub fn spawn_acquire(client: Client, remote: String, identity: Identity, tx: Sender<LockEvent>) {
    std::thread::spawn(move || {
        let event = match acquire(&client, &remote, &identity) {
            Ok(Acquire::Acquired { handle }) => LockEvent::Acquired {
                path: remote,
                handle,
            },
            Ok(Acquire::Held(held)) => LockEvent::Held { path: remote, held },
            Err(error) => LockEvent::Error {
                path: remote,
                message: format!("{error}"),
            },
        };
        let _ = tx.send(event);
    });
}

pub fn spawn_kick(
    client: Client,
    remote: String,
    expected_owner: String,
    identity: Identity,
    tx: Sender<LockEvent>,
) {
    std::thread::spawn(move || {
        let event = match kick_and_wait(&client, &remote, &expected_owner, &identity, KICK_WAIT) {
            Ok(WaitOutcome::Acquired { handle }) => LockEvent::Acquired {
                path: remote,
                handle,
            },
            Ok(WaitOutcome::Held(held)) => LockEvent::Held { path: remote, held },
            Ok(WaitOutcome::TimedOut) => LockEvent::TimedOut { path: remote },
            Err(error) => LockEvent::Error {
                path: remote,
                message: format!("{error}"),
            },
        };
        let _ = tx.send(event);
    });
}

pub fn spawn_force(client: Client, remote: String, identity: Identity, tx: Sender<LockEvent>) {
    std::thread::spawn(move || {
        let event = match force_acquire(&client, &remote, &identity) {
            Ok(Acquire::Acquired { handle }) => LockEvent::Acquired {
                path: remote,
                handle,
            },
            Ok(Acquire::Held(held)) => LockEvent::Held { path: remote, held },
            Err(error) => LockEvent::Error {
                path: remote,
                message: format!("{error}"),
            },
        };
        let _ = tx.send(event);
    });
}

/// Poll while this instance owns a file. Returns as soon as the lock is lost.
pub fn spawn_watch(client: Client, handle: LockHandle, stop: Receiver<()>, tx: Sender<LockEvent>) {
    std::thread::spawn(move || {
        loop {
            match check(&client, &handle) {
                Ok(Watch::StillMine) => {}
                Ok(Watch::KickRequested(by)) => {
                    let _ = tx.send(LockEvent::Kicked {
                        path: handle.path.clone(),
                        by,
                    });
                    return;
                }
                Ok(Watch::Lost(new_owner)) => {
                    let _ = tx.send(LockEvent::Lost {
                        path: handle.path.clone(),
                        new_owner,
                    });
                    return;
                }
                Ok(Watch::Missing) => {
                    let _ = tx.send(LockEvent::Missing {
                        path: handle.path.clone(),
                    });
                    return;
                }
                Err(_) => {}
            }

            match stop.recv_timeout(OWNER_POLL) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    });
}

fn handle_for(remote: &str, identity: &Identity) -> LockHandle {
    LockHandle {
        path: remote.to_string(),
        owner: identity.owner.clone(),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use crate::config::Config;

    use super::*;

    #[derive(Clone)]
    struct DavState {
        body: Vec<u8>,
        etag: String,
    }

    struct FakeDav {
        port: u16,
        stop: Arc<AtomicBool>,
    }

    impl FakeDav {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));
            let thread_state = state.clone();
            let thread_stop = stop.clone();
            thread::spawn(move || {
                let mut counter = 0u64;
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = handle_dav(stream, &thread_state, &mut counter);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self { port, stop }
        }

        fn client(&self) -> Client {
            let cfg = Config::new(
                format!("http://127.0.0.1:{}", self.port),
                "tester".to_string(),
                "secret".to_string(),
            );
            Client::with_timeout(&cfg, Duration::from_secs(2))
        }
    }

    impl Drop for FakeDav {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn acquire_blocks_a_second_instance_then_kick_takes_over() {
        let dav = FakeDav::start();
        let client = dav.client();
        let remote = "/Books/Novel.odt";
        let a = Identity::new("a");
        let b = Identity::new("b");

        match acquire(&client, remote, &a).unwrap() {
            Acquire::Acquired { handle } => assert_eq!(handle.owner, a.owner),
            Acquire::Held(_) => panic!("A should acquire the free lock"),
        }
        match acquire(&client, remote, &b).unwrap() {
            Acquire::Held(held) => assert_eq!(held.info.owner, a.owner),
            Acquire::Acquired { .. } => panic!("B must not steal A's lock"),
        }

        let watcher = {
            let client = client.clone();
            let owner = a.owner.clone();
            thread::spawn(move || {
                for _ in 0..100 {
                    if let Ok(Some(held)) = probe(&client, remote)
                        && held.info.owner == owner
                        && held.info.state == LockState::KickRequested
                    {
                        return release(&client, remote, &owner).unwrap();
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                false
            })
        };

        match kick_and_wait(&client, remote, &a.owner, &b, Duration::from_secs(2)).unwrap() {
            WaitOutcome::Acquired { handle } => assert_eq!(handle.owner, b.owner),
            WaitOutcome::Held(_) | WaitOutcome::TimedOut => {
                panic!("B should acquire after A releases")
            }
        }
        assert!(watcher.join().unwrap());
        let held = probe(&client, remote).unwrap().unwrap();
        assert_eq!(held.info.owner, b.owner);
        assert!(release(&client, remote, &b.owner).unwrap());
        assert!(probe(&client, remote).unwrap().is_none());
    }

    #[test]
    fn force_acquire_replaces_an_unresponsive_owner() {
        let dav = FakeDav::start();
        let client = dav.client();
        let remote = "/Books/Novel.odt";
        let a = Identity::new("a");
        let b = Identity::new("b");

        assert!(matches!(
            acquire(&client, remote, &a).unwrap(),
            Acquire::Acquired { .. }
        ));
        match force_acquire(&client, remote, &b).unwrap() {
            Acquire::Acquired { handle } => assert_eq!(handle.owner, b.owner),
            Acquire::Held(_) => panic!("force acquire should take the lock"),
        }
        assert!(!release(&client, remote, &a.owner).unwrap());
        assert_eq!(probe(&client, remote).unwrap().unwrap().info.owner, b.owner);
    }

    fn handle_dav(
        mut stream: TcpStream,
        state: &Arc<Mutex<Option<DavState>>>,
        counter: &mut u64,
    ) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let (request_line, headers, body) = read_request(&mut stream)?;
        let method = request_line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();

        match method.as_str() {
            "GET" => {
                let current = state.lock().unwrap().clone();
                match current {
                    Some(current) => {
                        write_response(&mut stream, "200 OK", &current.body, Some(&current.etag))
                    }
                    None => write_response(&mut stream, "404 Not Found", b"", None),
                }
            }
            "PUT" => {
                let mut current = state.lock().unwrap();
                let allowed = match headers.get("if-none-match") {
                    Some(value) if value == "*" => current.is_none(),
                    _ => match headers.get("if-match") {
                        Some(value) => current
                            .as_ref()
                            .map(|state| state.etag == *value)
                            .unwrap_or(false),
                        None => true,
                    },
                };
                if !allowed {
                    return write_response(&mut stream, "412 Precondition Failed", b"", None);
                }
                *counter += 1;
                let etag = format!("\"{counter}\"");
                *current = Some(DavState {
                    body,
                    etag: etag.clone(),
                });
                write_response(&mut stream, "200 OK", b"", Some(&etag))
            }
            "DELETE" => {
                let mut current = state.lock().unwrap();
                if current.is_none() {
                    return write_response(&mut stream, "404 Not Found", b"", None);
                }
                if let Some(value) = headers.get("if-match")
                    && current.as_ref().map(|state| &state.etag) != Some(value)
                {
                    return write_response(&mut stream, "412 Precondition Failed", b"", None);
                }
                *current = None;
                write_response(&mut stream, "204 No Content", b"", None)
            }
            _ => write_response(&mut stream, "405 Method Not Allowed", b"", None),
        }
    }

    fn read_request(
        stream: &mut TcpStream,
    ) -> std::io::Result<(String, HashMap<String, String>, Vec<u8>)> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Err(std::io::Error::other("connection closed"));
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("").to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = buffer[header_end..].to_vec();
        while body.len() < length {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(length);
        Ok((request_line, headers, body))
    }

    fn write_response(
        stream: &mut TcpStream,
        status: &str,
        body: &[u8],
        etag: Option<&str>,
    ) -> std::io::Result<()> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(etag) = etag {
            response.push_str(&format!("ETag: {etag}\r\n"));
        }
        response.push_str("\r\n");
        stream.write_all(response.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()
    }

    #[test]
    fn lock_path_is_a_visible_sibling() {
        assert_eq!(lock_path("/Books/Novel.odt"), "/Books/Novel.odt.lock");
        assert_eq!(lock_path("Books/Novel.odt"), "/Books/Novel.odt.lock");
        assert_eq!(lock_path("/Novel.odt"), "/Novel.odt.lock");
        assert_eq!(lock_path("Novel.odt"), "/Novel.odt.lock");
    }

    #[test]
    fn lock_info_round_trips() {
        let identity = Identity::new("tester");
        let info = identity.lock_info("/Books/Novel.odt");
        let bytes = serde_json::to_vec(&info).unwrap();
        let parsed: LockInfo = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.owner, info.owner);
        assert_eq!(parsed.file, "/Books/Novel.odt");
        assert_eq!(parsed.state, LockState::Active);
    }

    #[test]
    fn unreadable_lock_is_reported() {
        let held = parse_held(b"not json", "/Books/Novel.odt", Some("etag".into()));
        assert!(held.unreadable.is_some());
        assert_eq!(held.info.owner, "unreadable");
        assert_eq!(held.etag.as_deref(), Some("etag"));
    }

    #[test]
    fn identities_are_unique() {
        let first = Identity::new("tester");
        let second = Identity::new("tester");
        assert_ne!(first.owner, second.owner);
    }
}

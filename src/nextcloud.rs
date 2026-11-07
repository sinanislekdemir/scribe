use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use roxmltree::Document;
use serde::Deserialize;

use crate::config::{Config, normalize_server};

const USER_AGENT: &str = concat!("scribe/", env!("CARGO_PKG_VERSION"), " (Linux)");
const PROPFIND_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <d:getcontentlength/>
    <d:getlastmodified/>
    <d:getetag/>
  </d:prop>
</d:propfind>"#;

#[derive(Clone, Debug)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<String>,
    pub etag: Option<String>,
}

#[derive(Clone)]
pub struct Client {
    pub server: String,
    pub user: String,
    agent: ureq::Agent,
    auth: String,
}

impl Client {
    pub fn new(cfg: &Config) -> Self {
        Self::with_timeout(cfg, Duration::from_secs(30))
    }

    pub fn with_timeout(cfg: &Config, timeout: Duration) -> Self {
        let server = normalize_server(&cfg.server);
        let auth = format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", cfg.login_name, cfg.app_password))
        );
        let agent = ureq::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .build();
        Self {
            server,
            user: cfg.login_name.clone(),
            agent,
            auth,
        }
    }

    fn dav_url(&self, path: &str) -> String {
        let user = utf8_percent_encode(&self.user, NON_ALPHANUMERIC).to_string();
        let encoded = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string())
            .collect::<Vec<_>>()
            .join("/");
        if encoded.is_empty() {
            format!("{}/remote.php/dav/files/{}/", self.server, user)
        } else {
            format!("{}/remote.php/dav/files/{}/{}", self.server, user, encoded)
        }
    }

    pub fn list(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        let url = self.dav_url(path);
        let resp = self
            .agent
            .request("PROPFIND", &url)
            .set("Authorization", &self.auth)
            .set("Depth", "1")
            .set("Content-Type", "application/xml; charset=utf-8")
            .send_string(PROPFIND_BODY)
            .map_err(http_error)?;
        let body = resp.into_string().context("read PROPFIND response")?;
        let doc = Document::parse(&body).context("parse PROPFIND XML")?;
        let current = norm_path(path);
        let mut entries = Vec::new();
        for node in doc.descendants().filter(|n| {
            n.is_element()
                && n.tag_name().namespace() == Some("DAV:")
                && n.tag_name().name() == "response"
        }) {
            let href = node
                .descendants()
                .find(|n| n.is_element() && n.tag_name().name() == "href")
                .and_then(|n| n.text())
                .unwrap_or("");
            let Some(entry_path) = path_from_href(href) else {
                continue;
            };
            if entry_path == current {
                continue;
            }
            let name = entry_path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let mut is_dir = false;
            let mut size = None;
            let mut modified = None;
            let mut etag = None;
            for propstat in node.descendants().filter(|n| {
                n.is_element()
                    && n.tag_name().namespace() == Some("DAV:")
                    && n.tag_name().name() == "propstat"
            }) {
                let status = propstat
                    .descendants()
                    .find(|n| n.tag_name().name() == "status")
                    .and_then(|n| n.text())
                    .unwrap_or("");
                if !status.contains(" 200 ") {
                    continue;
                }
                for prop in propstat.descendants().filter(|n| n.is_element()) {
                    match prop.tag_name().name() {
                        "collection" => is_dir = true,
                        "getcontentlength" => {
                            size = prop.text().and_then(|t| t.parse::<u64>().ok())
                        }
                        "getlastmodified" => modified = prop.text().map(str::to_string),
                        "getetag" => etag = prop.text().map(str::to_string),
                        _ => {}
                    }
                }
            }
            entries.push(RemoteEntry {
                name,
                path: entry_path,
                is_dir,
                size,
                modified,
                etag,
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    pub fn get(&self, path: &str) -> Result<(Vec<u8>, Option<String>)> {
        match self.get_optional(path)? {
            Some(result) => Ok(result),
            None => Err(HttpStatusError { code: 404 }.into()),
        }
    }

    /// Fetch a file, returning `None` when the server says it does not exist.
    pub fn get_optional(&self, path: &str) -> Result<Option<(Vec<u8>, Option<String>)>> {
        let url = self.dav_url(path);
        let resp = match self.agent.get(&url).set("Authorization", &self.auth).call() {
            Ok(resp) => resp,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(err) => return Err(http_error(err)),
        };
        let etag = resp.header("etag").map(str::to_string);
        let mut bytes = Vec::new();
        resp.into_reader()
            .read_to_end(&mut bytes)
            .context("read file from Nextcloud")?;
        Ok(Some((bytes, etag)))
    }

    pub fn put(
        &self,
        path: &str,
        bytes: &[u8],
        etag: Option<&str>,
        create: bool,
    ) -> Result<Option<String>> {
        self.put_raw(
            path,
            bytes,
            "application/vnd.oasis.opendocument.text",
            etag,
            create,
        )
    }

    /// PUT arbitrary bytes with conditional headers, used by the lock files.
    pub fn put_raw(
        &self,
        path: &str,
        bytes: &[u8],
        content_type: &str,
        etag: Option<&str>,
        create: bool,
    ) -> Result<Option<String>> {
        let url = self.dav_url(path);
        let mut req = self
            .agent
            .put(&url)
            .set("Authorization", &self.auth)
            .set("Content-Type", content_type);
        if create {
            req = req.set("If-None-Match", "*");
        } else if let Some(etag) = etag {
            req = req.set("If-Match", etag);
        }
        let resp = req.send_bytes(bytes).map_err(http_error)?;
        Ok(resp.header("etag").map(str::to_string))
    }

    /// DELETE a file. Returns `false` when it was already gone.
    pub fn delete(&self, path: &str, etag: Option<&str>) -> Result<bool> {
        let url = self.dav_url(path);
        let mut req = self.agent.delete(&url).set("Authorization", &self.auth);
        if let Some(etag) = etag {
            req = req.set("If-Match", etag);
        }
        match req.call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(err) => Err(http_error(err)),
        }
    }

    pub fn verify(&self) -> Result<()> {
        self.list("/").map(|_| ())
    }
}

fn norm_path(path: &str) -> String {
    let p = format!("/{}", path.trim_matches('/'));
    if p == "/" {
        p
    } else {
        p.trim_end_matches('/').to_string()
    }
}

fn path_from_href(href: &str) -> Option<String> {
    let decoded = percent_decode_str(href).decode_utf8_lossy().to_string();
    let marker = "/remote.php/dav/files/";
    let rel = if let Some(pos) = decoded.find(marker) {
        let after = &decoded[pos + marker.len()..];
        let mut parts = after.splitn(2, '/');
        let _user = parts.next()?;
        parts.next().unwrap_or("")
    } else {
        decoded.trim_start_matches('/')
    };
    let path = format!("/{}", rel.trim_matches('/'));
    Some(if path == "/" {
        path
    } else {
        path.trim_end_matches('/').to_string()
    })
}

#[derive(Debug)]
pub struct HttpStatusError {
    pub code: u16,
}

impl fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Nextcloud returned HTTP {}", self.code)
    }
}

impl std::error::Error for HttpStatusError {}

/// Return the HTTP status carried by an anyhow error, when there is one.
pub fn status_of(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<HttpStatusError>().map(|e| e.code)
}

fn http_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, _) => HttpStatusError { code }.into(),
        other => anyhow!("Nextcloud request failed: {other}"),
    }
}

#[derive(Debug, Deserialize)]
struct LoginFlow {
    poll: Poll,
    login: String,
}

#[derive(Debug, Deserialize)]
struct Poll {
    token: String,
    endpoint: String,
}

#[derive(Debug, Deserialize)]
struct PollSuccess {
    server: String,
    #[serde(rename = "loginName")]
    login_name: String,
    #[serde(rename = "appPassword")]
    app_password: String,
}

pub fn login_flow(server: &str) -> Result<Config> {
    let server = normalize_server(server);
    let agent = ureq::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build();
    let url = format!("{server}/index.php/login/v2");
    let resp = agent
        .post(&url)
        .set("User-Agent", USER_AGENT)
        .send_string("")
        .map_err(http_error)?;
    let body = resp.into_string().context("read login flow response")?;
    let flow: LoginFlow = serde_json::from_str(&body).context("parse login flow JSON")?;

    println!("Opening browser to authorize scribe…");
    println!("If it doesn't open, visit:\n  {}", flow.login);
    open_browser(&flow.login);

    println!("Waiting for approval…");
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let mut last_note = std::time::Instant::now();
    loop {
        if std::time::Instant::now() > deadline {
            bail!("login timed out after 5 minutes");
        }
        match agent
            .post(&flow.poll.endpoint)
            .set("User-Agent", USER_AGENT)
            .send_form(&[("token", flow.poll.token.as_str())])
        {
            Ok(resp) => {
                let body = resp.into_string().context("read login poll response")?;
                let ok: PollSuccess =
                    serde_json::from_str(&body).context("parse login poll JSON")?;
                let cfg = Config::new(ok.server, ok.login_name, ok.app_password);
                let client = Client::new(&cfg);
                client
                    .verify()
                    .context("login succeeded, but WebDAV verification failed")?;
                return Ok(cfg);
            }
            Err(ureq::Error::Status(404, _)) => {
                if last_note.elapsed() > Duration::from_secs(15) {
                    println!("Still waiting… approve the request in your browser.");
                    last_note = std::time::Instant::now();
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => return Err(http_error(e)),
        }
    }
}

pub fn open_browser(url: &str) -> bool {
    let has_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !has_display {
        return false;
    }
    let commands: &[(&str, &[&str])] = &[
        ("xdg-open", &[]),
        ("gio", &["open"]),
        ("sensible-browser", &[]),
    ];
    for (cmd, args) in commands {
        let mut c = Command::new(cmd);
        c.args(*args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = c.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
    }
    false
}

pub fn manual_verify(server: &str, user: &str, password: &str) -> Result<()> {
    let cfg = Config::new(server.to_string(), user.to_string(), password.to_string());
    Client::new(&cfg).verify()
}

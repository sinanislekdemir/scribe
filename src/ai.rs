use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config;

pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";
pub const DEFAULT_MODEL: &str = "deepseek-chat";
const MAX_CHUNK_CHARS: usize = 12_000;

/// Local AI provider configuration, stored separately from Nextcloud
/// credentials in `~/.config/scribe/ai.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_json_mode")]
    pub json_mode: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

fn default_language() -> String {
    "auto".to_string()
}

fn default_json_mode() -> bool {
    true
}

fn default_timeout() -> u64 {
    60
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            model: default_model(),
            api_key: String::new(),
            language: default_language(),
            json_mode: default_json_mode(),
            timeout_seconds: default_timeout(),
        }
    }
}

impl AiConfig {
    /// Apply environment overrides without changing the file on disk.
    pub fn effective(mut self) -> Self {
        if let Ok(value) = std::env::var("SCRIBE_AI_API_KEY")
            && !value.is_empty()
        {
            self.api_key = value;
        }
        if let Ok(value) = std::env::var("SCRIBE_AI_BASE_URL")
            && !value.is_empty()
        {
            self.base_url = value;
        }
        if let Ok(value) = std::env::var("SCRIBE_AI_MODEL")
            && !value.is_empty()
        {
            self.model = value;
        }
        self
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    pub fn masked_key(&self) -> String {
        mask_key(&self.api_key)
    }

    pub fn validate(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            return Err(anyhow!("ai.toml: base_url is empty"));
        }
        if self.model.trim().is_empty() {
            return Err(anyhow!("ai.toml: model is empty"));
        }
        Ok(())
    }
}

/// Mask a secret for display. The full value is never returned.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.is_empty() {
        "(none)".to_string()
    } else if chars.len() <= 10 {
        "********".to_string()
    } else {
        let start: String = chars.iter().take(5).collect();
        let end: String = chars.iter().skip(chars.len() - 4).collect();
        format!("{start}…{end}")
    }
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join("ai.toml"))
}

pub fn load() -> Result<Option<AiConfig>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg: AiConfig = toml::from_str(&text).context("parse ai.toml")?;
    cfg.validate()?;
    Ok(Some(cfg))
}

pub fn save(cfg: &AiConfig) -> Result<()> {
    let dir = config::config_dir()?;
    fs::create_dir_all(&dir)?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    let path = config_path()?;
    let text = toml::to_string_pretty(cfg).context("serialize ai.toml")?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {} for writing", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// A resolved suggestion. `original` is always an exact substring of
/// paragraph `para`, so applying it never requires guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub para: usize,
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub replacement: Option<String>,
    pub kind: String,
    pub severity: String,
    pub message: String,
}

#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    issues: Vec<RawIssue>,
}

#[derive(Deserialize)]
struct RawIssue {
    #[serde(alias = "paragraph")]
    para: usize,
    #[serde(default, alias = "quote")]
    original: String,
    #[serde(default)]
    replacement: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    message: String,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message<'a>],
    temperature: f32,
    stream: bool,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
}

struct Chunk {
    items: Vec<(usize, String)>,
}

const SYSTEM_PROMPT: &str = r#"You are a conservative copy editor for literary prose. Your only job is to find objective writing errors. You must never improve or rewrite the author's prose.

Report ONLY these classes of problems:
- spelling mistakes and typos
- wrong or missing diacritics (especially Turkish ı, İ, ş, ğ, ç, ö, ü)
- duplicated words (for example "the the")
- clear subject-verb agreement, article, preposition, or pronoun errors
- clearly incorrect punctuation (unbalanced quotes, wrong apostrophe suffix separation in Turkish, a missing or doubled comma where it makes the sentence ungrammatical)
- genuinely broken sentences that cannot be parsed

Never report:
- style, tone, voice, rhythm, register, or word choice
- a sentence that is awkward but grammatically valid
- deliberate fragments, dialogue, dialect, slang, or unusual word order
- alternatives or synonyms
- names, invented words, or intentional capitalization
- anything that would alter the author's meaning or artistic intent

Do not translate. Detect the language from the text; the primary languages are Turkish and English.

For every issue, copy the exact original substring from the paragraph (including punctuation and curly quotes). The replacement must be the smallest possible correction and must not touch surrounding words. If there is no safe minimal correction, set replacement to null.

Return JSON only, with this shape:
{"issues":[{"para":0,"original":"...","replacement":"...","kind":"typo|grammar|punctuation|spelling|sentence","severity":"error|warning","message":"one short sentence"}]}

Use the paragraph index shown in brackets. If there are no issues, return {"issues":[]}."#;

/// Check one chapter. Paragraphs are indexed from zero. Progress is reported
/// after each request when the chapter is split into chunks.
pub fn check_chapter<F>(
    cfg: &AiConfig,
    paragraphs: &[String],
    mut on_progress: F,
) -> Result<Vec<Issue>>
where
    F: FnMut(usize, usize),
{
    if paragraphs.is_empty() {
        return Ok(Vec::new());
    }
    let chunks = build_chunks(paragraphs);
    let total = chunks.len().max(1);
    let mut issues = Vec::new();

    for (index, chunk) in chunks.iter().enumerate() {
        on_progress(index, total);
        let user = build_user_prompt(chunk, &cfg.language);
        let messages = [
            Message {
                role: "system",
                content: SYSTEM_PROMPT,
            },
            Message {
                role: "user",
                content: &user,
            },
        ];
        let content = request_chat(cfg, &messages, 4096)?;
        issues.extend(parse_issues(&content, paragraphs)?);
        on_progress(index + 1, total);
    }

    dedupe(&mut issues);
    Ok(issues)
}

/// Send a tiny request to verify the endpoint, model, and key.
pub fn test_connection(cfg: &AiConfig) -> Result<String> {
    let mut cfg = cfg.clone();
    cfg.json_mode = false;
    let messages = [
        Message {
            role: "system",
            content: "Reply with the single word OK.",
        },
        Message {
            role: "user",
            content: "ping",
        },
    ];
    request_chat(&cfg, &messages, 16)
}

fn build_chunks(paragraphs: &[String]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current: Vec<(usize, String)> = Vec::new();
    let mut size = 0usize;

    for (index, paragraph) in paragraphs.iter().enumerate() {
        let cost = paragraph.chars().count() + 16;
        if !current.is_empty() && size + cost > MAX_CHUNK_CHARS {
            chunks.push(Chunk {
                items: std::mem::take(&mut current),
            });
            size = 0;
        }
        current.push((index, paragraph.clone()));
        size += cost;
    }

    if !current.is_empty() {
        chunks.push(Chunk { items: current });
    }
    chunks
}

fn build_user_prompt(chunk: &Chunk, language: &str) -> String {
    let hint = match language.trim() {
        "" | "auto" => "Auto-detect the language. The primary languages are Turkish and English.",
        "tr" | "tr-TR" | "turkish" | "Türkçe" => "The text is Turkish.",
        "en" | "en-US" | "en-GB" | "english" => "The text is English.",
        other => other,
    };
    let mut prompt = String::new();
    prompt.push_str("Check the following chapter paragraphs for objective writing errors.\n");
    prompt.push_str("Language: ");
    prompt.push_str(hint);
    prompt.push_str("\n\nParagraphs (the number in brackets is the paragraph index):\n");
    for (index, text) in &chunk.items {
        prompt.push('[');
        prompt.push_str(&index.to_string());
        prompt.push_str("] ");
        prompt.push_str(text);
        prompt.push('\n');
    }
    prompt.push_str("\nReturn only the JSON object described in the system prompt.\n");
    prompt
}

fn request_chat(cfg: &AiConfig, messages: &[Message<'_>], max_tokens: u32) -> Result<String> {
    cfg.validate()?;
    let body = serde_json::to_string(&ChatRequest {
        model: &cfg.model,
        messages,
        temperature: 0.0,
        stream: false,
        max_tokens,
        response_format: cfg.json_mode.then_some(ResponseFormat {
            kind: "json_object",
        }),
    })
    .context("serialize AI request")?;

    let agent = ureq::builder()
        .timeout(Duration::from_secs(cfg.timeout_seconds.max(1)))
        .build();
    let mut request = agent
        .post(&cfg.endpoint())
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    if !cfg.api_key.trim().is_empty() {
        request = request.set("Authorization", &format!("Bearer {}", cfg.api_key.trim()));
    }

    let response = request.send_string(&body).map_err(http_error)?;
    let text = response.into_string().context("read AI response")?;
    let parsed: ChatResponse = serde_json::from_str(&text).with_context(|| {
        let preview: String = text.chars().take(300).collect();
        format!("parse AI response: {preview}")
    })?;
    parsed
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| anyhow!("AI returned an empty response"))
}

fn http_error(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let preview: String = body.trim().chars().take(400).collect();
            if preview.is_empty() {
                anyhow!("HTTP {code}")
            } else {
                anyhow!("HTTP {code}: {preview}")
            }
        }
        ureq::Error::Transport(transport) => anyhow!("network error: {transport}"),
    }
}

fn parse_issues(content: &str, paragraphs: &[String]) -> Result<Vec<Issue>> {
    let json = extract_json(content);
    let raw: RawResponse = serde_json::from_str(json).with_context(|| {
        let preview: String = content.trim().chars().take(300).collect();
        format!("parse AI JSON: {preview}")
    })?;
    let mut issues = Vec::new();

    for issue in raw.issues {
        if issue.original.is_empty() || issue.para >= paragraphs.len() {
            continue;
        }
        if issue.replacement.as_deref() == Some(issue.original.as_str()) {
            continue;
        }
        let Some((start, end)) = find_unique(&paragraphs[issue.para], &issue.original) else {
            continue;
        };
        let kind = if issue.kind.trim().is_empty() {
            "other".to_string()
        } else {
            issue.kind.trim().to_lowercase()
        };
        let severity = if issue.severity.trim().is_empty() {
            "error".to_string()
        } else {
            issue.severity.trim().to_lowercase()
        };
        issues.push(Issue {
            para: issue.para,
            start,
            end,
            original: issue.original,
            replacement: issue.replacement,
            kind,
            severity,
            message: issue.message.trim().to_string(),
        });
    }

    Ok(issues)
}

fn extract_json(content: &str) -> &str {
    let start = content.find('{');
    let end = content.rfind('}');
    match (start, end) {
        (Some(start), Some(end)) if end > start => &content[start..=end],
        _ => content,
    }
}

/// Find `needle` in `hay` only when it occurs exactly once. Ambiguous
/// matches are rejected instead of guessing an offset.
fn find_unique(hay: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let mut matches = hay.match_indices(needle);
    let (byte_start, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let start = hay[..byte_start].chars().count();
    let end = start + needle.chars().count();
    if end > hay.chars().count() {
        return None;
    }
    Some((start, end))
}

/// Re-resolve every suggestion against the current chapter text. Suggestions
/// whose original text no longer exists uniquely are dropped.
pub fn reanchor(issues: &mut Vec<Issue>, paragraphs: &[String]) {
    issues.retain_mut(|issue| {
        if issue.para >= paragraphs.len() {
            return false;
        }
        match find_unique(&paragraphs[issue.para], &issue.original) {
            Some((start, end)) => {
                issue.start = start;
                issue.end = end;
                true
            }
            None => false,
        }
    });
}

fn dedupe(issues: &mut Vec<Issue>) {
    issues.sort_by(|left, right| {
        (left.para, left.start, left.end, &left.replacement).cmp(&(
            right.para,
            right.start,
            right.end,
            &right.replacement,
        ))
    });
    issues.dedup_by(|left, right| {
        left.para == right.para
            && left.start == right.start
            && left.end == right.end
            && left.replacement == right.replacement
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraphs() -> Vec<String> {
        vec![
            "Bu bir testtir ve ve devam ediyor.".to_string(),
            "İkinci paragraf burada.".to_string(),
        ]
    }

    #[test]
    fn parses_and_resolves_issue() {
        let json = r#"{"issues":[{"para":0,"original":"ve ve","replacement":"ve","kind":"typo","severity":"error","message":"duplicated word"}]}"#;
        let issues = parse_issues(json, &paragraphs()).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!((issues[0].start, issues[0].end), (15, 20));
        assert_eq!(issues[0].replacement.as_deref(), Some("ve"));
    }

    #[test]
    fn parses_fenced_json() {
        let json = "```json\n{\"issues\":[{\"para\":1,\"original\":\"paragraf\",\"replacement\":\"paragraf\",\"kind\":\"typo\"}]}\n```";
        let issues = parse_issues(json, &paragraphs()).unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn rejects_ambiguous_anchor() {
        let texts = vec!["ve ve ve".to_string()];
        let json = r#"{"issues":[{"para":0,"original":"ve","replacement":"ve!","kind":"typo"}]}"#;
        let issues = parse_issues(json, &texts).unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn reanchor_after_replacement() {
        let mut issues = vec![Issue {
            para: 0,
            start: 0,
            end: 2,
            original: "ve".to_string(),
            replacement: Some("VE".to_string()),
            kind: "typo".to_string(),
            severity: "error".to_string(),
            message: String::new(),
        }];
        let texts = vec!["xx ve yy".to_string()];
        reanchor(&mut issues, &texts);
        assert_eq!(issues.len(), 1);
        assert_eq!((issues[0].start, issues[0].end), (3, 5));
    }

    #[test]
    fn chunks_respect_budget() {
        let texts = vec!["a".repeat(7_000), "b".repeat(7_000)];
        let chunks = build_chunks(&texts);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].items.len(), 1);
        assert_eq!(chunks[1].items.len(), 1);
    }

    #[test]
    fn masks_key() {
        let mut cfg = AiConfig::default();
        assert_eq!(cfg.masked_key(), "(none)");
        cfg.api_key = "test-1234567890abcdef".to_string();
        assert_eq!(cfg.masked_key(), "test-…cdef");
        assert_eq!(mask_key("short"), "********");
        assert!(!mask_key("sk-1234567890abcdef").contains("1234567890"));
    }

    #[test]
    fn posts_openai_compatible_request() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header_end = pos + 4;
                    let head = String::from_utf8_lossy(&data[..header_end]).to_lowercase();
                    let length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= header_end + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&data).to_string();
            assert!(request.starts_with("POST /v1/chat/completions"));
            assert!(
                request
                    .to_lowercase()
                    .contains("authorization: bearer test-key")
            );
            let body = r#"{"choices":[{"message":{"content":"{\"issues\":[]}"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let cfg = AiConfig {
            base_url: format!("http://{addr}/v1"),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            language: "auto".to_string(),
            json_mode: true,
            timeout_seconds: 5,
        };
        let content = request_chat(
            &cfg,
            &[Message {
                role: "user",
                content: "hello",
            }],
            16,
        )
        .unwrap();
        assert!(content.contains("issues"));
        server.join().unwrap();
    }
}

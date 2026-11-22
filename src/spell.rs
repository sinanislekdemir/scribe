use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config;

const DEFAULT_BINARY: &str = "hunspell";
const DEFAULT_DICTIONARY: &str = "tr_TR";
const MAX_SUGGESTIONS: usize = 9;

/// How capitalized words are treated. A mid-sentence capitalized word in
/// Turkish prose is usually a name, so the default checks only sentence
/// starts. Set `all` to check every capitalized word, or `none` to skip them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapitalizedMode {
    All,
    #[default]
    Sentence,
    None,
}

/// Local Hunspell configuration, stored in `~/.config/scribe/hunspell.toml`.
/// Nothing here is secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_binary")]
    pub binary: String,
    /// Optional Hunspell dictionary stem or path. Empty uses the `language`
    /// value from `settings.toml`; the environment variable still wins.
    #[serde(default)]
    pub dictionary: String,
    #[serde(default)]
    pub capitalized: CapitalizedMode,
    /// Skip `Ankara'da`, `TÜBİTAK'ın` and similar name+apostrophe tokens.
    #[serde(default = "default_true")]
    pub skip_apostrophe_names: bool,
    /// Empty means `~/.config/scribe/hunspell-words.txt`.
    #[serde(default)]
    pub ignore_file: String,
}

fn default_binary() -> String {
    DEFAULT_BINARY.to_string()
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            binary: default_binary(),
            dictionary: String::new(),
            capitalized: CapitalizedMode::default(),
            skip_apostrophe_names: true,
            ignore_file: String::new(),
        }
    }
}

impl Config {
    /// Apply environment overrides and expand a leading `~/`.
    pub fn effective(mut self) -> Self {
        if let Ok(value) = std::env::var("SCRIBE_HUNSPELL_BINARY")
            && !value.is_empty()
        {
            self.binary = value;
        }
        if let Ok(value) = std::env::var("SCRIBE_HUNSPELL_DICTIONARY")
            && !value.is_empty()
        {
            self.dictionary = value;
        }
        self.binary = expand_tilde(&self.binary);
        self.dictionary = expand_tilde(&self.dictionary);
        self.ignore_file = expand_tilde(&self.ignore_file);
        self
    }

    pub fn validate(&self) -> Result<()> {
        if self.binary.trim().is_empty() {
            bail!("hunspell.toml: binary is empty");
        }
        if self.dictionary.trim().is_empty() {
            bail!("hunspell.toml: dictionary is empty");
        }
        Ok(())
    }

    pub fn display_dictionary(&self) -> String {
        let dictionary = self.dictionary.trim();
        if dictionary.is_empty() {
            return DEFAULT_DICTIONARY.to_string();
        }
        Path::new(dictionary)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(dictionary)
            .to_string()
    }
}

/// A spelling finding in one paragraph. `start` and `end` are **character**
/// offsets, matching `Editor::replace_span`; `original` is always the exact
/// text at that span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub para: usize,
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub alternatives: Vec<String>,
    pub message: String,
}

pub fn path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join("hunspell.toml"))
}

pub fn load(language: &str) -> Result<Config> {
    let path = path()?;
    let cfg = if path.exists() {
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).context("parse hunspell.toml")?
    } else {
        Config::default()
    };
    Ok(resolve_dictionary(cfg, language).effective())
}

/// `settings.toml` language is the default; an explicit dictionary in
/// `hunspell.toml` wins. The environment is applied later by `effective`.
fn resolve_dictionary(mut cfg: Config, language: &str) -> Config {
    if cfg.dictionary.trim().is_empty() {
        let language = language.trim();
        cfg.dictionary = if language.is_empty() {
            DEFAULT_DICTIONARY.to_string()
        } else {
            language.to_string()
        };
    }
    cfg
}

/// Check every eligible word in the paragraphs with one Hunspell process.
/// The caller runs this on a worker thread; it performs no UI work.
pub fn check_chapter(
    cfg: &Config,
    paragraphs: &[String],
    ignored: &HashSet<String>,
) -> Result<Vec<Finding>> {
    cfg.validate()?;
    let tokens = collect_tokens(paragraphs, cfg, ignored);
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let mut words = Vec::new();
    let mut seen = HashSet::new();
    for token in &tokens {
        if seen.insert(token.lookup.clone()) {
            words.push(token.lookup.clone());
        }
    }
    let results = run_hunspell(cfg, &words)?;
    let by_word: HashMap<&str, &HunspellResult> = words
        .iter()
        .zip(results.iter())
        .map(|(word, result)| (word.as_str(), result))
        .collect();

    let mut findings = Vec::new();
    for token in tokens {
        let Some(result) = by_word.get(token.lookup.as_str()) else {
            continue;
        };
        if result.accepted {
            continue;
        }
        let mut alternatives = result.suggestions.clone();
        alternatives.retain(|suggestion| !suggestion.is_empty() && *suggestion != token.lookup);
        alternatives.dedup();
        alternatives.truncate(MAX_SUGGESTIONS);
        let message = if alternatives.is_empty() {
            "Not in the dictionary; no suggestion.".to_string()
        } else {
            "Not in the dictionary.".to_string()
        };
        findings.push(Finding {
            para: token.para,
            start: token.start,
            end: token.end,
            original: token.text,
            alternatives,
            message,
        });
    }
    Ok(findings)
}

/// The words ignored on this machine, including both the literal and the
/// circumflex-normalized form.
pub fn load_ignored(cfg: &Config) -> Result<HashSet<String>> {
    let path = ignore_path(cfg)?;
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut words = HashSet::new();
    for line in text.lines() {
        let word = line.trim();
        if word.is_empty() || word.starts_with('#') {
            continue;
        }
        words.insert(word.to_string());
        words.insert(normalize(word));
    }
    Ok(words)
}

/// Append a word to the user dictionary. Duplicate entries are not added.
pub fn add_word(cfg: &Config, word: &str) -> Result<PathBuf> {
    let word = word.trim();
    if word.is_empty() {
        bail!("empty word");
    }
    let path = ignore_path(cfg)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(existing) = fs::read_to_string(&path)
        && existing.lines().any(|line| line.trim() == word)
    {
        return Ok(path);
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {} for writing", path.display()))?;
    writeln!(file, "{word}")?;
    file.sync_all()?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn ignore_path(cfg: &Config) -> Result<PathBuf> {
    if cfg.ignore_file.trim().is_empty() {
        Ok(config::config_dir()?.join("hunspell-words.txt"))
    } else {
        Ok(PathBuf::from(expand_tilde(cfg.ignore_file.trim())))
    }
}

fn expand_tilde(path: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.to_string();
    };
    if path == "~" {
        return home.to_string_lossy().into_owned();
    }
    let Some(rest) = path.strip_prefix("~/") else {
        return path.to_string();
    };
    home.join(rest).to_string_lossy().into_owned()
}

#[derive(Clone, Debug)]
struct Token {
    para: usize,
    start: usize,
    end: usize,
    text: String,
    lookup: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HunspellResult {
    accepted: bool,
    suggestions: Vec<String>,
}

fn collect_tokens(paragraphs: &[String], cfg: &Config, ignored: &HashSet<String>) -> Vec<Token> {
    let mut tokens = Vec::new();
    for (para, text) in paragraphs.iter().enumerate() {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if !chars[i].is_alphabetic() {
                i += 1;
                continue;
            }
            let start = i;
            let mut end = i + 1;
            while end < chars.len() {
                let c = chars[end];
                if c.is_alphabetic() {
                    end += 1;
                    continue;
                }
                if is_apostrophe(c) && end + 1 < chars.len() && chars[end + 1].is_alphabetic() {
                    end += 2;
                    continue;
                }
                break;
            }

            // `1923'te` and `%50'si` start the word after a digit+apostrophe.
            // Those suffixes are not independent words.
            let after_number_apostrophe =
                start >= 2 && is_apostrophe(chars[start - 1]) && chars[start - 2].is_numeric();
            let word: String = chars[start..end].iter().collect();
            let sentence_start = is_sentence_start(&chars, start);
            if !after_number_apostrophe && should_check(&word, sentence_start, cfg, ignored) {
                tokens.push(Token {
                    para,
                    start,
                    end,
                    text: word.clone(),
                    lookup: normalize(&word),
                });
            }
            i = end;
        }
    }
    tokens
}

fn should_check(word: &str, sentence_start: bool, cfg: &Config, ignored: &HashSet<String>) -> bool {
    if word.chars().count() < 2 {
        return false;
    }
    if cfg.skip_apostrophe_names && has_apostrophe(word) && apostrophe_stem_is_capitalized(word) {
        return false;
    }
    let capitalized = word.chars().next().is_some_and(|c| c.is_uppercase());
    if capitalized {
        match cfg.capitalized {
            CapitalizedMode::All => {}
            CapitalizedMode::Sentence if !sentence_start => return false,
            CapitalizedMode::Sentence => {}
            CapitalizedMode::None => return false,
        }
    }
    if ignored.contains(word) || ignored.contains(&normalize(word)) {
        return false;
    }
    true
}

fn has_apostrophe(word: &str) -> bool {
    word.chars().any(is_apostrophe)
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '’'
}

fn apostrophe_stem_is_capitalized(word: &str) -> bool {
    word.split(is_apostrophe)
        .next()
        .and_then(|stem| stem.chars().next())
        .is_some_and(|c| c.is_uppercase())
}

/// Fold the Turkish circumflex for dictionary lookup. The document keeps its
/// own spelling; Hunspell's `tr_TR` dictionary uses the unaccented forms.
fn normalize(word: &str) -> String {
    word.chars()
        .map(|c| match c {
            'â' => 'a',
            'Â' => 'A',
            'î' => 'i',
            'Î' => 'I',
            'û' => 'u',
            'Û' => 'U',
            '’' => '\'',
            _ => c,
        })
        .collect()
}

fn is_sentence_start(chars: &[char], start: usize) -> bool {
    let mut i = start;
    let mut saw_space = false;
    loop {
        let before = i;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
            saw_space = true;
        }
        if i > 0 && is_opening_punct(chars[i - 1]) {
            i -= 1;
            continue;
        }
        if i == before {
            break;
        }
    }
    if i == 0 {
        return true;
    }
    while i > 0 && is_closing_punct(chars[i - 1]) {
        i -= 1;
    }
    saw_space && i > 0 && matches!(chars[i - 1], '.' | '!' | '?')
}

fn is_opening_punct(c: char) -> bool {
    matches!(
        c,
        '“' | '‘' | '"' | '\'' | '(' | '[' | '{' | '«' | '‹' | '〈' | '《'
    )
}

fn is_closing_punct(c: char) -> bool {
    matches!(
        c,
        '”' | '’' | '"' | '\'' | ')' | ']' | '}' | '»' | '›' | '〉' | '》'
    )
}

fn run_hunspell(cfg: &Config, words: &[String]) -> Result<Vec<HunspellResult>> {
    let mut command = Command::new(&cfg.binary);
    command
        .arg("-a")
        .arg("-i")
        .arg("UTF-8")
        .arg("-d")
        .arg(&cfg.dictionary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().with_context(|| {
        format!(
            "could not run `{}`. Install hunspell and a dictionary, or set \
             binary/dictionary in ~/.config/scribe/hunspell.toml",
            cfg.binary
        )
    })?;
    // Write stdin from a separate thread: a large chapter can fill both pipe
    // buffers if the child is not drained while it is being fed.
    let mut stdin = child
        .stdin
        .take()
        .context("hunspell did not provide stdin")?;
    let input = words.to_vec();
    let writer = std::thread::spawn(move || -> Result<()> {
        for word in &input {
            writeln!(stdin, "{word}").context("write to hunspell")?;
        }
        Ok(())
    });
    let output = child.wait_with_output().context("wait for hunspell")?;
    let written = writer
        .join()
        .map_err(|_| anyhow!("hunspell input thread panicked"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "hunspell failed: {}. Check the dictionary path in ~/.config/scribe/hunspell.toml",
            error.trim()
        ));
    }
    written?;
    let stdout = String::from_utf8(output.stdout).context("hunspell output is not UTF-8")?;
    if stdout.trim().is_empty() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "hunspell returned no results{}. Check the dictionary path in ~/.config/scribe/hunspell.toml",
            if error.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", error.trim())
            }
        ));
    }
    parse_output(&stdout, words)
}

fn parse_output(stdout: &str, words: &[String]) -> Result<Vec<HunspellResult>> {
    let mut groups: Vec<Vec<&str>> = Vec::new();
    let mut current = Vec::new();
    for line in stdout.lines() {
        if line.starts_with("@(#)") {
            continue;
        }
        if line.trim().is_empty() {
            if !current.is_empty() {
                groups.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(line);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    if groups.len() != words.len() {
        return Err(anyhow!(
            "hunspell returned {} result group(s) for {} word(s)",
            groups.len(),
            words.len()
        ));
    }
    Ok(groups.into_iter().map(parse_group).collect())
}

fn parse_group(lines: Vec<&str>) -> HunspellResult {
    let mut accepted = true;
    let mut suggestions = Vec::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix('&') {
            accepted = false;
            if let Some((_, list)) = rest.split_once(": ") {
                for suggestion in list.split(',') {
                    let suggestion = suggestion.trim();
                    if suggestion.is_empty() || suggestions.iter().any(|s| s == suggestion) {
                        continue;
                    }
                    suggestions.push(suggestion.to_string());
                    if suggestions.len() == MAX_SUGGESTIONS {
                        break;
                    }
                }
            }
        } else if line.starts_with('#') {
            accepted = false;
        }
    }
    HunspellResult {
        accepted,
        suggestions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens_for(text: &str) -> Vec<Token> {
        collect_tokens(
            &[text.to_string()],
            &Config {
                capitalized: CapitalizedMode::All,
                ..Config::default()
            },
            &HashSet::new(),
        )
    }

    #[test]
    fn tokenizer_uses_char_offsets_for_turkish_text() {
        let tokens = tokens_for("Şu kitapda bir hata var.");
        let token = tokens.iter().find(|t| t.text == "kitapda").unwrap();
        assert_eq!(token.start, 3);
        assert_eq!(token.end, 10);
        let chars: Vec<char> = "Şu kitapda bir hata var.".chars().collect();
        let original: String = chars[token.start..token.end].iter().collect();
        assert_eq!(original, "kitapda");
    }

    #[test]
    fn tokenizer_keeps_apostrophes_inside_words() {
        let cfg = Config {
            capitalized: CapitalizedMode::All,
            skip_apostrophe_names: false,
            ..Config::default()
        };
        let tokens = collect_tokens(
            &["Dün Ankara'da kaldım.".to_string()],
            &cfg,
            &HashSet::new(),
        );
        let token = tokens.iter().find(|t| t.text == "Ankara'da").unwrap();
        assert_eq!(token.start, 4);
        assert_eq!(token.end, 13);
    }

    #[test]
    fn tokenizer_skips_number_apostrophe_suffixes() {
        let tokens = tokens_for("1923'te ve %50'si.");
        assert!(tokens.iter().all(|t| t.text != "te" && t.text != "si"));
    }

    #[test]
    fn circumflex_is_folded_only_for_lookup() {
        let tokens = tokens_for("Kâse ve tezgâh.");
        assert_eq!(tokens[0].text, "Kâse");
        assert_eq!(tokens[0].lookup, "Kase");
        assert_eq!(tokens[2].text, "tezgâh");
        assert_eq!(tokens[2].lookup, "tezgah");
    }

    #[test]
    fn skips_capitalized_apostrophe_names_by_default() {
        let tokens = tokens_for("Dün Ankara'da ve kitap'da kaldım.");
        assert!(tokens.iter().all(|t| t.text != "Ankara'da"));
        assert!(tokens.iter().any(|t| t.text == "kitap'da"));
    }

    #[test]
    fn sentence_mode_skips_mid_sentence_names_but_checks_starts() {
        let cfg = Config {
            capitalized: CapitalizedMode::Sentence,
            ..Config::default()
        };
        let mid = collect_tokens(&["Dün Zeynep geldi.".to_string()], &cfg, &HashSet::new());
        assert!(mid.iter().all(|t| t.text != "Zeynep"));
        let start = collect_tokens(&["Zeynep geldi.".to_string()], &cfg, &HashSet::new());
        assert!(start.iter().any(|t| t.text == "Zeynep"));
    }

    #[test]
    fn sentence_start_looks_through_quotes() {
        let chars: Vec<char> = "“Geldin mi?” dedi.".chars().collect();
        let dedi = chars.len() - 5;
        assert_eq!(chars[dedi], 'd');
        assert!(is_sentence_start(&chars, dedi));
        let chars: Vec<char> = "Dün “Zeynep” geldi.".chars().collect();
        let zeynep = chars.iter().position(|c| *c == 'Z').unwrap();
        assert!(!is_sentence_start(&chars, zeynep));
    }

    #[test]
    fn parses_banner_blank_lines_and_suggestions() {
        let output = "@(#) International Ispell Version 3.2.06\n\
                      *\n\
                      \n\
                      & kitapda 4 0: kitapta, kitapla\n\
                      \n\
                      # gelti 0\n\
                      \n";
        let results = parse_output(
            output,
            &[
                "kitap".to_string(),
                "kitapda".to_string(),
                "gelti".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            results[0],
            HunspellResult {
                accepted: true,
                suggestions: Vec::new(),
            }
        );
        assert!(!results[1].accepted);
        assert_eq!(results[1].suggestions, vec!["kitapta", "kitapla"]);
        assert!(!results[2].accepted);
        assert!(results[2].suggestions.is_empty());
    }

    #[test]
    fn combines_all_parts_of_a_split_token() {
        let output = "@(#) International Ispell Version 3.2.06\n\
                      & TÜBİTAK 1 0: TAKİBİ\n\
                      & ın 15 10: nı, ı\n\
                      \n";
        let results = parse_output(output, &["TÜBİTAK'ın".to_string()]).unwrap();
        assert!(!results[0].accepted);
        assert_eq!(results[0].suggestions, vec!["TAKİBİ", "nı", "ı"]);
    }

    #[test]
    fn parse_output_rejects_group_count_mismatch() {
        let error = parse_output("@(#) x\n*\n\n", &["a".to_string(), "b".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("1 result group"));
    }

    #[test]
    fn empty_dictionary_uses_the_settings_language() {
        let cfg = resolve_dictionary(Config::default(), "en_US");
        assert_eq!(cfg.dictionary, "en_US");
        let cfg = resolve_dictionary(Config::default(), "");
        assert_eq!(cfg.dictionary, DEFAULT_DICTIONARY);
    }

    #[test]
    fn explicit_dictionary_wins_over_the_settings_language() {
        let cfg = resolve_dictionary(
            Config {
                dictionary: "~/dict/custom".to_string(),
                ..Config::default()
            },
            "en_US",
        );
        assert_eq!(cfg.dictionary, "~/dict/custom");
    }

    #[test]
    fn check_chapter_runs_hunspell_and_anchors_findings() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-hunspell");
        fs::write(
            &script,
            "#!/bin/sh\n\
             while IFS= read -r w; do\n\
               case \"$w\" in\n\
                 kitapda) printf '& kitapda 4 0: kitapta, kitapla\\n\\n' ;;\n\
                 *) printf '*\\n\\n' ;;\n\
               esac\n\
             done\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = Config {
            binary: script.to_string_lossy().into_owned(),
            dictionary: "tr_TR".to_string(),
            ..Config::default()
        };
        let findings = check_chapter(
            &cfg,
            &["Şu kitapda bir hata var.".to_string()],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].para, 0);
        assert_eq!(findings[0].original, "kitapda");
        assert_eq!(findings[0].start, 3);
        assert_eq!(findings[0].end, 10);
        assert_eq!(
            findings[0].alternatives,
            vec!["kitapta".to_string(), "kitapla".to_string()]
        );
    }

    #[test]
    fn expansion_handles_home_paths() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home.to_string_lossy());
        assert_eq!(
            expand_tilde("~/Documents/tr_TR"),
            home.join("Documents/tr_TR").to_string_lossy()
        );
        assert_eq!(expand_tilde("tr_TR"), "tr_TR");
        assert_eq!(expand_tilde("~other/tr_TR"), "~other/tr_TR");
    }
}

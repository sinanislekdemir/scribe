mod ai;
mod app;
mod backups;
mod config;
mod editor;
mod lock;
mod model;
mod nextcloud;
mod odt;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("login") => {
            let manual = args.iter().any(|a| a == "--manual");
            let url = match args.iter().skip(2).find(|a| !a.starts_with("--")) {
                Some(url) => {
                    let url = config::normalize_server(url);
                    if url.is_empty() {
                        bail!("a Nextcloud URL is required");
                    }
                    url
                }
                None => prompt_server_url()?,
            };
            let cfg = if manual {
                manual_login(&url)?
            } else {
                nextcloud::login_flow(&url)?
            };
            config::save(&cfg)?;
            println!(
                "Logged in as {} on {}.\nCredentials saved.",
                cfg.login_name, cfg.server
            );
            Ok(())
        }
        Some("logout") => {
            config::remove()?;
            println!("Local credentials removed.");
            Ok(())
        }
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("ai-config") => run_ai_config(&args[2..]),
        Some("inspect") => {
            let path = args.get(2).context("usage: scribe inspect <file.odt>")?;
            let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
            let title = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Untitled");
            let doc = odt::OdtDocument::from_bytes(&bytes, title)?;
            println!("title: {}", doc.book.title);
            println!("chapters: {}", doc.book.chapters.len());
            println!(
                "words: {} chars: {}",
                doc.book.total_words(),
                doc.book.total_chars()
            );
            for c in &doc.book.chapters {
                println!(
                    "  {}. {} ({} paragraphs, {} words)",
                    c.number,
                    c.title,
                    c.paragraphs.len(),
                    c.word_count()
                );
            }
            Ok(())
        }
        Some("roundtrip") => {
            let input = args
                .get(2)
                .context("usage: scribe roundtrip <in.odt> <out.odt>")?;
            let output = args
                .get(3)
                .context("usage: scribe roundtrip <in.odt> <out.odt>")?;
            let bytes = std::fs::read(input).with_context(|| format!("read {input}"))?;
            let title = std::path::Path::new(input)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Untitled");
            let mut doc = odt::OdtDocument::from_bytes(&bytes, title)?;
            std::fs::write(output, doc.to_bytes()?)?;
            println!("wrote {output}");
            Ok(())
        }
        Some("open") => {
            let path = args.get(2).context("usage: scribe open <file.odt>")?;
            run_local(PathBuf::from(path))
        }
        Some(p) if p.ends_with(".odt") => run_local(PathBuf::from(p)),
        Some(p) if p.starts_with('-') => {
            bail!("unknown option: {p}\n\nRun `scribe help`.");
        }
        Some(p) if p.to_lowercase().ends_with(".docx") => {
            bail!("scribe only opens .odt files; DOCX is not supported");
        }
        Some(_) => {
            print_help();
            Ok(())
        }
        None => run_remote(),
    }
}

fn run_remote() -> Result<()> {
    let cfg = match config::load()? {
        Some(c) => c,
        None => match first_run()? {
            FirstRun::Remote(c) => {
                config::save(&c)?;
                c
            }
            FirstRun::Local(path) => return run_local(path),
            FirstRun::Quit => return Ok(()),
        },
    };
    let mut app = app::App::new_remote(cfg)?;
    run_tui(&mut app)
}

fn run_local(path: PathBuf) -> Result<()> {
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if !extension.eq_ignore_ascii_case("odt") {
        bail!("scribe only opens .odt files");
    }
    let mut app = app::App::new_local(path)?;
    run_tui(&mut app)
}

fn run_tui(app: &mut app::App) -> Result<()> {
    let mut terminal = ratatui::init();
    crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
    terminal.hide_cursor()?;
    let result = app.run(&mut terminal);
    let notice = app.exit_notice().map(str::to_string);
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    if let Some(notice) = notice {
        println!("\n{notice}");
    }
    result
}

enum FirstRun {
    Remote(config::Config),
    Local(PathBuf),
    Quit,
}

fn first_run() -> Result<FirstRun> {
    println!("No Nextcloud login found.\n");
    println!("  [1] Log in to Nextcloud with browser");
    println!("  [2] Enter Nextcloud URL + app password");
    println!("  [3] Open a local .odt file");
    println!("  [4] Quit\n");
    let choice = prompt("Choice [3]: ")?;
    let choice = choice.trim();
    match choice {
        "4" | "q" | "Q" => Ok(FirstRun::Quit),
        "1" | "b" | "B" => {
            let url = prompt_server_url()?;
            Ok(FirstRun::Remote(nextcloud::login_flow(&url)?))
        }
        "2" | "m" | "M" => {
            let url = prompt_server_url()?;
            Ok(FirstRun::Remote(manual_login(&url)?))
        }
        _ => {
            let path = prompt("Path to .odt (existing or new): ")?;
            let path = path.trim();
            if path.is_empty() {
                bail!("no local file path given");
            }
            Ok(FirstRun::Local(PathBuf::from(path)))
        }
    }
}

fn prompt_server_url() -> Result<String> {
    let url = prompt("Nextcloud URL (for example https://cloud.example.com): ")?;
    let url = config::normalize_server(&url);
    if url.is_empty() {
        bail!("a Nextcloud URL is required");
    }
    Ok(url)
}

fn manual_login(url: &str) -> Result<config::Config> {
    let user = prompt("Nextcloud username: ")?;
    let user = user.trim().to_string();
    if user.is_empty() {
        bail!("username is empty");
    }
    let password = rpassword::prompt_password("App password: ")?;
    if password.is_empty() {
        bail!("password is empty");
    }
    nextcloud::manual_verify(url, &user, &password)
        .context("could not verify the Nextcloud credentials")?;
    Ok(config::Config::new(url.to_string(), user, password))
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn run_ai_config(args: &[String]) -> Result<()> {
    let show = args.iter().any(|a| a == "--show");
    let test = args.iter().any(|a| a == "--test");
    let current = ai::load()?.unwrap_or_default();

    if show {
        println!("path: {}", ai::config_path()?.display());
        println!("base_url: {}", current.base_url);
        println!("model: {}", current.model);
        println!("api_key: {}", current.masked_key());
        println!("language: {}", current.language);
        println!("json_mode: {}", current.json_mode);
        println!("timeout_seconds: {}", current.timeout_seconds);
        return Ok(());
    }

    if test {
        let reply = ai::test_connection(&current)?;
        println!("AI endpoint OK: {}", reply.trim());
        return Ok(());
    }

    let mut cfg = current;
    let value = prompt(&format!("Base URL [{}]: ", cfg.base_url))?;
    if !value.trim().is_empty() {
        cfg.base_url = value.trim().to_string();
    }
    let value = prompt(&format!("Model [{}]: ", cfg.model))?;
    if !value.trim().is_empty() {
        cfg.model = value.trim().to_string();
    }
    let value = prompt(&format!("Language (auto/tr/en) [{}]: ", cfg.language))?;
    if !value.trim().is_empty() {
        cfg.language = value.trim().to_string();
    }
    let key = rpassword::prompt_password(if cfg.api_key.is_empty() {
        "API key (empty for local Ollama): "
    } else {
        "API key [keep current]: "
    })?;
    if !key.is_empty() {
        cfg.api_key = key;
    }
    let value = prompt(&format!(
        "Enable JSON mode? [{}]: ",
        if cfg.json_mode { "Y/n" } else { "y/N" }
    ))?;
    let value = value.trim().to_lowercase();
    if value == "y" || value == "yes" {
        cfg.json_mode = true;
    } else if value == "n" || value == "no" {
        cfg.json_mode = false;
    }
    let value = prompt(&format!("Timeout seconds [{}]: ", cfg.timeout_seconds))?;
    if let Ok(seconds) = value.trim().parse::<u64>() {
        cfg.timeout_seconds = seconds.max(5);
    }

    ai::save(&cfg)?;
    println!(
        "Wrote {} (mode 0600).\nCopy this file to another machine when needed.",
        ai::config_path()?.display()
    );
    Ok(())
}

fn print_help() {
    println!(
        "scribe — a focused ODT book editor\n\n\
         Usage:\n\
           scribe                 start the TUI (Nextcloud or a local file)\n\
           scribe login [URL]     log in to Nextcloud through the browser\n\
           scribe login --manual  enter Nextcloud URL + app password\n\
           scribe logout          remove local credentials\n\
           scribe open FILE.odt   edit a local ODT file (no login required)\n\
           scribe FILE.odt        same as open\n\
           scribe ai-config       configure the AI writing check\n\
           scribe ai-config --show  show the AI config (key masked)\n\
           scribe ai-config --test  test the AI endpoint\n\n\
         Only .odt files are supported."
    );
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use crate::ai;
use crate::backups;
use crate::config::{self, Config};
use crate::editor::{
    Cursor, Editor, LineKind, MarkResult, smart_quote_for, smart_single_quote_for,
};
use crate::lock::{self, HeldLock, Identity, Kicker, LockEvent, LockHandle, LockInfo};
use crate::model::{Align, Book, Chapter, ParaKind};
use crate::nextcloud::{Client, RemoteEntry};
use crate::odt::OdtDocument;
use crate::search::{self, SearchMatch};
use crate::settings::{self, Settings};
use crate::theme::{Palette, ThemeName};

const APP_AUTHOR: &str = env!("CARGO_PKG_AUTHORS");
const APP_REPO: &str = env!("CARGO_PKG_REPOSITORY");
const APP_LICENSE: &str = env!("CARGO_PKG_LICENSE");

fn app_version() -> &'static str {
    option_env!("SCRIBE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Clone, Debug)]
enum SaveState {
    Idle,
    Saving,
    Saved,
    Offline,
    Conflict,
}

impl SaveState {
    fn text(&self) -> &str {
        match self {
            SaveState::Idle => "unsaved",
            SaveState::Saving => "saving…",
            SaveState::Saved => "saved",
            SaveState::Offline => "offline",
            SaveState::Conflict => "conflict",
        }
    }

    fn color(&self, palette: Palette) -> Color {
        match self {
            SaveState::Idle => palette.accent,
            SaveState::Saving => palette.heading,
            SaveState::Saved => palette.good,
            SaveState::Offline => palette.error,
            SaveState::Conflict => palette.error,
        }
    }
}

type NotesFetchResult = std::result::Result<Option<(Vec<u8>, Option<String>)>, String>;

enum NetEvent {
    Listed {
        path: String,
        result: std::result::Result<Vec<RemoteEntry>, String>,
    },
    Fetched {
        path: String,
        result: std::result::Result<(Vec<u8>, Option<String>), String>,
    },
    NotesFetched {
        path: String,
        result: NotesFetchResult,
    },
    Saved {
        path: String,
        rev: u64,
        result: std::result::Result<Option<String>, String>,
        conflict: bool,
        backup_error: Option<String>,
    },
}

struct Input {
    kind: InputKind,
    label: String,
    value: String,
    cursor: usize,
}

#[derive(Clone, Debug)]
enum InputKind {
    NewChapter,
    ChapterTitle(usize),
    BookTitle,
    NewFileName,
    SaveCopy,
    ConfigField,
}

impl Input {
    fn new(kind: InputKind, label: impl Into<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self {
            kind,
            label: label.into(),
            value,
            cursor,
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    fn insert(&mut self, ch: char) {
        let ch = if self.smart_quotes() {
            let prev = if self.cursor > 0 {
                self.value.chars().nth(self.cursor - 1)
            } else {
                None
            };
            match ch {
                '"' => smart_quote_for(prev),
                '\'' => smart_single_quote_for(prev),
                _ => ch,
            }
        } else {
            ch
        };
        let at = self.byte_at(self.cursor);
        self.value.insert(at, ch);
        self.cursor += 1;
    }

    fn smart_quotes(&self) -> bool {
        matches!(
            self.kind,
            InputKind::NewChapter | InputKind::ChapterTitle(_) | InputKind::BookTitle
        )
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.cursor < self.value.chars().count() {
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.value.chars().count();
    }

    fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = self.byte_at(i);
        let end = self.byte_at(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor = i;
    }
}

struct FilePicker {
    path: String,
    entries: Vec<RemoteEntry>,
    selected: usize,
    loading: bool,
    error: Option<String>,
    filter: String,
    filtering: bool,
}

impl FilePicker {
    fn new(path: String) -> Self {
        Self {
            path,
            entries: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            filter: String::new(),
            filtering: false,
        }
    }

    fn visible(&self) -> Vec<&RemoteEntry> {
        let f = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|e| e.is_dir || f.is_empty() || e.name.to_lowercase().contains(&f))
            .collect()
    }

    fn selected_entry(&self) -> Option<&RemoteEntry> {
        self.visible().get(self.selected).copied()
    }
}

struct ChapterPicker {
    selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlay {
    None,
    Menu,
    Help,
    ChapterPicker,
    FilePicker,
    Input,
    Conflict,
    LockPrompt,
    LockWaiting,
    LockForce,
    AiPanel,
    Config,
    Selection,
    About,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Open,
    Save,
    DownloadCopy,
    Quit,
    Undo,
    Redo,
    Find,
    MarkSelection,
    SelectionActions,
    Bold,
    Italic,
    Underline,
    Strike,
    Subheading,
    AlignLeft,
    AlignCenter,
    AlignRight,
    AlignJustify,
    LineBreak,
    NewChapter,
    ChapterTitle,
    BookTitle,
    GotoChapter,
    PrevChapter,
    NextChapter,
    Redraw,
    Help,
    AiCheck,
    Config,
    Notes,
    About,
}

#[derive(Clone, Copy, Debug)]
enum MenuEntry {
    Item {
        label: &'static str,
        shortcut: &'static str,
        action: MenuAction,
    },
    Separator,
}

struct MenuDef {
    label: &'static str,
    entries: &'static [MenuEntry],
}

const MENUS: &[MenuDef] = &[
    MenuDef {
        label: "File",
        entries: &[
            MenuEntry::Item {
                label: "Open…",
                shortcut: "Ctrl+O",
                action: MenuAction::Open,
            },
            MenuEntry::Item {
                label: "Save",
                shortcut: "Ctrl+S",
                action: MenuAction::Save,
            },
            MenuEntry::Item {
                label: "Download a copy",
                shortcut: "",
                action: MenuAction::DownloadCopy,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "Quit",
                shortcut: "Ctrl+Q",
                action: MenuAction::Quit,
            },
        ],
    },
    MenuDef {
        label: "Edit",
        entries: &[
            MenuEntry::Item {
                label: "Undo",
                shortcut: "Ctrl+Z",
                action: MenuAction::Undo,
            },
            MenuEntry::Item {
                label: "Redo",
                shortcut: "Ctrl+Y",
                action: MenuAction::Redo,
            },
            MenuEntry::Item {
                label: "Find…",
                shortcut: "F3",
                action: MenuAction::Find,
            },
            MenuEntry::Item {
                label: "Mark selection",
                shortcut: "Ctrl+Space",
                action: MenuAction::MarkSelection,
            },
            MenuEntry::Item {
                label: "Selection actions…",
                shortcut: "",
                action: MenuAction::SelectionActions,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "Bold",
                shortcut: "Ctrl+B",
                action: MenuAction::Bold,
            },
            MenuEntry::Item {
                label: "Italic",
                shortcut: "Alt+I",
                action: MenuAction::Italic,
            },
            MenuEntry::Item {
                label: "Underline",
                shortcut: "Alt+U",
                action: MenuAction::Underline,
            },
            MenuEntry::Item {
                label: "Strikethrough",
                shortcut: "Alt+S",
                action: MenuAction::Strike,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "Subheading (H3)",
                shortcut: "",
                action: MenuAction::Subheading,
            },
            MenuEntry::Item {
                label: "Align left",
                shortcut: "",
                action: MenuAction::AlignLeft,
            },
            MenuEntry::Item {
                label: "Align center",
                shortcut: "",
                action: MenuAction::AlignCenter,
            },
            MenuEntry::Item {
                label: "Align right",
                shortcut: "",
                action: MenuAction::AlignRight,
            },
            MenuEntry::Item {
                label: "Justify",
                shortcut: "",
                action: MenuAction::AlignJustify,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "Line break",
                shortcut: "Shift+Enter",
                action: MenuAction::LineBreak,
            },
        ],
    },
    MenuDef {
        label: "Chapter",
        entries: &[
            MenuEntry::Item {
                label: "New chapter",
                shortcut: "Ctrl+N",
                action: MenuAction::NewChapter,
            },
            MenuEntry::Item {
                label: "Rename chapter…",
                shortcut: "Ctrl+T",
                action: MenuAction::ChapterTitle,
            },
            MenuEntry::Item {
                label: "Book title…",
                shortcut: "Ctrl+E",
                action: MenuAction::BookTitle,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "Go to chapter…",
                shortcut: "Ctrl+G",
                action: MenuAction::GotoChapter,
            },
            MenuEntry::Item {
                label: "Previous chapter",
                shortcut: "Alt+Up",
                action: MenuAction::PrevChapter,
            },
            MenuEntry::Item {
                label: "Next chapter",
                shortcut: "Alt+Down",
                action: MenuAction::NextChapter,
            },
        ],
    },
    MenuDef {
        label: "Tools",
        entries: &[
            MenuEntry::Item {
                label: "Configuration…",
                shortcut: "Ctrl+,",
                action: MenuAction::Config,
            },
            MenuEntry::Item {
                label: "Notes",
                shortcut: "F7",
                action: MenuAction::Notes,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "Check writing (AI)…",
                shortcut: "F8",
                action: MenuAction::AiCheck,
            },
        ],
    },
    MenuDef {
        label: "View",
        entries: &[MenuEntry::Item {
            label: "Redraw screen",
            shortcut: "Ctrl+L",
            action: MenuAction::Redraw,
        }],
    },
    MenuDef {
        label: "Help",
        entries: &[
            MenuEntry::Item {
                label: "Keyboard shortcuts",
                shortcut: "Alt+H",
                action: MenuAction::Help,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "About scribe",
                shortcut: "Alt+A",
                action: MenuAction::About,
            },
        ],
    },
];

#[derive(Clone, Copy, Debug, Default)]
struct MenuState {
    active: usize,
    selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionAction {
    Bold,
    Italic,
    Underline,
    Strike,
    Delete,
    None,
}

const SELECTION_ACTIONS: [(&str, SelectionAction); 6] = [
    ("Bold", SelectionAction::Bold),
    ("Italic", SelectionAction::Italic),
    ("Underline", SelectionAction::Underline),
    ("Strikethrough", SelectionAction::Strike),
    ("Delete", SelectionAction::Delete),
    ("None (keep selection)", SelectionAction::None),
];

#[derive(Clone, Copy, Debug, Default)]
struct SelectionMenu {
    selected: usize,
}

#[derive(Clone, Debug, Default)]
struct SearchState {
    active: bool,
    query: String,
    cursor: usize,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
    origin: Cursor,
    dirty: bool,
    no_match: bool,
    revision: u64,
}

struct CacheMeta {
    etag: Option<String>,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingAction {
    Open,
    Create,
    SaveCopy,
}

#[derive(Clone, Debug)]
struct PendingLock {
    path: String,
    action: PendingAction,
}

#[derive(Clone, Debug)]
enum ExitMode {
    Kicked(Kicker),
    Lost(LockInfo),
    Missing,
}

#[derive(Clone, Debug)]
enum FinalSave {
    NoChanges,
    Saved,
    Conflict,
    Failed(String),
    LocalFailed,
    Lost,
}

enum AiEvent {
    Progress {
        job_id: u64,
        done: usize,
        total: usize,
    },
    Done {
        job_id: u64,
        chapter: usize,
        text_hash: u64,
        result: std::result::Result<Vec<ai::Issue>, String>,
    },
    TestDone {
        result: std::result::Result<String, String>,
    },
}

struct AiJob {
    id: u64,
    chapter: usize,
    text_hash: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AiStatus {
    Checking,
    Ready,
    Error,
}

struct AiPanel {
    chapter: usize,
    status: AiStatus,
    message: String,
    issues: Vec<ai::Issue>,
    selected: usize,
    progress_done: usize,
    progress_total: usize,
    model: String,
    scroll: usize,
}

impl AiPanel {
    fn checking(chapter: usize, progress_total: usize, model: String) -> Self {
        Self {
            chapter,
            status: AiStatus::Checking,
            message: "Sending chapter to the AI…".to_string(),
            issues: Vec::new(),
            selected: 0,
            progress_done: 0,
            progress_total,
            model,
            scroll: 0,
        }
    }

    fn error(chapter: usize, message: impl Into<String>) -> Self {
        Self {
            chapter,
            status: AiStatus::Error,
            message: message.into(),
            issues: Vec::new(),
            selected: 0,
            progress_done: 0,
            progress_total: 0,
            model: String::new(),
            scroll: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigField {
    BaseUrl,
    Model,
    ApiKey,
    Language,
    JsonMode,
    Timeout,
    Theme,
}

const CONFIG_FIELDS: [ConfigField; 7] = [
    ConfigField::BaseUrl,
    ConfigField::Model,
    ConfigField::ApiKey,
    ConfigField::Language,
    ConfigField::JsonMode,
    ConfigField::Timeout,
    ConfigField::Theme,
];

impl ConfigField {
    fn label(self) -> &'static str {
        match self {
            ConfigField::BaseUrl => "Base URL",
            ConfigField::Model => "Model",
            ConfigField::ApiKey => "API key",
            ConfigField::Language => "Language",
            ConfigField::JsonMode => "JSON mode",
            ConfigField::Timeout => "Timeout",
            ConfigField::Theme => "Theme",
        }
    }

    fn is_text(self) -> bool {
        matches!(
            self,
            ConfigField::BaseUrl
                | ConfigField::Model
                | ConfigField::ApiKey
                | ConfigField::Language
                | ConfigField::Timeout
        )
    }
}

struct ConfigPanel {
    ai: ai::AiConfig,
    original_theme: ThemeName,
    selected: usize,
    editing: Option<Input>,
    status: Option<(String, bool)>,
    testing: bool,
}

impl ConfigPanel {
    fn new(ai: ai::AiConfig, theme: ThemeName) -> Self {
        Self {
            ai,
            original_theme: theme,
            selected: 0,
            editing: None,
            status: None,
            testing: false,
        }
    }

    fn field(&self) -> ConfigField {
        CONFIG_FIELDS[self.selected.min(CONFIG_FIELDS.len() - 1)]
    }

    /// The key is only ever shown masked, including while it is being replaced.
    fn display_key(&self) -> String {
        if self.field() == ConfigField::ApiKey
            && let Some(input) = &self.editing
        {
            let count = input.value.chars().count().min(40);
            if count == 0 {
                "(type a new key; blank keeps current)".to_string()
            } else {
                "•".repeat(count)
            }
        } else {
            ai::mask_key(&self.ai.api_key)
        }
    }

    fn display_value(&self, field: ConfigField) -> String {
        match field {
            ConfigField::BaseUrl => self.ai.base_url.clone(),
            ConfigField::Model => self.ai.model.clone(),
            ConfigField::ApiKey => self.display_key(),
            ConfigField::Language => self.ai.language.clone(),
            ConfigField::JsonMode => {
                if self.ai.json_mode {
                    "on".to_string()
                } else {
                    "off".to_string()
                }
            }
            ConfigField::Timeout => format!("{} s", self.ai.timeout_seconds),
            ConfigField::Theme => String::new(),
        }
    }

    fn start_edit(&mut self) {
        let field = self.field();
        if !field.is_text() {
            return;
        }
        let current = match field {
            ConfigField::ApiKey => String::new(),
            ConfigField::Timeout => self.ai.timeout_seconds.to_string(),
            _ => self.display_value(field),
        };
        self.editing = Some(Input::new(InputKind::ConfigField, field.label(), current));
        self.status = None;
    }

    fn commit_edit(&mut self) {
        let Some(input) = self.editing.take() else {
            return;
        };
        let value = input.value.trim().to_string();
        match self.field() {
            ConfigField::BaseUrl => self.ai.base_url = value,
            ConfigField::Model => self.ai.model = value,
            ConfigField::ApiKey => {
                if !value.is_empty() {
                    self.ai.api_key = value;
                }
            }
            ConfigField::Language => self.ai.language = value,
            ConfigField::Timeout => match value.parse::<u64>() {
                Ok(seconds) => self.ai.timeout_seconds = seconds.max(5),
                Err(_) => self.status = Some(("Timeout must be a number".to_string(), true)),
            },
            ConfigField::JsonMode | ConfigField::Theme => {}
        }
    }

    fn set_status(&mut self, message: impl Into<String>, error: bool) {
        self.status = Some((message.into(), error));
    }
}

/// The per-document state that is parked while the notes sidecar is open.
struct DocMemory {
    editor: Editor,
    local_path: Option<PathBuf>,
    remote_path: Option<String>,
    etag: Option<String>,
    create_new: bool,
    save_state: SaveState,
    conflict: bool,
}

/// Remote notes are fetched in the background. The generated document is kept
/// here in case the sidecar does not exist yet.
struct PendingNotes {
    path: String,
    created: OdtDocument,
}

pub struct App {
    cfg: Option<Config>,
    client: Option<Client>,
    editor: Option<Editor>,
    local_path: Option<PathBuf>,
    remote_path: Option<String>,
    pending_open: Option<String>,
    force_remote_reload: bool,
    etag: Option<String>,
    create_new: bool,
    save_state: SaveState,
    conflict: bool,
    net_tx: Sender<NetEvent>,
    net_rx: Receiver<NetEvent>,
    net_busy: bool,
    last_edit: Instant,
    last_local_save: Instant,
    last_remote_attempt: Instant,
    toast: Option<(String, Instant)>,
    redraw_requested: bool,
    quit: bool,
    quit_armed: Option<Instant>,
    overlay: Overlay,
    menu: MenuState,
    input: Option<Input>,
    file_picker: Option<FilePicker>,
    chapter_picker: Option<ChapterPicker>,
    lock_identity: Identity,
    lock_client: Option<Client>,
    lock: Option<LockHandle>,
    previous_lock: Option<LockHandle>,
    lock_tx: Sender<LockEvent>,
    lock_rx: Receiver<LockEvent>,
    lock_watch_stop: Option<Sender<()>>,
    lock_pending: Option<PendingLock>,
    lock_prompt: Option<HeldLock>,
    exit_mode: Option<ExitMode>,
    exit_notice: Option<String>,
    ai_tx: Sender<AiEvent>,
    ai_rx: Receiver<AiEvent>,
    ai_seq: u64,
    ai_job: Option<AiJob>,
    ai_panel: Option<AiPanel>,
    settings: Settings,
    palette: Palette,
    config_panel: Option<ConfigPanel>,
    selection_menu: Option<SelectionMenu>,
    notes_active: bool,
    notes_loading: bool,
    notes_main: Option<DocMemory>,
    notes_doc: Option<DocMemory>,
    pending_notes: Option<PendingNotes>,
    search: SearchState,
}

impl App {
    pub fn new_remote(cfg: Config) -> Result<Self> {
        config::ensure_dirs()?;
        let ui_settings = settings::load().unwrap_or_default();
        let palette = ui_settings.theme.palette();
        let client = Client::new(&cfg);
        let lock_client = Some(Client::with_timeout(&cfg, Duration::from_secs(5)));
        let lock_identity = Identity::new(&cfg.login_name);
        let (net_tx, net_rx) = mpsc::channel();
        let (lock_tx, lock_rx) = mpsc::channel();
        let (ai_tx, ai_rx) = mpsc::channel();
        let folder = if cfg.last_folder == "/" && cfg.remote_root != "/" {
            cfg.remote_root.clone()
        } else {
            cfg.last_folder.clone()
        };
        let mut app = Self {
            cfg: Some(cfg),
            client: Some(client),
            editor: None,
            local_path: None,
            remote_path: None,
            pending_open: None,
            force_remote_reload: false,
            etag: None,
            create_new: false,
            save_state: SaveState::Idle,
            conflict: false,
            net_tx,
            net_rx,
            net_busy: false,
            last_edit: Instant::now(),
            last_local_save: Instant::now(),
            last_remote_attempt: Instant::now() - Duration::from_secs(60),
            toast: None,
            redraw_requested: false,
            quit: false,
            quit_armed: None,
            overlay: Overlay::FilePicker,
            menu: MenuState::default(),
            input: None,
            file_picker: Some(FilePicker::new(folder)),
            chapter_picker: None,
            lock_identity,
            lock_client,
            lock: None,
            previous_lock: None,
            lock_tx,
            lock_rx,
            lock_watch_stop: None,
            lock_pending: None,
            lock_prompt: None,
            exit_mode: None,
            exit_notice: None,
            ai_tx,
            ai_rx,
            ai_seq: 0,
            ai_job: None,
            ai_panel: None,
            settings: ui_settings,
            palette,
            config_panel: None,
            selection_menu: None,
            notes_active: false,
            notes_loading: false,
            notes_main: None,
            notes_doc: None,
            pending_notes: None,
            search: SearchState::default(),
        };
        app.refresh_file_picker();
        Ok(app)
    }

    pub fn new_local(path: PathBuf) -> Result<Self> {
        config::ensure_dirs()?;
        let ui_settings = settings::load().unwrap_or_default();
        let palette = ui_settings.theme.palette();
        let (net_tx, net_rx) = mpsc::channel();
        let (lock_tx, lock_rx) = mpsc::channel();
        let (ai_tx, ai_rx) = mpsc::channel();
        let mut app = Self {
            cfg: None,
            client: None,
            editor: None,
            local_path: Some(path.clone()),
            remote_path: None,
            pending_open: None,
            force_remote_reload: false,
            etag: None,
            create_new: false,
            save_state: SaveState::Idle,
            conflict: false,
            net_tx,
            net_rx,
            net_busy: false,
            last_edit: Instant::now(),
            last_local_save: Instant::now(),
            last_remote_attempt: Instant::now(),
            toast: None,
            redraw_requested: false,
            quit: false,
            quit_armed: None,
            overlay: Overlay::None,
            menu: MenuState::default(),
            input: None,
            file_picker: None,
            chapter_picker: None,
            lock_identity: Identity::new("local"),
            lock_client: None,
            lock: None,
            previous_lock: None,
            lock_tx,
            lock_rx,
            lock_watch_stop: None,
            lock_pending: None,
            lock_prompt: None,
            exit_mode: None,
            exit_notice: None,
            ai_tx,
            ai_rx,
            ai_seq: 0,
            ai_job: None,
            ai_panel: None,
            settings: ui_settings,
            palette,
            config_panel: None,
            selection_menu: None,
            notes_active: false,
            notes_loading: false,
            notes_main: None,
            notes_doc: None,
            pending_notes: None,
            search: SearchState::default(),
        };
        if path.exists() {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Untitled");
            let doc = OdtDocument::from_bytes(&bytes, title)?;
            app.install_editor(doc, Some(path), None, None, false);
        } else {
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Untitled");
            let doc = OdtDocument::new(title)?;
            app.install_editor(doc, Some(path), None, None, true);
            app.save_local_now();
        }
        Ok(app)
    }

    pub fn exit_notice(&self) -> Option<&str> {
        self.exit_notice.as_deref()
    }

    pub fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            if self.redraw_requested {
                terminal.clear()?;
                self.redraw_requested = false;
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
                    Event::Paste(text) => {
                        if self.overlay == Overlay::None {
                            if let Some(ed) = &mut self.editor {
                                let rev = ed.revision;
                                ed.insert_text(&text);
                                if ed.revision != rev {
                                    self.last_edit = Instant::now();
                                    self.save_state = SaveState::Idle;
                                }
                            }
                        }
                    }
                    Event::Resize(w, h) => {
                        if let Some(ed) = &mut self.editor {
                            ed.set_viewport(w, h);
                        }
                    }
                    _ => {}
                }
            }
            self.drain_net();
            self.drain_lock();
            self.drain_ai();
            self.tick();
            if self.quit {
                break;
            }
        }
        Ok(())
    }

    fn install_editor(
        &mut self,
        doc: OdtDocument,
        local_path: Option<PathBuf>,
        remote_path: Option<String>,
        etag: Option<String>,
        create_new: bool,
    ) {
        let (w, h) = crossterm::terminal::size().unwrap_or((100, 40));
        let ed = Editor::new(doc, w, h);
        self.local_path = local_path;
        self.remote_path = remote_path;
        self.pending_open = None;
        self.force_remote_reload = false;
        self.etag = etag;
        self.create_new = create_new;
        self.editor = Some(ed);
        self.search = SearchState::default();
        self.overlay = Overlay::None;
        self.file_picker = None;
        self.chapter_picker = None;
        self.input = None;
        self.conflict = false;
        self.save_state = if create_new {
            SaveState::Idle
        } else {
            SaveState::Saved
        };
        self.last_edit = Instant::now();
        self.last_local_save = Instant::now() - Duration::from_secs(60);
        self.last_remote_attempt = Instant::now() - Duration::from_secs(60);
        self.update_config_last_file();
    }

    fn update_config_last_file(&mut self) {
        if self
            .remote_path
            .as_deref()
            .is_some_and(|path| path.ends_with(".notes.odt"))
        {
            return;
        }
        if let (Some(cfg), Some(path)) = (&mut self.cfg, &self.remote_path) {
            cfg.last_file = Some(path.clone());
            if let Some((folder, _)) = path.rsplit_once('/') {
                cfg.last_folder = if folder.is_empty() {
                    "/".to_string()
                } else {
                    folder.to_string()
                };
            }
            let _ = config::save(cfg);
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now() + Duration::from_secs(4)));
    }

    fn drain_net(&mut self) {
        loop {
            let ev = match self.net_rx.try_recv() {
                Ok(ev) => ev,
                Err(_) => break,
            };
            match ev {
                NetEvent::Listed { path, result } => {
                    if let Some(fp) = &mut self.file_picker {
                        if fp.path == path {
                            fp.loading = false;
                            match result {
                                Ok(mut entries) => {
                                    if fp.path != "/" {
                                        entries.insert(0, parent_entry(&fp.path));
                                    }
                                    fp.entries = entries
                                        .into_iter()
                                        .filter(|e| {
                                            e.is_dir || e.name.to_lowercase().ends_with(".odt")
                                        })
                                        .collect();
                                    fp.selected = 0;
                                    fp.error = None;
                                }
                                Err(e) => {
                                    fp.error = Some(e);
                                    fp.entries.clear();
                                }
                            }
                        }
                    }
                }
                NetEvent::Fetched { path, result } => {
                    if self.pending_open.as_deref() != Some(path.as_str()) {
                        continue;
                    }
                    self.pending_open = None;
                    let force = self.force_remote_reload;
                    self.force_remote_reload = false;
                    match result {
                        Ok((bytes, etag)) => {
                            let title = path
                                .rsplit('/')
                                .next()
                                .unwrap_or("Untitled")
                                .trim_end_matches(".odt");
                            let cached = if force { None } else { self.read_cache(&path) };
                            let mut dirty = false;
                            let mut conflict = false;
                            let open_bytes = match cached {
                                Some((cached_bytes, meta)) if meta.dirty => {
                                    dirty = true;
                                    conflict = meta.etag != etag;
                                    cached_bytes
                                }
                                _ => bytes.clone(),
                            };
                            match OdtDocument::from_bytes(&open_bytes, title) {
                                Ok(doc) => {
                                    self.install_editor(
                                        doc,
                                        None,
                                        Some(path.clone()),
                                        etag.clone(),
                                        false,
                                    );
                                    let release_warning = self.release_previous_lock();
                                    if dirty {
                                        if let Some(ed) = &mut self.editor {
                                            ed.dirty = true;
                                        }
                                        self.last_edit = Instant::now();
                                        self.last_local_save =
                                            Instant::now() - Duration::from_secs(60);
                                    }
                                    if conflict {
                                        self.conflict = true;
                                        self.save_state = SaveState::Conflict;
                                        self.overlay = Overlay::Conflict;
                                        self.toast("Local changes conflict with the remote file.");
                                    } else if dirty {
                                        self.save_state = SaveState::Idle;
                                        self.toast("Restored unsynced local changes.");
                                    } else {
                                        self.write_cache(&path, &bytes, etag.as_deref(), false);
                                        self.save_state = SaveState::Saved;
                                        self.toast = None;
                                    }
                                    if let Some(warning) = release_warning {
                                        self.toast(warning);
                                    }
                                }
                                Err(e) => {
                                    if force {
                                        self.conflict = true;
                                        self.save_state = SaveState::Conflict;
                                        self.overlay = Overlay::Conflict;
                                        self.toast(format!("Reload failed; conflict remains: {e}"));
                                    } else if dirty {
                                        if let Ok(doc) = OdtDocument::from_bytes(&bytes, title) {
                                            self.install_editor(
                                                doc,
                                                None,
                                                Some(path.clone()),
                                                etag.clone(),
                                                false,
                                            );
                                            let release_warning = self.release_previous_lock();
                                            self.write_cache(&path, &bytes, etag.as_deref(), false);
                                            self.save_state = SaveState::Saved;
                                            self.toast(
                                                "Cached copy was unreadable; loaded remote.",
                                            );
                                            if let Some(warning) = release_warning {
                                                self.toast(warning);
                                            }
                                        } else {
                                            self.restore_previous_lock();
                                            self.toast(format!("Cannot open: {e}"));
                                        }
                                    } else {
                                        self.restore_previous_lock();
                                        self.toast(format!("Cannot open: {e}"));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if force {
                                self.conflict = true;
                                self.save_state = SaveState::Conflict;
                                self.overlay = Overlay::Conflict;
                                self.toast(format!("Reload failed; conflict remains: {e}"));
                                continue;
                            }
                            if let Some((bytes, meta)) = self.read_cache(&path) {
                                let title = path
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or("Untitled")
                                    .trim_end_matches(".odt");
                                match OdtDocument::from_bytes(&bytes, title) {
                                    Ok(doc) => {
                                        let dirty = meta.dirty;
                                        self.install_editor(
                                            doc,
                                            None,
                                            Some(path),
                                            meta.etag,
                                            false,
                                        );
                                        let release_warning = self.release_previous_lock();
                                        if dirty {
                                            if let Some(ed) = &mut self.editor {
                                                ed.dirty = true;
                                            }
                                            self.last_edit = Instant::now();
                                            self.last_local_save =
                                                Instant::now() - Duration::from_secs(60);
                                        }
                                        self.save_state = SaveState::Offline;
                                        self.toast(format!("Offline copy: {e}"));
                                        if let Some(warning) = release_warning {
                                            self.toast(warning);
                                        }
                                    }
                                    Err(ce) => {
                                        self.restore_previous_lock();
                                        self.toast(format!("Cannot open cached copy: {ce}"));
                                    }
                                }
                            } else {
                                self.restore_previous_lock();
                                self.toast(format!("Open failed: {e}"));
                            }
                        }
                    }
                }
                NetEvent::NotesFetched { path, result } => {
                    if self
                        .pending_notes
                        .as_ref()
                        .map(|pending| pending.path.as_str())
                        != Some(path.as_str())
                    {
                        continue;
                    }
                    let Some(pending) = self.pending_notes.take() else {
                        continue;
                    };
                    self.finish_notes_fetch(pending, result);
                }
                NetEvent::Saved {
                    path,
                    rev,
                    result,
                    conflict,
                    backup_error,
                } => {
                    self.net_busy = false;
                    if let Some(e) = backup_error {
                        self.toast(format!("Backup failed: {e}"));
                    }
                    let parked = if conflict {
                        self.update_parked_save(&path, None, rev, true, false)
                    } else {
                        match &result {
                            Ok(etag) => {
                                self.update_parked_save(&path, etag.clone(), rev, false, false)
                            }
                            Err(_) => self.update_parked_save(&path, None, rev, false, true),
                        }
                    };
                    if let Some(label) = parked {
                        if conflict {
                            self.toast(format!(
                                "{label} changed remotely; resolve it when you switch back."
                            ));
                        } else if result.is_err() {
                            self.toast(format!("{label} save failed."));
                        }
                        continue;
                    }
                    if conflict {
                        self.conflict = true;
                        self.save_state = SaveState::Conflict;
                        self.overlay = Overlay::Conflict;
                        self.toast("Remote file changed. Resolve the conflict.");
                        continue;
                    }
                    match result {
                        Ok(etag) => {
                            if self.remote_path.as_deref() == Some(path.as_str()) {
                                self.etag = etag.clone();
                                self.create_new = false;
                                let current = self
                                    .editor
                                    .as_ref()
                                    .map(|e| e.revision == rev)
                                    .unwrap_or(false);
                                if current {
                                    if let Some(ed) = &mut self.editor {
                                        ed.mark_saved();
                                    }
                                    self.save_state = SaveState::Saved;
                                    if let Some(Ok(bytes)) =
                                        self.editor.as_mut().map(|e| e.save_bytes())
                                    {
                                        self.write_cache(&path, &bytes, etag.as_deref(), false);
                                    }
                                } else {
                                    self.save_state = SaveState::Idle;
                                }
                            }
                        }
                        Err(e) => {
                            self.save_state = SaveState::Offline;
                            self.toast(format!("Remote save failed: {e}"));
                        }
                    }
                }
            }
        }
    }

    fn drain_lock(&mut self) {
        if self.exit_mode.is_some() {
            while let Ok(event) = self.lock_rx.try_recv() {
                if let LockEvent::Acquired { handle, .. } = event {
                    self.release_orphan(&handle);
                }
            }
            return;
        }
        while let Ok(event) = self.lock_rx.try_recv() {
            match event {
                LockEvent::Acquired { path, handle } => self.on_lock_acquired(path, handle),
                LockEvent::Held { path, held } => self.on_lock_held(path, held),
                LockEvent::TimedOut { path } => self.on_lock_timed_out(path),
                LockEvent::Kicked { path, by } => self.on_kicked(path, by),
                LockEvent::Lost { path, new_owner } => self.on_lock_lost(path, new_owner),
                LockEvent::Missing { path } => self.on_lock_missing(path),
                LockEvent::Error { path, message } => self.on_lock_error(path, message),
            }
        }
    }

    fn pending_matches(&self, path: &str) -> bool {
        self.lock_pending
            .as_ref()
            .map(|pending| pending.path == path)
            .unwrap_or(false)
    }

    fn on_lock_acquired(&mut self, path: String, handle: LockHandle) {
        let Some(pending) = self.lock_pending.take() else {
            self.release_orphan(&handle);
            return;
        };
        if pending.path != path {
            self.release_orphan(&handle);
            return;
        }
        if self.exit_mode.is_some() {
            self.release_orphan(&handle);
            return;
        }
        self.stash_current_lock();
        self.lock = Some(handle.clone());
        self.start_lock_watcher(handle);
        self.overlay = Overlay::None;
        match pending.action {
            PendingAction::Open => self.open_remote(path),
            PendingAction::Create => self.finish_create_remote_file(path),
            PendingAction::SaveCopy => self.finish_save_copy(path),
        }
    }

    fn on_lock_held(&mut self, path: String, held: HeldLock) {
        if !self.pending_matches(&path) {
            return;
        }
        let action = self
            .lock_pending
            .as_ref()
            .map(|pending| pending.action)
            .unwrap_or(PendingAction::Open);
        if action != PendingAction::Open {
            self.lock_pending = None;
            self.toast("That name is already open in another scribe instance.");
            if self.conflict {
                self.overlay = Overlay::Conflict;
            } else if self.editor.is_none() {
                self.return_to_file_picker();
            } else {
                self.overlay = Overlay::None;
            }
            return;
        }
        self.lock_prompt = Some(held);
        self.overlay = Overlay::LockPrompt;
        self.toast = None;
    }

    fn on_lock_timed_out(&mut self, path: String) {
        if !self.pending_matches(&path) {
            return;
        }
        if self.lock_pending.as_ref().map(|pending| pending.action) == Some(PendingAction::Open) {
            self.overlay = Overlay::LockForce;
            self.toast = None;
        }
    }

    fn on_lock_error(&mut self, path: String, message: String) {
        if !self.pending_matches(&path) {
            return;
        }
        self.lock_pending = None;
        self.lock_prompt = None;
        self.toast(format!("Lock error: {message}"));
        if self.conflict {
            self.overlay = Overlay::Conflict;
        } else if self.editor.is_none() {
            self.return_to_file_picker();
        } else {
            self.overlay = Overlay::None;
        }
    }

    fn return_to_file_picker(&mut self) {
        if self.file_picker.is_some() {
            self.overlay = Overlay::FilePicker;
            self.refresh_file_picker();
        } else {
            self.open_file_picker();
        }
    }

    fn on_kicked(&mut self, path: String, by: Kicker) {
        if self.lock.as_ref().map(|lock| lock.path.as_str()) != Some(path.as_str()) {
            return;
        }
        self.stop_lock_watcher();
        self.exit_mode = Some(ExitMode::Kicked(by));
        self.overlay = Overlay::None;
        self.toast("Enforced exit by the new editor.");
    }

    fn on_lock_lost(&mut self, path: String, new_owner: LockInfo) {
        if self.lock.as_ref().map(|lock| lock.path.as_str()) != Some(path.as_str()) {
            return;
        }
        self.stop_lock_watcher();
        self.exit_mode = Some(ExitMode::Lost(new_owner));
        self.overlay = Overlay::None;
    }

    fn on_lock_missing(&mut self, path: String) {
        if self.lock.as_ref().map(|lock| lock.path.as_str()) != Some(path.as_str()) {
            return;
        }
        self.stop_lock_watcher();
        self.exit_mode = Some(ExitMode::Missing);
        self.overlay = Overlay::None;
    }

    fn start_lock_watcher(&mut self, handle: LockHandle) {
        let Some(client) = self.lock_client.clone() else {
            return;
        };
        let (stop_tx, stop_rx) = mpsc::channel();
        self.lock_watch_stop = Some(stop_tx);
        lock::spawn_watch(client, handle, stop_rx, self.lock_tx.clone());
    }

    fn stop_lock_watcher(&mut self) {
        if let Some(stop) = self.lock_watch_stop.take() {
            let _ = stop.send(());
        }
    }

    fn stash_current_lock(&mut self) {
        self.stop_lock_watcher();
        if let Some(old) = self.lock.take()
            && let Some(previous) = self.previous_lock.replace(old)
        {
            self.release_orphan(&previous);
        }
    }

    fn release_handle(&self, handle: &LockHandle) -> Option<String> {
        let Some(client) = &self.lock_client else {
            return None;
        };
        match lock::release(client, &handle.path, &handle.owner) {
            Ok(true) => None,
            Ok(false) => Some(format!(
                "Lock file {} was left in place because another instance owns it.",
                lock::lock_path(&handle.path)
            )),
            Err(error) => Some(format!(
                "Could not remove lock file {}: {error}",
                lock::lock_path(&handle.path)
            )),
        }
    }

    fn release_orphan(&mut self, handle: &LockHandle) {
        let _ = self.release_handle(handle);
    }

    fn release_current_lock(&mut self) -> Option<String> {
        self.stop_lock_watcher();
        let handle = self.lock.take()?;
        self.release_handle(&handle)
    }

    fn release_previous_lock(&mut self) -> Option<String> {
        let handle = self.previous_lock.take()?;
        self.release_handle(&handle)
    }

    fn restore_previous_lock(&mut self) {
        let _ = self.release_current_lock();
        if let Some(handle) = self.previous_lock.take() {
            self.lock = Some(handle.clone());
            self.start_lock_watcher(handle);
        }
    }

    fn finish_quit(&mut self) {
        let mut warnings = Vec::new();
        if let Some(warning) = self.release_current_lock() {
            warnings.push(warning);
        }
        if let Some(warning) = self.release_previous_lock() {
            warnings.push(warning);
        }
        if !warnings.is_empty() {
            self.exit_notice = Some(warnings.join("\n"));
        }
        self.quit = true;
    }

    fn lock_is_ours(&self) -> bool {
        let (Some(handle), Some(client)) = (self.lock.as_ref(), self.lock_client.as_ref()) else {
            return false;
        };
        match lock::probe(client, &handle.path) {
            Ok(Some(held)) => held.info.owner == handle.owner,
            Ok(None) => true,
            Err(_) => true,
        }
    }

    fn final_remote_save(&mut self) -> FinalSave {
        let Some(path) = self.remote_path.clone() else {
            return FinalSave::NoChanges;
        };
        let Some(client) = self.client.clone() else {
            return FinalSave::LocalFailed;
        };
        let etag = self.etag.clone();
        let create = self.create_new;
        let bytes = match self.editor.as_mut().map(|editor| editor.save_bytes()) {
            Some(Ok(bytes)) => bytes,
            Some(Err(error)) => {
                self.toast(format!("Serialize failed: {error}"));
                return FinalSave::LocalFailed;
            }
            None => return FinalSave::NoChanges,
        };
        match client.put(&path, &bytes, etag.as_deref(), create) {
            Ok(etag) => {
                self.etag = etag;
                self.create_new = false;
                if let Some(editor) = &mut self.editor {
                    editor.mark_saved();
                }
                self.save_state = SaveState::Saved;
                self.write_cache(&path, &bytes, self.etag.as_deref(), false);
                if let Err(error) = backups::save_backup(
                    &remote_identity(self.cfg.as_ref(), &path),
                    &backup_display(&path),
                    &bytes,
                ) {
                    self.toast(format!("Backup failed: {error}"));
                }
                FinalSave::Saved
            }
            Err(error) => {
                let text = format!("{error}");
                if text.contains("412") {
                    self.conflict = true;
                    self.save_state = SaveState::Conflict;
                    FinalSave::Conflict
                } else {
                    self.save_state = SaveState::Offline;
                    FinalSave::Failed(text)
                }
            }
        }
    }

    fn process_exit_mode(&mut self) {
        let Some(mode) = self.exit_mode.clone() else {
            return;
        };
        if matches!(mode, ExitMode::Kicked(_)) && self.net_busy {
            return;
        }

        let dirty = self
            .editor
            .as_ref()
            .map(|editor| editor.dirty)
            .unwrap_or(false);
        let mut outcome = FinalSave::NoChanges;
        if dirty {
            let local_ok = self.save_local_now();
            if !local_ok {
                outcome = FinalSave::LocalFailed;
            } else {
                match mode {
                    ExitMode::Kicked(_) => {
                        if self.conflict {
                            outcome = FinalSave::Conflict;
                        } else if self.lock_is_ours() {
                            outcome = self.final_remote_save();
                        } else {
                            outcome = FinalSave::Lost;
                        }
                    }
                    ExitMode::Lost(_) | ExitMode::Missing => {
                        outcome = FinalSave::Lost;
                    }
                }
            }
        }

        let mut warnings = Vec::new();
        match mode {
            ExitMode::Kicked(_) => {
                if let Some(warning) = self.release_current_lock() {
                    warnings.push(warning);
                }
                if let Some(warning) = self.release_previous_lock() {
                    warnings.push(warning);
                }
            }
            ExitMode::Lost(_) | ExitMode::Missing => {
                self.stop_lock_watcher();
                self.lock = None;
                if let Some(warning) = self.release_previous_lock() {
                    warnings.push(warning);
                }
            }
        }

        self.exit_notice = Some(self.exit_notice_text(&mode, &outcome, &warnings));
        self.quit = true;
    }

    fn exit_notice_text(
        &self,
        mode: &ExitMode,
        outcome: &FinalSave,
        warnings: &[String],
    ) -> String {
        let mut text = match mode {
            ExitMode::Kicked(by) => format!(
                "Enforced exit by the new editor ({} / {}).",
                by.host, by.user
            ),
            ExitMode::Lost(owner) => format!(
                "Enforced exit by the new editor ({} / {}).",
                owner.host, owner.user
            ),
            ExitMode::Missing => "The file lock was removed by another editor.".to_string(),
        };
        let tail = match outcome {
            FinalSave::NoChanges => "There were no unsaved changes.",
            FinalSave::Saved => "Your latest edits were saved to Nextcloud.",
            FinalSave::Conflict => {
                "A save conflict occurred; your latest edits are in the local cache."
            }
            FinalSave::Failed(error) => {
                text.push_str(&format!(
                    "\nYour latest edits are in the local cache; the Nextcloud save failed: {error}"
                ));
                return append_warnings(text, warnings);
            }
            FinalSave::LocalFailed => {
                "Your latest edits could not be written to the local cache either."
            }
            FinalSave::Lost => {
                "Another editor had already taken over; your latest edits are in the local cache."
            }
        };
        text.push('\n');
        text.push_str(tail);
        append_warnings(text, warnings)
    }

    fn handle_lock_prompt_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('k') | KeyCode::Char('K') => {
                let Some(held) = self.lock_prompt.take() else {
                    return;
                };
                let path = match self.lock_pending.as_ref() {
                    Some(pending) => pending.path.clone(),
                    None => {
                        self.lock_prompt = Some(held);
                        return;
                    }
                };
                let Some(client) = self.lock_client.clone() else {
                    return;
                };
                self.overlay = Overlay::LockWaiting;
                self.toast = None;
                lock::spawn_kick(
                    client,
                    path,
                    held.info.owner,
                    self.lock_identity.clone(),
                    self.lock_tx.clone(),
                );
            }
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => {
                self.lock_prompt = None;
                self.lock_pending = None;
                if self.editor.is_none() {
                    self.return_to_file_picker();
                } else {
                    self.overlay = Overlay::None;
                    self.toast("Open cancelled.");
                }
            }
            _ => {}
        }
    }

    fn handle_lock_force_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('f') | KeyCode::Char('F') => {
                let path = match self.lock_pending.as_ref() {
                    Some(pending) => pending.path.clone(),
                    None => return,
                };
                let Some(client) = self.lock_client.clone() else {
                    return;
                };
                self.overlay = Overlay::LockWaiting;
                self.toast = None;
                lock::spawn_force(
                    client,
                    path,
                    self.lock_identity.clone(),
                    self.lock_tx.clone(),
                );
            }
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => {
                self.lock_pending = None;
                if self.editor.is_none() {
                    self.return_to_file_picker();
                } else {
                    self.overlay = Overlay::None;
                    self.toast("Open cancelled.");
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        if let Some((_, until)) = &self.toast {
            if Instant::now() > *until {
                self.toast = None;
            }
        }
        if self.exit_mode.is_some() {
            self.process_exit_mode();
            return;
        }
        let Some(ed) = &self.editor else {
            return;
        };
        if !ed.dirty || self.conflict {
            return;
        }
        if self.last_edit.elapsed() >= Duration::from_secs(2)
            && self.last_local_save.elapsed() >= Duration::from_secs(2)
        {
            self.save_local_now();
        }
        if self.remote_path.is_some()
            && self.last_edit.elapsed() >= Duration::from_secs(8)
            && self.last_remote_attempt.elapsed() >= Duration::from_secs(8)
            && !self.net_busy
        {
            self.save_remote_now();
        }
    }

    fn save_local_now(&mut self) -> bool {
        let local = self.local_path.clone();
        let remote = self.remote_path.clone();
        let etag = self.etag.clone();
        let Some(ed) = &mut self.editor else {
            return true;
        };
        let bytes = match ed.save_bytes() {
            Ok(b) => b,
            Err(e) => {
                self.toast(format!("Serialize failed: {e}"));
                return false;
            }
        };
        let mut backup_error = None;
        if let Some(path) = &local {
            if let Err(e) = atomic_write(path, &bytes) {
                self.toast(format!("Local save failed: {e}"));
                return false;
            }
            ed.mark_saved();
            self.save_state = SaveState::Saved;
            if let Err(e) =
                backups::save_backup(&local_identity(path), &local_display(path), &bytes)
            {
                backup_error = Some(e.to_string());
            }
        } else if let Some(path) = &remote {
            self.write_cache(path, &bytes, etag.as_deref(), true);
        }
        self.last_local_save = Instant::now();
        if let Some(e) = backup_error {
            self.toast(format!("Backup failed: {e}"));
        }
        true
    }

    fn save_remote_now(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(path) = self.remote_path.clone() else {
            return;
        };
        let etag = self.etag.clone();
        let create = self.create_new;
        let Some(ed) = &mut self.editor else {
            return;
        };
        let bytes = match ed.save_bytes() {
            Ok(b) => b,
            Err(e) => {
                self.toast(format!("Serialize failed: {e}"));
                return;
            }
        };
        let rev = ed.revision;
        self.write_cache(&path, &bytes, etag.as_deref(), true);
        let backup_identity = remote_identity(self.cfg.as_ref(), &path);
        let backup_display = backup_display(&path);
        let tx = self.net_tx.clone();
        self.net_busy = true;
        self.save_state = SaveState::Saving;
        self.last_remote_attempt = Instant::now();
        std::thread::spawn(move || {
            let res = client.put(&path, &bytes, etag.as_deref(), create);
            match res {
                Ok(etag) => {
                    let backup_error =
                        backups::save_backup(&backup_identity, &backup_display, &bytes)
                            .err()
                            .map(|e| e.to_string());
                    let _ = tx.send(NetEvent::Saved {
                        path,
                        rev,
                        result: Ok(etag),
                        conflict: false,
                        backup_error,
                    });
                }
                Err(e) => {
                    let s = format!("{e}");
                    let conflict = s.contains("412") || s.contains("HTTP 412");
                    let _ = tx.send(NetEvent::Saved {
                        path,
                        rev,
                        result: Err(s),
                        conflict,
                        backup_error: None,
                    });
                }
            }
        });
    }

    fn refresh_file_picker(&mut self) {
        let Some(fp) = &mut self.file_picker else {
            return;
        };
        fp.loading = true;
        fp.error = None;
        let path = fp.path.clone();
        let Some(client) = &self.client else {
            return;
        };
        let client = client.clone();
        let tx = self.net_tx.clone();
        let send_path = path.clone();
        std::thread::spawn(move || {
            let result = client.list(&path).map_err(|e| format!("{e}"));
            let _ = tx.send(NetEvent::Listed {
                path: send_path,
                result,
            });
        });
    }

    fn request_open_remote(&mut self, path: String) {
        if self.remote_path.as_deref() == Some(path.as_str()) && self.editor.is_some() {
            return;
        }
        self.clear_notes_session();
        self.search = SearchState::default();
        let Some(client) = self.lock_client.clone() else {
            self.open_remote(path);
            return;
        };
        self.lock_pending = Some(PendingLock {
            path: path.clone(),
            action: PendingAction::Open,
        });
        self.overlay = Overlay::None;
        self.toast("Checking file lock…");
        lock::spawn_acquire(
            client,
            path,
            self.lock_identity.clone(),
            self.lock_tx.clone(),
        );
    }

    fn open_remote(&mut self, path: String) {
        let Some(client) = &self.client else {
            return;
        };
        let client = client.clone();
        let tx = self.net_tx.clone();
        self.force_remote_reload = false;
        self.pending_open = Some(path.clone());
        self.toast = Some(("Opening…".into(), Instant::now() + Duration::from_secs(30)));
        std::thread::spawn(move || {
            let result = client.get(&path).map_err(|e| format!("{e}"));
            let _ = tx.send(NetEvent::Fetched { path, result });
        });
    }

    fn cache_paths(&self, remote: &str) -> (PathBuf, PathBuf) {
        let key = if let Some(cfg) = &self.cfg {
            format!("{}|{}|{}", cfg.server, cfg.login_name, remote)
        } else {
            remote.to_string()
        };
        let hash = format!("{:016x}", stable_hash(&key));
        let dir = config::cache_dir().unwrap_or_else(|_| PathBuf::from("/tmp/scribe"));
        let odt = dir.join("files").join(format!("{hash}.odt"));
        let meta = dir.join("files").join(format!("{hash}.json"));
        (odt, meta)
    }

    fn write_cache(&self, remote: &str, bytes: &[u8], etag: Option<&str>, dirty: bool) {
        let (odt, meta) = self.cache_paths(remote);
        if let Some(dir) = odt.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = atomic_write(&odt, bytes);
        let _ = fs::set_permissions(&odt, fs::Permissions::from_mode(0o600));
        let data = serde_json::json!({
            "remote_path": remote,
            "etag": etag,
            "dirty": dirty,
        });
        let _ = atomic_write(&meta, data.to_string().as_bytes());
        let _ = fs::set_permissions(&meta, fs::Permissions::from_mode(0o600));
    }

    fn read_cache(&self, remote: &str) -> Option<(Vec<u8>, CacheMeta)> {
        let (odt, meta) = self.cache_paths(remote);
        let bytes = fs::read(odt).ok()?;
        let text = fs::read_to_string(meta).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let remote_path = v.get("remote_path")?.as_str()?.to_string();
        if remote_path != remote {
            return None;
        }
        let etag = v.get("etag").and_then(|x| x.as_str()).map(str::to_string);
        let dirty = v.get("dirty").and_then(|x| x.as_bool()).unwrap_or(false);
        Some((bytes, CacheMeta { etag, dirty }))
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.exit_mode.is_some() {
            return;
        }
        match self.overlay {
            Overlay::LockPrompt => {
                self.handle_lock_prompt_key(key);
                return;
            }
            Overlay::LockForce => {
                self.handle_lock_force_key(key);
                return;
            }
            Overlay::LockWaiting => return,
            _ => {}
        }
        if key.code == KeyCode::Char('q') && ctrl(&key) {
            if self.overlay == Overlay::Conflict {
                return;
            }
            self.try_quit();
            return;
        }
        if key.code == KeyCode::F(3) {
            if self.overlay == Overlay::None {
                if self.search.active {
                    self.search_next();
                } else {
                    self.start_search();
                }
            }
            return;
        }
        if self.search.active {
            self.handle_search_key(key);
            return;
        }
        if key.code == KeyCode::F(9) {
            match self.overlay {
                Overlay::None => self.open_menu(),
                Overlay::Menu => self.overlay = Overlay::None,
                Overlay::Help => {
                    self.overlay = Overlay::None;
                    self.open_menu();
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::F(8) {
            match self.overlay {
                Overlay::None => self.action_ai_check(),
                Overlay::AiPanel => self.close_ai_panel(),
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::F(7) {
            if self.overlay == Overlay::None {
                self.action_notes();
            }
            return;
        }
        if key.code == KeyCode::F(1) || (key.code == KeyCode::Char('h') && alt_only(&key)) {
            if self.overlay == Overlay::Config {
                return;
            }
            self.overlay = if self.overlay == Overlay::Help {
                Overlay::None
            } else {
                Overlay::Help
            };
            return;
        }
        if key.code == KeyCode::Char('a') && alt_only(&key) {
            if self.overlay == Overlay::Config {
                return;
            }
            self.overlay = if self.overlay == Overlay::About {
                Overlay::None
            } else {
                Overlay::About
            };
            return;
        }
        if key.code == KeyCode::Char('l') && ctrl(&key) {
            self.redraw_requested = true;
            self.toast("Screen redrawn.");
            return;
        }
        if key.code == KeyCode::Char(',') && (ctrl(&key) || alt_only(&key)) {
            match self.overlay {
                Overlay::Config => self.cancel_config(),
                Overlay::None | Overlay::Menu => self.action_config(),
                _ => {}
            }
            return;
        }
        match self.overlay {
            Overlay::Input => {
                self.handle_input_key(key);
                return;
            }
            Overlay::FilePicker => {
                self.handle_file_picker_key(key);
                return;
            }
            Overlay::ChapterPicker => {
                self.handle_chapter_picker_key(key);
                return;
            }
            Overlay::Conflict => {
                self.handle_conflict_key(key);
                return;
            }
            Overlay::AiPanel => {
                self.handle_ai_panel_key(key);
                return;
            }
            Overlay::Config => {
                self.handle_config_key(key);
                return;
            }
            Overlay::Selection => {
                self.handle_selection_key(key);
                return;
            }
            Overlay::Menu => {
                self.handle_menu_key(key);
                return;
            }
            Overlay::Help => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::About => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::LockPrompt | Overlay::LockWaiting | Overlay::LockForce => return,
            Overlay::None => {}
        }
        if ctrl_space(&key) {
            self.action_mark();
            return;
        }
        if key.code == KeyCode::Esc {
            if let Some(ed) = &mut self.editor {
                if ed.clear_selection() {
                    self.toast("Selection cleared.");
                }
            }
            return;
        }
        if key.code == KeyCode::Char('o') && ctrl(&key) {
            self.action_open();
            return;
        }
        if key.code == KeyCode::Char('n') && ctrl(&key) {
            self.action_new_chapter();
            return;
        }
        if key.code == KeyCode::Char('e') && ctrl(&key) {
            self.action_book_title();
            return;
        }
        if key.code == KeyCode::Char('t') && ctrl(&key) {
            self.action_chapter_title();
            return;
        }
        if key.code == KeyCode::Char('g') && ctrl(&key) {
            self.action_goto_chapter();
            return;
        }
        if key.code == KeyCode::Char('s') && ctrl(&key) {
            self.action_save();
            return;
        }
        if key.code == KeyCode::Char('z') && ctrl(&key) {
            self.action_undo();
            return;
        }
        if (key.code == KeyCode::Char('y') && ctrl(&key))
            || (key.code == KeyCode::Char('z')
                && ctrl(&key)
                && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.action_redo();
            return;
        }
        if key.code == KeyCode::Char('b') && (ctrl(&key) || alt_only(&key)) {
            self.action_bold();
            return;
        }
        if key.code == KeyCode::Char('i') && (ctrl(&key) || alt_only(&key)) {
            self.action_italic();
            return;
        }
        if key.code == KeyCode::Char('u') && alt_only(&key) {
            self.action_underline();
            return;
        }
        if key.code == KeyCode::Char('s') && alt_only(&key) {
            self.action_strike();
            return;
        }
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        match key.code {
            KeyCode::Char(c) if is_text_key(&key) => {
                ed.insert_typed_char(c);
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) || alt_only(&key) => {
                ed.insert_line_break()
            }
            KeyCode::Enter => ed.newline(),
            KeyCode::Char('w') if ctrl(&key) => ed.delete_word_back(),
            KeyCode::Backspace => ed.backspace(),
            KeyCode::Delete if ctrl(&key) => ed.delete_word_forward(),
            KeyCode::Delete => ed.delete(),
            KeyCode::Char('u') if ctrl(&key) => ed.delete_to_para_start(),
            KeyCode::Char('k') if ctrl(&key) => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    ed.delete_paragraph();
                } else {
                    ed.delete_to_para_end();
                }
            }
            KeyCode::Left if ctrl(&key) => ed.move_word_left(),
            KeyCode::Left => ed.move_left(),
            KeyCode::Right if ctrl(&key) => ed.move_word_right(),
            KeyCode::Right => ed.move_right(),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => ed.prev_chapter(),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => ed.next_chapter(),
            KeyCode::Up => ed.move_up(),
            KeyCode::Down => ed.move_down(),
            KeyCode::Home if ctrl(&key) => ed.chapter_start(),
            KeyCode::End if ctrl(&key) => ed.chapter_end(),
            KeyCode::Home => ed.home(),
            KeyCode::End => ed.end(),
            KeyCode::PageUp => ed.page_up(),
            KeyCode::PageDown => ed.page_down(),
            KeyCode::Tab => {
                for _ in 0..4 {
                    ed.insert_char(' ');
                }
            }
            _ => {}
        }
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn open_menu(&mut self) {
        self.menu.active = 0;
        self.menu.selected = self.menu_first_enabled(self.menu.active);
        self.overlay = Overlay::Menu;
    }

    fn close_menu(&mut self) {
        if self.overlay == Overlay::Menu {
            self.overlay = Overlay::None;
        }
    }

    fn menu_item_enabled(&self, action: MenuAction) -> bool {
        match action {
            MenuAction::Quit
            | MenuAction::Redraw
            | MenuAction::Help
            | MenuAction::Config
            | MenuAction::About => true,
            MenuAction::Open => self.client.is_some(),
            MenuAction::AiCheck => self.editor.is_some(),
            MenuAction::Notes => self.editor.is_some() && !self.notes_loading,
            MenuAction::DownloadCopy => self.remote_path.is_some() && self.editor.is_some(),
            MenuAction::SelectionActions
            | MenuAction::Bold
            | MenuAction::Italic
            | MenuAction::Underline
            | MenuAction::Strike => self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.has_selection()),
            MenuAction::Subheading
            | MenuAction::AlignLeft
            | MenuAction::AlignCenter
            | MenuAction::AlignRight
            | MenuAction::AlignJustify
            | MenuAction::LineBreak => self.editor.is_some(),
            _ => self.editor.is_some(),
        }
    }

    fn menu_first_enabled(&self, menu: usize) -> usize {
        MENUS[menu]
            .entries
            .iter()
            .position(|entry| match entry {
                MenuEntry::Item { action, .. } => self.menu_item_enabled(*action),
                MenuEntry::Separator => false,
            })
            .unwrap_or(0)
    }

    fn menu_last_enabled(&self, menu: usize) -> usize {
        MENUS[menu]
            .entries
            .iter()
            .rposition(|entry| match entry {
                MenuEntry::Item { action, .. } => self.menu_item_enabled(*action),
                MenuEntry::Separator => false,
            })
            .unwrap_or(0)
    }

    fn menu_switch(&mut self, delta: isize) {
        let count = MENUS.len() as isize;
        let next = (self.menu.active as isize + delta).rem_euclid(count) as usize;
        self.menu.active = next;
        self.menu.selected = self.menu_first_enabled(next);
    }

    fn menu_move(&mut self, delta: isize) {
        let entries = MENUS[self.menu.active].entries;
        self.menu.selected = next_enabled_entry(entries, self.menu.selected, delta, |action| {
            self.menu_item_enabled(action)
        });
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_menu(),
            KeyCode::Left => self.menu_switch(-1),
            KeyCode::Right => self.menu_switch(1),
            KeyCode::Up => self.menu_move(-1),
            KeyCode::Down => self.menu_move(1),
            KeyCode::Home => self.menu.selected = self.menu_first_enabled(self.menu.active),
            KeyCode::End => self.menu.selected = self.menu_last_enabled(self.menu.active),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_menu_item(),
            _ => {}
        }
    }

    fn activate_menu_item(&mut self) {
        let entry = MENUS[self.menu.active]
            .entries
            .get(self.menu.selected)
            .copied();
        if let Some(MenuEntry::Item { action, .. }) = entry
            && self.menu_item_enabled(action)
        {
            self.overlay = Overlay::None;
            self.run_menu_action(action);
        }
    }

    fn run_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::Open => self.action_open(),
            MenuAction::Save => self.action_save(),
            MenuAction::DownloadCopy => self.action_download_copy(),
            MenuAction::Quit => self.try_quit(),
            MenuAction::Undo => self.action_undo(),
            MenuAction::Redo => self.action_redo(),
            MenuAction::Find => self.start_search(),
            MenuAction::MarkSelection => self.action_mark(),
            MenuAction::SelectionActions => self.action_selection_actions(),
            MenuAction::Bold => self.action_bold(),
            MenuAction::Italic => self.action_italic(),
            MenuAction::Underline => self.action_underline(),
            MenuAction::Strike => self.action_strike(),
            MenuAction::Subheading => self.action_subheading(),
            MenuAction::AlignLeft => self.action_align(Align::Left),
            MenuAction::AlignCenter => self.action_align(Align::Center),
            MenuAction::AlignRight => self.action_align(Align::Right),
            MenuAction::AlignJustify => self.action_align(Align::Justify),
            MenuAction::LineBreak => self.action_line_break(),
            MenuAction::NewChapter => self.action_new_chapter(),
            MenuAction::ChapterTitle => self.action_chapter_title(),
            MenuAction::BookTitle => self.action_book_title(),
            MenuAction::GotoChapter => self.action_goto_chapter(),
            MenuAction::PrevChapter => self.action_prev_chapter(),
            MenuAction::NextChapter => self.action_next_chapter(),
            MenuAction::Redraw => self.action_redraw(),
            MenuAction::Help => self.action_help(),
            MenuAction::AiCheck => self.action_ai_check(),
            MenuAction::Config => self.action_config(),
            MenuAction::Notes => self.action_notes(),
            MenuAction::About => self.action_about(),
        }
    }

    fn action_open(&mut self) {
        self.open_file_picker();
    }

    fn action_save(&mut self) {
        self.save_local_now();
        if self.remote_path.is_some() && !self.net_busy {
            self.save_remote_now();
        }
    }

    /// Copy the current cloud document into `./existing/`, keeping unsaved
    /// edits. A name clash gets a Unix-seconds suffix.
    fn action_download_copy(&mut self) {
        let Some(remote) = self.remote_path.clone() else {
            self.toast("Download a copy needs a cloud file");
            return;
        };
        let Some(ed) = self.editor.as_mut() else {
            self.toast("No document open");
            return;
        };
        let name = remote
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("untitled.odt");
        let bytes = match ed.save_bytes() {
            Ok(bytes) => bytes,
            Err(e) => {
                self.toast(format!("Copy failed: {e}"));
                return;
            }
        };
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        match write_downloaded_copy(Path::new("existing"), name, &bytes, secs) {
            Ok(path) => self.toast(format!("Copy saved: {}", path.display())),
            Err(e) => self.toast(format!("Copy failed: {e}")),
        }
    }

    fn action_new_chapter(&mut self) {
        if self.editor.is_some() {
            self.start_prompt(InputKind::NewChapter, "New chapter title", "");
        }
    }

    fn action_book_title(&mut self) {
        if let Some(ed) = &self.editor {
            let title = ed.doc.book.title.clone();
            self.start_prompt(InputKind::BookTitle, "Book title", &title);
        }
    }

    fn action_chapter_title(&mut self) {
        if let Some(ed) = &self.editor {
            let idx = ed.current_chapter();
            let title = ed.doc.book.chapters[idx].title.clone();
            self.start_prompt(InputKind::ChapterTitle(idx), "Chapter title", &title);
        }
    }

    fn action_goto_chapter(&mut self) {
        if let Some(ed) = &self.editor {
            self.chapter_picker = Some(ChapterPicker {
                selected: ed.current_chapter(),
            });
            self.overlay = Overlay::ChapterPicker;
        }
    }

    fn action_prev_chapter(&mut self) {
        if let Some(ed) = &mut self.editor {
            ed.prev_chapter();
        }
    }

    fn action_next_chapter(&mut self) {
        if let Some(ed) = &mut self.editor {
            ed.next_chapter();
        }
    }

    fn action_undo(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.undo();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn action_redo(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.redo();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn start_search(&mut self) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        editor.clear_selection();
        if editor.revision != self.search.revision {
            self.search.matches.clear();
            self.search.current = None;
            self.search.dirty = true;
            self.search.no_match = false;
        }
        self.search.origin = editor.cursor;
        self.search.active = true;
        self.search.cursor = self.search.query.chars().count();
        self.search.no_match = false;
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search.active = false;
                if self.search.current.is_none()
                    && let Some(editor) = &mut self.editor
                {
                    let origin = self.search.origin;
                    if editor.current_chapter() != origin.chapter {
                        editor.goto_chapter(origin.chapter);
                    }
                    editor.reveal(origin.para, origin.offset);
                }
            }
            KeyCode::Enter => self.run_search_from_cursor(),
            KeyCode::Backspace => self.search_backspace(),
            KeyCode::Delete => self.search_delete(),
            KeyCode::Left => self.search_move_cursor(-1),
            KeyCode::Right => self.search_move_cursor(1),
            KeyCode::Home => self.search.cursor = 0,
            KeyCode::End => self.search.cursor = self.search.query.chars().count(),
            KeyCode::Char('w') if ctrl(&key) => self.search_delete_word_back(),
            KeyCode::Char('u') if ctrl(&key) => self.search_clear_query(),
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.search_insert(ch)
            }
            _ => {}
        }
    }

    fn search_next(&mut self) {
        if self.search.query.is_empty() {
            return;
        }
        if self.search.dirty || self.search.matches.is_empty() {
            self.run_search_from_cursor();
            return;
        }
        let Some(current) = self.search.current else {
            self.run_search_from_cursor();
            return;
        };
        let Some(item) = self.search.matches.get(current).copied() else {
            self.run_search_from_cursor();
            return;
        };
        let Some((index, wrapped)) =
            search::next_match(&self.search.matches, item.position(), false)
        else {
            return;
        };
        self.apply_search_match(index, wrapped);
    }

    fn run_search_from_cursor(&mut self) {
        if self.search.query.is_empty() {
            self.search.matches.clear();
            self.search.current = None;
            self.search.no_match = false;
            self.search.dirty = false;
            return;
        }
        let Some(editor) = &self.editor else {
            return;
        };
        let matches = search::find_matches(&editor.doc.book, &self.search.query);
        let position = (
            editor.cursor.chapter,
            editor.cursor.para,
            editor.cursor.offset,
        );
        self.search.matches = matches;
        self.search.dirty = false;
        self.search.current = None;
        self.search.revision = editor.revision;
        let Some((index, wrapped)) = search::next_match(&self.search.matches, position, true)
        else {
            self.search.no_match = true;
            return;
        };
        self.search.no_match = false;
        self.apply_search_match(index, wrapped);
    }

    fn apply_search_match(&mut self, index: usize, wrapped: bool) {
        let Some(item) = self.search.matches.get(index).copied() else {
            return;
        };
        self.search.current = Some(index);
        self.search.no_match = false;
        if let Some(editor) = &mut self.editor {
            if editor.current_chapter() != item.chapter {
                editor.goto_chapter(item.chapter);
            }
            editor.reveal(item.para, item.start);
        }
        if wrapped {
            self.toast("Search wrapped.");
        }
    }

    fn search_insert(&mut self, ch: char) {
        let width = crossterm::terminal::size().unwrap_or((100, 40)).0;
        let max = search_query_capacity(width);
        let current_width: usize = self
            .search
            .query
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + char_width > max {
            return;
        }
        let at = search_byte_at(&self.search.query, self.search.cursor);
        self.search.query.insert(at, ch);
        self.search.cursor += 1;
        self.search_edited();
    }

    fn search_backspace(&mut self) {
        if self.search.cursor == 0 {
            return;
        }
        let start = search_byte_at(&self.search.query, self.search.cursor - 1);
        let end = search_byte_at(&self.search.query, self.search.cursor);
        self.search.query.replace_range(start..end, "");
        self.search.cursor -= 1;
        self.search_edited();
    }

    fn search_delete(&mut self) {
        if self.search.cursor >= self.search.query.chars().count() {
            return;
        }
        let start = search_byte_at(&self.search.query, self.search.cursor);
        let end = search_byte_at(&self.search.query, self.search.cursor + 1);
        self.search.query.replace_range(start..end, "");
        self.search_edited();
    }

    fn search_move_cursor(&mut self, delta: isize) {
        let len = self.search.query.chars().count() as isize;
        self.search.cursor = (self.search.cursor as isize + delta).clamp(0, len) as usize;
    }

    fn search_delete_word_back(&mut self) {
        if self.search.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.search.query.chars().collect();
        let mut index = self.search.cursor;
        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !chars[index - 1].is_whitespace() {
            index -= 1;
        }
        let start = search_byte_at(&self.search.query, index);
        let end = search_byte_at(&self.search.query, self.search.cursor);
        self.search.query.replace_range(start..end, "");
        self.search.cursor = index;
        self.search_edited();
    }

    fn search_clear_query(&mut self) {
        self.search.query.clear();
        self.search.cursor = 0;
        self.search_edited();
    }

    fn search_edited(&mut self) {
        self.search.dirty = true;
        self.search.matches.clear();
        self.search.current = None;
        self.search.no_match = false;
    }

    fn action_mark(&mut self) {
        let Some(result) = self.editor.as_mut().map(Editor::toggle_mark) else {
            return;
        };
        match result {
            MarkResult::Started => self.toast("Mark set. Move, then Ctrl+Space again."),
            MarkResult::Finished(count) => {
                self.selection_menu = Some(SelectionMenu::default());
                self.overlay = Overlay::Selection;
                self.toast(format!("{count} chars selected."));
            }
            MarkResult::Restarted => self.toast("New mark set at cursor."),
            MarkResult::Cancelled => self.toast("Empty mark cleared."),
        }
    }

    fn action_selection_actions(&mut self) {
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.has_selection())
        {
            self.selection_menu = Some(SelectionMenu::default());
            self.overlay = Overlay::Selection;
        }
    }

    fn action_bold(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.toggle_bold();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn action_italic(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.toggle_italic();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn action_underline(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.toggle_underline();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn action_strike(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.toggle_strike();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn action_subheading(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        let subheading = ed.toggle_subheading();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
        self.toast(if subheading {
            "Paragraph is now a subheading."
        } else {
            "Subheading changed to body text."
        });
    }

    fn action_align(&mut self, align: Align) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.set_alignment(align);
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
        let label = match align {
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
            Align::Justify => "justified",
        };
        self.toast(format!("Text aligned {label}."));
    }

    fn action_line_break(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.insert_line_break();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn action_delete_selection(&mut self) {
        let Some(ed) = &mut self.editor else {
            return;
        };
        let rev = ed.revision;
        ed.delete_selection();
        if ed.revision != rev {
            self.last_edit = Instant::now();
            self.save_state = SaveState::Idle;
        }
    }

    fn action_redraw(&mut self) {
        self.redraw_requested = true;
        self.toast("Screen redrawn.");
    }

    fn action_help(&mut self) {
        self.overlay = Overlay::Help;
    }

    fn action_about(&mut self) {
        self.overlay = Overlay::About;
    }

    fn action_notes(&mut self) {
        if self.notes_loading || self.editor.is_none() {
            return;
        }
        self.search = SearchState::default();
        if self.notes_active {
            self.leave_notes();
        } else {
            self.open_notes();
        }
    }

    fn open_notes(&mut self) {
        if let Some(notes) = self.notes_doc.take() {
            self.flush_current_doc();
            let main = self.take_doc_memory();
            self.restore_doc_memory(notes);
            self.notes_main = main;
            self.notes_active = true;
            self.toast("Notes.");
            return;
        }

        self.flush_current_doc();
        if let Some(path) = self.local_path.clone() {
            let notes_path = notes_local_path(&path);
            let created = !notes_path.exists();
            let doc = if created {
                let Some(editor) = &self.editor else {
                    return;
                };
                let Ok(mut doc) = notes_document_from(&editor.doc.book) else {
                    self.toast("Could not create the notes document.");
                    return;
                };
                let Ok(bytes) = doc.to_bytes() else {
                    self.toast("Could not create the notes document.");
                    return;
                };
                if let Err(error) = atomic_write(&notes_path, &bytes) {
                    self.toast(format!("Could not create notes: {error}"));
                    return;
                }
                doc
            } else {
                let title = notes_path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Notes");
                let bytes = match fs::read(&notes_path) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        self.toast(format!("Could not read notes: {error}"));
                        return;
                    }
                };
                match OdtDocument::from_bytes(&bytes, title) {
                    Ok(doc) => doc,
                    Err(error) => {
                        self.toast(format!("Could not open notes: {error}"));
                        return;
                    }
                }
            };
            let main = self.take_doc_memory();
            self.install_editor(doc, Some(notes_path), None, None, false);
            self.notes_main = main;
            self.notes_active = true;
            self.toast(if created { "Notes created." } else { "Notes." });
            return;
        }

        let Some(remote) = self.remote_path.clone() else {
            return;
        };
        let Some(editor) = &self.editor else {
            return;
        };
        let created = match notes_document_from(&editor.doc.book) {
            Ok(doc) => doc,
            Err(error) => {
                self.toast(format!("Could not create notes: {error}"));
                return;
            }
        };
        let notes_path = notes_remote_path(&remote);
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.net_tx.clone();
        self.pending_notes = Some(PendingNotes {
            path: notes_path.clone(),
            created,
        });
        self.notes_loading = true;
        self.toast("Opening notes…");
        std::thread::spawn(move || {
            let result = client
                .get_optional(&notes_path)
                .map_err(|error| format!("{error}"));
            let _ = tx.send(NetEvent::NotesFetched {
                path: notes_path,
                result,
            });
        });
    }

    fn finish_notes_fetch(&mut self, pending: PendingNotes, result: NotesFetchResult) {
        self.notes_loading = false;
        match result {
            Ok(Some((bytes, etag))) => {
                let title = notes_title(&pending.path);
                match OdtDocument::from_bytes(&bytes, &title) {
                    Ok(doc) => {
                        self.swap_to_notes(doc, Some(pending.path), etag, false, false);
                        self.toast("Notes.");
                    }
                    Err(error) => self.toast(format!("Could not open notes: {error}")),
                }
            }
            Ok(None) => {
                let mut created = pending.created;
                let Ok(bytes) = created.to_bytes() else {
                    self.toast("Could not create the notes document.");
                    return;
                };
                self.write_cache(&pending.path, &bytes, None, true);
                self.swap_to_notes(created, Some(pending.path), None, true, true);
                self.toast("Notes created.");
            }
            Err(error) => self.toast(format!("Could not open notes: {error}")),
        }
    }

    fn swap_to_notes(
        &mut self,
        doc: OdtDocument,
        remote_path: Option<String>,
        etag: Option<String>,
        create_new: bool,
        upload: bool,
    ) {
        let main = self.take_doc_memory();
        self.install_editor(doc, None, remote_path, etag, create_new);
        self.notes_main = main;
        self.notes_active = true;
        self.notes_loading = false;
        self.pending_notes = None;
        if upload {
            self.save_remote_now();
        }
    }

    fn leave_notes(&mut self) {
        self.flush_current_doc();
        self.notes_doc = self.take_doc_memory();
        if let Some(main) = self.notes_main.take() {
            self.restore_doc_memory(main);
        }
        self.notes_active = false;
        self.toast("Back to document.");
    }

    fn flush_current_doc(&mut self) {
        self.save_local_now();
        if self.remote_path.is_some() && !self.net_busy {
            self.save_remote_now();
        }
    }

    fn take_doc_memory(&mut self) -> Option<DocMemory> {
        let editor = self.editor.take()?;
        Some(DocMemory {
            editor,
            local_path: self.local_path.take(),
            remote_path: self.remote_path.take(),
            etag: self.etag.take(),
            create_new: self.create_new,
            save_state: self.save_state.clone(),
            conflict: self.conflict,
        })
    }

    fn restore_doc_memory(&mut self, doc: DocMemory) {
        let (width, height) = crossterm::terminal::size().unwrap_or((100, 40));
        let mut editor = doc.editor;
        editor.set_viewport(width, height);
        editor.sync_page();
        self.editor = Some(editor);
        self.local_path = doc.local_path;
        self.remote_path = doc.remote_path;
        self.etag = doc.etag;
        self.create_new = doc.create_new;
        self.save_state = doc.save_state;
        self.conflict = doc.conflict;
        self.search = SearchState::default();
        self.overlay = if self.conflict {
            Overlay::Conflict
        } else {
            Overlay::None
        };
        self.input = None;
        self.file_picker = None;
        self.chapter_picker = None;
        self.selection_menu = None;
        self.pending_open = None;
        self.force_remote_reload = false;
        self.last_edit = Instant::now();
        self.last_local_save = Instant::now();
        self.last_remote_attempt = Instant::now();
    }

    fn update_parked_save(
        &mut self,
        path: &str,
        etag: Option<String>,
        rev: u64,
        conflict: bool,
        failed: bool,
    ) -> Option<&'static str> {
        if let Some(memory) = self.notes_main.as_mut()
            && memory.remote_path.as_deref() == Some(path)
        {
            Self::apply_parked_save(memory, etag, rev, conflict, failed);
            return Some("Document");
        }
        if let Some(memory) = self.notes_doc.as_mut()
            && memory.remote_path.as_deref() == Some(path)
        {
            Self::apply_parked_save(memory, etag, rev, conflict, failed);
            return Some("Notes");
        }
        None
    }

    fn apply_parked_save(
        memory: &mut DocMemory,
        etag: Option<String>,
        rev: u64,
        conflict: bool,
        failed: bool,
    ) {
        if conflict {
            memory.conflict = true;
            memory.save_state = SaveState::Conflict;
        } else if failed {
            memory.save_state = SaveState::Offline;
        } else if let Some(etag) = etag {
            memory.etag = Some(etag);
            memory.create_new = false;
            if memory.editor.revision == rev {
                memory.editor.mark_saved();
                memory.save_state = SaveState::Saved;
            } else {
                memory.save_state = SaveState::Idle;
            }
        }
    }

    fn clear_notes_session(&mut self) {
        self.notes_active = false;
        self.notes_loading = false;
        self.notes_main = None;
        self.notes_doc = None;
        self.pending_notes = None;
    }

    fn action_config(&mut self) {
        let mut status = None;
        let ai = match ai::load() {
            Ok(Some(cfg)) => cfg,
            Ok(None) => ai::AiConfig::default(),
            Err(error) => {
                status = Some((format!("Could not read ai.toml: {error}"), true));
                ai::AiConfig::default()
            }
        };
        let mut panel = ConfigPanel::new(ai, self.settings.theme);
        panel.status = status;
        self.config_panel = Some(panel);
        self.overlay = Overlay::Config;
    }

    fn cancel_config(&mut self) {
        if let Some(panel) = self.config_panel.take() {
            self.set_theme(panel.original_theme);
        }
        if self.overlay == Overlay::Config {
            self.overlay = Overlay::None;
        }
    }

    fn set_theme(&mut self, theme: ThemeName) {
        self.settings.theme = theme;
        self.palette = theme.palette();
    }

    fn save_config(&mut self) {
        let Some(panel) = &mut self.config_panel else {
            return;
        };
        if panel.editing.is_some() {
            panel.commit_edit();
        }
        if let Err(error) = panel.ai.validate() {
            panel.set_status(format!("{error:#}"), true);
            return;
        }
        let ai_config = panel.ai.clone();
        if let Err(error) = ai::save(&ai_config) {
            panel.set_status(format!("Could not save ai.toml: {error:#}"), true);
            return;
        }
        if let Err(error) = settings::save(&self.settings) {
            panel.set_status(format!("Could not save settings.toml: {error:#}"), true);
            return;
        }
        self.config_panel = None;
        self.overlay = Overlay::None;
        self.toast("Configuration saved.");
    }

    fn test_config(&mut self) {
        let Some(panel) = &mut self.config_panel else {
            return;
        };
        if panel.testing {
            return;
        }
        panel.testing = true;
        panel.set_status("Testing the AI endpoint…", false);
        let cfg = panel.ai.clone().effective();
        let tx = self.ai_tx.clone();
        std::thread::spawn(move || {
            let result = ai::test_connection(&cfg).map_err(|error| format!("{error}"));
            let _ = tx.send(AiEvent::TestDone { result });
        });
    }

    fn handle_config_key(&mut self, key: KeyEvent) {
        if self.config_panel.is_none() {
            self.overlay = Overlay::None;
            return;
        }

        if self
            .config_panel
            .as_ref()
            .is_some_and(|panel| panel.editing.is_some())
        {
            match key.code {
                KeyCode::Esc => {
                    if let Some(panel) = &mut self.config_panel {
                        panel.editing = None;
                    }
                }
                KeyCode::Enter => {
                    if let Some(panel) = &mut self.config_panel {
                        panel.commit_edit();
                    }
                }
                _ => {
                    if let Some(panel) = &mut self.config_panel
                        && let Some(input) = panel.editing.as_mut()
                    {
                        match key.code {
                            KeyCode::Left => input.left(),
                            KeyCode::Right => input.right(),
                            KeyCode::Home => input.home(),
                            KeyCode::End => input.end(),
                            KeyCode::Backspace => input.backspace(),
                            KeyCode::Delete => input.delete(),
                            KeyCode::Char(ch) if key.modifiers == KeyModifiers::NONE => {
                                input.insert(ch)
                            }
                            _ => {}
                        }
                    }
                }
            }
            return;
        }

        let field = self
            .config_panel
            .as_ref()
            .map(|panel| panel.field())
            .unwrap_or(ConfigField::BaseUrl);
        match key.code {
            KeyCode::Esc => self.cancel_config(),
            KeyCode::Up => {
                if let Some(panel) = &mut self.config_panel {
                    panel.selected = panel.selected.saturating_sub(1);
                    panel.status = None;
                }
            }
            KeyCode::Down => {
                if let Some(panel) = &mut self.config_panel {
                    panel.selected = (panel.selected + 1).min(CONFIG_FIELDS.len() - 1);
                    panel.status = None;
                }
            }
            KeyCode::Left if field == ConfigField::Theme => {
                let theme = self.settings.theme.prev();
                self.set_theme(theme);
            }
            KeyCode::Right if field == ConfigField::Theme => {
                let theme = self.settings.theme.next();
                self.set_theme(theme);
            }
            KeyCode::Enter => match field {
                ConfigField::JsonMode => {
                    if let Some(panel) = &mut self.config_panel {
                        panel.ai.json_mode = !panel.ai.json_mode;
                    }
                }
                ConfigField::Theme => {
                    let theme = self.settings.theme.next();
                    self.set_theme(theme);
                }
                _ => {
                    if let Some(panel) = &mut self.config_panel {
                        panel.start_edit();
                    }
                }
            },
            KeyCode::Char(' ') if field == ConfigField::JsonMode => {
                if let Some(panel) = &mut self.config_panel {
                    panel.ai.json_mode = !panel.ai.json_mode;
                }
            }
            KeyCode::Char(' ') if field == ConfigField::Theme => {
                let theme = self.settings.theme.next();
                self.set_theme(theme);
            }
            KeyCode::Delete if field == ConfigField::ApiKey => {
                if let Some(panel) = &mut self.config_panel {
                    panel.ai.api_key.clear();
                    panel.set_status("API key cleared; press s to save.", false);
                }
            }
            KeyCode::Char('s') if key.modifiers == KeyModifiers::NONE || ctrl(&key) => {
                self.save_config()
            }
            KeyCode::Char('t') if key.modifiers == KeyModifiers::NONE => self.test_config(),
            _ => {}
        }
    }

    fn action_ai_check(&mut self) {
        let Some(ed) = &self.editor else {
            return;
        };
        let chapter = ed.current_chapter();
        let paragraphs = ed.chapter_texts(chapter);
        if paragraphs.iter().all(|p| p.trim().is_empty()) {
            self.toast("This chapter is empty.");
            return;
        }

        let cfg = match ai::load() {
            Ok(Some(cfg)) => cfg.effective(),
            Ok(None) => {
                self.ai_panel = Some(AiPanel::error(
                    chapter,
                    "AI is not configured. Run `scribe ai-config`.",
                ));
                self.ai_job = None;
                self.overlay = Overlay::AiPanel;
                self.toast("AI is not configured.");
                return;
            }
            Err(error) => {
                self.ai_panel = Some(AiPanel::error(chapter, format!("AI config error: {error}")));
                self.ai_job = None;
                self.overlay = Overlay::AiPanel;
                self.toast("AI configuration error.");
                return;
            }
        };
        if let Err(error) = cfg.validate() {
            self.ai_panel = Some(AiPanel::error(chapter, format!("AI config error: {error}")));
            self.ai_job = None;
            self.overlay = Overlay::AiPanel;
            self.toast("AI configuration error.");
            return;
        }

        self.ai_seq = self.ai_seq.wrapping_add(1);
        let job_id = self.ai_seq;
        let text_hash = text_hash(&paragraphs);
        self.ai_job = Some(AiJob {
            id: job_id,
            chapter,
            text_hash,
        });
        self.ai_panel = Some(AiPanel::checking(chapter, 1, cfg.model.clone()));
        self.overlay = Overlay::AiPanel;
        self.toast("Checking chapter with AI…");

        let tx = self.ai_tx.clone();
        std::thread::spawn(move || {
            let result = ai::check_chapter(&cfg, &paragraphs, |done, total| {
                let _ = tx.send(AiEvent::Progress {
                    job_id,
                    done,
                    total,
                });
            })
            .map_err(|error| format!("{error}"));
            let _ = tx.send(AiEvent::Done {
                job_id,
                chapter,
                text_hash,
                result,
            });
        });
    }

    fn close_ai_panel(&mut self) {
        self.ai_panel = None;
        self.ai_job = None;
        if self.overlay == Overlay::AiPanel {
            self.overlay = Overlay::None;
        }
    }

    fn drain_ai(&mut self) {
        loop {
            let event = match self.ai_rx.try_recv() {
                Ok(event) => event,
                Err(_) => break,
            };
            match event {
                AiEvent::Progress {
                    job_id,
                    done,
                    total,
                } => {
                    if self.ai_job.as_ref().map(|job| job.id) != Some(job_id) {
                        continue;
                    }
                    if let Some(panel) = &mut self.ai_panel {
                        panel.progress_done = done;
                        panel.progress_total = total;
                        if panel.status == AiStatus::Checking {
                            panel.message = format!("Checking… {done}/{total}");
                        }
                    }
                }
                AiEvent::Done {
                    job_id,
                    chapter,
                    text_hash: expected_hash,
                    result,
                } => {
                    let Some(job) = &self.ai_job else {
                        continue;
                    };
                    if job.id != job_id || job.chapter != chapter || job.text_hash != expected_hash
                    {
                        continue;
                    }
                    self.ai_job = None;

                    let Some(ed) = &self.editor else {
                        self.ai_panel = None;
                        self.overlay = Overlay::None;
                        continue;
                    };
                    if ed.current_chapter() != chapter {
                        self.ai_panel = None;
                        self.overlay = Overlay::None;
                        self.toast("Chapter changed; AI check discarded.");
                        continue;
                    }
                    if text_hash(&ed.chapter_texts(chapter)) != expected_hash {
                        if let Some(panel) = &mut self.ai_panel {
                            panel.status = AiStatus::Error;
                            panel.message =
                                "Chapter changed during the check. Press R to run again."
                                    .to_string();
                            panel.issues.clear();
                        }
                        self.toast("Chapter changed during check; run again.");
                        continue;
                    }

                    match result {
                        Ok(issues) => {
                            let count = issues.len();
                            if let Some(panel) = &mut self.ai_panel {
                                panel.status = AiStatus::Ready;
                                panel.progress_done = panel.progress_total;
                                panel.selected = 0;
                                panel.scroll = 0;
                                panel.issues = issues;
                                panel.message = if count == 0 {
                                    "No issues found.".to_string()
                                } else {
                                    format!("{count} suggestion(s).")
                                };
                            }
                            self.reveal_ai_selection();
                            self.toast("AI check complete.");
                        }
                        Err(error) => {
                            if let Some(panel) = &mut self.ai_panel {
                                panel.status = AiStatus::Error;
                                panel.message = error;
                                panel.issues.clear();
                            }
                            self.toast("AI check failed.");
                        }
                    }
                }
                AiEvent::TestDone { result } => {
                    if let Some(panel) = &mut self.config_panel {
                        panel.testing = false;
                        match result {
                            Ok(reply) => {
                                panel.set_status(format!("Endpoint OK: {}", reply.trim()), false)
                            }
                            Err(error) => {
                                panel.set_status(format!("Endpoint failed: {error}"), true)
                            }
                        }
                    } else {
                        match result {
                            Ok(_) => self.toast("AI endpoint OK."),
                            Err(error) => self.toast(format!("AI endpoint failed: {error}")),
                        }
                    }
                }
            }
        }
    }

    fn reveal_ai_selection(&mut self) {
        let Some(panel) = &self.ai_panel else {
            return;
        };
        let Some(issue) = panel.issues.get(panel.selected) else {
            return;
        };
        let (para, start) = (issue.para, issue.start);
        if let Some(ed) = &mut self.editor {
            ed.reveal(para, start);
        }
    }

    fn handle_selection_key(&mut self, key: KeyEvent) {
        let Some(menu) = self.selection_menu else {
            self.overlay = Overlay::None;
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.selection_menu = None;
                self.overlay = Overlay::None;
            }
            KeyCode::Up => {
                if let Some(menu) = &mut self.selection_menu {
                    menu.selected = menu.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(menu) = &mut self.selection_menu {
                    menu.selected = (menu.selected + 1).min(SELECTION_ACTIONS.len() - 1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.run_selection_action(SELECTION_ACTIONS[menu.selected].1);
            }
            KeyCode::Char('b') | KeyCode::Char('B') => {
                self.run_selection_action(SelectionAction::Bold);
            }
            KeyCode::Char('i') | KeyCode::Char('I') => {
                self.run_selection_action(SelectionAction::Italic);
            }
            KeyCode::Char('u') | KeyCode::Char('U') => {
                self.run_selection_action(SelectionAction::Underline);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.run_selection_action(SelectionAction::Strike);
            }
            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Delete => {
                self.run_selection_action(SelectionAction::Delete);
            }
            _ => {}
        }
    }

    fn run_selection_action(&mut self, action: SelectionAction) {
        self.selection_menu = None;
        self.overlay = Overlay::None;
        match action {
            SelectionAction::Bold => self.action_bold(),
            SelectionAction::Italic => self.action_italic(),
            SelectionAction::Underline => self.action_underline(),
            SelectionAction::Strike => self.action_strike(),
            SelectionAction::Delete => self.action_delete_selection(),
            SelectionAction::None => {}
        }
    }

    fn handle_ai_panel_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_ai_panel(),
            KeyCode::Char('z') if ctrl(&key) => {
                self.action_undo();
                self.close_ai_panel();
                self.toast("Undone. Press F8 to check again.");
            }
            KeyCode::Char('y') if ctrl(&key) => {
                self.action_redo();
                self.close_ai_panel();
                self.toast("Redone. Press F8 to check again.");
            }
            KeyCode::Up => self.ai_move(-1),
            KeyCode::Down => self.ai_move(1),
            KeyCode::PageUp => self.ai_move(-10),
            KeyCode::PageDown => self.ai_move(10),
            KeyCode::Left => self.ai_scroll(-8),
            KeyCode::Right => self.ai_scroll(8),
            KeyCode::Home => self.ai_select(0),
            KeyCode::End => {
                let last = self
                    .ai_panel
                    .as_ref()
                    .map(|panel| panel.issues.len().saturating_sub(1))
                    .unwrap_or(0);
                self.ai_select(last);
            }
            KeyCode::Enter | KeyCode::Char('a') => self.apply_selected_ai(),
            KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => self.dismiss_selected_ai(),
            KeyCode::Char('r') | KeyCode::Char('R') => self.action_ai_check(),
            _ => {}
        }
    }

    fn ai_move(&mut self, delta: isize) {
        let Some(panel) = &self.ai_panel else {
            return;
        };
        if panel.issues.is_empty() {
            return;
        }
        let last = panel.issues.len().saturating_sub(1) as isize;
        let next = (panel.selected as isize + delta).clamp(0, last) as usize;
        self.ai_select(next);
    }

    fn ai_select(&mut self, index: usize) {
        if let Some(panel) = &mut self.ai_panel {
            if panel.issues.is_empty() {
                return;
            }
            panel.selected = index.min(panel.issues.len() - 1);
            panel.scroll = 0;
        }
        self.reveal_ai_selection();
    }

    fn ai_scroll(&mut self, delta: isize) {
        let Some(panel) = &mut self.ai_panel else {
            return;
        };
        if panel.status != AiStatus::Ready || panel.issues.is_empty() {
            return;
        }
        let Some(issue) = panel.issues.get(panel.selected) else {
            return;
        };
        let content = ai_issue_line(issue, panel.selected);
        let width = crossterm::terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(120)
            .saturating_sub(3);
        let max_scroll = text_width(&content).saturating_sub(width);
        panel.scroll = (panel.scroll as isize + delta).clamp(0, max_scroll as isize) as usize;
    }

    fn apply_selected_ai(&mut self) {
        let Some(panel) = &self.ai_panel else {
            return;
        };
        if panel.status != AiStatus::Ready {
            return;
        }
        let chapter = panel.chapter;
        let selected = panel.selected;
        let Some(issue) = panel.issues.get(selected).cloned() else {
            return;
        };
        let Some(replacement) = issue.replacement.clone() else {
            self.toast("No automatic fix for this suggestion; dismiss it instead.");
            return;
        };

        if self.editor.as_ref().map(Editor::current_chapter) != Some(chapter) {
            self.close_ai_panel();
            return;
        }
        let Some(ed) = &mut self.editor else {
            return;
        };
        let applied = ed.replace_span(
            issue.para,
            issue.start,
            issue.end,
            &issue.original,
            &replacement,
        );
        let paragraphs = ed.chapter_texts(chapter);

        if !applied {
            if let Some(panel) = &mut self.ai_panel {
                panel.issues.retain(|item| {
                    !(item.para == issue.para
                        && item.original == issue.original
                        && item.replacement == issue.replacement)
                });
                ai::reanchor(&mut panel.issues, &paragraphs);
                panel.selected = panel.selected.min(panel.issues.len().saturating_sub(1));
                panel.scroll = 0;
                panel.message = "Suggestion no longer matches the text.".to_string();
            }
            self.toast("Suggestion is stale; run the check again.");
            return;
        }

        self.last_edit = Instant::now();
        self.save_state = SaveState::Idle;
        if let Some(panel) = &mut self.ai_panel {
            panel.issues.remove(selected);
            ai::reanchor(&mut panel.issues, &paragraphs);
            panel.selected = panel.selected.min(panel.issues.len().saturating_sub(1));
            panel.scroll = 0;
            panel.message = if panel.issues.is_empty() {
                "All suggestions handled.".to_string()
            } else {
                format!("{} suggestion(s) left.", panel.issues.len())
            };
        }
        self.reveal_ai_selection();
        self.toast("Suggestion applied. Ctrl+Z to undo.");
    }

    fn dismiss_selected_ai(&mut self) {
        let Some(panel) = &mut self.ai_panel else {
            return;
        };
        if panel.status != AiStatus::Ready || panel.issues.is_empty() {
            return;
        }
        panel.issues.remove(panel.selected);
        panel.selected = panel.selected.min(panel.issues.len().saturating_sub(1));
        panel.scroll = 0;
        panel.message = if panel.issues.is_empty() {
            "No suggestions left.".to_string()
        } else {
            format!("{} suggestion(s) left.", panel.issues.len())
        };
        self.reveal_ai_selection();
    }

    fn ai_panel_height(&self, total: u16) -> u16 {
        if self.overlay != Overlay::AiPanel {
            return 0;
        }
        let available = total.saturating_sub(2);
        if available < 6 {
            return 0;
        }
        let desired = ((total as u32 * 35 / 100) as u16).clamp(6, 12);
        desired.min(available - 3).max(3)
    }

    fn start_prompt(&mut self, kind: InputKind, label: &str, value: &str) {
        self.input = Some(Input::new(kind, label, value));
        self.overlay = Overlay::Input;
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        let Some(input) = &mut self.input else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                self.overlay = if self.conflict {
                    Overlay::Conflict
                } else if self.file_picker.is_some() {
                    Overlay::FilePicker
                } else if self.chapter_picker.is_some() {
                    Overlay::ChapterPicker
                } else {
                    Overlay::None
                };
            }
            KeyCode::Enter => {
                let kind = input.kind.clone();
                let value = input.value.trim().to_string();
                self.input = None;
                self.overlay = Overlay::None;
                match kind {
                    InputKind::NewChapter => {
                        if let Some(ed) = &mut self.editor {
                            ed.create_chapter(value);
                            self.last_edit = Instant::now();
                            self.save_state = SaveState::Idle;
                        }
                    }
                    InputKind::ChapterTitle(idx) => {
                        if let Some(ed) = &mut self.editor {
                            ed.set_chapter_title(idx, value);
                            self.last_edit = Instant::now();
                            self.save_state = SaveState::Idle;
                            self.chapter_picker = None;
                        }
                    }
                    InputKind::BookTitle => {
                        if let Some(ed) = &mut self.editor {
                            let title = if value.is_empty() {
                                "Untitled".to_string()
                            } else {
                                value
                            };
                            ed.set_book_title(title);
                            self.last_edit = Instant::now();
                            self.save_state = SaveState::Idle;
                        }
                    }
                    InputKind::NewFileName => {
                        self.create_remote_file(&value);
                    }
                    InputKind::SaveCopy => {
                        self.save_copy(&value);
                    }
                    InputKind::ConfigField => {}
                }
            }
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Char('u') if ctrl(&key) => input.clear(),
            KeyCode::Char('w') if ctrl(&key) => input.delete_word_back(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            KeyCode::Char(c) if is_text_key(&key) => input.insert(c),
            _ => {}
        }
    }

    fn handle_file_picker_key(&mut self, key: KeyEvent) {
        let Some(fp) = &mut self.file_picker else {
            return;
        };
        if fp.filtering {
            match key.code {
                KeyCode::Esc => {
                    fp.filtering = false;
                    fp.filter.clear();
                    fp.selected = 0;
                }
                KeyCode::Enter => {
                    fp.filtering = false;
                    fp.selected = 0;
                }
                KeyCode::Backspace => {
                    fp.filter.pop();
                    fp.selected = 0;
                }
                KeyCode::Char(c) if is_text_key(&key) => {
                    fp.filter.push(c);
                    fp.selected = 0;
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.file_picker = None;
                self.overlay = if self.editor.is_some() {
                    Overlay::None
                } else {
                    Overlay::None
                };
            }
            KeyCode::Up => {
                fp.selected = fp.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let n = fp.visible().len();
                if fp.selected + 1 < n {
                    fp.selected += 1;
                }
            }
            KeyCode::PageUp => fp.selected = fp.selected.saturating_sub(15),
            KeyCode::PageDown => {
                let n = fp.visible().len();
                fp.selected = (fp.selected + 15).min(n.saturating_sub(1));
            }
            KeyCode::Enter => {
                let entry = fp.selected_entry().cloned();
                if let Some(e) = entry {
                    if e.is_dir {
                        let path = e.path.clone();
                        fp.path = path;
                        fp.selected = 0;
                        fp.filter.clear();
                        self.refresh_file_picker();
                    } else {
                        self.request_open_remote(e.path);
                        self.file_picker = None;
                        self.overlay = Overlay::None;
                    }
                }
            }
            KeyCode::Backspace => {
                if fp.path != "/" {
                    let parent = parent_path(&fp.path);
                    fp.path = parent;
                    fp.selected = 0;
                    fp.filter.clear();
                    self.refresh_file_picker();
                }
            }
            KeyCode::Char('r') => self.refresh_file_picker(),
            KeyCode::Char('/') => {
                fp.filtering = true;
                fp.filter.clear();
                fp.selected = 0;
            }
            KeyCode::Char('n') => {
                self.start_prompt(InputKind::NewFileName, "New file name (without .odt)", "");
            }
            _ => {}
        }
    }

    fn handle_chapter_picker_key(&mut self, key: KeyEvent) {
        let Some(cp) = &mut self.chapter_picker else {
            return;
        };
        let count = self
            .editor
            .as_ref()
            .map(|e| e.doc.book.chapters.len())
            .unwrap_or(0);
        match key.code {
            KeyCode::Esc => {
                self.chapter_picker = None;
                self.overlay = Overlay::None;
            }
            KeyCode::Up => cp.selected = cp.selected.saturating_sub(1),
            KeyCode::Down => {
                if cp.selected + 1 < count {
                    cp.selected += 1;
                }
            }
            KeyCode::PageUp => cp.selected = cp.selected.saturating_sub(15),
            KeyCode::PageDown => cp.selected = (cp.selected + 15).min(count.saturating_sub(1)),
            KeyCode::Home => cp.selected = 0,
            KeyCode::End => cp.selected = count.saturating_sub(1),
            KeyCode::Char('e') => {
                let idx = cp.selected;
                if let Some(ed) = &self.editor {
                    let title = ed.doc.book.chapters[idx].title.clone();
                    self.start_prompt(InputKind::ChapterTitle(idx), "Chapter title", &title);
                }
            }
            KeyCode::Enter => {
                let idx = cp.selected;
                self.chapter_picker = None;
                self.overlay = Overlay::None;
                if let Some(ed) = &mut self.editor {
                    ed.goto_chapter(idx);
                    self.save_local_now();
                }
            }
            _ => {}
        }
    }

    fn handle_conflict_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('o') => {
                self.conflict = false;
                self.overlay = Overlay::None;
                self.etag = None;
                self.create_new = false;
                self.save_remote_now();
            }
            KeyCode::Char('r') => {
                self.overlay = Overlay::None;
                if let Some(path) = self.remote_path.clone() {
                    self.open_remote(path);
                    self.force_remote_reload = true;
                    self.conflict = true;
                    self.save_state = SaveState::Saving;
                    self.toast("Reloading remote…");
                }
            }
            KeyCode::Char('s') => {
                self.overlay = Overlay::Input;
                self.input = Some(Input::new(
                    InputKind::SaveCopy,
                    "Save a copy as",
                    "copy.odt",
                ));
            }
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                self.toast("Conflict unresolved. Ctrl+S reopens it.");
            }
            _ => {}
        }
    }

    fn flush_current(&mut self) -> bool {
        let Some(ed) = &self.editor else {
            return true;
        };
        if !ed.dirty {
            return true;
        }
        if self.conflict {
            self.overlay = Overlay::Conflict;
            return false;
        }
        if self.net_busy {
            self.toast("Saving… try again in a moment.");
            return false;
        }
        if !self.save_local_now() {
            return false;
        }
        if self.remote_path.is_none() {
            return true;
        }
        let Some(client) = self.client.clone() else {
            return true;
        };
        let Some(path) = self.remote_path.clone() else {
            return true;
        };
        let etag = self.etag.clone();
        let create = self.create_new;
        let bytes = match self.editor.as_mut().map(|e| e.save_bytes()) {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => {
                self.toast(format!("Serialize failed: {e}"));
                return false;
            }
            None => return true,
        };
        match client.put(&path, &bytes, etag.as_deref(), create) {
            Ok(etag) => {
                self.etag = etag;
                self.create_new = false;
                if let Some(ed) = &mut self.editor {
                    ed.mark_saved();
                }
                self.save_state = SaveState::Saved;
                self.write_cache(&path, &bytes, self.etag.as_deref(), false);
                if let Err(e) = backups::save_backup(
                    &remote_identity(self.cfg.as_ref(), &path),
                    &backup_display(&path),
                    &bytes,
                ) {
                    self.toast(format!("Backup failed: {e}"));
                }
                true
            }
            Err(e) => {
                let s = format!("{e}");
                if s.contains("412") {
                    self.conflict = true;
                    self.save_state = SaveState::Conflict;
                    self.overlay = Overlay::Conflict;
                } else {
                    self.save_state = SaveState::Offline;
                    self.toast(format!("Save failed: {e}"));
                }
                false
            }
        }
    }

    fn open_file_picker(&mut self) {
        if self.lock_pending.is_some() || self.pending_open.is_some() {
            self.toast("Waiting for the current file operation to finish.");
            return;
        }
        if self.client.is_none() {
            self.toast("This is a local file. Start scribe without a path to use Nextcloud.");
            return;
        }
        if !self.flush_current() {
            return;
        }
        let path = self
            .cfg
            .as_ref()
            .map(|c| {
                if c.last_folder == "/" && c.remote_root != "/" {
                    c.remote_root.clone()
                } else {
                    c.last_folder.clone()
                }
            })
            .unwrap_or_else(|| "/".to_string());
        self.file_picker = Some(FilePicker::new(path));
        self.overlay = Overlay::FilePicker;
        self.refresh_file_picker();
    }

    fn create_remote_file(&mut self, name: &str) {
        if self.lock_pending.is_some() {
            self.toast("Waiting for the lock operation to finish.");
            return;
        }
        let folder = self
            .file_picker
            .as_ref()
            .map(|f| f.path.clone())
            .unwrap_or_else(|| "/".to_string());
        let mut name = name.trim().to_string();
        if name.is_empty() {
            self.toast("File name is empty");
            return;
        }
        if !name.to_lowercase().ends_with(".odt") {
            name.push_str(".odt");
        }
        if name.contains('/') {
            self.toast("File name cannot contain /");
            return;
        }
        let path = join_path(&folder, &name);
        if self.remote_path.as_deref() == Some(path.as_str()) {
            self.toast("That is the current file");
            return;
        }
        let Some(client) = self.lock_client.clone() else {
            self.toast("Nextcloud is not available");
            return;
        };
        self.lock_pending = Some(PendingLock {
            path: path.clone(),
            action: PendingAction::Create,
        });
        self.overlay = Overlay::None;
        self.toast("Checking file lock…");
        lock::spawn_acquire(
            client,
            path,
            self.lock_identity.clone(),
            self.lock_tx.clone(),
        );
    }

    fn finish_create_remote_file(&mut self, path: String) {
        self.clear_notes_session();
        let title = path
            .rsplit('/')
            .next()
            .unwrap_or("Untitled")
            .trim_end_matches(".odt")
            .to_string();
        match OdtDocument::new(&title) {
            Ok(doc) => {
                self.install_editor(doc, None, Some(path), None, true);
                if let Some(warning) = self.release_previous_lock() {
                    self.toast(warning);
                }
                self.save_local_now();
                self.save_remote_now();
                self.toast(format!("Creating {title}"));
            }
            Err(e) => {
                self.restore_previous_lock();
                self.toast(format!("Create failed: {e}"));
            }
        }
    }

    fn save_copy(&mut self, name: &str) {
        if self.lock_pending.is_some() {
            self.toast("Waiting for the lock operation to finish.");
            return;
        }
        let Some(path) = &self.remote_path else {
            return;
        };
        let folder = path.rsplit_once('/').map(|(f, _)| f).unwrap_or("");
        let mut name = name.trim().to_string();
        if name.is_empty() {
            self.toast("File name is empty");
            return;
        }
        if !name.to_lowercase().ends_with(".odt") {
            name.push_str(".odt");
        }
        let new_path = join_path(folder, &name);
        if self.remote_path.as_deref() == Some(new_path.as_str()) {
            self.toast("That is the current file");
            return;
        }
        let Some(client) = self.lock_client.clone() else {
            self.toast("Nextcloud is not available");
            return;
        };
        self.lock_pending = Some(PendingLock {
            path: new_path.clone(),
            action: PendingAction::SaveCopy,
        });
        self.overlay = Overlay::None;
        self.toast("Checking file lock…");
        lock::spawn_acquire(
            client,
            new_path,
            self.lock_identity.clone(),
            self.lock_tx.clone(),
        );
    }

    fn finish_save_copy(&mut self, path: String) {
        self.remote_path = Some(path);
        self.etag = None;
        self.create_new = true;
        self.conflict = false;
        self.save_state = SaveState::Idle;
        self.update_config_last_file();
        if let Some(warning) = self.release_previous_lock() {
            self.toast(warning);
        }
        self.save_remote_now();
        self.toast("Saving copy…");
    }

    fn try_quit(&mut self) {
        if self.notes_active {
            self.leave_notes();
            if self.net_busy {
                self.toast("Saving notes… try again in a moment.");
                return;
            }
        }
        if self.lock_pending.is_some() {
            self.toast("Waiting for the lock operation to finish.");
            return;
        }
        if let Some(ed) = &self.editor {
            if !ed.dirty && !self.conflict {
                self.finish_quit();
                return;
            }
        }
        if self.conflict {
            self.overlay = Overlay::Conflict;
            return;
        }
        if self.net_busy {
            self.toast("Saving… try again in a moment.");
            return;
        }
        if let Some(armed) = self.quit_armed {
            if Instant::now() < armed {
                self.finish_quit();
                return;
            }
        }
        if !self.save_local_now() {
            self.toast("Local save failed; not quitting.");
            return;
        }
        if self.remote_path.is_some() {
            if let (Some(client), Some(path), Some(ed)) = (
                self.client.as_ref(),
                self.remote_path.clone(),
                self.editor.as_mut(),
            ) {
                match ed.save_bytes() {
                    Ok(bytes) => {
                        let res = client.put(&path, &bytes, self.etag.as_deref(), self.create_new);
                        match res {
                            Ok(etag) => {
                                self.etag = etag.clone();
                                self.create_new = false;
                                ed.mark_saved();
                                self.save_state = SaveState::Saved;
                                self.write_cache(&path, &bytes, self.etag.as_deref(), false);
                                if let Err(e) = backups::save_backup(
                                    &remote_identity(self.cfg.as_ref(), &path),
                                    &backup_display(&path),
                                    &bytes,
                                ) {
                                    self.toast(format!("Backup failed: {e}"));
                                }
                                self.finish_quit();
                                return;
                            }
                            Err(e) => {
                                self.save_state = SaveState::Offline;
                                self.toast(format!("Remote save failed: {e}"));
                            }
                        }
                    }
                    Err(e) => self.toast(format!("Serialize failed: {e}")),
                }
            }
        } else {
            self.finish_quit();
            return;
        }
        self.quit_armed = Some(Instant::now() + Duration::from_secs(4));
        self.toast("Saved locally. Press Ctrl+Q again to quit anyway.");
    }

    fn draw(&mut self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let menu_height = 1u16.min(area.height);
        let panel_height = self.ai_panel_height(area.height);
        if let Some(ed) = &mut self.editor {
            ed.set_viewport(
                area.width,
                area.height
                    .saturating_sub(menu_height)
                    .saturating_sub(panel_height),
            );
        }
        let editor_area = Rect {
            x: 0,
            y: menu_height,
            width: area.width,
            height: area
                .height
                .saturating_sub(menu_height)
                .saturating_sub(panel_height)
                .saturating_sub(1),
        };
        frame.render_widget(Clear, editor_area);
        fill_default_bg(frame.buffer_mut(), editor_area, p.editor_bg);
        if let Some(ed) = &self.editor {
            self.draw_editor(frame, ed, editor_area);
        } else {
            self.draw_empty(frame, editor_area);
        }
        self.draw_status(frame, area);
        self.draw_menu_bar(frame, area);
        if panel_height > 0 {
            self.draw_ai_panel(frame, area, panel_height);
        }
        match self.overlay {
            Overlay::None => {}
            Overlay::Menu => self.draw_menu_dropdown(frame, area),
            Overlay::Help => self.draw_help(frame),
            Overlay::ChapterPicker => self.draw_chapter_picker(frame),
            Overlay::FilePicker => self.draw_file_picker(frame),
            Overlay::Input => self.draw_input(frame),
            Overlay::Conflict => self.draw_conflict(frame),
            Overlay::LockPrompt => self.draw_lock_prompt(frame),
            Overlay::LockWaiting => self.draw_lock_waiting(frame),
            Overlay::LockForce => self.draw_lock_force(frame),
            Overlay::AiPanel => {}
            Overlay::Config => self.draw_config(frame),
            Overlay::Selection => self.draw_selection_menu(frame),
            Overlay::About => self.draw_about(frame),
        }
    }

    fn draw_menu_bar(&self, frame: &mut Frame, area: Rect) {
        let p = self.palette;
        if area.height == 0 {
            return;
        }
        let rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        fill_bg(frame.buffer_mut(), rect, p.bar_bg);
        let buf = frame.buffer_mut();
        let mut x = area.x + 1;
        for (idx, menu) in MENUS.iter().enumerate() {
            let text = format!(" {} ", menu.label);
            let width = text_width(&text) as u16;
            if x + width > area.x + area.width {
                break;
            }
            let style = if self.overlay == Overlay::Menu && idx == self.menu.active {
                Style::default()
                    .bg(p.accent)
                    .fg(p.editor_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(p.bar_bg).fg(p.plain)
            };
            buf.set_stringn(x, rect.y, &text, width as usize, style);
            x += width;
        }
        let hint = "F9 menu";
        let hint_width = text_width(hint) as u16;
        if area.width > hint_width + 2 {
            let hint_x = area.x + area.width - hint_width - 1;
            buf.set_stringn(
                hint_x,
                rect.y,
                hint,
                hint_width as usize,
                Style::default().bg(p.bar_bg).fg(p.dim),
            );
        }
    }

    fn draw_ai_panel(&self, frame: &mut Frame, area: Rect, height: u16) {
        let p = self.palette;
        let Some(panel) = &self.ai_panel else {
            return;
        };
        let top = area.height.saturating_sub(1).saturating_sub(height);
        let rect = Rect {
            x: 0,
            y: top,
            width: area.width,
            height,
        };
        let buf = frame.buffer_mut();
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                buf[(x, y)].set_char(' ');
                buf[(x, y)].set_style(Style::default().bg(p.panel_bg).fg(p.plain));
            }
        }
        for x in rect.x..rect.x + rect.width {
            buf[(x, rect.y)].set_char('─');
            buf[(x, rect.y)].set_style(Style::default().bg(p.panel_bg).fg(p.page_border));
        }

        let header = match panel.status {
            AiStatus::Checking => format!(
                " AI check · chapter {} · {}/{} · {} ",
                panel.chapter + 1,
                panel.progress_done.max(1),
                panel.progress_total.max(1),
                panel.model
            ),
            AiStatus::Ready => format!(
                " AI check · chapter {} · {} suggestion(s) · {} ",
                panel.chapter + 1,
                panel.issues.len(),
                panel.model
            ),
            AiStatus::Error => format!(" AI check · chapter {} · error ", panel.chapter + 1),
        };
        buf.set_stringn(
            rect.x,
            rect.y,
            &header,
            rect.width as usize,
            Style::default()
                .bg(p.panel_bg)
                .fg(p.heading)
                .add_modifier(Modifier::BOLD),
        );

        if rect.height < 3 {
            return;
        }
        let footer_y = rect.y + rect.height - 1;
        let detail_rows = 2u16.min(rect.height.saturating_sub(3));
        let detail_y = footer_y.saturating_sub(detail_rows);
        let footer = match panel.status {
            AiStatus::Checking => " Esc cancel · check again with R",
            AiStatus::Error => " R retry · Esc close",
            AiStatus::Ready if rect.width >= 95 => {
                " ↑↓ move · ←→ scroll · Enter apply · D dismiss · R re-check · Esc close"
            }
            AiStatus::Ready => " ↑↓ ←→ move/scroll · Enter apply · D dismiss · R recheck · Esc",
        };
        buf.set_stringn(
            rect.x,
            footer_y,
            footer,
            rect.width as usize,
            Style::default().bg(p.panel_bg).fg(p.dim),
        );

        if panel.status != AiStatus::Ready {
            let color = if panel.status == AiStatus::Error {
                p.error
            } else {
                p.accent
            };
            buf.set_stringn(
                rect.x + 1,
                rect.y + 1,
                &panel.message,
                rect.width.saturating_sub(2) as usize,
                Style::default().bg(p.panel_bg).fg(color),
            );
            return;
        }

        if panel.issues.is_empty() {
            buf.set_stringn(
                rect.x + 1,
                rect.y + 1,
                &panel.message,
                rect.width.saturating_sub(2) as usize,
                Style::default().bg(p.panel_bg).fg(p.good),
            );
            return;
        }

        let list_rows = detail_y.saturating_sub(rect.y + 1) as usize;
        if list_rows == 0 {
            return;
        }
        let start = panel.selected.saturating_sub(list_rows.saturating_sub(1));
        for (row, (index, issue)) in panel
            .issues
            .iter()
            .enumerate()
            .skip(start)
            .take(list_rows)
            .enumerate()
        {
            let y = rect.y + 1 + row as u16;
            let selected = index == panel.selected;
            let style = if selected {
                Style::default()
                    .bg(p.select_bg)
                    .fg(p.select_fg)
                    .add_modifier(Modifier::BOLD)
            } else if issue.severity == "warning" {
                Style::default().bg(p.panel_bg).fg(p.accent)
            } else {
                Style::default().bg(p.panel_bg).fg(p.plain)
            };
            for x in rect.x..rect.x + rect.width {
                buf[(x, y)].set_char(' ');
                buf[(x, y)].set_style(style);
            }
            let prefix = if selected { "▶ " } else { "  " };
            let content = ai_issue_line(issue, index);
            let visible = if selected {
                let max_scroll =
                    text_width(&content).saturating_sub(rect.width.saturating_sub(3) as usize);
                scroll_text(
                    &content,
                    panel.scroll.min(max_scroll),
                    rect.width.saturating_sub(2) as usize,
                )
            } else {
                content
            };
            buf.set_stringn(rect.x, y, prefix, 2, style);
            buf.set_stringn(
                rect.x + 2,
                y,
                &visible,
                rect.width.saturating_sub(2) as usize,
                style,
            );
        }

        let detail = panel
            .issues
            .get(panel.selected)
            .map(|issue| {
                if issue.message.is_empty() {
                    format!("{} of {}", panel.selected + 1, panel.issues.len())
                } else {
                    format!("{}: {}", panel.selected + 1, issue.message)
                }
            })
            .unwrap_or_else(|| panel.message.clone());
        if detail_rows > 0 {
            let detail_area = Rect {
                x: rect.x + 1,
                y: detail_y,
                width: rect.width.saturating_sub(2),
                height: detail_rows,
            };
            frame.render_widget(
                Paragraph::new(detail)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().bg(p.panel_bg).fg(p.dim)),
                detail_area,
            );
        }
    }

    fn draw_config(&self, frame: &mut Frame) {
        let p = self.palette;
        let Some(panel) = &self.config_panel else {
            return;
        };
        let area = frame.area();
        let height = 19.min(area.height);
        let rect = centered(area, 88.min(area.width), height);
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.page_border))
            .title(" Configuration ")
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let buf = frame.buffer_mut();
        let footer_y = inner.y + inner.height - 1;
        let status_y = footer_y.saturating_sub(1);
        let field_bottom = status_y.saturating_sub(1);
        let label_w = 14u16;
        let value_x = inner.x + label_w;
        let value_w = inner.width.saturating_sub(label_w);
        let mut y = inner.y;

        if y < field_bottom {
            buf.set_stringn(
                inner.x,
                y,
                " AI writing check",
                inner.width as usize,
                Style::default()
                    .bg(p.panel_bg)
                    .fg(p.heading)
                    .add_modifier(Modifier::BOLD),
            );
            y += 1;
        }
        for (index, field) in CONFIG_FIELDS.iter().take(6).enumerate() {
            if y >= field_bottom {
                break;
            }
            let selected = index == panel.selected;
            let editing = selected && panel.editing.is_some();
            let row_style = if selected {
                Style::default()
                    .bg(p.select_bg)
                    .fg(p.select_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(p.panel_bg).fg(p.plain)
            };
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_char(' ');
                buf[(x, y)].set_style(row_style);
            }
            buf.set_stringn(
                inner.x + 1,
                y,
                field.label(),
                label_w as usize,
                Style::default()
                    .bg(if selected { p.select_bg } else { p.panel_bg })
                    .fg(if selected { p.select_fg } else { p.dim }),
            );
            let value = if let Some(input) = panel.editing.as_ref().filter(|_| editing) {
                if *field == ConfigField::ApiKey {
                    panel.display_key()
                } else {
                    input.value.clone()
                }
            } else {
                panel.display_value(*field)
            };
            buf.set_stringn(value_x, y, &value, value_w as usize, row_style);
            if editing {
                let cursor_chars = panel
                    .editing
                    .as_ref()
                    .map(|input| input.cursor.min(40))
                    .unwrap_or(0);
                let cx = value_x + cursor_chars as u16;
                if cx < inner.x + inner.width {
                    buf[(cx, y)].set_style(Style::default().bg(p.cursor_bg).fg(p.cursor_fg));
                }
            }
            y += 1;
        }

        if y < field_bottom {
            y += 1;
        }
        if y < field_bottom {
            buf.set_stringn(
                inner.x,
                y,
                " Appearance",
                inner.width as usize,
                Style::default()
                    .bg(p.panel_bg)
                    .fg(p.heading)
                    .add_modifier(Modifier::BOLD),
            );
            y += 1;
        }
        let theme_index = CONFIG_FIELDS.len() - 1;
        if y < field_bottom {
            let selected = panel.selected == theme_index;
            let row_style = if selected {
                Style::default()
                    .bg(p.select_bg)
                    .fg(p.select_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(p.panel_bg).fg(p.plain)
            };
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_char(' ');
                buf[(x, y)].set_style(row_style);
            }
            buf.set_stringn(
                inner.x + 1,
                y,
                ConfigField::Theme.label(),
                label_w as usize,
                Style::default()
                    .bg(if selected { p.select_bg } else { p.panel_bg })
                    .fg(if selected { p.select_fg } else { p.dim }),
            );
            let theme = if selected {
                format!("◀ {} ▶", self.settings.theme.label())
            } else {
                self.settings.theme.label().to_string()
            };
            buf.set_stringn(value_x, y, &theme, value_w as usize, row_style);
        }

        if status_y < footer_y {
            if let Some((message, error)) = &panel.status {
                let color = if *error { p.error } else { p.good };
                buf.set_stringn(
                    inner.x + 1,
                    status_y,
                    message,
                    inner.width.saturating_sub(2) as usize,
                    Style::default().fg(color),
                );
            } else if std::env::var_os("SCRIBE_AI_API_KEY").is_some() {
                buf.set_stringn(
                    inner.x + 1,
                    status_y,
                    "SCRIBE_AI_API_KEY is set and overrides the saved key",
                    inner.width.saturating_sub(2) as usize,
                    Style::default().fg(p.dim),
                );
            }
        }

        let (footer, footer_color) = if panel.editing.is_some() {
            (
                "Type a new value · Enter confirm · Esc cancel · key stays masked",
                p.dim,
            )
        } else if panel.testing {
            ("Testing the AI endpoint…", p.accent)
        } else {
            (
                "↑↓ move · Enter edit/toggle · ←→ theme · s save · t test · Esc cancel",
                p.dim,
            )
        };
        buf.set_stringn(
            inner.x + 1,
            footer_y,
            footer,
            inner.width.saturating_sub(2) as usize,
            Style::default().fg(footer_color),
        );
    }

    fn draw_selection_menu(&self, frame: &mut Frame) {
        let p = self.palette;
        let Some(menu) = &self.selection_menu else {
            return;
        };
        let area = frame.area();
        let height = (SELECTION_ACTIONS.len() as u16 + 6).min(area.height);
        let rect = centered(area, 52.min(area.width), height);
        frame.render_widget(Clear, rect);
        let count = self
            .editor
            .as_ref()
            .map(|editor| editor.selection_len())
            .unwrap_or(0);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.page_border))
            .title(" Selection ")
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let buf = frame.buffer_mut();
        let footer_y = inner.y + inner.height - 1;
        buf.set_stringn(
            inner.x,
            inner.y,
            format!(" {count} characters selected"),
            inner.width as usize,
            Style::default().fg(p.dim),
        );
        for (index, (label, action)) in SELECTION_ACTIONS.iter().enumerate() {
            let y = inner.y + 2 + index as u16;
            if y >= footer_y {
                break;
            }
            let selected = index == menu.selected;
            let row_style = if selected {
                Style::default()
                    .bg(p.select_bg)
                    .fg(p.select_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(p.panel_bg).fg(p.plain)
            };
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_char(' ');
                buf[(x, y)].set_style(row_style);
            }
            buf.set_stringn(inner.x + 1, y, label, inner.width as usize, row_style);
            let hint = match action {
                SelectionAction::Bold => "B",
                SelectionAction::Italic => "I",
                SelectionAction::Underline => "U",
                SelectionAction::Strike => "S",
                SelectionAction::Delete => "D",
                SelectionAction::None => "Esc",
            };
            let hint_width = text_width(hint) as u16;
            if hint_width + 2 < inner.width {
                buf.set_stringn(
                    inner.x + inner.width - hint_width - 1,
                    y,
                    hint,
                    hint_width as usize,
                    Style::default()
                        .bg(if selected { p.select_bg } else { p.panel_bg })
                        .fg(p.dim),
                );
            }
        }
        buf.set_stringn(
            inner.x + 1,
            footer_y,
            "↑↓ choose · Enter apply · Esc keeps selection",
            inner.width.saturating_sub(2) as usize,
            Style::default().fg(p.dim),
        );
    }

    fn draw_menu_dropdown(&self, frame: &mut Frame, area: Rect) {
        let p = self.palette;
        if area.height < 2 {
            return;
        }
        let Some(menu) = MENUS.get(self.menu.active) else {
            return;
        };
        let mut x = area.x + 1;
        for item in MENUS.iter().take(self.menu.active) {
            x += text_width(&format!(" {} ", item.label)) as u16;
        }
        let width = menu_dropdown_width(menu).max(12).min(area.width);
        if x + width > area.x + area.width {
            x = (area.x + area.width).saturating_sub(width);
        }
        let available_height = area.height.saturating_sub(1);
        let height = (menu.entries.len() as u16 + 2)
            .min(available_height)
            .max(3.min(available_height));
        let rect = Rect {
            x,
            y: area.y + 1,
            width,
            height,
        };
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.page_border))
            .style(Style::default().bg(p.bar_bg));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        fill_bg(frame.buffer_mut(), inner, p.bar_bg);
        let buf = frame.buffer_mut();
        for (row, entry) in menu.entries.iter().take(inner.height as usize).enumerate() {
            let y = inner.y + row as u16;
            match entry {
                MenuEntry::Separator => {
                    let style = Style::default().bg(p.bar_bg).fg(p.page_border);
                    for x in inner.x..inner.x + inner.width {
                        buf[(x, y)].set_char('─');
                        buf[(x, y)].set_style(style);
                    }
                }
                MenuEntry::Item {
                    label,
                    shortcut,
                    action,
                } => {
                    let enabled = self.menu_item_enabled(*action);
                    let selected = row == self.menu.selected;
                    let row_style = if selected {
                        Style::default().bg(p.select_bg).fg(p.select_fg)
                    } else if enabled {
                        Style::default().bg(p.bar_bg).fg(p.plain)
                    } else {
                        Style::default().bg(p.bar_bg).fg(p.dim)
                    };
                    for x in inner.x..inner.x + inner.width {
                        buf[(x, y)].set_char(' ');
                        buf[(x, y)].set_style(row_style);
                    }
                    let label_style = if selected {
                        row_style.add_modifier(Modifier::BOLD)
                    } else {
                        row_style
                    };
                    buf.set_stringn(inner.x + 1, y, label, inner.width as usize, label_style);
                    let shortcut_width = text_width(shortcut) as u16;
                    if shortcut_width > 0 {
                        let shortcut_x = inner.x + inner.width.saturating_sub(shortcut_width + 1);
                        let label_end = inner.x + 1 + text_width(label) as u16;
                        if shortcut_x > label_end {
                            let shortcut_bg = if selected { p.select_bg } else { p.bar_bg };
                            buf.set_stringn(
                                shortcut_x,
                                y,
                                shortcut,
                                shortcut_width as usize,
                                Style::default().bg(shortcut_bg).fg(p.dim),
                            );
                        }
                    }
                }
            }
        }
    }

    fn draw_editor(&self, frame: &mut Frame, ed: &Editor, area: Rect) {
        let p = self.palette;
        let width = area.width as usize;
        let tw = ed.text_width();
        let margin = width.saturating_sub(tw) / 2;
        let has_left = margin >= 1;
        let has_right = margin + tw < width;
        let page_x = if has_left { margin - 1 } else { margin };
        let page_end = if has_right {
            margin + tw + 1
        } else {
            margin + tw
        };
        let page_w = page_end
            .saturating_sub(page_x)
            .min(width.saturating_sub(page_x));
        let page_rect = Rect {
            x: page_x as u16,
            y: area.y,
            width: page_w as u16,
            height: area.height,
        };
        fill_bg(frame.buffer_mut(), page_rect, p.page_bg);
        let buf = frame.buffer_mut();
        let page_start = ed.page_start();
        for (row, line) in ed.page_lines().iter().enumerate() {
            let y = area.y + row as u16;
            if y >= area.y + area.height {
                break;
            }
            match line.kind {
                LineKind::Blank => {}
                LineKind::Heading => {
                    let text = ed.doc.book.heading_text(ed.current_chapter());
                    let chars: Vec<char> = text.chars().collect();
                    let mut x = margin as u16;
                    for &ch in chars
                        .iter()
                        .skip(line.start)
                        .take(line.end.saturating_sub(line.start))
                    {
                        if x >= area.x + area.width {
                            break;
                        }
                        buf[(x, y)].set_char(ch);
                        buf[(x, y)]
                            .set_style(Style::default().fg(p.heading).add_modifier(Modifier::BOLD));
                        x += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
                    }
                }
                LineKind::Para => {
                    let para = &ed.doc.book.chapters[ed.current_chapter()].paragraphs[line.para];
                    let subheading = para.kind == ParaKind::Subheading;
                    let mut x = (margin + line.indent) as u16;
                    for (i, c) in para.chars[line.start..line.end].iter().enumerate() {
                        if x >= area.x + area.width {
                            break;
                        }
                        let color = if subheading {
                            p.heading
                        } else if c.bold && c.italic {
                            p.both
                        } else if c.bold {
                            p.bold
                        } else if c.italic {
                            p.italic
                        } else {
                            p.plain
                        };
                        let mut style = Style::default().fg(color);
                        let selected = ed.is_selected(line.para, line.start + i);
                        if selected {
                            style = style.bg(p.selection_bg);
                        }
                        if !selected
                            && self.overlay == Overlay::AiPanel
                            && let Some(panel) = &self.ai_panel
                            && panel.chapter == ed.current_chapter()
                        {
                            let offset = line.start + i;
                            if let Some((index, _)) =
                                panel.issues.iter().enumerate().find(|(_, issue)| {
                                    issue.para == line.para
                                        && offset >= issue.start
                                        && offset < issue.end
                                })
                            {
                                style = style.bg(if index == panel.selected {
                                    p.ai_focus_bg
                                } else {
                                    p.ai_issue_bg
                                });
                            }
                        }
                        if self.search.active
                            && let Some(index) = self.search.current
                            && let Some(found) = self.search.matches.get(index)
                            && found.chapter == ed.current_chapter()
                            && found.para == line.para
                        {
                            let offset = line.start + i;
                            if offset >= found.start && offset < found.end {
                                style = style.bg(p.selection_bg);
                            }
                        }
                        if c.bold || subheading {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if c.italic {
                            style = style.add_modifier(Modifier::ITALIC);
                        }
                        if c.underline {
                            style = style.add_modifier(Modifier::UNDERLINED);
                        }
                        if c.strike {
                            style = style.add_modifier(Modifier::CROSSED_OUT);
                        }
                        buf[(x, y)].set_char(c.ch);
                        buf[(x, y)].set_style(style);
                        x += UnicodeWidthChar::width(c.ch).unwrap_or(1) as u16;
                    }
                }
            }
        }
        let _ = page_start;
        {
            let buf = frame.buffer_mut();
            if has_left {
                let x = (margin - 1) as u16;
                for y in area.y..area.y.saturating_add(area.height) {
                    buf[(x, y)].set_char('│');
                    buf[(x, y)].set_style(Style::default().fg(p.page_border).bg(p.page_bg));
                }
            }
            if has_right {
                let x = (margin + tw) as u16;
                for y in area.y..area.y.saturating_add(area.height) {
                    buf[(x, y)].set_char('│');
                    buf[(x, y)].set_style(Style::default().fg(p.page_border).bg(p.page_bg));
                }
            }
        }
        if self.overlay != Overlay::None {
            return;
        }
        let cp = ed.cursor_pos();
        let row = cp.line.saturating_sub(page_start);
        if row < area.height as usize {
            let x = (margin + cp.col).min(width.saturating_sub(1)) as u16;
            let y = area.y + row as u16;
            let cell = &mut frame.buffer_mut()[(x, y)];
            cell.set_style(Style::default().bg(p.cursor_bg).fg(p.cursor_fg));
        }
    }

    fn draw_empty(&self, frame: &mut Frame, area: Rect) {
        let p = self.palette;
        let text = vec![
            Line::from(Span::styled(
                "scribe",
                Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Ctrl+O  open a book from Nextcloud",
                Style::default().fg(p.plain),
            )),
            Line::from(Span::styled(
                "Ctrl+N  new chapter after opening a book",
                Style::default().fg(p.plain),
            )),
            Line::from(Span::styled(
                "F9 menu · Alt+H help",
                Style::default().fg(p.dim),
            )),
        ];
        let w = 60.min(area.width);
        let h = text.len() as u16 + 2;
        let rect = centered(area, w, h);
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.dim))
            .title(" scribe ")
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new(text), inner);
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let p = self.palette;
        let rect = Rect {
            x: 0,
            y: area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };
        let buf = frame.buffer_mut();
        for x in rect.x..rect.x + rect.width {
            buf[(x, rect.y)].set_char(' ');
            buf[(x, rect.y)].set_style(Style::default().bg(p.bar_bg).fg(p.dim));
        }
        if self.search.active {
            let prefix = " Find: ";
            let prefix_width = text_width(prefix) as u16;
            buf.set_stringn(
                rect.x,
                rect.y,
                prefix,
                rect.width as usize,
                Style::default().bg(p.bar_bg).fg(p.dim),
            );
            let max = search_query_capacity(rect.width);
            let mut x = rect.x + prefix_width;
            let mut used = 0usize;
            for ch in self.search.query.chars() {
                let width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + width > max || x >= rect.x + rect.width {
                    break;
                }
                buf[(x, rect.y)].set_char(ch);
                buf[(x, rect.y)].set_style(Style::default().bg(p.bar_bg).fg(p.plain));
                x += width as u16;
                used += width;
            }
            let cursor_width: usize = self
                .search
                .query
                .chars()
                .take(self.search.cursor)
                .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
                .sum();
            let caret_x = rect.x + prefix_width + cursor_width.min(max) as u16;
            if caret_x < rect.x + rect.width {
                buf[(caret_x, rect.y)].set_style(Style::default().bg(p.cursor_bg).fg(p.cursor_fg));
            }
            let status = if self.search.no_match {
                Some(("No match".to_string(), p.error))
            } else {
                self.search.current.map(|index| {
                    (
                        format!("{}/{}", index + 1, self.search.matches.len()),
                        p.heading,
                    )
                })
            };
            if let Some((text, color)) = status {
                let width = text_width(&text) as u16;
                if rect.width > prefix_width + width + 1 {
                    let x = rect.x + rect.width - width - 1;
                    buf.set_stringn(
                        x,
                        rect.y,
                        &text,
                        width as usize,
                        Style::default().bg(p.bar_bg).fg(color),
                    );
                }
            }
            return;
        }
        let mut spans: Vec<Span> = Vec::new();
        if let Some((msg, _)) = &self.toast {
            spans.push(Span::styled(msg.clone(), Style::default().fg(p.accent)));
            spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
        }
        if let Some(ed) = &self.editor {
            spans.push(Span::styled(
                format!("{} words", fmt_num(ed.total_words)),
                Style::default().fg(p.plain),
            ));
            spans.push(Span::styled(" · ", Style::default().fg(p.dim)));
            spans.push(Span::styled(
                format!("{} chars", fmt_num(ed.total_chars)),
                Style::default().fg(p.plain),
            ));
            spans.push(Span::styled(" · ", Style::default().fg(p.dim)));
            spans.push(Span::styled(
                format!("{} pages", fmt_num(ed.total_pages)),
                Style::default().fg(p.plain),
            ));
            spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
            spans.push(Span::styled(
                format!(
                    "Ch {}/{}  p {}/{}",
                    ed.current_chapter() + 1,
                    ed.doc.book.chapters.len(),
                    ed.current_page() + 1,
                    ed.current_chapter_pages()
                ),
                Style::default().fg(p.heading),
            ));
            spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
            spans.push(Span::styled(
                self.save_state.text().to_string(),
                Style::default().fg(self.save_state.color(p)),
            ));
            if ed.has_selection() {
                spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
                spans.push(Span::styled(
                    format!("SEL {}", fmt_num(ed.selection_len())),
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ));
            }
            let style = ed.status_style();
            let mut marks: Vec<(&str, Color)> = Vec::new();
            if style.bold {
                marks.push(("B", p.bold));
            }
            if style.italic {
                marks.push(("I", p.italic));
            }
            if style.underline {
                marks.push(("U", p.heading));
            }
            if style.strike {
                marks.push(("S", p.both));
            }
            if !marks.is_empty() {
                spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
                for (index, (label, color)) in marks.into_iter().enumerate() {
                    if index > 0 {
                        spans.push(Span::styled("+", Style::default().fg(p.dim)));
                    }
                    spans.push(Span::styled(label, Style::default().fg(color)));
                }
            }
            if ed.pending_style.is_some() {
                spans.push(Span::styled("*", Style::default().fg(p.accent)));
            }
            if self.notes_active {
                spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
                spans.push(Span::styled(
                    "NOTES",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ));
            }
            if ed.current_is_subheading() {
                spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
                spans.push(Span::styled(
                    "H3",
                    Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
                ));
            }
            let alignment = ed.current_alignment();
            if alignment != Align::Justify {
                spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
                let label = match alignment {
                    Align::Left => "ALIGN L",
                    Align::Center => "ALIGN C",
                    Align::Right => "ALIGN R",
                    Align::Justify => "",
                };
                spans.push(Span::styled(label, Style::default().fg(p.accent)));
            }
            spans.push(Span::styled("  │  ", Style::default().fg(p.dim)));
            spans.push(Span::styled(
                ed.doc.book.title.clone(),
                Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                "F9 menu  ·  Ctrl+O open  ·  Alt+H help",
                Style::default().fg(p.dim),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(p.bar_bg)),
            rect,
        );
    }

    fn draw_about(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let rect = centered(area, 68.min(area.width), 16.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.dim))
            .title(" About scribe ")
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines = vec![
            Line::from(Span::styled(
                format!("scribe {}", app_version()),
                Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("A focused terminal editor for long-form prose"),
            Line::from("stored as ODT in Nextcloud."),
            Line::from(""),
            Line::from(vec![
                Span::styled("Author       ", Style::default().fg(p.dim)),
                Span::raw(APP_AUTHOR),
            ]),
            Line::from(vec![
                Span::styled("Repository   ", Style::default().fg(p.dim)),
                Span::raw(APP_REPO),
            ]),
            Line::from(vec![
                Span::styled("License      ", Style::default().fg(p.dim)),
                Span::raw(APP_LICENSE),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Local files  ", Style::default().fg(p.dim)),
                Span::raw("scribe open book.odt"),
            ]),
            Line::from(vec![
                Span::styled("Nextcloud    ", Style::default().fg(p.dim)),
                Span::raw("optional — scribe login <url>"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Esc / Alt+A close",
                Style::default().fg(p.dim),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(p.plain)),
            inner,
        );
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_help(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let rect = centered(area, 76.min(area.width), 33.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.dim))
            .title(" Help ")
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines = vec![
            Line::from(Span::styled(
                "Editing",
                Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
            )),
            Line::from("Enter new paragraph          Backspace merge/delete"),
            Line::from("Ctrl+W delete word           Ctrl+Delete delete next word"),
            Line::from("Ctrl+U delete to para start  Ctrl+K delete to para end"),
            Line::from("Ctrl+Shift+K delete paragraph"),
            Line::from("Ctrl+Z undo                  Ctrl+Y redo"),
            Line::from("Ctrl+Space mark start/end    Esc clear selection"),
            Line::from("Shift/Alt+Enter line break  Finish mark = selection menu"),
            Line::from(""),
            Line::from(Span::styled(
                "Formatting",
                Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
            )),
            Line::from("Ctrl+B / Alt+B bold word     Alt+I italic word"),
            Line::from("Alt+U underline              Alt+S strikethrough"),
            Line::from("B* / I* / U* / S* applies to the next typed text"),
            Line::from("H3 subheading and alignment are in the Edit menu"),
            Line::from("Typing \" or ' becomes curly based on surrounding text"),
            Line::from("New sentences capitalize the first typed letter"),
            Line::from(""),
            Line::from(Span::styled(
                "Chapters",
                Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
            )),
            Line::from("Ctrl+N new chapter           Ctrl+T edit chapter title"),
            Line::from("Ctrl+E edit book title"),
            Line::from("Ctrl+G goto chapter          Alt+Up/Down previous/next chapter"),
            Line::from(""),
            Line::from(Span::styled(
                "Files and navigation",
                Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
            )),
            Line::from("Ctrl+O open from Nextcloud   Ctrl+S save now"),
            Line::from("F3 find                      Ctrl+, configuration"),
            Line::from("F7 notes sidecar             F8 AI writing check"),
            Line::from("F9 menu bar · themes: Tools → Configuration"),
            Line::from("Alt+H help / Alt+A about    PgUp/PgDn page"),
            Line::from("Ctrl+Home/End chapter start/end"),
            Line::from("Ctrl+L full redraw            Ctrl+Q quit"),
            Line::from("Esc close menu/help"),
        ];
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(p.plain)),
            inner,
        );
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_chapter_picker(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let Some(ed) = &self.editor else {
            return;
        };
        let Some(cp) = &self.chapter_picker else {
            return;
        };
        let h = (ed.doc.book.chapters.len() as u16 + 4).min(area.height.saturating_sub(2));
        let rect = centered(area, 86.min(area.width), h.max(6));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.dim))
            .title(" Goto chapter ")
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let visible_h = inner.height.saturating_sub(1) as usize;
        let offset = cp.selected.saturating_sub(visible_h / 2);
        let buf = frame.buffer_mut();
        for row in 0..visible_h {
            let idx = offset + row;
            if idx >= ed.doc.book.chapters.len() {
                break;
            }
            let c = &ed.doc.book.chapters[idx];
            let title = if c.title.trim().is_empty() {
                "(untitled)"
            } else {
                c.title.trim()
            };
            let text = format!(
                "{:>3}. {:<52} {:>6} words",
                c.number,
                title,
                fmt_num(c.word_count())
            );
            let y = inner.y + row as u16;
            let style = if idx == cp.selected {
                Style::default().bg(p.select_bg).fg(p.select_fg)
            } else {
                Style::default().fg(p.plain)
            };
            buf.set_stringn(inner.x, y, text, inner.width as usize, style);
        }
        let footer = "Enter jump · e edit title · Esc close";
        if inner.height > 0 {
            buf.set_stringn(
                inner.x,
                inner.y + inner.height - 1,
                footer,
                inner.width as usize,
                Style::default().fg(p.dim),
            );
        }
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_file_picker(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let Some(fp) = &self.file_picker else {
            return;
        };
        let rect = centered(area, 90.min(area.width), 30.min(area.height));
        frame.render_widget(Clear, rect);
        let title = format!(" Open from Nextcloud — {} ", fp.path);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.dim))
            .title(title)
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if fp.loading {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Loading…",
                    Style::default().fg(p.heading),
                ))),
                inner,
            );
            fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
            return;
        }
        if let Some(err) = &fp.error {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(err.clone(), Style::default().fg(p.error))),
                    Line::from(Span::styled(
                        "n new file · r retry · Esc close",
                        Style::default().fg(p.dim),
                    )),
                ]),
                inner,
            );
            fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
            return;
        }
        let visible = fp.visible();
        let list_h = inner.height.saturating_sub(2) as usize;
        let offset = fp.selected.saturating_sub(list_h / 2);
        {
            let buf = frame.buffer_mut();
            for row in 0..list_h {
                let idx = offset + row;
                if idx >= visible.len() {
                    break;
                }
                let e = visible[idx];
                let name = if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                };
                let size = if e.is_dir {
                    String::new()
                } else {
                    e.size
                        .map(|s| format!("{:>10}", human_size(s)))
                        .unwrap_or_default()
                };
                let text = format!("{:<62} {}", name, size);
                let y = inner.y + row as u16;
                let style = if idx == fp.selected {
                    Style::default().bg(p.select_bg).fg(p.select_fg)
                } else if e.is_dir {
                    Style::default().fg(p.heading)
                } else {
                    Style::default().fg(p.plain)
                };
                buf.set_stringn(inner.x, y, text, inner.width as usize, style);
            }
            let footer_y = inner.y + inner.height.saturating_sub(2);
            if fp.filtering || !fp.filter.is_empty() {
                buf.set_stringn(
                    inner.x,
                    footer_y,
                    format!("/{}", fp.filter),
                    inner.width as usize,
                    Style::default().fg(p.accent),
                );
                let x = inner.x + 1 + fp.filter.chars().count() as u16;
                if x < inner.x + inner.width {
                    buf[(x, footer_y)].set_style(Style::default().bg(p.cursor_bg).fg(p.cursor_fg));
                }
            } else {
                buf.set_stringn(
                    inner.x,
                    footer_y,
                    "Enter open · n new · r refresh · / filter · Backspace up · Esc close",
                    inner.width as usize,
                    Style::default().fg(p.dim),
                );
            }
            let hint_y = inner.y + inner.height.saturating_sub(1);
            if hint_y > footer_y {
                buf.set_stringn(
                    inner.x,
                    hint_y,
                    "Only .odt files are shown",
                    inner.width as usize,
                    Style::default().fg(p.dim),
                );
            }
        }
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_input(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let Some(input) = &self.input else {
            return;
        };
        let rect = centered(area, 72.min(area.width), 7.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.dim))
            .title(format!(" {} ", input.label))
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let line_y = inner.y + 1;
        let prefix_width: usize = input
            .value
            .chars()
            .take(input.cursor)
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(1))
            .sum();
        let available = inner.width.saturating_sub(1) as usize;
        let scroll = prefix_width.saturating_sub(available);
        let cx = inner.x + (prefix_width - scroll) as u16;
        {
            let mut x = inner.x;
            let mut consumed = 0usize;
            let buf = frame.buffer_mut();
            for ch in input.value.chars() {
                let w = UnicodeWidthChar::width(ch).unwrap_or(1);
                if consumed + w <= scroll {
                    consumed += w;
                    continue;
                }
                if x >= inner.x + inner.width {
                    break;
                }
                buf[(x, line_y)].set_char(ch);
                buf[(x, line_y)].set_style(Style::default().fg(p.plain));
                x += w as u16;
                consumed += w;
            }
            if inner.height > 2 {
                buf.set_stringn(
                    inner.x,
                    inner.y + inner.height - 1,
                    "Enter confirm · Esc cancel",
                    inner.width as usize,
                    Style::default().fg(p.dim),
                );
            }
            if cx < inner.x + inner.width {
                buf[(cx, line_y)].set_style(Style::default().bg(p.cursor_bg).fg(p.cursor_fg));
            }
        }
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_conflict(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let rect = centered(area, 64.min(area.width), 12.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.error))
            .title(" Save conflict ")
            .title_style(Style::default().fg(p.error));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines = vec![
            Line::from(Span::styled(
                "The file changed on Nextcloud since you opened it.",
                Style::default().fg(p.plain),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "[O] Overwrite remote with my version",
                Style::default().fg(p.accent),
            )),
            Line::from(Span::styled(
                "[R] Reload remote and discard my changes",
                Style::default().fg(p.accent),
            )),
            Line::from(Span::styled(
                "[S] Save my version as a copy",
                Style::default().fg(p.accent),
            )),
            Line::from(Span::styled(
                "[Esc] Decide later",
                Style::default().fg(p.dim),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_lock_prompt(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let rect = centered(area, 72.min(area.width), 14.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(" File is locked ")
            .title_style(Style::default().fg(p.accent));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let Some(held) = &self.lock_prompt else {
            return;
        };
        let name = held
            .info
            .file
            .rsplit('/')
            .next()
            .unwrap_or("This file")
            .to_string();
        let mut lines = vec![
            Line::from(Span::styled(
                format!("{name} is open in another scribe instance."),
                Style::default().fg(p.plain),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    "Host: {}   User: {}   PID: {}",
                    held.info.host, held.info.user, held.info.pid
                ),
                Style::default().fg(p.plain),
            )),
            Line::from(Span::styled(
                format!("Opened: {} ago", fmt_age(held.info.age_secs())),
                Style::default().fg(p.dim),
            )),
        ];
        if let Some(error) = &held.unreadable {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("The lock file is unreadable: {error}"),
                Style::default().fg(p.error),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[K] Kick the other instance and open here",
            Style::default().fg(p.accent),
        )));
        lines.push(Line::from(Span::styled(
            "[C] Cancel",
            Style::default().fg(p.dim),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_lock_waiting(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let rect = centered(area, 68.min(area.width), 8.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.heading))
            .title(" Waiting for lock ")
            .title_style(Style::default().fg(p.heading));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines = vec![
            Line::from(Span::styled(
                "Waiting for the other instance to release the lock…",
                Style::default().fg(p.plain),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "The file opens here as soon as the old editor exits.",
                Style::default().fg(p.dim),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }

    fn draw_lock_force(&self, frame: &mut Frame) {
        let p = self.palette;
        let area = frame.area();
        let rect = centered(area, 68.min(area.width), 12.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.error))
            .title(" Other instance did not respond ")
            .title_style(Style::default().fg(p.error));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines = vec![
            Line::from(Span::styled(
                "It may be offline, crashed, or no longer running.",
                Style::default().fg(p.plain),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "[F] Force open anyway (it may not have saved)",
                Style::default().fg(p.accent),
            )),
            Line::from(Span::styled("[C] Cancel", Style::default().fg(p.dim))),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        fill_default_bg(frame.buffer_mut(), rect, p.panel_bg);
    }
}

fn fmt_age(seconds: Option<u64>) -> String {
    match seconds {
        Some(seconds) if seconds < 60 => format!("{seconds}s"),
        Some(seconds) if seconds < 3_600 => format!("{}m", seconds / 60),
        Some(seconds) if seconds < 86_400 => format!("{}h", seconds / 3_600),
        Some(seconds) => format!("{}d", seconds / 86_400),
        None => "unknown".to_string(),
    }
}

fn append_warnings(mut text: String, warnings: &[String]) -> String {
    for warning in warnings {
        text.push('\n');
        text.push_str(warning);
    }
    text
}

fn local_identity(path: &Path) -> String {
    format!("local|{}", path.display())
}

fn local_display(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("book")
        .to_string()
}

fn remote_identity(cfg: Option<&Config>, path: &str) -> String {
    match cfg {
        Some(cfg) => format!("{}|{}|{}", cfg.server, cfg.login_name, path),
        None => format!("remote|{path}"),
    }
}

fn backup_display(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or("book")
        .to_string()
}

fn fill_bg(buf: &mut Buffer, rect: Rect, color: Color) {
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            buf[(x, y)].bg = color;
        }
    }
}

fn fill_default_bg(buf: &mut Buffer, rect: Rect, color: Color) {
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            let cell = &mut buf[(x, y)];
            if cell.bg == Color::Reset {
                cell.bg = color;
            }
        }
    }
}

fn stable_hash(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in value.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn text_hash(paragraphs: &[String]) -> u64 {
    let mut joined = String::new();
    for paragraph in paragraphs {
        joined.push_str(paragraph);
        joined.push('\u{0}');
    }
    stable_hash(&joined)
}

fn ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

fn ctrl_space(key: &KeyEvent) -> bool {
    ctrl(key) && matches!(key.code, KeyCode::Char(' ') | KeyCode::Null)
}

fn alt_only(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_text_key(key: &KeyEvent) -> bool {
    !key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT)
}

fn notes_local_path(path: &Path) -> PathBuf {
    if path.to_string_lossy().ends_with(".notes.odt") {
        return path.to_path_buf();
    }
    let mut value = path.as_os_str().to_os_string();
    value.push(".notes.odt");
    PathBuf::from(value)
}

fn notes_remote_path(path: &str) -> String {
    if path.ends_with(".notes.odt") {
        path.to_string()
    } else {
        format!("{path}.notes.odt")
    }
}

fn notes_title(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or("Notes")
        .trim_end_matches(".odt")
        .to_string()
}

fn notes_document_from(book: &Book) -> Result<OdtDocument> {
    let title = book.title.trim();
    let title = if title.is_empty() {
        "Notes".to_string()
    } else {
        format!("{title} — Notes")
    };
    let mut doc = OdtDocument::new(title)?;
    doc.book.chapters = book
        .chapters
        .iter()
        .map(|chapter| Chapter::new(chapter.number, chapter.title.clone()))
        .collect();
    if doc.book.chapters.is_empty() {
        doc.book.chapters.push(Chapter::new(1, ""));
    }
    doc.book.renumber();
    Ok(doc)
}

fn parent_path(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rsplit_once('/') {
        Some(("", _)) | None => "/".to_string(),
        Some((parent, _)) => {
            if parent.is_empty() {
                "/".to_string()
            } else {
                parent.to_string()
            }
        }
    }
}

/// Write `bytes` into `dir`, creating the directory, and return the path used.
fn write_downloaded_copy(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    secs: u64,
) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = copy_destination(dir, name, secs);
    fs::write(&path, bytes)?;
    Ok(path)
}

/// Destination for a downloaded copy. The original name is kept unless it
/// already exists; then `stem_<secs>.ext` (and `_2`, `_3`, … for a same-second
/// collision) is used.
fn copy_destination(dir: &Path, name: &str, secs: u64) -> PathBuf {
    let original = dir.join(name);
    if !original.exists() {
        return original;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("untitled");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("odt");
    let mut candidate = dir.join(format!("{stem}_{secs}.{ext}"));
    let mut counter = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem}_{secs}_{counter}.{ext}"));
        counter += 1;
    }
    candidate
}

fn join_path(folder: &str, name: &str) -> String {
    let folder = folder.trim_end_matches('/');
    if folder.is_empty() {
        format!("/{name}")
    } else {
        format!("{folder}/{name}")
    }
}

fn parent_entry(path: &str) -> RemoteEntry {
    RemoteEntry {
        name: "..".to_string(),
        path: parent_path(path),
        is_dir: true,
        size: None,
        modified: None,
        etag: None,
    }
}

fn human_size(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    use std::io::Write;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let len = s.len();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn text_width(value: &str) -> usize {
    value
        .chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(1))
        .sum()
}

fn search_byte_at(value: &str, char_idx: usize) -> usize {
    value
        .char_indices()
        .nth(char_idx)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

/// Query columns available in the status bar after " Find: " and the match
/// counter. The query never scrolls horizontally.
fn search_query_capacity(width: u16) -> usize {
    (width as usize)
        .saturating_sub(text_width(" Find: ") + 10)
        .max(1)
}

fn ai_issue_line(issue: &ai::Issue, index: usize) -> String {
    let replacement = issue.replacement.as_deref().unwrap_or("(no auto-fix)");
    format!(
        "{:>2}. {:<10} {}  →  {}",
        index + 1,
        issue.kind,
        issue.original,
        replacement
    )
}

/// Show a horizontal window into `text`, using `…` at the clipped edges.
fn scroll_text(text: &str, skip: usize, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut start = chars.len();
    let mut col = 0usize;
    for (index, ch) in chars.iter().enumerate() {
        let width = UnicodeWidthChar::width(*ch).unwrap_or(0);
        if col + width > skip {
            start = index;
            break;
        }
        col += width;
    }

    let mut out = String::new();
    let mut used = 0usize;
    if skip > 0 {
        out.push('…');
        used = 1;
    }
    let mut index = start;
    while index < chars.len() {
        let width = UnicodeWidthChar::width(chars[index]).unwrap_or(0);
        let reserve = if index + 1 < chars.len() { 1 } else { 0 };
        if used + width + reserve > max_width {
            break;
        }
        out.push(chars[index]);
        used += width;
        index += 1;
    }
    if index < chars.len() && used < max_width {
        out.push('…');
    }
    out
}

fn menu_dropdown_width(menu: &MenuDef) -> u16 {
    let mut max = 0;
    for entry in menu.entries {
        if let MenuEntry::Item {
            label, shortcut, ..
        } = entry
        {
            let mut width = text_width(label);
            if !shortcut.is_empty() {
                width += 2 + text_width(shortcut);
            }
            max = max.max(width);
        }
    }
    (max as u16 + 4).max(14)
}

fn next_enabled_entry(
    entries: &[MenuEntry],
    current: usize,
    delta: isize,
    enabled: impl Fn(MenuAction) -> bool,
) -> usize {
    let count = entries.len();
    if count == 0 {
        return 0;
    }
    let mut idx = current.min(count - 1) as isize;
    for _ in 0..count {
        idx = (idx + delta).rem_euclid(count as isize);
        if let MenuEntry::Item { action, .. } = &entries[idx as usize]
            && enabled(*action)
        {
            return idx as usize;
        }
    }
    current.min(count - 1)
}

#[allow(dead_code)]
fn _unused(_: &Book, _: &Buffer) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(action: MenuAction) -> MenuEntry {
        MenuEntry::Item {
            label: "x",
            shortcut: "",
            action,
        }
    }

    #[test]
    fn menu_navigation_skips_separators_and_disabled_items() {
        let entries = [
            item(MenuAction::Undo),
            MenuEntry::Separator,
            item(MenuAction::Redo),
            MenuEntry::Separator,
            item(MenuAction::Bold),
        ];
        assert_eq!(
            next_enabled_entry(&entries, 0, 1, |action| action != MenuAction::Redo),
            4
        );
        assert_eq!(
            next_enabled_entry(&entries, 4, -1, |action| action != MenuAction::Redo),
            0
        );
    }

    #[test]
    fn menu_navigation_wraps_at_the_ends() {
        let entries = [item(MenuAction::Undo), item(MenuAction::Redo)];
        assert_eq!(next_enabled_entry(&entries, 1, 1, |_| true), 0);
        assert_eq!(next_enabled_entry(&entries, 0, -1, |_| true), 1);
    }

    #[test]
    fn menu_navigation_stays_when_nothing_is_enabled() {
        let entries = [item(MenuAction::Undo), item(MenuAction::Redo)];
        assert_eq!(next_enabled_entry(&entries, 1, 1, |_| false), 1);
    }

    #[test]
    fn scroll_text_clips_and_marks_edges() {
        assert_eq!(scroll_text("0123456789", 0, 5), "0123…");
        assert_eq!(scroll_text("0123456789", 5, 5), "…567…");
        assert_eq!(scroll_text("0123456789", 5, 10), "…56789");
        assert_eq!(scroll_text("abc", 0, 10), "abc");
    }

    #[test]
    fn every_menu_has_items_and_no_disabled_gaps() {
        assert_eq!(MENUS.len(), 6);
        for menu in MENUS {
            assert!(!menu.entries.is_empty(), "{} is empty", menu.label);
            assert!(menu_dropdown_width(menu) >= 14);
        }
    }

    #[test]
    fn notes_paths_append_the_sidecar_suffix() {
        assert_eq!(
            notes_local_path(Path::new("/tmp/book.odt")),
            PathBuf::from("/tmp/book.odt.notes.odt")
        );
        assert_eq!(
            notes_remote_path("/Docs/book.odt"),
            "/Docs/book.odt.notes.odt"
        );
        assert_eq!(notes_title("/Docs/book.odt.notes.odt"), "book.odt.notes");
        assert_eq!(
            notes_remote_path("/Docs/book.odt.notes.odt"),
            "/Docs/book.odt.notes.odt"
        );
    }

    #[test]
    fn notes_document_mirrors_chapters() {
        let mut book = Book::new("My Book");
        book.chapters[0].title = "One".to_string();
        book.chapters.push(Chapter::new(2, "Two"));
        let notes = notes_document_from(&book).unwrap();
        assert_eq!(notes.book.title, "My Book — Notes");
        assert_eq!(notes.book.chapters.len(), 2);
        assert_eq!(notes.book.chapters[0].title, "One");
        assert_eq!(notes.book.chapters[1].title, "Two");
        assert!(
            notes
                .book
                .chapters
                .iter()
                .all(|chapter| chapter.paragraphs.len() == 1
                    && chapter.paragraphs[0].chars.is_empty())
        );
    }

    #[test]
    fn search_query_space_is_derived_from_the_status_width() {
        assert_eq!(search_query_capacity(100), 83);
        assert_eq!(search_query_capacity(10), 1);
        assert_eq!(search_byte_at("abc", 0), 0);
        assert_eq!(search_byte_at("abc", 2), 2);
        assert_eq!(search_byte_at("abc", 9), 3);
    }

    #[test]
    fn config_panel_never_shows_the_raw_key() {
        let cfg = ai::AiConfig {
            api_key: "sk-verysecretvalue123".to_string(),
            ..ai::AiConfig::default()
        };
        let mut panel = ConfigPanel::new(cfg, ThemeName::Monokai);
        let shown = panel.display_key();
        assert!(!shown.contains("verysecretvalue"));
        assert!(shown.starts_with("sk-ve"));
        assert!(shown.ends_with("e123"));

        panel.selected = CONFIG_FIELDS
            .iter()
            .position(|field| *field == ConfigField::ApiKey)
            .unwrap();
        panel.start_edit();
        panel.editing.as_mut().unwrap().insert('x');
        panel.editing.as_mut().unwrap().insert('y');
        assert_eq!(panel.display_key(), "••");
    }

    #[test]
    fn downloaded_copy_avoids_name_clashes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("existing");
        let name = "My.Book.odt";
        let first = write_downloaded_copy(&dir, name, b"one", 1_700_000_000).unwrap();
        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("My.Book.odt")
        );
        assert_eq!(fs::read(&first).unwrap(), b"one");
        let second = write_downloaded_copy(&dir, name, b"two", 1_700_000_000).unwrap();
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("My.Book_1700000000.odt")
        );
        assert_eq!(fs::read(&second).unwrap(), b"two");
        let third = write_downloaded_copy(&dir, name, b"three", 1_700_000_000).unwrap();
        assert_eq!(
            third.file_name().and_then(|n| n.to_str()),
            Some("My.Book_1700000000_2.odt")
        );
        assert_eq!(fs::read(&third).unwrap(), b"three");
    }
}

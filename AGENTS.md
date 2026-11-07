# AGENTS.md

Project: **scribe** — a focused TUI editor for long-form prose stored as ODT in Nextcloud.

Read this before changing behavior. This file records the design decisions that are easy to break.

## First commands

```bash
cd ~/projects/scribe
cargo test
cargo fmt
cargo build --release
scribe help
```

Install locally:

```bash
cargo install --path . --force --locked
```

## Project map

```text
src/main.rs       CLI, login wizard, terminal setup
src/app.rs        TUI state, event loop, rendering, autosave, conflict handling
src/editor.rs     document cursor, editing, wrapping, pagination, undo, smart quotes
src/ai.rs         AI writing check: config, OpenAI-compatible client, prompt
build.rs          embeds the git tag / version into the binary
src/model.rs      Book / Chapter / Para / styled characters, counts
src/odt/mod.rs    ODT read/write, style resolution, unsupported-content refusal
src/nextcloud.rs  WebDAV client and Nextcloud Login Flow v2
src/lock.rs       sidecar file locks, kick protocol, lock watcher
src/backups.rs    backup ring under ~/.cache/scribe/backups
src/config.rs     credentials/config paths
assets/template.odt  embedded new-document template
```

Hidden debug commands:

```bash
scribe inspect book.odt
scribe roundtrip in.odt out.odt
```

## Hard invariants

### ODT format

- **ODT only.** Do not add DOCX support or conversion.
- Existing ODT files are edited **in place**: preserve `styles.xml`, `settings.xml`, `meta.xml`, `META-INF/manifest.xml`, page layout, fonts, and original automatic styles.
- Preserve the original ODF version. `content.xml` must not be written as `office:version="1.3"` into a 1.2 package. `OdtDocument::odf_version` controls this.
- Unsupported body blocks are refused: tables, images/frames, lists, sections, footnotes, comments, tracked changes, hyperlinks, fields, drawings.
- Unsupported top-level elements are refused too. Only `office:font-face-decls`, `office:automatic-styles`, and `office:body` are allowed.
- The ODT zip must keep `mimetype` as the first, uncompressed entry.
- New documents use `assets/template.odt`: Letter, 1-inch margins, Title 28pt Arial, H2 16pt, Normal 12pt Book Antiqua, justified, 1.27cm first-line indent.
- First chapter uses `MydocsFirstChapter` with `fo:break-before="auto"`. Chapters 2+ use `MydocsChapter` with `fo:break-before="page"`.
- Automatic styles written by us are named `MydocsTitle`, `MydocsFirstChapter`, `MydocsChapter`, `MydocsBody`, `MydocsBold`, `MydocsItalic`, `MydocsBoldItalic`. On read, `strip_mydocs_styles` removes old copies before re-adding them.
- `detect_refs` must resolve `Mydocs*` styles back to their base parent styles. Never allow a `Mydocs*` style to become its own parent; `valid_ref` guards against this.
- Paragraph style detection is heuristic: `text:h` level, outline level, style names, then font-size fallback (>=20pt H1, >=14pt H2). The sample ODT has no outline levels, so font-size fallback matters.
- Save atomically. Local files use temp+persist. Cache/backup writes are atomic too.

### Editor / TUI

- Mark/selection only. `Ctrl+Space` sets the mark at the cursor; moving extends the live selection; `Ctrl+Space` again finalizes it. `Esc` clears it. After finalizing, a cursor move clears the selection and moves normally. `Delete`/`Backspace` removes the selection, typing replaces it, and bold/italic apply to it. Selection is limited to the current chapter (switching chapters clears it). No cut/copy/search. Paste works via bracketed paste, and pasting replaces an active selection.
- Text width: `min(120, terminal_width - 4)`. Page height: `terminal_height - 2` (one row menu bar + one row status).
- The top menu bar is always visible. `F9` focuses it and opens the first menu; `←`/`→` switch menus, `↑`/`↓` move between enabled items, `Enter` runs, and `Esc`/`F9` closes. Menu actions must call the same `action_*` methods as the shortcuts; never duplicate command logic in the menu path.
- Page background is `PAGE_BG` with dim `│` rules on both sides. Do not reduce the document text width for the rules; they sit outside the text area.
- Cursor is a **drawn ANSI white block** with black foreground. The native terminal cursor is hidden in `run_tui`. Do **not** call `frame.set_cursor_position` for the editor/prompt cursor again: that causes double inversion on block-cursor terminals and makes the cursor look like only a character color change.
- A marked range paints `SELECTION_BG` behind its characters and shows `SEL n` in the status bar. The cursor block still draws over the endpoint.
- Cursor must always stay in the viewport after moves, edits, page flips, chapter jumps, and resize.
- `Ctrl+L` requests a full terminal clear + redraw. External processes writing to the TTY can corrupt ratatui’s diff and persist garbage; `Ctrl+L` is the recovery path.
- Browser launches during login must detach stdio to `/dev/null` and not wait. Firefox/Glean logs inheriting the TTY will corrupt the TUI.
- Help is `Alt+H`; `F1` is kept only as a fallback. Do **not** bind `Ctrl+H` — terminals send it as Backspace. Word deletion is `Ctrl+W`, not `Ctrl+Backspace`. `F9` is reserved for the menu bar. `F8` is reserved for the AI writing check. `Alt+A` opens About (also `Help → About scribe`).
- The About overlay shows the version, `CARGO_PKG_AUTHORS` (name + email), `CARGO_PKG_REPOSITORY`, and `CARGO_PKG_LICENSE`. The version comes from `SCRIBE_VERSION`, set by `build.rs` from `git describe --tags --always --dirty` (or the `SCRIBE_VERSION` env var), with `CARGO_PKG_VERSION` as the fallback when git metadata is unavailable. Do not hardcode it. Keep personal infrastructure details (Nextcloud domain, SSH hosts, IPs) out of it.
- Nextcloud is optional. `scribe open FILE.odt` and `scribe FILE.odt` must never require credentials. With no saved credentials, the no-argument first-run menu offers Nextcloud login, a local `.odt`, and quit; do not force Nextcloud login.
- Italic is `Alt+I`; most terminals send `Ctrl+I` as Tab. Bold is `Ctrl+B` or `Alt+B`.
- Smart quotes apply only to **typed** characters via `insert_typed_char`. `insert_text` (paste) must not convert quotes.
- Auto-capitalization also applies only to **typed** characters. A lowercase letter is uppercased at a paragraph start or after `.`, `!`, or `?` plus whitespace, looking through opening quotes/brackets; pasted text is never rewritten. Turkish dotted `i` uppercases to `İ`, not ASCII `I`.
- Consecutive typing is grouped into word-sized undo steps: do not restore a snapshot per keystroke. `insert_char` starts a new step after a cursor move, a selection replacement, or at the start of a new word; newline and paste stay separate steps.
  - Opening `"` → `“`, opening `'` → `‘` at start/after whitespace/after `( [ { — – / \ |`.
  - Everything else closes: `”` / `’`, including after comma, period, `!`, `?`, `)`, and inside words (`Anette’nin`).
  - Chapter/book title prompts use smart quotes; file-name prompts do not.
- Bold/italic toggles the word under the cursor, or sets pending style for the next text. `B*` / `I*` in the status bar means pending.
- Status bar totals include the book title, chapter titles, and body. Page counts are **terminal pages**, not printed ODT pages.
- Keep the hardcoded dark palette; do not add a theme system unless asked.

### AI writing check

- `F8` (and `Tools → Check writing (AI)…`) checks the **current chapter only**. Do not check the whole book or silently change scope.
- **The AI never edits automatically.** Suggestions are inert until the user presses Enter/A on one. That path must call `Editor::replace_span`, which verifies the exact `original` text, replaces only that span, preserves surrounding text and bold/italic, and pushes a normal undo snapshot.
- The system prompt in `src/ai.rs` is intentionally conservative. Do not weaken it. It must not rewrite or "improve" prose, change style/tone/voice/rhythm/word choice, suggest synonyms, flag deliberate fragments, dialogue, dialect, slang, names, or artistic capitalization, or translate. Awkward-but-valid sentences are not reported.
- Turkish and English are supported. `language = "auto"` lets the model detect the text; `tr`/`en` pin it.
- Suggestions are anchored by an exact, **unique** substring match inside the stated paragraph. Never guess offsets. Ambiguous or missing matches are dropped; after an apply, remaining suggestions are re-anchored with `ai::reanchor`.
- If the chapter text hash changes while a request is in flight, or the chapter is switched, discard the result. Never apply suggestions to text they were not computed from.
- The request runs on a worker thread and reports through an `mpsc` `AiEvent` channel, like `NetEvent`. Do not block the event loop or add an async runtime.
- Long chapters are split at paragraph boundaries (currently ~12k chars) and merged. Paragraph indices are global, so do not renumber per chunk.
- Config is `~/.config/scribe/ai.toml`, mode `0600`, separate from Nextcloud credentials. `SCRIBE_AI_API_KEY` / `SCRIBE_AI_BASE_URL` / `SCRIBE_AI_MODEL` override it at runtime. Never commit or log the key or chapter text.
- The panel is a bottom drawer above the status bar. The editor viewport must shrink by the panel height so pagination and cursor stay correct. Do not overlap the status bar.
- The panel is modal while open. `Esc`/`F8` closes it, `R` re-checks, `D`/`X`/`Delete` dismisses, `Enter`/`A` applies. `Ctrl+Z`/`Ctrl+Y` undo/redo and close the panel.
- Dismissing removes a suggestion from the current result set only; a fresh check may show it again.
- AI failures (missing config, HTTP error, malformed JSON) are non-fatal and must never affect saving, locking, or the book model.

### Nextcloud / networking

- Use **ureq 2.12**, not ureq 3. ureq 3 rejects WebDAV `PROPFIND` with `MethodVersionMismatch`; this was the reason for the downgrade.
- WebDAV path: `/remote.php/dav/files/<user>/...`.
- Login uses Nextcloud Login Flow v2:
  - `POST <server>/index.php/login/v2`
  - open the returned `login` URL
  - poll `poll.endpoint` with `token`
- Save uses ETag `If-Match`, create uses `If-None-Match: *`. A 412 opens the conflict modal.
- Autosave: local cache after 2s idle, remote PUT after 8s idle. Flush on chapter switch, `Ctrl+S`, quit, and before switching files.
- `net_busy` prevents concurrent remote saves. `flush_current` does a synchronous PUT before opening the file picker; do not remove this without a replacement data-safety strategy.
- `pending_open` ignores stale GET responses. `force_remote_reload` makes conflict “Reload” bypass dirty cache.
- Remote cache is keyed by `server|login|remote_path` using a stable FNV hash. Backup identity uses that same remote key, or `local|absolute_path` for local files. Do not switch to `DefaultHasher` for cache paths.
- `File → Download a copy` is menu-only (no keybinding) and enabled only when a cloud file is open. It serializes the current buffer (unsaved edits included) into `./existing/` relative to the launch directory, creating the directory. If the name exists, use `<stem>_<unix-seconds>.<ext>` (then `_2`, `_3` for same-second collisions).
- Cache metadata has a `dirty` flag. On open:
  - dirty cache + same ETag → restore cache and schedule upload
  - dirty cache + different ETag → conflict modal
  - clean cache → use remote
- Credentials: `~/.config/scribe/credentials.toml`, mode `0600`. Never commit it. The default file is intentionally absent.
- Legacy migration: the first `load()` copies `~/.config/mydocs-tui/credentials.toml` when the new file is missing; `ensure_dirs()` renames `~/.cache/mydocs-tui` to `~/.cache/scribe` when the new cache is absent. Keep this one-time bridge unless the user explicitly wants it removed.

### Nextcloud file locking

- Remote files are protected by a visible sidecar: `/Books/Novel.odt` → `/Books/Novel.odt.lock`.
- Do not prefix the lock with a dot. Some reverse-proxy configurations reject hidden paths with 403 before Nextcloud sees them; a visible `.lock` sidecar is allowed. The picker still hides it because only `.odt` files are listed.
- Acquire with a conditional `PUT If-None-Match: *`. Never steal an existing lock on a normal open.
- Lock JSON contains `owner`, `host`, `pid`, `user`, `file`, `acquired_at`, and `state` (`active` / `kick_requested`). A kick request also has `kicked_by`.
- Only one `scribe` process may own a lock. Local `scribe open file.odt` opens are intentionally not locked.
- The owner watches the lock every `lock::OWNER_POLL` (3s). If it sees `kick_requested` for itself, it must save local cache, attempt a final remote save unless conflicted, delete the lock, and exit.
- The new opener waits `lock::KICK_WAIT` (10s) for release. On timeout it shows the force prompt; force-take replaces the lock. After force-take, the old owner must not remote-save; it writes cache-only and exits.
- On a clean quit or switching files, release the lock with `DELETE If-Match`. A crash intentionally leaves the sidecar for the next opener to kick.
- The old instance exits with the phrase `Enforced exit by the new editor` printed by `main` after `ratatui::restore()`. Keep that message visible and do not swallow it.
- Lock failures must never silently bypass locking. A transient watch error is ignored; a definite owner change, kick request, or missing lock triggers exit.
- A sidecar only coordinates `scribe`. Nextcloud web, ONLYOFFICE, and desktop sync ignore it; ETag conflicts remain the fallback.
- When opening a new file, acquire its lock before fetching the ODT. Do not release the old lock until the new file has installed successfully; `previous_lock` exists for that.

### Backups

- Path: `~/.cache/scribe/backups/<safe-book-name>-<hash>/<epoch-ms>-<counter>.odt`.
- Keep the last **100 distinct saved versions per file**.
- Back up after successful local writes and after successful remote PUTs.
- Deduplicate against the newest backup’s bytes.
- Files are `0600`; directories `0700`.
- There is intentionally **no restore code**. The user copies a backup manually.
- Backup failures should surface as a toast where possible; never let them fail the save.

## Validation checklist

Run after changes:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets
cargo build --release
```

ODT changes must also be round-tripped:

```bash
scribe roundtrip /path/to/sample.odt /tmp/roundtrip.odt
soffice --headless --convert-to pdf --outdir /tmp /tmp/roundtrip.odt
pdfinfo /tmp/roundtrip.pdf
pdftotext -layout /tmp/roundtrip.pdf - | head
```

Check that chapter headings after the first start on new pages and that the first chapter still shares page 1 with the title.

UI changes: smoke-test in tmux:

```bash
tmux new-session -d -s smoke -x 120 -y 40 \
  'TERM=xterm-256color scribe open /tmp/test.odt'
tmux send-keys -t smoke F9
tmux capture-pane -p -t smoke
tmux send-keys -t smoke Right Down Enter
tmux send-keys -t smoke Escape
tmux capture-pane -p -t smoke
# mark 2 chars and delete them; then undo
tmux send-keys -t smoke C-Space Right Right C-Space
tmux capture-pane -p -t smoke
tmux send-keys -t smoke BSpace C-z
tmux send-keys -t smoke C-q
```

Check that the menu bar occupies the first row, the dropdown overlaps the editor without moving the page borders, disabled items are dim, `F9`/`Esc` close the menu, and the cursor returns after closing. For marking, check the `SELECTION_BG` highlight and `SEL n` status, that `Backspace` removes the range, `Esc` clears it, and undo restores it. Use a temporary `XDG_CACHE_HOME` when testing backups/cache.

## Current limitations / do not add unless asked

- No DOCX support.
- No manual page break inside a chapter.
- No search/replace, cut/copy, multi-cursor, or theme system. Selection exists only as a mark range; there is no clipboard integration.
- No automatic restore command for backups.
- Support is strict: ODT structure/unsupported elements are refused rather than guessed.
- Formatting is normalized to H1/H2/body plus bold/italic. Other inline formatting is dropped; unsupported blocks are refused.

## Safe change workflow

1. Read the relevant module and preserve the invariants above.
2. Make the smallest change.
3. `cargo fmt`, `cargo test`, `cargo clippy --all-targets`.
4. For ODT changes, do the roundtrip + LibreOffice check.
5. For TUI changes, do a tmux smoke test and check `Ctrl+L`, cursor visibility, page borders, and status totals.
6. If the user asks to publish, install locally with `cargo install --path . --force --locked`.

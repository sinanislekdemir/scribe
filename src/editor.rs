use unicode_width::UnicodeWidthChar;

use crate::model::{Align, Book, Para, ParaKind, StyleAttr, StyledChar, TextStyle};
use crate::odt::OdtDocument;

pub const INDENT: usize = 4;
pub const MAX_TEXT_WIDTH: usize = 120;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub chapter: usize,
    pub para: usize,
    pub offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Heading,
    Blank,
    Para,
}

#[derive(Clone, Debug)]
pub struct VisualLine {
    pub kind: LineKind,
    pub para: usize,
    pub start: usize,
    pub end: usize,
    pub indent: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorPos {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkResult {
    Started,
    Finished(usize),
    Restarted,
    Cancelled,
}

#[derive(Clone)]
struct Snapshot {
    book: Book,
    cursor: Cursor,
}

pub struct Editor {
    pub doc: OdtDocument,
    pub cursor: Cursor,
    pub pending_style: Option<TextStyle>,
    pub dirty: bool,
    pub revision: u64,
    mark: Option<Cursor>,
    mark_finalized: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Cursor position after the last typed character while a typing undo
    /// group is open. `None` means the next typed character starts a new
    /// snapshot.
    typing_undo: Option<Cursor>,
    text_width: usize,
    page_height: usize,
    lines: Vec<VisualLine>,
    chapter_pages: Vec<usize>,
    pub page: usize,
    view_start: usize,
    pub total_pages: usize,
    pub total_words: usize,
    pub total_chars: usize,
}

impl Editor {
    pub fn new(doc: OdtDocument, term_width: u16, term_height: u16) -> Self {
        let mut ed = Self {
            doc,
            cursor: Cursor::default(),
            pending_style: None,
            dirty: false,
            revision: 0,
            mark: None,
            mark_finalized: false,
            undo: Vec::new(),
            redo: Vec::new(),
            typing_undo: None,
            text_width: 1,
            page_height: 1,
            lines: Vec::new(),
            chapter_pages: Vec::new(),
            page: 0,
            view_start: 0,
            total_pages: 0,
            total_words: 0,
            total_chars: 0,
        };
        ed.set_viewport(term_width, term_height);
        ed.recompute_stats();
        ed
    }

    pub fn text_width(&self) -> usize {
        self.text_width
    }

    pub fn page_height(&self) -> usize {
        self.page_height
    }

    pub fn set_viewport(&mut self, term_width: u16, term_height: u16) {
        let w = term_width as usize;
        let text = w.saturating_sub(4).clamp(1, MAX_TEXT_WIDTH);
        let ph = (term_height as usize).saturating_sub(1).max(1);
        if text == self.text_width && ph == self.page_height {
            return;
        }
        self.text_width = text;
        self.page_height = ph;
        self.rebuild_all_pages();
        self.sync_page();
    }

    pub fn rebuild_all_pages(&mut self) {
        self.chapter_pages.clear();
        for i in 0..self.doc.book.chapters.len() {
            let lines = self.wrap_chapter(i);
            let pages = lines.len().div_ceil(self.page_height).max(1);
            self.chapter_pages.push(pages);
        }
        self.total_pages = self.chapter_pages.iter().sum();
        self.rebuild_lines();
        self.sync_page();
    }

    pub fn rebuild_current_pages(&mut self) {
        if self.doc.book.chapters.is_empty() {
            return;
        }
        self.rebuild_lines();
        let pages = self.lines.len().div_ceil(self.page_height).max(1);
        if self.cursor.chapter < self.chapter_pages.len() {
            self.chapter_pages[self.cursor.chapter] = pages;
            self.total_pages = self.chapter_pages.iter().sum();
        }
        self.sync_page();
    }

    fn rebuild_lines(&mut self) {
        self.lines = self.wrap_chapter(self.cursor.chapter);
        self.view_start = self.view_start.min(self.lines.len().saturating_sub(1));
        if self.page >= self.chapter_pages() {
            self.page = self.chapter_pages().saturating_sub(1);
        }
    }

    fn chapter_pages(&self) -> usize {
        self.chapter_pages
            .get(self.cursor.chapter)
            .copied()
            .unwrap_or(1)
    }

    pub fn lines(&self) -> &[VisualLine] {
        &self.lines
    }

    pub fn page_start(&self) -> usize {
        self.view_start
    }

    pub fn page_lines(&self) -> &[VisualLine] {
        let start = self.page_start().min(self.lines.len());
        let end = (start + self.page_height).min(self.lines.len());
        &self.lines[start..end]
    }

    fn wrap_chapter(&self, chapter: usize) -> Vec<VisualLine> {
        let mut lines = Vec::new();
        let heading = self.doc.book.heading_text(chapter);
        let chars: Vec<StyledChar> = heading.chars().map(StyledChar::plain).collect();
        for (start, end) in wrap_chars(&chars, self.text_width, self.text_width) {
            lines.push(VisualLine {
                kind: LineKind::Heading,
                para: usize::MAX,
                start,
                end,
                indent: 0,
            });
        }
        lines.push(VisualLine {
            kind: LineKind::Blank,
            para: usize::MAX,
            start: 0,
            end: 0,
            indent: 0,
        });
        if let Some(ch) = self.doc.book.chapters.get(chapter) {
            for (pi, para) in ch.paragraphs.iter().enumerate() {
                let indented = para.kind == ParaKind::Body
                    && matches!(para.align, Align::Justify | Align::Left);
                let first_w = if indented {
                    self.text_width.saturating_sub(INDENT).max(1)
                } else {
                    self.text_width
                };
                let ranges = wrap_chars(&para.chars, first_w, self.text_width);
                let line_count = ranges.len();
                for (li, (start, end)) in ranges.into_iter().enumerate() {
                    let width = visible_width(&para.chars[start..end]);
                    let indent = match para.align {
                        Align::Center => self.text_width.saturating_sub(width) / 2,
                        Align::Right => self.text_width.saturating_sub(width),
                        Align::Left | Align::Justify => {
                            if indented && li == 0 {
                                INDENT
                            } else {
                                0
                            }
                        }
                    };
                    lines.push(VisualLine {
                        kind: LineKind::Para,
                        para: pi,
                        start,
                        end,
                        indent,
                    });
                }
                if para.kind == ParaKind::Subheading && line_count > 0 {
                    lines.push(VisualLine {
                        kind: LineKind::Blank,
                        para: usize::MAX,
                        start: 0,
                        end: 0,
                        indent: 0,
                    });
                }
            }
        }
        lines
    }

    fn para(&self) -> &Para {
        &self.doc.book.chapters[self.cursor.chapter].paragraphs[self.cursor.para]
    }

    fn para_mut(&mut self) -> &mut Para {
        &mut self.doc.book.chapters[self.cursor.chapter].paragraphs[self.cursor.para]
    }

    pub fn current_chapter(&self) -> usize {
        self.cursor.chapter
    }

    /// Current text of a chapter, one string per paragraph.
    pub fn chapter_texts(&self, chapter: usize) -> Vec<String> {
        self.doc
            .book
            .chapters
            .get(chapter)
            .map(|c| c.paragraphs.iter().map(Para::text).collect())
            .unwrap_or_default()
    }

    /// Move the cursor to a paragraph/offset without editing, so a selected
    /// AI suggestion can be shown in the viewport.
    pub fn reveal(&mut self, para: usize, offset: usize) {
        let chapter = self.cursor.chapter;
        let Some(ch) = self.doc.book.chapters.get(chapter) else {
            return;
        };
        let Some(target) = ch.paragraphs.get(para) else {
            return;
        };
        self.cursor.para = para;
        self.cursor.offset = offset.min(target.len());
        self.clear_selection();
        self.sync_page();
    }

    /// Replace an exact, already-resolved span. Returns false if the text no
    /// longer matches `expected`, which keeps stale AI suggestions from being
    /// applied. The replacement inherits the style at the span start.
    pub fn replace_span(
        &mut self,
        para: usize,
        start: usize,
        end: usize,
        expected: &str,
        replacement: &str,
    ) -> bool {
        let chapter = self.cursor.chapter;
        let actual: String = {
            let Some(ch) = self.doc.book.chapters.get(chapter) else {
                return false;
            };
            let Some(target) = ch.paragraphs.get(para) else {
                return false;
            };
            if start > end || end > target.len() {
                return false;
            }
            target.chars[start..end].iter().map(|c| c.ch).collect()
        };
        if actual != expected {
            return false;
        }
        let style = self.doc.book.chapters[chapter]
            .paragraphs
            .get(para)
            .map(|p| p.style_at(start))
            .unwrap_or_default();

        self.push_undo();
        self.clear_pending();
        self.clear_selection();
        {
            let chapter = &mut self.doc.book.chapters[chapter];
            let Some(target) = chapter.paragraphs.get_mut(para) else {
                return false;
            };
            target.chars.splice(
                start..end,
                replacement
                    .chars()
                    .map(|ch| StyledChar::with_style(ch, style)),
            );
        }
        self.cursor = Cursor {
            chapter,
            para,
            offset: start + replacement.chars().count(),
        };
        self.mark_dirty();
        self.rebuild_current_pages();
        self.sync_page();
        true
    }

    pub fn current_page(&self) -> usize {
        self.page
    }

    pub fn current_chapter_pages(&self) -> usize {
        self.chapter_pages()
    }

    pub fn cursor_line(&self) -> usize {
        let p = self.cursor.para;
        let off = self.cursor.offset;
        let mut fallback = 0;
        for (idx, line) in self.lines.iter().enumerate() {
            if line.kind != LineKind::Para || line.para != p {
                continue;
            }
            fallback = idx;
            if off >= line.start && off <= line.end {
                if off == line.end
                    && idx + 1 < self.lines.len()
                    && self.lines[idx + 1].kind == LineKind::Para
                    && self.lines[idx + 1].para == p
                    && self.lines[idx + 1].start == off
                    && off < self.para().len()
                {
                    continue;
                }
                return idx;
            }
        }
        fallback
    }

    pub fn cursor_pos(&self) -> CursorPos {
        let mut line_idx = self.cursor_line();
        let mut line = self.lines[line_idx].clone();
        let mut col = line.indent
            + self
                .para()
                .chars
                .get(line.start..self.cursor.offset.min(line.end))
                .unwrap_or(&[])
                .iter()
                .map(|c| c.ch.width().unwrap_or(0))
                .sum::<usize>();
        if col >= self.text_width {
            if line_idx + 1 < self.lines.len() {
                line_idx += 1;
                line = self.lines[line_idx].clone();
                col = line.indent;
            } else {
                col = self.text_width.saturating_sub(1);
            }
        }
        CursorPos {
            line: line_idx,
            col,
        }
    }

    pub fn sync_page(&mut self) {
        if self.lines.is_empty() {
            self.page = 0;
            self.view_start = 0;
            return;
        }
        let line = self.cursor_line();
        self.view_start = scrolled_view_start(line, self.view_start, self.page_height);
        let max_page = self.chapter_pages().saturating_sub(1);
        let wanted = line / self.page_height.max(1);
        self.page = wanted.min(max_page);
    }

    fn scroll_step(&self) -> usize {
        let height = self.page_height.max(1);
        height - height / 4
    }

    pub fn chapter_word_count(&self, i: usize) -> usize {
        self.doc
            .book
            .chapters
            .get(i)
            .map(|c| c.word_count())
            .unwrap_or(0)
    }

    pub fn recompute_stats(&mut self) {
        self.total_words = self.doc.book.total_words();
        self.total_chars = self.doc.book.total_chars();
    }

    fn para_len_at(&self, para: usize) -> usize {
        self.doc.book.chapters[self.cursor.chapter]
            .paragraphs
            .get(para)
            .map(Para::len)
            .unwrap_or(0)
    }

    pub fn selection_bounds(&self) -> Option<(Cursor, Cursor)> {
        let anchor = self.mark?;
        if anchor.chapter != self.cursor.chapter {
            return None;
        }
        let a = (anchor.para, anchor.offset);
        let b = (self.cursor.para, self.cursor.offset);
        if a == b {
            return None;
        }
        if a < b {
            Some((anchor, self.cursor))
        } else {
            Some((self.cursor, anchor))
        }
    }

    pub fn has_selection(&self) -> bool {
        self.selection_bounds().is_some()
    }

    pub fn selection_len(&self) -> usize {
        let Some((start, end)) = self.selection_bounds() else {
            return 0;
        };
        let count = |para: usize, from: usize, to: usize| {
            self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .get(para)
                .map(|p| {
                    p.chars[from.min(p.len())..to.min(p.len())]
                        .iter()
                        .filter(|c| c.ch != '\n')
                        .count()
                })
                .unwrap_or(0)
        };
        if start.para == end.para {
            return count(start.para, start.offset, end.offset);
        }
        let mut len = count(start.para, start.offset, self.para_len_at(start.para));
        for para in start.para + 1..end.para {
            len += count(para, 0, self.para_len_at(para));
        }
        len + count(end.para, 0, end.offset)
    }

    pub fn selection_word_count(&self) -> usize {
        let Some((start, end)) = self.selection_bounds() else {
            return 0;
        };
        let mut words = 0;
        for para in start.para..=end.para {
            let from = if para == start.para { start.offset } else { 0 };
            let to = if para == end.para {
                end.offset
            } else {
                self.para_len_at(para)
            };
            let Some(p) = self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .get(para)
            else {
                continue;
            };
            let mut in_word = false;
            for c in &p.chars[from.min(p.len())..to.min(p.len())] {
                if c.ch.is_whitespace() {
                    in_word = false;
                } else if !in_word {
                    words += 1;
                    in_word = true;
                }
            }
        }
        words
    }

    pub fn is_selected(&self, para: usize, offset: usize) -> bool {
        let Some((start, end)) = self.selection_bounds() else {
            return false;
        };
        if para < start.para || para > end.para {
            return false;
        }
        if para == start.para && offset < start.offset {
            return false;
        }
        if para == end.para && offset >= end.offset {
            return false;
        }
        true
    }

    pub fn toggle_mark(&mut self) -> MarkResult {
        match self.mark {
            None => {
                self.mark = Some(self.cursor);
                self.mark_finalized = false;
                MarkResult::Started
            }
            Some(_) if !self.mark_finalized => {
                if self.selection_len() == 0 {
                    self.clear_selection();
                    MarkResult::Cancelled
                } else {
                    self.mark_finalized = true;
                    MarkResult::Finished(self.selection_len())
                }
            }
            Some(_) => {
                self.mark = Some(self.cursor);
                self.mark_finalized = false;
                MarkResult::Restarted
            }
        }
    }

    pub fn clear_selection(&mut self) -> bool {
        let had_mark = self.mark.is_some();
        self.mark = None;
        self.mark_finalized = false;
        had_mark
    }

    fn begin_move(&mut self) {
        self.clear_pending();
        if self.mark_finalized {
            self.clear_selection();
        }
    }

    fn remove_range(&mut self, start: Cursor, end: Cursor) {
        if start.chapter != end.chapter || start.para > end.para {
            return;
        }
        let chapter = &mut self.doc.book.chapters[start.chapter];
        if start.para == end.para {
            if let Some(para) = chapter.paragraphs.get_mut(start.para) {
                para.remove_range(start.offset, end.offset);
            }
        } else if let (Some(_), Some(_)) = (
            chapter.paragraphs.get(start.para),
            chapter.paragraphs.get(end.para),
        ) {
            let mut end_para = chapter.paragraphs.remove(end.para);
            let suffix = end_para.split_off(end.offset);
            chapter.paragraphs.drain(start.para + 1..end.para);
            if let Some(start_para) = chapter.paragraphs.get_mut(start.para) {
                start_para
                    .chars
                    .truncate(start.offset.min(start_para.len()));
                start_para.chars.extend(suffix.chars);
            }
        }
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_bounds() else {
            self.clear_selection();
            return false;
        };
        self.cursor = start;
        self.push_undo();
        self.clear_pending();
        self.remove_range(start, end);
        self.clear_selection();
        self.mark_dirty();
        self.rebuild_current_pages();
        true
    }

    fn begin_edit(&mut self) -> bool {
        let selection = self.selection_bounds();
        self.push_undo();
        if let Some((start, end)) = selection {
            self.remove_range(start, end);
            self.cursor = start;
        }
        self.clear_selection();
        selection.is_some()
    }

    fn push_undo(&mut self) {
        self.undo.push(Snapshot {
            book: self.doc.book.clone(),
            cursor: self.cursor,
        });
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.typing_undo = None;
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.revision = self.revision.wrapping_add(1);
        self.recompute_stats();
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(Snapshot {
                book: self.doc.book.clone(),
                cursor: self.cursor,
            });
            self.doc.book = s.book;
            self.cursor = s.cursor;
            self.typing_undo = None;
            self.clear_selection();
            self.doc.book.ensure_chapter();
            self.rebuild_all_pages();
            self.recompute_stats();
            self.mark_dirty();
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(Snapshot {
                book: self.doc.book.clone(),
                cursor: self.cursor,
            });
            self.doc.book = s.book;
            self.cursor = s.cursor;
            self.typing_undo = None;
            self.clear_selection();
            self.doc.book.ensure_chapter();
            self.rebuild_all_pages();
            self.recompute_stats();
            self.mark_dirty();
        }
    }

    fn clear_pending(&mut self) {
        self.pending_style = None;
    }

    pub fn move_left(&mut self) {
        self.begin_move();
        if self.cursor.offset > 0 {
            self.cursor.offset -= 1;
        } else if self.cursor.para > 0 {
            self.cursor.para -= 1;
            self.cursor.offset = self.para().len();
        }
        self.sync_page();
    }

    pub fn move_right(&mut self) {
        self.begin_move();
        if self.cursor.offset < self.para().len() {
            self.cursor.offset += 1;
        } else if self.cursor.para + 1
            < self.doc.book.chapters[self.cursor.chapter].paragraphs.len()
        {
            self.cursor.para += 1;
            self.cursor.offset = 0;
        }
        self.sync_page();
    }

    pub fn move_word_left(&mut self) {
        self.begin_move();
        if self.cursor.offset == 0 {
            self.move_left();
            return;
        }
        let chars = &self.para().chars;
        let mut i = self.cursor.offset;
        while i > 0 && chars[i - 1].ch.is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].ch.is_whitespace() {
            i -= 1;
        }
        self.cursor.offset = i;
        self.sync_page();
    }

    pub fn move_word_right(&mut self) {
        self.begin_move();
        let len = self.para().len();
        if self.cursor.offset >= len {
            self.move_right();
            return;
        }
        let chars = &self.para().chars;
        let mut i = self.cursor.offset;
        while i < len && !chars[i].ch.is_whitespace() {
            i += 1;
        }
        while i < len && chars[i].ch.is_whitespace() {
            i += 1;
        }
        self.cursor.offset = i;
        self.sync_page();
    }

    pub fn move_up(&mut self) {
        self.begin_move();
        let cur = self.cursor_line();
        let desired = self.cursor_pos().col;
        let mut target = cur;
        while target > 0 {
            target -= 1;
            if self.lines[target].kind == LineKind::Para {
                self.set_cursor_on_line(target, desired);
                return;
            }
        }
    }

    pub fn move_down(&mut self) {
        self.begin_move();
        let cur = self.cursor_line();
        let desired = self.cursor_pos().col;
        let mut target = cur;
        while target + 1 < self.lines.len() {
            target += 1;
            if self.lines[target].kind == LineKind::Para {
                self.set_cursor_on_line(target, desired);
                return;
            }
        }
    }

    fn set_cursor_on_line(&mut self, idx: usize, desired_col: usize) {
        let line = self.lines[idx].clone();
        if line.kind != LineKind::Para {
            return;
        }
        self.cursor.para = line.para;
        let mut col = line.indent;
        let mut off = line.start;
        let chars = &self.para().chars;
        while off < line.end {
            let w = chars[off].ch.width().unwrap_or(0);
            if col + w > desired_col {
                break;
            }
            col += w;
            off += 1;
        }
        self.cursor.offset = off;
        self.sync_page();
    }

    pub fn home(&mut self) {
        self.begin_move();
        let cur = self.cursor_line();
        let line = &self.lines[cur];
        if self.cursor.offset == line.start {
            self.cursor.offset = 0;
        } else {
            self.cursor.offset = line.start;
        }
        self.sync_page();
    }

    pub fn end(&mut self) {
        self.begin_move();
        let cur = self.cursor_line();
        let line = &self.lines[cur];
        self.cursor.offset = line.end;
        self.sync_page();
    }

    pub fn chapter_start(&mut self) {
        self.begin_move();
        self.cursor.para = 0;
        self.cursor.offset = 0;
        self.page = 0;
        self.sync_page();
    }

    pub fn chapter_end(&mut self) {
        self.begin_move();
        let n = self.doc.book.chapters[self.cursor.chapter].paragraphs.len();
        self.cursor.para = n.saturating_sub(1);
        self.cursor.offset = self.para().len();
        self.sync_page();
    }

    pub fn page_down(&mut self) {
        self.begin_move();
        let height = self.page_height.max(1);
        let step = self.scroll_step();
        let max_start = self.lines.len().saturating_sub(height);
        let target = (self.view_start + step).min(max_start);
        if target == self.view_start && self.view_start + height >= self.lines.len() {
            if self.cursor.chapter + 1 < self.doc.book.chapters.len() {
                self.goto_chapter(self.cursor.chapter + 1);
            }
            return;
        }
        self.view_start = target;
        if let Some(idx) =
            (target..self.lines.len()).find(|&i| self.lines[i].kind == LineKind::Para)
        {
            self.set_cursor_on_line(idx, 0);
        } else if self.cursor.chapter + 1 < self.doc.book.chapters.len() {
            self.goto_chapter(self.cursor.chapter + 1);
        }
        self.sync_page();
    }

    pub fn page_up(&mut self) {
        self.begin_move();
        let height = self.page_height.max(1);
        if self.view_start == 0 {
            if self.cursor.chapter > 0 {
                let chapter = self.cursor.chapter - 1;
                self.goto_chapter(chapter);
                let max_start = self.lines.len().saturating_sub(height);
                self.view_start = max_start;
                if let Some(idx) = (max_start..self.lines.len())
                    .rev()
                    .find(|&i| self.lines[i].kind == LineKind::Para)
                    .or_else(|| {
                        (0..self.lines.len())
                            .rev()
                            .find(|&i| self.lines[i].kind == LineKind::Para)
                    })
                {
                    self.set_cursor_on_line(idx, self.text_width);
                }
            }
            self.sync_page();
            return;
        }
        let target = self.view_start.saturating_sub(self.scroll_step());
        self.view_start = target;
        let end = (target + height).min(self.lines.len());
        if let Some(idx) = (target..end)
            .rev()
            .find(|&i| self.lines[i].kind == LineKind::Para)
        {
            self.set_cursor_on_line(idx, self.text_width);
        }
        self.sync_page();
    }

    pub fn goto_chapter(&mut self, chapter: usize) {
        self.clear_selection();
        self.clear_pending();
        if chapter >= self.doc.book.chapters.len() {
            return;
        }
        self.cursor = Cursor {
            chapter,
            para: 0,
            offset: 0,
        };
        self.page = 0;
        self.view_start = 0;
        self.rebuild_lines();
        self.sync_page();
    }

    pub fn next_chapter(&mut self) {
        if self.cursor.chapter + 1 < self.doc.book.chapters.len() {
            self.goto_chapter(self.cursor.chapter + 1);
        }
    }

    pub fn prev_chapter(&mut self) {
        if self.cursor.chapter > 0 {
            self.goto_chapter(self.cursor.chapter - 1);
        }
    }

    fn insert_style(&self) -> TextStyle {
        if let Some(s) = self.pending_style {
            return s;
        }
        self.para().style_at(self.cursor.offset)
    }

    pub fn insert_char(&mut self, ch: char) {
        let continuing = self.typing_undo == Some(self.cursor) && !self.has_selection();
        if !continuing {
            self.begin_edit();
        } else if !ch.is_whitespace() && self.prev_char().is_some_and(|c| c.is_whitespace()) {
            // Start the next word as its own undo step; trailing whitespace
            // stays with the word that precedes it.
            self.push_undo();
        }
        let style = self.insert_style();
        let at = self.cursor.offset;
        self.para_mut().insert_char(at, ch, style);
        self.cursor.offset += 1;
        self.mark_dirty();
        self.rebuild_current_pages();
        self.typing_undo = Some(self.cursor);
    }

    pub fn insert_typed_char(&mut self, ch: char) {
        let ch = match ch {
            '"' => self.smart_double_quote(),
            '\'' => self.smart_single_quote(),
            _ => ch,
        };
        self.insert_char(self.capitalize_sentence_start(ch));
    }

    /// Uppercase a typed lowercase letter when it starts a sentence.
    fn capitalize_sentence_start(&self, ch: char) -> char {
        if !ch.is_lowercase() || !self.at_sentence_start() {
            return ch;
        }
        // Turkish dotted capital I. The project targets Turkish prose, so
        // sentence-initial `i` must not become the dotless ASCII `I`.
        if ch == 'i' {
            return 'İ';
        }
        let mut upper = ch.to_uppercase();
        match (upper.next(), upper.next()) {
            (Some(c), None) => c,
            _ => ch,
        }
    }

    /// True when the insertion point begins a sentence: at a paragraph start,
    /// or after `.`, `!`, `?` followed by whitespace. Opening quotes and
    /// brackets before the insertion point are looked through, and closing
    /// ones after the punctuation are skipped. A selection is replaced at its
    /// start, so that position is used instead.
    fn at_sentence_start(&self) -> bool {
        let (para, offset) = match self.selection_bounds() {
            Some((start, _)) => (start.para, start.offset),
            None => (self.cursor.para, self.cursor.offset),
        };
        let Some(para) = self.doc.book.chapters[self.cursor.chapter]
            .paragraphs
            .get(para)
        else {
            return false;
        };
        let chars = &para.chars;
        let mut i = offset.min(chars.len());
        let mut saw_space = false;
        loop {
            let before = i;
            while i > 0 && chars[i - 1].ch.is_whitespace() {
                i -= 1;
                saw_space = true;
            }
            if i > 0 && is_opening_punct(chars[i - 1].ch) {
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
        while i > 0 && is_closing_punct(chars[i - 1].ch) {
            i -= 1;
        }
        saw_space && i > 0 && matches!(chars[i - 1].ch, '.' | '!' | '?')
    }

    fn prev_char(&self) -> Option<char> {
        self.para()
            .chars
            .get(self.cursor.offset.wrapping_sub(1))
            .map(|c| c.ch)
    }

    fn smart_double_quote(&self) -> char {
        smart_quote_for(self.prev_char())
    }

    fn smart_single_quote(&self) -> char {
        smart_single_quote_for(self.prev_char())
    }

    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.begin_edit();
        let style = self.insert_style();
        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => {
                    let at = self.cursor.offset;
                    let rest = self.para_mut().split_off(at);
                    let idx = self.cursor.para + 1;
                    self.doc.book.chapters[self.cursor.chapter]
                        .paragraphs
                        .insert(idx, rest);
                    self.cursor.para = idx;
                    self.cursor.offset = 0;
                }
                '\t' => {
                    for _ in 0..4 {
                        let at = self.cursor.offset;
                        self.para_mut().insert_char(at, ' ', style);
                        self.cursor.offset += 1;
                    }
                }
                c if c.is_control() => {}
                _ => {
                    let at = self.cursor.offset;
                    self.para_mut().insert_char(at, ch, style);
                    self.cursor.offset += 1;
                }
            }
        }
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn newline(&mut self) {
        self.begin_edit();
        self.clear_pending();
        let at = self.cursor.offset;
        let rest = self.para_mut().split_off(at);
        let idx = self.cursor.para + 1;
        self.doc.book.chapters[self.cursor.chapter]
            .paragraphs
            .insert(idx, rest);
        self.cursor.para = idx;
        self.cursor.offset = 0;
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    /// Shift+Enter: stay in the same ODF paragraph but force a new line.
    pub fn insert_line_break(&mut self) {
        self.begin_edit();
        let style = self.insert_style();
        let at = self.cursor.offset;
        self.para_mut().insert_char(at, '\n', style);
        self.cursor.offset += 1;
        self.mark_dirty();
        self.rebuild_current_pages();
        self.typing_undo = None;
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor.offset == 0 && self.cursor.para == 0 {
            return;
        }
        self.push_undo();
        self.clear_pending();
        if self.cursor.offset > 0 {
            let off = self.cursor.offset;
            self.para_mut().remove_range(off - 1, off);
            self.cursor.offset -= 1;
        } else {
            let prev_len =
                self.doc.book.chapters[self.cursor.chapter].paragraphs[self.cursor.para - 1].len();
            let mut prev = self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .remove(self.cursor.para - 1);
            let cur =
                &mut self.doc.book.chapters[self.cursor.chapter].paragraphs[self.cursor.para - 1];
            cur.append(&mut prev);
            self.cursor.para -= 1;
            self.cursor.offset = prev_len;
        }
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        let para_len = self.para().len();
        let para_count = self.doc.book.chapters[self.cursor.chapter].paragraphs.len();
        if self.cursor.offset >= para_len && self.cursor.para + 1 >= para_count {
            return;
        }
        self.push_undo();
        self.clear_pending();
        if self.cursor.offset < para_len {
            let off = self.cursor.offset;
            self.para_mut().remove_range(off, off + 1);
        } else {
            let mut next = self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .remove(self.cursor.para + 1);
            self.para_mut().append(&mut next);
        }
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn delete_word_back(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor.offset == 0 {
            self.backspace();
            return;
        }
        self.push_undo();
        self.clear_pending();
        let chars = &self.para().chars;
        let mut i = self.cursor.offset;
        while i > 0 && chars[i - 1].ch.is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].ch.is_whitespace() {
            i -= 1;
        }
        let off = self.cursor.offset;
        self.para_mut().remove_range(i, off);
        self.cursor.offset = i;
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let len = self.para().len();
        if self.cursor.offset >= len {
            self.delete();
            return;
        }
        self.push_undo();
        self.clear_pending();
        let chars = &self.para().chars;
        let mut i = self.cursor.offset;
        while i < len && !chars[i].ch.is_whitespace() {
            i += 1;
        }
        while i < len && chars[i].ch.is_whitespace() {
            i += 1;
        }
        let off = self.cursor.offset;
        self.para_mut().remove_range(off, i);
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn delete_to_para_start(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor.offset == 0 {
            self.backspace();
            return;
        }
        self.push_undo();
        self.clear_pending();
        let off = self.cursor.offset;
        self.para_mut().remove_range(0, off);
        self.cursor.offset = 0;
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn delete_to_para_end(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor.offset >= self.para().len() {
            self.delete();
            return;
        }
        self.push_undo();
        self.clear_pending();
        let len = self.para().len();
        let off = self.cursor.offset;
        self.para_mut().remove_range(off, len);
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn delete_paragraph(&mut self) {
        if self.delete_selection() {
            return;
        }
        self.push_undo();
        self.clear_pending();
        let count = self.doc.book.chapters[self.cursor.chapter].paragraphs.len();
        if count <= 1 {
            self.doc.book.chapters[self.cursor.chapter].paragraphs[0] = Para::default();
            self.cursor.offset = 0;
        } else {
            self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .remove(self.cursor.para);
            if self.cursor.para >= count - 1 {
                self.cursor.para = count - 2;
            }
            self.cursor.offset = 0;
        }
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn toggle_bold(&mut self) {
        self.toggle_style(StyleAttr::Bold);
    }

    pub fn toggle_italic(&mut self) {
        self.toggle_style(StyleAttr::Italic);
    }

    pub fn toggle_underline(&mut self) {
        self.toggle_style(StyleAttr::Underline);
    }

    pub fn toggle_strike(&mut self) {
        self.toggle_style(StyleAttr::Strike);
    }

    fn selection_all_style(&self, attr: StyleAttr) -> bool {
        let Some((start, end)) = self.selection_bounds() else {
            return false;
        };
        let mut any = false;
        for para in start.para..=end.para {
            let from = if para == start.para { start.offset } else { 0 };
            let to = if para == end.para {
                end.offset
            } else {
                self.para_len_at(para)
            };
            let Some(p) = self.doc.book.chapters[start.chapter].paragraphs.get(para) else {
                continue;
            };
            for c in &p.chars[from.min(p.len())..to.min(p.len())] {
                any = true;
                if !c.style().get(attr) {
                    return false;
                }
            }
        }
        any
    }

    fn apply_selection_style(
        &mut self,
        start: Cursor,
        end: Cursor,
        attr: StyleAttr,
        enabled: bool,
    ) {
        let chapter = &mut self.doc.book.chapters[start.chapter];
        for para in start.para..=end.para {
            let from = if para == start.para { start.offset } else { 0 };
            let to = if para == end.para {
                end.offset
            } else {
                chapter.paragraphs.get(para).map(Para::len).unwrap_or(0)
            };
            if let Some(p) = chapter.paragraphs.get_mut(para) {
                p.set_attr_range(from, to, attr, enabled);
            }
        }
    }

    fn toggle_style(&mut self, attr: StyleAttr) {
        if let Some((start, end)) = self.selection_bounds() {
            self.push_undo();
            self.clear_pending();
            let enabled = !self.selection_all_style(attr);
            self.apply_selection_style(start, end, attr, enabled);
            self.cursor = end;
            self.clear_selection();
            self.mark_dirty();
            self.rebuild_current_pages();
            return;
        }
        if self.mark.is_some() {
            self.clear_selection();
        }
        if let Some((start, end)) = self.para().word_range(self.cursor.offset) {
            self.push_undo();
            let all = self.para().chars[start..end]
                .iter()
                .all(|c| c.style().get(attr));
            let enabled = !all;
            self.para_mut().set_attr_range(start, end, attr, enabled);
            self.cursor.offset = end;
            self.pending_style = Some(self.para().chars[start].style());
            self.mark_dirty();
            self.rebuild_current_pages();
        } else {
            let mut style = self
                .pending_style
                .unwrap_or_else(|| self.para().style_at(self.cursor.offset));
            style = style.toggled(attr);
            self.pending_style = Some(style);
        }
    }

    pub fn status_style(&self) -> TextStyle {
        if let Some(style) = self.pending_style {
            return style;
        }
        let p = self.para();
        let off = self.cursor.offset;
        if off < p.len() && !p.chars[off].ch.is_whitespace() {
            p.style_at(off)
        } else if off > 0 {
            p.style_at(off - 1)
        } else {
            p.style_at(off)
        }
    }

    fn selection_paragraphs(&self) -> (usize, usize) {
        self.selection_bounds()
            .map(|(start, end)| (start.para, end.para))
            .unwrap_or((self.cursor.para, self.cursor.para))
    }

    /// Toggle the current paragraph (or every paragraph touching the
    /// selection) between body text and an H3 subheading.
    pub fn toggle_subheading(&mut self) -> bool {
        let (first, last) = self.selection_paragraphs();
        let make_subheading = !(first..=last).all(|para| {
            self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .get(para)
                .is_some_and(|p| p.kind == ParaKind::Subheading)
        });
        self.push_undo();
        self.clear_pending();
        for para in first..=last {
            if let Some(p) = self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .get_mut(para)
            {
                p.kind = if make_subheading {
                    ParaKind::Subheading
                } else {
                    ParaKind::Body
                };
                if make_subheading && p.align == Align::Justify {
                    p.align = Align::Left;
                }
            }
        }
        self.clear_selection();
        self.mark_dirty();
        self.rebuild_all_pages();
        make_subheading
    }

    pub fn set_alignment(&mut self, align: Align) {
        let (first, last) = self.selection_paragraphs();
        self.push_undo();
        self.clear_pending();
        for para in first..=last {
            if let Some(p) = self.doc.book.chapters[self.cursor.chapter]
                .paragraphs
                .get_mut(para)
            {
                p.align = align;
            }
        }
        self.clear_selection();
        self.mark_dirty();
        self.rebuild_current_pages();
    }

    pub fn current_alignment(&self) -> Align {
        self.para().align
    }

    pub fn current_is_subheading(&self) -> bool {
        self.para().kind == ParaKind::Subheading
    }

    pub fn create_chapter(&mut self, title: String) {
        self.clear_selection();
        self.push_undo();
        self.clear_pending();
        let at = self.cursor.chapter + 1;
        self.doc
            .book
            .chapters
            .insert(at, crate::model::Chapter::new(1, title));
        self.doc.book.renumber();
        self.cursor = Cursor {
            chapter: at,
            para: 0,
            offset: 0,
        };
        self.page = 0;
        self.mark_dirty();
        self.rebuild_all_pages();
    }

    pub fn set_chapter_title(&mut self, chapter: usize, title: String) {
        if chapter >= self.doc.book.chapters.len() {
            return;
        }
        self.clear_selection();
        self.push_undo();
        self.doc.book.chapters[chapter].title = title;
        self.mark_dirty();
        self.rebuild_all_pages();
    }

    pub fn set_book_title(&mut self, title: String) {
        self.clear_selection();
        self.push_undo();
        self.doc.book.title = title;
        self.mark_dirty();
    }

    pub fn save_bytes(&mut self) -> anyhow::Result<Vec<u8>> {
        self.doc.to_bytes()
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }
}

fn quote_opens(prev: Option<char>) -> bool {
    match prev {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '(' | '[' | '{' | '—' | '–' | '/' | '\\' | '|'),
    }
}

pub fn smart_quote_for(prev: Option<char>) -> char {
    if quote_opens(prev) { '“' } else { '”' }
}

pub fn smart_single_quote_for(prev: Option<char>) -> char {
    if quote_opens(prev) { '‘' } else { '’' }
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

/// Keep the cursor visible while scrolling in 75% steps, retaining the last
/// 25% of the previous viewport above the cursor.
fn scrolled_view_start(line: usize, view_start: usize, page_height: usize) -> usize {
    let height = page_height.max(1);
    let overlap = height / 4;
    let step = (height - overlap).max(1);
    let mut start = view_start;
    if line < start {
        start = line.saturating_sub(overlap);
    } else if line >= start + height {
        while line >= start + height {
            start = start.saturating_add(step);
        }
    }
    start
}

fn visible_width(chars: &[StyledChar]) -> usize {
    chars
        .iter()
        .filter(|c| c.ch != '\n')
        .map(|c| c.ch.width().unwrap_or(0))
        .sum()
}

fn wrap_chars(chars: &[StyledChar], first_width: usize, width: usize) -> Vec<(usize, usize)> {
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < chars.len() {
        // A line break creates an empty visual line when it starts one.
        if chars[start].ch == '\n' {
            out.push((start, start));
            start += 1;
            first = false;
            continue;
        }
        let avail = if first {
            first_width.max(1)
        } else {
            width.max(1)
        };
        let mut w = 0;
        let mut end = start;
        let mut last_space = None;
        while end < chars.len() {
            if chars[end].ch == '\n' {
                break;
            }
            let cw = chars[end].ch.width().unwrap_or(0);
            if w + cw > avail {
                break;
            }
            if chars[end].ch == ' ' {
                last_space = Some(end);
            }
            w += cw;
            end += 1;
        }
        if end == start {
            end = start + 1;
        } else if end < chars.len() && chars[end].ch != '\n' {
            if let Some(sp) = last_space {
                if sp > start && sp < end {
                    end = sp;
                }
            }
        }
        out.push((start, end));
        if end < chars.len() {
            if matches!(chars[end].ch, '\n' | ' ') {
                start = end + 1;
            } else {
                start = end;
            }
        } else {
            start = end;
        }
        first = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<StyledChar> {
        s.chars().map(StyledChar::plain).collect()
    }

    #[test]
    fn viewport_scrolls_75_percent_with_25_percent_overlap() {
        let height = 24;
        assert_eq!(scrolled_view_start(23, 0, height), 0);
        assert_eq!(scrolled_view_start(24, 0, height), 18);
        assert_eq!(scrolled_view_start(25, 18, height), 18);
        assert_eq!(scrolled_view_start(42, 18, height), 36);
        assert_eq!(scrolled_view_start(30, 36, height), 24);
        assert_eq!(scrolled_view_start(0, 36, height), 0);
    }

    #[test]
    fn wrapping_respects_width_and_words() {
        let c = chars("hello world this is a test");
        let lines = wrap_chars(&c, 10, 10);
        for (start, end) in &lines {
            let widths: usize = c[*start..*end].iter().map(|x| x.width()).sum();
            assert!(widths <= 10);
        }
        let reconstructed: String = lines
            .iter()
            .flat_map(|(s, e)| c[*s..*e].iter().map(|x| x.ch))
            .collect();
        assert_eq!(reconstructed.replace(' ', ""), "helloworldthisisatest");
    }

    #[test]
    fn long_word_is_hard_broken() {
        let c = chars("abcdefghijklmnop");
        let lines = wrap_chars(&c, 5, 5);
        assert!(lines.len() >= 4);
        for (s, e) in lines {
            assert!(e - s <= 5);
        }
    }

    #[test]
    fn editing_and_undo() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Hello".chars() {
            ed.insert_char(ch);
        }
        assert_eq!(ed.total_words, 2); // book title + word
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "Hello");
        ed.toggle_bold();
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].chars[0].bold, true);
        ed.newline();
        assert_eq!(ed.doc.book.chapters[0].paragraphs.len(), 2);
        ed.undo();
        assert_eq!(ed.doc.book.chapters[0].paragraphs.len(), 1);
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "Hello");
    }

    #[test]
    fn replace_span_applies_and_undoes() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "ve ve devam".chars() {
            ed.insert_char(ch);
        }
        ed.doc.book.chapters[0].paragraphs[0].set_style_range(
            3,
            5,
            TextStyle {
                bold: true,
                ..TextStyle::default()
            },
        );
        assert!(ed.replace_span(0, 3, 5, "ve", "VE"));
        let para = &ed.doc.book.chapters[0].paragraphs[0];
        assert_eq!(para.text(), "ve VE devam");
        assert!(para.chars[3].bold);
        assert!(para.chars[4].bold);
        ed.undo();
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "ve ve devam");
        assert!(!ed.replace_span(0, 0, 2, "xx", "yy"));
    }

    #[test]
    fn mark_selection_deletes_within_paragraph() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Hello world".chars() {
            ed.insert_char(ch);
        }
        ed.cursor.offset = 6;
        assert_eq!(ed.toggle_mark(), MarkResult::Started);
        ed.cursor.offset = 11;
        assert_eq!(ed.toggle_mark(), MarkResult::Finished(5));
        assert!(ed.has_selection());
        assert_eq!(ed.selection_len(), 5);
        assert!(ed.delete_selection());
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "Hello ");
        assert!(!ed.has_selection());
    }

    #[test]
    fn mark_selection_deletes_across_paragraphs_and_undoes() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Hello".chars() {
            ed.insert_char(ch);
        }
        ed.newline();
        for ch in "world".chars() {
            ed.insert_char(ch);
        }
        ed.cursor.para = 0;
        ed.cursor.offset = 2;
        assert_eq!(ed.toggle_mark(), MarkResult::Started);
        ed.cursor.para = 1;
        ed.cursor.offset = 3;
        assert_eq!(ed.toggle_mark(), MarkResult::Finished(6));
        assert!(ed.delete_selection());
        let chapter = &ed.doc.book.chapters[0];
        assert_eq!(chapter.paragraphs.len(), 1);
        assert_eq!(chapter.paragraphs[0].text(), "Held");
        assert_eq!(ed.cursor.para, 0);
        assert_eq!(ed.cursor.offset, 2);

        ed.undo();
        let chapter = &ed.doc.book.chapters[0];
        assert_eq!(chapter.paragraphs.len(), 2);
        assert_eq!(chapter.paragraphs[0].text(), "Hello");
        assert_eq!(chapter.paragraphs[1].text(), "world");
    }

    #[test]
    fn typing_replaces_selection() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Hello world".chars() {
            ed.insert_char(ch);
        }
        ed.cursor.offset = 5;
        ed.toggle_mark();
        ed.cursor.offset = 11;
        ed.toggle_mark();
        ed.insert_typed_char('!');
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "Hello!");
        assert!(!ed.has_selection());
    }

    #[test]
    fn selection_reports_words_and_chars() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Hello world".chars() {
            ed.insert_char(ch);
        }
        ed.cursor.offset = 0;
        ed.toggle_mark();
        ed.cursor.offset = 11;
        ed.toggle_mark();
        assert_eq!(ed.selection_word_count(), 2);
        assert_eq!(ed.selection_len(), 11);

        ed.clear_selection();
        ed.cursor.offset = 5;
        ed.toggle_mark();
        ed.cursor.offset = 6;
        ed.toggle_mark();
        assert_eq!(ed.selection_word_count(), 0);
        assert_eq!(ed.selection_len(), 1);
    }

    #[test]
    fn bold_applies_to_marked_range() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Hello world".chars() {
            ed.insert_char(ch);
        }
        ed.cursor.offset = 0;
        ed.toggle_mark();
        ed.cursor.offset = 5;
        ed.toggle_mark();
        ed.toggle_bold();
        let chars = &ed.doc.book.chapters[0].paragraphs[0].chars;
        assert!(chars[..5].iter().all(|c| c.bold));
        assert!(chars[5..].iter().all(|c| !c.bold));
        assert!(!ed.has_selection());
    }

    #[test]
    fn underline_and_strike_apply_to_marked_range() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Hello world".chars() {
            ed.insert_char(ch);
        }
        ed.cursor.offset = 0;
        ed.toggle_mark();
        ed.cursor.offset = 5;
        ed.toggle_mark();
        ed.toggle_underline();
        ed.cursor.offset = 6;
        ed.toggle_mark();
        ed.cursor.offset = 11;
        ed.toggle_mark();
        ed.toggle_strike();
        let chars = &ed.doc.book.chapters[0].paragraphs[0].chars;
        assert!(chars[..5].iter().all(|c| c.underline));
        assert!(chars[6..11].iter().all(|c| c.strike));
        assert!(!chars[0].strike);
    }

    #[test]
    fn line_break_stays_inside_the_paragraph() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "first".chars() {
            ed.insert_char(ch);
        }
        ed.insert_line_break();
        for ch in "second".chars() {
            ed.insert_char(ch);
        }
        assert_eq!(ed.doc.book.chapters[0].paragraphs.len(), 1);
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "first\nsecond"
        );
        assert_eq!(
            ed.lines()
                .iter()
                .filter(|line| line.kind == LineKind::Para)
                .count(),
            2
        );
    }

    #[test]
    fn alignment_offsets_short_lines() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "centered".chars() {
            ed.insert_char(ch);
        }
        ed.set_alignment(Align::Center);
        let line = ed
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Para)
            .unwrap();
        assert!(line.indent > 0);
        ed.set_alignment(Align::Right);
        let line = ed
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Para)
            .unwrap();
        assert!(line.indent + "centered".len() == ed.text_width());
    }

    #[test]
    fn subheading_toggle_preserves_order() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Heading".chars() {
            ed.insert_char(ch);
        }
        assert!(ed.toggle_subheading());
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].kind,
            ParaKind::Subheading
        );
        assert!(!ed.toggle_subheading());
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].kind, ParaKind::Body);
    }

    #[test]
    fn smart_double_quotes() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "He said ".chars() {
            ed.insert_typed_char(ch);
        }
        ed.insert_typed_char('"');
        for ch in "hello".chars() {
            ed.insert_typed_char(ch);
        }
        ed.insert_typed_char('"');
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "He said “hello”"
        );

        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        ed.insert_typed_char('"');
        ed.insert_typed_char('"');
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "“”");
    }

    #[test]
    fn smart_single_quotes_and_apostrophes() {
        let text = "'Ben bu genç adamın kolundayım, buraya aitim, beni buradan alamazsınız.'";
        let expected = "‘Ben bu genç adamın kolundayım, buraya aitim, beni buradan alamazsınız.’";
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in text.chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), expected);

        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "Anette'nin evi".chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "Anette’nin evi"
        );
    }

    #[test]
    fn closing_quote_after_punctuation() {
        let cases = [
            ("He said, \"Hello,\"", "He said, “Hello,”"),
            ("Wait!\"", "Wait!”"),
            ("Why?\"", "Why?”"),
            ("Done.\"", "Done.”"),
            ("(\"x\")", "(“X”)"),
        ];
        for (input, expected) in cases {
            let doc = OdtDocument::new("Test").unwrap();
            let mut ed = Editor::new(doc, 80, 24);
            for ch in input.chars() {
                ed.insert_typed_char(ch);
            }
            assert_eq!(
                ed.doc.book.chapters[0].paragraphs[0].text(),
                expected,
                "input: {input}"
            );
        }
    }

    #[test]
    fn autocapitalizes_new_sentences() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "hello. world! again? yes".chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "Hello. World! Again? Yes"
        );
    }

    #[test]
    fn does_not_autocapitalize_mid_sentence() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "hello world.again, ok".chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "Hello world.again, ok"
        );
    }

    #[test]
    fn autocapitalizes_after_quotes_and_paragraphs() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "he said \"stop.\" then".chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "He said “stop.” Then"
        );

        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "\"hello\", she said".chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "“Hello”, she said"
        );

        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "hello".chars() {
            ed.insert_typed_char(ch);
        }
        ed.newline();
        for ch in "world".chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "Hello");
        assert_eq!(ed.doc.book.chapters[0].paragraphs[1].text(), "World");
    }

    #[test]
    fn autocapitalizes_turkish_dotted_i() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "istanbul güzel. işte öyle".chars() {
            ed.insert_typed_char(ch);
        }
        assert_eq!(
            ed.doc.book.chapters[0].paragraphs[0].text(),
            "İstanbul güzel. İşte öyle"
        );
    }

    #[test]
    fn undo_groups_typed_words() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "one two three".chars() {
            ed.insert_char(ch);
        }
        ed.undo();
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "one two ");
        ed.undo();
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "one ");
        ed.undo();
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "");
    }

    #[test]
    fn undo_starts_new_group_after_cursor_move() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "hello".chars() {
            ed.insert_char(ch);
        }
        ed.move_left();
        ed.move_left();
        ed.insert_char('X');
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "helXlo");
        ed.undo();
        assert_eq!(ed.doc.book.chapters[0].paragraphs[0].text(), "hello");
    }

    #[test]
    fn toggling_bold_preserves_italic() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        for ch in "abcd".chars() {
            ed.insert_char(ch);
        }
        ed.doc.book.chapters[0].paragraphs[0].chars[1].italic = true;
        ed.cursor.offset = 1;
        ed.toggle_bold();
        let p = &ed.doc.book.chapters[0].paragraphs[0];
        assert!(p.chars[0].bold && !p.chars[0].italic);
        assert!(p.chars[1].bold && p.chars[1].italic);
        assert!(p.chars[2].bold && !p.chars[2].italic);
    }

    #[test]
    fn chapter_creation_renumbers() {
        let doc = OdtDocument::new("Test").unwrap();
        let mut ed = Editor::new(doc, 80, 24);
        ed.create_chapter("Middle".into());
        assert_eq!(ed.doc.book.chapters.len(), 2);
        assert_eq!(ed.cursor.chapter, 1);
        assert_eq!(ed.doc.book.chapters[1].number, 2);
        ed.goto_chapter(0);
        ed.create_chapter("Between".into());
        assert_eq!(ed.doc.book.chapters.len(), 3);
        assert_eq!(ed.doc.book.chapters[1].title, "Between");
        assert_eq!(ed.doc.book.chapters[1].number, 2);
        assert_eq!(ed.doc.book.chapters[2].number, 3);
    }
}

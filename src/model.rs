use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyledChar {
    pub ch: char,
    pub bold: bool,
    pub italic: bool,
}

impl StyledChar {
    pub fn new(ch: char, bold: bool, italic: bool) -> Self {
        Self { ch, bold, italic }
    }

    pub fn plain(ch: char) -> Self {
        Self {
            ch,
            bold: false,
            italic: false,
        }
    }

    pub fn width(&self) -> usize {
        self.ch.width().unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Para {
    pub chars: Vec<StyledChar>,
}

impl Para {
    pub fn from_text(text: &str) -> Self {
        Self {
            chars: text.chars().map(StyledChar::plain).collect(),
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().map(|c| c.ch).collect()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn insert_str(&mut self, at: usize, text: &str, bold: bool, italic: bool) {
        let at = at.min(self.chars.len());
        let added: Vec<StyledChar> = text
            .chars()
            .map(|ch| StyledChar::new(ch, bold, italic))
            .collect();
        self.chars.splice(at..at, added);
    }

    pub fn insert_char(&mut self, at: usize, ch: char, bold: bool, italic: bool) {
        let at = at.min(self.chars.len());
        self.chars.insert(at, StyledChar::new(ch, bold, italic));
    }

    pub fn remove_range(&mut self, start: usize, end: usize) {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        if start < end {
            self.chars.drain(start..end);
        }
    }

    pub fn split_off(&mut self, at: usize) -> Para {
        let at = at.min(self.chars.len());
        let rest = self.chars.split_off(at);
        Para { chars: rest }
    }

    pub fn append(&mut self, other: &mut Para) {
        self.chars.append(&mut other.chars);
    }

    pub fn word_range(&self, offset: usize) -> Option<(usize, usize)> {
        if self.chars.is_empty() {
            return None;
        }
        let offset = offset.min(self.chars.len());
        let mut probe = offset;
        if probe >= self.chars.len() {
            probe = self.chars.len() - 1;
        }
        if !is_word_char(self.chars[probe].ch)
            && probe > 0
            && is_word_char(self.chars[probe - 1].ch)
        {
            probe -= 1;
        }
        if !is_word_char(self.chars[probe].ch) {
            return None;
        }
        let mut start = probe;
        while start > 0 && is_word_char(self.chars[start - 1].ch) {
            start -= 1;
        }
        let mut end = probe + 1;
        while end < self.chars.len() && is_word_char(self.chars[end].ch) {
            end += 1;
        }
        Some((start, end))
    }

    pub fn style_at(&self, offset: usize) -> (bool, bool) {
        if self.chars.is_empty() {
            return (false, false);
        }
        if offset < self.chars.len() {
            let c = self.chars[offset];
            return (c.bold, c.italic);
        }
        let c = self.chars[self.chars.len() - 1];
        (c.bold, c.italic)
    }

    pub fn set_style_range(&mut self, start: usize, end: usize, bold: bool, italic: bool) {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        for c in &mut self.chars[start..end] {
            c.bold = bold;
            c.italic = italic;
        }
    }

    pub fn set_bold_range(&mut self, start: usize, end: usize, bold: bool) {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        for c in &mut self.chars[start..end] {
            c.bold = bold;
        }
    }

    pub fn set_italic_range(&mut self, start: usize, end: usize, italic: bool) {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        for c in &mut self.chars[start..end] {
            c.italic = italic;
        }
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric()
}

#[derive(Clone, Debug)]
pub struct Chapter {
    pub number: u32,
    pub title: String,
    pub paragraphs: Vec<Para>,
}

impl Chapter {
    pub fn new(number: u32, title: impl Into<String>) -> Self {
        Self {
            number,
            title: title.into(),
            paragraphs: vec![Para::default()],
        }
    }

    pub fn word_count(&self) -> usize {
        let mut n = self.title.split_whitespace().count();
        for p in &self.paragraphs {
            n += p.text().split_whitespace().count();
        }
        n
    }

    pub fn char_count(&self) -> usize {
        self.title.chars().count() + self.paragraphs.iter().map(|p| p.chars.len()).sum::<usize>()
    }

    pub fn ensure_paragraph(&mut self) {
        if self.paragraphs.is_empty() {
            self.paragraphs.push(Para::default());
        }
    }
}

#[derive(Clone, Debug)]
pub struct Book {
    pub title: String,
    pub chapters: Vec<Chapter>,
}

impl Book {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            chapters: vec![Chapter::new(1, "")],
        }
    }

    pub fn ensure_chapter(&mut self) {
        if self.chapters.is_empty() {
            self.chapters.push(Chapter::new(1, ""));
        }
        for c in &mut self.chapters {
            c.ensure_paragraph();
        }
        self.renumber();
    }

    pub fn renumber(&mut self) {
        for (i, c) in self.chapters.iter_mut().enumerate() {
            c.number = (i + 1) as u32;
        }
    }

    pub fn word_count(&self) -> usize {
        let mut n = self.title.split_whitespace().count();
        for c in &self.chapters {
            n += c.word_count();
        }
        n
    }

    pub fn char_count(&self) -> usize {
        self.title.chars().count() + self.chapters.iter().map(|c| c.char_count()).sum::<usize>()
    }

    pub fn total_chars(&self) -> usize {
        self.char_count()
    }

    pub fn total_words(&self) -> usize {
        self.word_count()
    }

    pub fn heading_text(&self, chapter: usize) -> String {
        let c = &self.chapters[chapter];
        if c.title.trim().is_empty() {
            format!("{}.", c.number)
        } else {
            format!("{}. {}", c.number, c.title.trim())
        }
    }
}

pub fn parse_chapter_number(raw: &str) -> (Option<u32>, String) {
    let s = raw.trim();
    let mut digits = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }
    if !digits.is_empty() {
        if let Ok(n) = digits.parse::<u32>() {
            let rest = &s[digits.len()..];
            let next = rest.chars().next();
            let is_separator = match next {
                Some('.') | Some(')') => true,
                Some(c) if c.is_whitespace() => true,
                _ => false,
            };
            if is_separator {
                let rest = rest
                    .trim_start_matches('.')
                    .trim_start_matches(')')
                    .trim_start_matches(' ')
                    .trim_start();
                return (Some(n), rest.to_string());
            }
        }
    }
    (None, s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_numbers() {
        assert_eq!(
            parse_chapter_number("1. Chapter"),
            (Some(1), "Chapter".into())
        );
        assert_eq!(parse_chapter_number("12) Part"), (Some(12), "Part".into()));
        assert_eq!(parse_chapter_number("3. "), (Some(3), "".into()));
        assert_eq!(parse_chapter_number("1984"), (None, "1984".into()));
        assert_eq!(
            parse_chapter_number("Chapter 1"),
            (None, "Chapter 1".into())
        );
    }

    #[test]
    fn word_ranges() {
        let p = Para::from_text("Merhaba dünya!");
        assert_eq!(p.word_range(0), Some((0, 7)));
        assert_eq!(p.word_range(7), Some((0, 7)));
        assert_eq!(p.word_range(8), Some((8, 13)));
        assert_eq!(p.word_range(13), Some((8, 13)));
        assert_eq!(Para::default().word_range(0), None);
    }

    #[test]
    fn style_range() {
        let mut p = Para::from_text("abcd");
        p.set_style_range(1, 3, true, false);
        assert_eq!(p.chars[0].bold, false);
        assert_eq!(p.chars[1].bold, true);
        assert_eq!(p.chars[2].bold, true);
        assert_eq!(p.chars[3].bold, false);
    }
}

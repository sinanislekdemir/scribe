use crate::model::Book;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub chapter: usize,
    pub para: usize,
    pub start: usize,
    pub end: usize,
}

impl SearchMatch {
    pub fn position(self) -> (usize, usize, usize) {
        (self.chapter, self.para, self.start)
    }
}

/// Find every occurrence of `query` in body paragraphs. Matching is
/// case-insensitive, never crosses a paragraph boundary, and treats a soft
/// line break as a space.
pub fn find_matches(book: &Book, query: &str) -> Vec<SearchMatch> {
    let needle: Vec<char> = query.chars().map(fold_char).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (chapter_index, chapter) in book.chapters.iter().enumerate() {
        for (para_index, para) in chapter.paragraphs.iter().enumerate() {
            let haystack: Vec<char> = para
                .chars
                .iter()
                .map(|c| fold_char(if c.ch == '\n' { ' ' } else { c.ch }))
                .collect();
            if haystack.len() < needle.len() {
                continue;
            }
            for start in 0..=haystack.len() - needle.len() {
                if haystack[start..start + needle.len()] == needle[..] {
                    matches.push(SearchMatch {
                        chapter: chapter_index,
                        para: para_index,
                        start,
                        end: start + needle.len(),
                    });
                }
            }
        }
    }
    matches
}

/// Pick the next match in document order. `inclusive` controls whether a match
/// exactly at `after` is accepted. Returns the index and whether it wrapped.
pub fn next_match(
    matches: &[SearchMatch],
    after: (usize, usize, usize),
    inclusive: bool,
) -> Option<(usize, bool)> {
    if matches.is_empty() {
        return None;
    }
    let found = matches.iter().position(|item| {
        let position = item.position();
        if inclusive {
            position >= after
        } else {
            position > after
        }
    });
    match found {
        Some(index) => Some((index, false)),
        None => Some((0, true)),
    }
}

fn fold_char(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chapter, Para};

    fn book(paragraphs: &[&str]) -> Book {
        let mut book = Book::new("Book");
        book.chapters[0].title = "Chapter".to_string();
        book.chapters[0].paragraphs = paragraphs
            .iter()
            .map(|text| Para::from_text(text))
            .collect();
        book.chapters.push(Chapter::new(2, "Two"));
        book.chapters[1].paragraphs[0] = Para::from_text("second chapter");
        book
    }

    #[test]
    fn finds_case_insensitively() {
        let book = book(&["Hello World", "hello again"]);
        let matches = find_matches(&book, "hello");
        assert_eq!(matches.len(), 2);
        assert_eq!((matches[0].para, matches[0].start), (0, 0));
        assert_eq!((matches[1].para, matches[1].start), (1, 0));
    }

    #[test]
    fn folds_turkish_dotted_i() {
        let book = book(&["İstanbul güzel"]);
        let matches = find_matches(&book, "istanbul");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, 0);
    }

    #[test]
    fn soft_line_break_matches_a_space() {
        let book = book(&["one\ntwo"]);
        let matches = find_matches(&book, "one two");
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start, matches[0].end), (0, 7));
    }

    #[test]
    fn next_match_skips_current_and_wraps() {
        let book = book(&["one one", "one"]);
        let matches = find_matches(&book, "one");
        assert_eq!(matches.len(), 3);
        assert_eq!(next_match(&matches, (0, 0, 0), true), Some((0, false)));
        assert_eq!(next_match(&matches, (0, 0, 0), false), Some((1, false)));
        assert_eq!(next_match(&matches, (1, 0, 0), false), Some((0, true)));
        assert_eq!(next_match(&[], (0, 0, 0), true), None);
    }
}

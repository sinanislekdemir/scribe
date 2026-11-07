use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use roxmltree::{Document, Node};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::model::{Book, Chapter, Para, StyledChar, parse_chapter_number};

pub const TEMPLATE_ODT: &[u8] = include_bytes!("../../assets/template.odt");

const NS_OFFICE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const NS_STYLE: &str = "urn:oasis:names:tc:opendocument:xmlns:style:1.0";
const NS_TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const NS_FO: &str = "urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0";

#[derive(Clone, Debug)]
pub struct PackageEntry {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct OdtPackage {
    pub entries: Vec<PackageEntry>,
}

impl OdtPackage {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).context("open ODT zip")?;
        let mut entries = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).context("read ODT entry")?;
            if file.is_dir() {
                continue;
            }
            let name = file.name().context("read ODT entry name")?.to_string();
            let mut data = Vec::new();
            file.read_to_end(&mut data)
                .with_context(|| format!("read ODT entry {name}"))?;
            entries.push(PackageEntry { name, data });
        }
        Ok(Self { entries })
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.data.as_slice())
    }

    pub fn set(&mut self, name: &str, data: Vec<u8>) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.name == name) {
            e.data = data;
        } else {
            self.entries.push(PackageEntry {
                name: name.to_string(),
                data,
            });
        }
    }

    pub fn content_xml(&self) -> Result<String> {
        let bytes = self
            .get("content.xml")
            .ok_or_else(|| anyhow!("ODT has no content.xml"))?;
        String::from_utf8(bytes.to_vec()).context("content.xml is not UTF-8")
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        writer.start_file("mimetype", stored)?;
        writer.write_all(b"application/vnd.oasis.opendocument.text")?;

        for entry in &self.entries {
            if entry.name == "mimetype" {
                continue;
            }
            writer.start_file(entry.name.as_str(), deflated)?;
            writer.write_all(&entry.data)?;
        }
        let cursor = writer.finish()?;
        Ok(cursor.into_inner())
    }
}

#[derive(Clone, Debug)]
pub struct StyleRefs {
    pub title: String,
    pub chapter: String,
    pub body: String,
}

impl StyleRefs {
    pub fn template() -> Self {
        Self {
            title: "BookTitle".into(),
            chapter: "ChapterHeading".into(),
            body: "Body".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OdtDocument {
    pub book: Book,
    pub package: OdtPackage,
    pub refs: StyleRefs,
    pub automatic_styles: String,
    pub font_face_decls: String,
    pub odf_version: String,
}

impl OdtDocument {
    pub fn new(title: impl Into<String>) -> Result<Self> {
        let package = OdtPackage::from_bytes(TEMPLATE_ODT)?;
        let content = package.content_xml()?;
        let automatic_styles = strip_mydocs_styles(
            &extract_element_inner(&content, "office:automatic-styles").unwrap_or_default(),
        );
        let font_face_decls =
            extract_element(&content, "office:font-face-decls").unwrap_or_default();
        let mut book = Book::new(title);
        book.ensure_chapter();
        let odf_version = content_version(&content).unwrap_or_else(|| "1.3".to_string());
        Ok(Self {
            book,
            package,
            refs: StyleRefs::template(),
            automatic_styles,
            font_face_decls,
            odf_version,
        })
    }

    pub fn from_bytes(bytes: &[u8], fallback_title: &str) -> Result<Self> {
        let package = OdtPackage::from_bytes(bytes)?;
        let content = package.content_xml()?;
        let styles_xml = package
            .get("styles.xml")
            .map(|b| String::from_utf8_lossy(b).to_string())
            .unwrap_or_default();
        let book = parse_content(&content, &styles_xml, fallback_title)?;
        let automatic_styles = strip_mydocs_styles(
            &extract_element_inner(&content, "office:automatic-styles").unwrap_or_default(),
        );
        let font_face_decls =
            extract_element(&content, "office:font-face-decls").unwrap_or_default();
        let refs = detect_refs(&content, &styles_xml);
        let odf_version = content_version(&content).unwrap_or_else(|| "1.3".to_string());
        Ok(Self {
            book,
            package,
            refs,
            automatic_styles,
            font_face_decls,
            odf_version,
        })
    }

    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        self.book.ensure_chapter();
        let content = write_content(
            &self.book,
            &self.refs,
            &self.automatic_styles,
            &self.font_face_decls,
            &self.odf_version,
        )?;
        self.package.set("content.xml", content.into_bytes());
        update_meta_title(&mut self.package, &self.book.title);
        self.package.to_bytes()
    }
}

#[derive(Clone, Debug, Default)]
struct StyleInfo {
    display_name: Option<String>,
    family: String,
    parent: Option<String>,
    outline: Option<u32>,
    font_size: Option<f32>,
    bold: bool,
    italic: bool,
    break_before: bool,
}

#[derive(Default)]
struct StyleMap {
    map: HashMap<String, StyleInfo>,
}

impl StyleMap {
    fn parse(xml: &str) -> Self {
        let mut map = HashMap::new();
        if xml.trim().is_empty() {
            return Self { map };
        }
        let Ok(doc) = Document::parse(xml) else {
            return Self { map };
        };
        for node in doc.descendants().filter(Node::is_element) {
            if node.tag_name().name() != "style" || node.tag_name().namespace() != Some(NS_STYLE) {
                continue;
            }
            let Some(name) = attr(node, NS_STYLE, "name") else {
                continue;
            };
            let family = attr(node, NS_STYLE, "family")
                .unwrap_or_default()
                .to_string();
            let mut info = StyleInfo {
                display_name: attr(node, NS_STYLE, "display-name").map(str::to_string),
                family,
                parent: attr(node, NS_STYLE, "parent-style-name").map(str::to_string),
                ..Default::default()
            };
            if let Some(level) =
                attr(node, NS_STYLE, "default-outline-level").and_then(|s| s.parse::<u32>().ok())
            {
                info.outline = Some(level);
            }
            for child in node.children().filter(Node::is_element) {
                match child.tag_name().name() {
                    "paragraph-properties" => {
                        if attr(child, NS_FO, "break-before") == Some("page") {
                            info.break_before = true;
                        }
                        for a in child.attributes() {
                            if a.name().ends_with("outline-level") {
                                if let Ok(level) = a.value().parse::<u32>() {
                                    info.outline = Some(level);
                                }
                            }
                        }
                    }
                    "text-properties" => {
                        if let Some(size) = attr(child, NS_FO, "font-size") {
                            info.font_size = parse_pt(size);
                        }
                        if let Some(weight) = attr(child, NS_FO, "font-weight") {
                            info.bold =
                                matches!(weight, "bold" | "bolder" | "600" | "700" | "800" | "900");
                        }
                        if let Some(style) = attr(child, NS_FO, "font-style") {
                            info.italic = matches!(style, "italic" | "oblique");
                        }
                    }
                    _ => {}
                }
            }
            map.insert(name.to_string(), info);
        }
        Self { map }
    }

    fn resolved(&self, name: &str) -> StyleInfo {
        let mut chain = Vec::new();
        let mut cur = Some(name.to_string());
        let mut guard = 0;
        while let Some(n) = cur {
            guard += 1;
            if guard > 32 {
                break;
            }
            let Some(info) = self.map.get(&n) else {
                break;
            };
            chain.push(info.clone());
            cur = info.parent.clone();
        }
        let mut out = StyleInfo::default();
        for info in chain.into_iter().rev() {
            if out.family.is_empty() {
                out.family = info.family.clone();
            }
            if info.display_name.is_some() {
                out.display_name = info.display_name.clone();
            }
            if info.outline.is_some() {
                out.outline = info.outline;
            }
            if info.font_size.is_some() {
                out.font_size = info.font_size;
            }
            out.bold |= info.bold;
            out.italic |= info.italic;
            out.break_before |= info.break_before;
        }
        out
    }

    fn heading_level(&self, name: &str) -> Option<u32> {
        let info = self.resolved(name);
        let mut names = Vec::new();
        names.push(name.to_lowercase().replace('_', " "));
        if let Some(d) = &info.display_name {
            names.push(d.to_lowercase().replace('_', " "));
        }
        for n in &names {
            if n.contains("title") {
                return Some(1);
            }
            if n.contains("heading 1") || n.contains("heading1") {
                return Some(1);
            }
            if n.contains("heading 2") || n.contains("heading2") {
                return Some(2);
            }
        }
        if let Some(level) = info.outline {
            if level == 1 || level == 2 {
                return Some(level);
            }
        }
        if let Some(size) = info.font_size {
            if size >= 20.0 {
                return Some(1);
            }
            if size >= 14.0 {
                return Some(2);
            }
        }
        None
    }

    fn text_style(&self, name: &str) -> (bool, bool) {
        let info = self.resolved(name);
        (info.bold, info.italic)
    }
}

fn parse_pt(value: &str) -> Option<f32> {
    let value = value.trim();
    let number = value.trim_end_matches("pt").trim_end_matches("px").trim();
    number.parse::<f32>().ok()
}

fn attr<'a>(node: Node<'a, 'a>, ns: &str, name: &str) -> Option<&'a str> {
    node.attribute((ns, name))
}

fn text_content(node: Node<'_, '_>) -> String {
    let mut out = String::new();
    collect_text(node, false, false, &mut out);
    out
}

fn collect_text(node: Node<'_, '_>, bold: bool, italic: bool, out: &mut String) {
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                out.push_str(t);
            }
        } else if child.is_element() {
            match (child.tag_name().namespace(), child.tag_name().name()) {
                (Some(NS_TEXT), "s") => {
                    let n = attr(child, NS_TEXT, "c")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(1);
                    out.extend(std::iter::repeat(' ').take(n));
                }
                (Some(NS_TEXT), "tab") => out.push('\t'),
                (Some(NS_TEXT), "line-break") => out.push('\n'),
                (Some(NS_TEXT), "span") => {
                    collect_text(child, bold, italic, out);
                }
                (Some(NS_TEXT), "bookmark-start")
                | (Some(NS_TEXT), "bookmark-end")
                | (Some(NS_TEXT), "bookmark")
                | (Some(NS_TEXT), "soft-page-break") => {}
                _ => {
                    collect_text(child, bold, italic, out);
                }
            }
        }
    }
}

fn extract_inline(
    node: Node<'_, '_>,
    styles: &StyleMap,
    bold: bool,
    italic: bool,
    out: &mut Vec<StyledChar>,
) {
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                for ch in t.chars() {
                    out.push(StyledChar::new(ch, bold, italic));
                }
            }
        } else if child.is_element() {
            match (child.tag_name().namespace(), child.tag_name().name()) {
                (Some(NS_TEXT), "s") => {
                    let n = attr(child, NS_TEXT, "c")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(1);
                    for _ in 0..n {
                        out.push(StyledChar::new(' ', bold, italic));
                    }
                }
                (Some(NS_TEXT), "tab") => {
                    for _ in 0..4 {
                        out.push(StyledChar::new(' ', bold, italic));
                    }
                }
                (Some(NS_TEXT), "line-break") => {
                    out.push(StyledChar::new(' ', bold, italic));
                }
                (Some(NS_TEXT), "span") => {
                    let (mut b, mut i) = (bold, italic);
                    if let Some(name) = attr(child, NS_TEXT, "style-name") {
                        let (sb, si) = styles.text_style(name);
                        b |= sb;
                        i |= si;
                    }
                    extract_inline(child, styles, b, i, out);
                }
                (Some(NS_TEXT), "bookmark-start")
                | (Some(NS_TEXT), "bookmark-end")
                | (Some(NS_TEXT), "bookmark")
                | (Some(NS_TEXT), "soft-page-break") => {}
                _ => {
                    extract_inline(child, styles, bold, italic, out);
                }
            }
        }
    }
}

fn unsupported_body_elements(body: Node<'_, '_>) -> Vec<String> {
    let mut found = Vec::new();
    for node in body.descendants().filter(Node::is_element) {
        if node == body {
            continue;
        }
        let ns = node.tag_name().namespace().unwrap_or("");
        let name = node.tag_name().name();
        let ok = matches!(
            (ns, name),
            (NS_TEXT, "h")
                | (NS_TEXT, "p")
                | (NS_TEXT, "span")
                | (NS_TEXT, "s")
                | (NS_TEXT, "tab")
                | (NS_TEXT, "line-break")
                | (NS_TEXT, "bookmark-start")
                | (NS_TEXT, "bookmark-end")
                | (NS_TEXT, "bookmark")
                | (NS_TEXT, "soft-page-break")
        );
        if !ok {
            let label = if ns.is_empty() {
                name.to_string()
            } else {
                format!("{}:{}", short_ns(ns), name)
            };
            if !found.contains(&label) {
                found.push(label);
            }
        }
    }
    found
}

fn short_ns(ns: &str) -> &str {
    match ns {
        NS_TEXT => "text",
        NS_STYLE => "style",
        NS_OFFICE => "office",
        NS_FO => "fo",
        _ => ns.rsplit('/').next().unwrap_or(ns),
    }
}

fn parse_content(content: &str, styles_xml: &str, fallback_title: &str) -> Result<Book> {
    let doc = Document::parse(content).context("parse content.xml")?;
    let styles = merge_styles(content, styles_xml);

    for child in doc.root_element().children().filter(Node::is_element) {
        if child.tag_name().namespace() != Some(NS_OFFICE) {
            bail!(
                "document contains an unsupported top-level element: {}",
                child.tag_name().name()
            );
        }
        if !matches!(
            child.tag_name().name(),
            "font-face-decls" | "automatic-styles" | "body"
        ) {
            bail!(
                "document contains an unsupported top-level element: {}",
                child.tag_name().name()
            );
        }
    }

    let body = doc
        .descendants()
        .find(|n| {
            n.is_element()
                && n.tag_name().namespace() == Some(NS_OFFICE)
                && n.tag_name().name() == "text"
        })
        .ok_or_else(|| anyhow!("ODT has no office:text body"))?;

    let unsupported = unsupported_body_elements(body);
    if !unsupported.is_empty() {
        bail!(
            "This document contains elements scribe cannot edit safely: {}",
            unsupported.join(", ")
        );
    }

    let mut title: Option<String> = None;
    let mut chapters: Vec<Chapter> = Vec::new();
    let mut current: Option<usize> = None;

    for node in body.children().filter(Node::is_element) {
        match (node.tag_name().namespace(), node.tag_name().name()) {
            (Some(NS_TEXT), "h") => {
                let level = attr(node, NS_TEXT, "outline-level")
                    .and_then(|s| s.parse::<u32>().ok())
                    .or_else(|| {
                        attr(node, NS_TEXT, "style-name").and_then(|s| styles.heading_level(s))
                    })
                    .unwrap_or(1);
                let text = text_content(node).trim().to_string();
                if level <= 1 {
                    if title.is_none() {
                        title = Some(text);
                    } else {
                        bail!("document has more than one H1 heading");
                    }
                } else {
                    let (number, chapter_title) = parse_chapter_number(&text);
                    let number = number.unwrap_or((chapters.len() + 1) as u32);
                    chapters.push(Chapter {
                        number,
                        title: chapter_title,
                        paragraphs: Vec::new(),
                    });
                    current = Some(chapters.len() - 1);
                }
            }
            (Some(NS_TEXT), "p") => {
                let style_name = attr(node, NS_TEXT, "style-name").unwrap_or("");
                if let Some(level) = styles.heading_level(style_name) {
                    let text = text_content(node).trim().to_string();
                    if level <= 1 {
                        if title.is_none() {
                            title = Some(text);
                        } else {
                            bail!("document has more than one H1 heading");
                        }
                    } else {
                        let (number, chapter_title) = parse_chapter_number(&text);
                        let number = number.unwrap_or((chapters.len() + 1) as u32);
                        chapters.push(Chapter {
                            number,
                            title: chapter_title,
                            paragraphs: Vec::new(),
                        });
                        current = Some(chapters.len() - 1);
                    }
                    continue;
                }
                let mut chars = Vec::new();
                extract_inline(node, &styles, false, false, &mut chars);
                let para = Para { chars };
                if current.is_none() {
                    chapters.push(Chapter {
                        number: (chapters.len() + 1) as u32,
                        title: "Untitled".to_string(),
                        paragraphs: Vec::new(),
                    });
                    current = Some(chapters.len() - 1);
                }
                chapters[current.unwrap()].paragraphs.push(para);
            }
            (Some(NS_TEXT), "soft-page-break") => {}
            _ => {}
        }
    }

    if chapters.is_empty() {
        chapters.push(Chapter::new(1, "Untitled"));
    }
    for c in &mut chapters {
        c.ensure_paragraph();
    }

    let mut book = Book {
        title: title.unwrap_or_else(|| fallback_title.to_string()),
        chapters,
    };
    book.renumber();
    Ok(book)
}

fn most_common(counts: HashMap<String, usize>) -> Option<String> {
    counts
        .into_iter()
        .filter(|(k, _)| !k.is_empty())
        .max_by_key(|(_, v)| *v)
        .map(|(k, _)| k)
}

fn merge_styles(content: &str, styles_xml: &str) -> StyleMap {
    let mut map = StyleMap::parse(content);
    let from_styles = StyleMap::parse(styles_xml);
    for (k, v) in from_styles.map {
        map.map.entry(k).or_insert(v);
    }
    map
}

fn detect_refs(content: &str, styles_xml: &str) -> StyleRefs {
    let doc = match Document::parse(content) {
        Ok(d) => d,
        Err(_) => return StyleRefs::template(),
    };
    let styles = merge_styles(content, styles_xml);
    let body = match doc.descendants().find(|n| {
        n.is_element()
            && n.tag_name().namespace() == Some(NS_OFFICE)
            && n.tag_name().name() == "text"
    }) {
        Some(b) => b,
        None => return StyleRefs::template(),
    };
    let mut title_style = None;
    let mut chapter_counts: HashMap<String, usize> = HashMap::new();
    let mut body_counts: HashMap<String, usize> = HashMap::new();
    for node in body.children().filter(Node::is_element) {
        match (node.tag_name().namespace(), node.tag_name().name()) {
            (Some(NS_TEXT), "h") => {
                let level = attr(node, NS_TEXT, "outline-level")
                    .and_then(|s| s.parse::<u32>().ok())
                    .or_else(|| {
                        attr(node, NS_TEXT, "style-name").and_then(|s| styles.heading_level(s))
                    })
                    .unwrap_or(1);
                let style = attr(node, NS_TEXT, "style-name").unwrap_or("").to_string();
                if level <= 1 && title_style.is_none() {
                    title_style = Some(style);
                } else if level >= 2 {
                    *chapter_counts.entry(style).or_default() += 1;
                }
            }
            (Some(NS_TEXT), "p") => {
                let style = attr(node, NS_TEXT, "style-name").unwrap_or("").to_string();
                if let Some(level) = styles.heading_level(&style) {
                    if level <= 1 && title_style.is_none() {
                        title_style = Some(style);
                    } else if level >= 2 {
                        *chapter_counts.entry(style).or_default() += 1;
                    }
                } else {
                    *body_counts.entry(style).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    let title = title_style
        .filter(|s| !s.is_empty())
        .map(|s| base_ref(&s, &styles))
        .unwrap_or_else(|| "Standard".to_string());
    let chapter = most_common(chapter_counts)
        .map(|s| base_ref(&s, &styles))
        .unwrap_or_else(|| "Standard".to_string());
    let body = most_common(body_counts)
        .map(|s| base_ref(&s, &styles))
        .unwrap_or_else(|| "Standard".to_string());
    StyleRefs {
        title: valid_ref(title, &styles),
        chapter: valid_ref(chapter, &styles),
        body: valid_ref(body, &styles),
    }
}

fn base_ref(name: &str, styles: &StyleMap) -> String {
    let mut current = name.to_string();
    for _ in 0..8 {
        if !current.starts_with("Mydocs") {
            break;
        }
        let Some(info) = styles.map.get(&current) else {
            break;
        };
        let Some(parent) = &info.parent else {
            break;
        };
        current = parent.clone();
    }
    current
}

fn valid_ref(name: String, styles: &StyleMap) -> String {
    if name.starts_with("Mydocs") {
        return "Standard".to_string();
    }
    if styles.map.contains_key(&name) || name == "Standard" {
        name
    } else {
        "Standard".to_string()
    }
}

fn content_version(content: &str) -> Option<String> {
    let doc = Document::parse(content).ok()?;
    doc.root_element()
        .attribute((NS_OFFICE, "version"))
        .map(str::to_string)
}

fn write_content(
    book: &Book,
    refs: &StyleRefs,
    automatic_styles: &str,
    font_face_decls: &str,
    version: &str,
) -> Result<String> {
    let ns = concat!(
        "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" ",
        "xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" ",
        "xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" ",
        "xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" ",
        "xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" ",
        "xmlns:meta=\"urn:oasis:names:tc:opendocument:xmlns:meta:1.0\" ",
        "xmlns:dc=\"http://purl.org/dc/elements/1.1/\""
    );

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<office:document-content {ns} office:version=\"{}\">\n",
        escape_attr(version)
    ));
    if !font_face_decls.trim().is_empty() {
        out.push_str(font_face_decls.trim());
        out.push('\n');
    }
    out.push_str("<office:automatic-styles>\n");
    if !automatic_styles.trim().is_empty() {
        out.push_str(automatic_styles.trim());
        out.push('\n');
    }
    out.push_str(&format!(
        "<style:style style:name=\"MydocsTitle\" style:family=\"paragraph\" style:parent-style-name=\"{}\"/>\n",
        escape_attr(&refs.title)
    ));
    out.push_str(&format!(
        "<style:style style:name=\"MydocsFirstChapter\" style:family=\"paragraph\" style:parent-style-name=\"{}\"><style:paragraph-properties fo:break-before=\"auto\"/></style:style>\n",
        escape_attr(&refs.chapter)
    ));
    out.push_str(&format!(
        "<style:style style:name=\"MydocsChapter\" style:family=\"paragraph\" style:parent-style-name=\"{}\"><style:paragraph-properties fo:break-before=\"page\"/></style:style>\n",
        escape_attr(&refs.chapter)
    ));
    out.push_str(&format!(
        "<style:style style:name=\"MydocsBody\" style:family=\"paragraph\" style:parent-style-name=\"{}\"/>\n",
        escape_attr(&refs.body)
    ));
    out.push_str("<style:style style:name=\"MydocsBold\" style:family=\"text\"><style:text-properties fo:font-weight=\"bold\"/></style:style>\n");
    out.push_str("<style:style style:name=\"MydocsItalic\" style:family=\"text\"><style:text-properties fo:font-style=\"italic\"/></style:style>\n");
    out.push_str("<style:style style:name=\"MydocsBoldItalic\" style:family=\"text\"><style:text-properties fo:font-weight=\"bold\" fo:font-style=\"italic\"/></style:style>\n");
    out.push_str("</office:automatic-styles>\n");
    out.push_str("<office:body><office:text>\n");
    out.push_str(&format!(
        "<text:h text:style-name=\"MydocsTitle\" text:outline-level=\"1\">{}</text:h>\n",
        encode_text(&book.title)
    ));
    for (i, chapter) in book.chapters.iter().enumerate() {
        let style = if i == 0 {
            "MydocsFirstChapter"
        } else {
            "MydocsChapter"
        };
        let heading = if chapter.title.trim().is_empty() {
            format!("{}.", chapter.number)
        } else {
            format!("{}. {}", chapter.number, chapter.title.trim())
        };
        out.push_str(&format!(
            "<text:h text:style-name=\"{style}\" text:outline-level=\"2\">{}</text:h>\n",
            encode_text(&heading)
        ));
        for para in &chapter.paragraphs {
            if para.chars.is_empty() {
                out.push_str("<text:p text:style-name=\"MydocsBody\"/>\n");
            } else {
                out.push_str("<text:p text:style-name=\"MydocsBody\">");
                out.push_str(&encode_runs(&para.chars));
                out.push_str("</text:p>\n");
            }
        }
    }
    out.push_str("</office:text></office:body>\n</office:document-content>\n");
    Ok(out)
}

fn encode_runs(chars: &[StyledChar]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let bold = chars[i].bold;
        let italic = chars[i].italic;
        let mut j = i;
        while j < chars.len() && chars[j].bold == bold && chars[j].italic == italic {
            j += 1;
        }
        let text = chars[i..j].iter().map(|c| c.ch).collect::<String>();
        let encoded = encode_text(&text);
        if bold || italic {
            let style = match (bold, italic) {
                (true, true) => "MydocsBoldItalic",
                (true, false) => "MydocsBold",
                (false, true) => "MydocsItalic",
                _ => unreachable!(),
            };
            out.push_str(&format!(
                "<text:span text:style-name=\"{style}\">{encoded}</text:span>"
            ));
        } else {
            out.push_str(&encoded);
        }
        i = j;
    }
    out
}

fn encode_text(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            ' ' => {
                let mut n = 1;
                while chars.peek() == Some(&' ') {
                    chars.next();
                    n += 1;
                }
                if n == 1 {
                    out.push_str("<text:s/>");
                } else {
                    out.push_str(&format!("<text:s text:c=\"{n}\"/>"));
                }
            }
            '\t' => out.push_str("<text:tab/>"),
            '\n' => out.push_str("<text:line-break/>"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn strip_mydocs_styles(xml: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while let Some(pos) = xml[i..].find("<style:style") {
        let abs = i + pos;
        out.push_str(&xml[i..abs]);
        let rest = &xml[abs..];
        let Some(gt) = rest.find('>') else {
            out.push_str(rest);
            return out;
        };
        let open = &rest[..=gt];
        if open.contains("style:name=\"Mydocs") {
            if open.ends_with("/>") {
                i = abs + gt + 1;
            } else if let Some(close) = rest.find("</style:style>") {
                i = abs + close + "</style:style>".len();
            } else {
                i = abs + gt + 1;
            }
        } else {
            out.push_str(open);
            i = abs + gt + 1;
        }
    }
    out.push_str(&xml[i..]);
    out
}

fn extract_element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after = &xml[start..];
    let end_open = after.find('>')?;
    if after[..=end_open].ends_with("/>") {
        return Some(after[..=end_open].to_string());
    }
    let close = format!("</{tag}>");
    let end = after.find(&close)? + close.len();
    Some(after[..end].to_string())
}

fn extract_element_inner(xml: &str, tag: &str) -> Option<String> {
    let full = extract_element(xml, tag)?;
    let after = full.find('>')? + 1;
    if full[..after].ends_with("/>") {
        return Some(String::new());
    }
    let close = format!("</{tag}>");
    let end = full.rfind(&close)?;
    Some(full[after..end].to_string())
}

fn update_meta_title(package: &mut OdtPackage, title: &str) {
    let Some(bytes) = package.get("meta.xml") else {
        return;
    };
    let meta = String::from_utf8_lossy(bytes).to_string();
    let open = match meta.find("<dc:title>") {
        Some(p) => p,
        None => return,
    };
    let Some(close_rel) = meta[open..].find("</dc:title>") else {
        return;
    };
    let close = open + close_rel;
    let mut updated = String::new();
    updated.push_str(&meta[..open]);
    updated.push_str("<dc:title>");
    updated.push_str(&escape_attr(title));
    updated.push_str("</dc:title>");
    updated.push_str(&meta[close + "</dc:title>".len()..]);
    package.set("meta.xml", updated.into_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_document_roundtrip() {
        let mut doc = OdtDocument::new("Test Book").unwrap();
        doc.book.chapters[0].title = "First".into();
        doc.book.chapters[0].paragraphs[0] = Para::from_text("Hello bold italic world");
        doc.book.chapters[0].paragraphs[0].set_style_range(6, 10, true, false);
        doc.book.chapters[0].paragraphs[0].set_style_range(11, 17, false, true);
        doc.book.chapters.push(Chapter::new(2, "Second"));
        doc.book.chapters[1].paragraphs[0] = Para::from_text("Another chapter");
        let bytes = doc.to_bytes().unwrap();

        let mut archive = ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
        assert_eq!(archive.by_index(0).unwrap().name().unwrap(), "mimetype");
        drop(archive);

        let mut back = OdtDocument::from_bytes(&bytes, "fallback").unwrap();
        assert_eq!(back.book.title, "Test Book");
        assert_eq!(back.book.chapters.len(), 2);
        assert_eq!(back.book.chapters[0].title, "First");
        assert_eq!(back.book.chapters[1].title, "Second");
        let p = &back.book.chapters[0].paragraphs[0];
        assert!(p.chars[6].bold && !p.chars[6].italic);
        assert!(!p.chars[11].bold && p.chars[11].italic);
        let again = back.to_bytes().unwrap();
        assert_eq!(again.len() > 100, true);
    }

    #[test]
    fn double_roundtrip_does_not_self_parent() {
        let mut doc = OdtDocument::new("Book").unwrap();
        doc.book.chapters.push(Chapter::new(2, "Two"));
        let first = doc.to_bytes().unwrap();
        let mut second = OdtDocument::from_bytes(&first, "Book").unwrap();
        let bytes = second.to_bytes().unwrap();
        let content = String::from_utf8(
            OdtPackage::from_bytes(&bytes)
                .unwrap()
                .get("content.xml")
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(content.contains("parent-style-name=\"BookTitle\""));
        assert!(!content.contains("parent-style-name=\"Mydocs"));
    }

    #[test]
    fn first_chapter_has_no_forced_break() {
        let mut doc = OdtDocument::new("Book").unwrap();
        doc.book.chapters.push(Chapter::new(2, "Two"));
        let bytes = doc.to_bytes().unwrap();
        let content = String::from_utf8(
            OdtPackage::from_bytes(&bytes)
                .unwrap()
                .get("content.xml")
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(content.contains("MydocsFirstChapter"));
        assert!(content.contains("fo:break-before=\"auto\""));
        assert!(content.contains("fo:break-before=\"page\""));
    }

    #[test]
    fn refuses_unsupported_content() {
        let mut doc = OdtDocument::new("Book").unwrap();
        let bytes = doc.to_bytes().unwrap();
        let mut pkg = OdtPackage::from_bytes(&bytes).unwrap();
        let mut content = pkg.content_xml().unwrap();
        content = content.replace("</office:text>", "<text:list/></office:text>");
        pkg.set("content.xml", content.into_bytes());
        let bytes = pkg.to_bytes().unwrap();
        let err = OdtDocument::from_bytes(&bytes, "fallback").unwrap_err();
        assert!(format!("{err}").contains("text:list"));
    }
}

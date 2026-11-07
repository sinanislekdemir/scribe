use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// Named UI themes. The variant names are what appear in `settings.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    #[default]
    Monokai,
    Nord,
    OneDark,
    Paper,
}

impl ThemeName {
    pub const ALL: [ThemeName; 4] = [
        ThemeName::Monokai,
        ThemeName::Nord,
        ThemeName::OneDark,
        ThemeName::Paper,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Monokai => "Monokai",
            ThemeName::Nord => "Nord",
            ThemeName::OneDark => "One Dark",
            ThemeName::Paper => "Paper (light)",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let index = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn palette(self) -> Palette {
        match self {
            ThemeName::Monokai => MONOKAI,
            ThemeName::Nord => NORD,
            ThemeName::OneDark => ONE_DARK,
            ThemeName::Paper => PAPER,
        }
    }
}

/// Every color role used by the TUI. Keeping these together lets the whole
/// interface switch themes without touching the rendering code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub plain: Color,
    pub bold: Color,
    pub italic: Color,
    pub both: Color,
    pub heading: Color,
    pub accent: Color,
    pub dim: Color,
    pub good: Color,
    pub error: Color,
    pub panel_bg: Color,
    pub bar_bg: Color,
    pub editor_bg: Color,
    pub page_bg: Color,
    pub page_border: Color,
    pub select_bg: Color,
    pub select_fg: Color,
    pub selection_bg: Color,
    pub ai_issue_bg: Color,
    pub ai_focus_bg: Color,
    pub cursor_fg: Color,
    pub cursor_bg: Color,
}

const MONOKAI: Palette = Palette {
    plain: Color::Rgb(248, 248, 242),
    bold: Color::Rgb(253, 151, 31),
    italic: Color::Rgb(174, 129, 255),
    both: Color::Rgb(249, 38, 114),
    heading: Color::Rgb(102, 217, 239),
    accent: Color::Rgb(230, 219, 116),
    dim: Color::Rgb(117, 113, 94),
    good: Color::Rgb(166, 226, 46),
    error: Color::Rgb(249, 38, 114),
    panel_bg: Color::Rgb(39, 40, 34),
    bar_bg: Color::Rgb(62, 61, 50),
    editor_bg: Color::Rgb(30, 31, 28),
    page_bg: Color::Rgb(44, 45, 38),
    page_border: Color::Rgb(73, 72, 62),
    select_bg: Color::Rgb(73, 72, 62),
    select_fg: Color::Rgb(248, 248, 242),
    selection_bg: Color::Rgb(73, 74, 63),
    ai_issue_bg: Color::Rgb(62, 61, 50),
    ai_focus_bg: Color::Rgb(90, 78, 42),
    cursor_fg: Color::Rgb(39, 40, 34),
    cursor_bg: Color::Rgb(248, 248, 242),
};

const NORD: Palette = Palette {
    plain: Color::Rgb(216, 222, 233),
    bold: Color::Rgb(208, 135, 112),
    italic: Color::Rgb(180, 142, 173),
    both: Color::Rgb(191, 97, 106),
    heading: Color::Rgb(136, 192, 208),
    accent: Color::Rgb(235, 203, 139),
    dim: Color::Rgb(97, 110, 136),
    good: Color::Rgb(163, 190, 140),
    error: Color::Rgb(191, 97, 106),
    panel_bg: Color::Rgb(59, 66, 82),
    bar_bg: Color::Rgb(70, 80, 102),
    editor_bg: Color::Rgb(46, 52, 64),
    page_bg: Color::Rgb(52, 59, 73),
    page_border: Color::Rgb(76, 86, 106),
    select_bg: Color::Rgb(67, 76, 94),
    select_fg: Color::Rgb(236, 239, 244),
    selection_bg: Color::Rgb(76, 86, 106),
    ai_issue_bg: Color::Rgb(74, 64, 52),
    ai_focus_bg: Color::Rgb(106, 90, 54),
    cursor_fg: Color::Rgb(46, 52, 64),
    cursor_bg: Color::Rgb(216, 222, 233),
};

const ONE_DARK: Palette = Palette {
    plain: Color::Rgb(171, 178, 191),
    bold: Color::Rgb(209, 154, 102),
    italic: Color::Rgb(198, 120, 221),
    both: Color::Rgb(224, 108, 117),
    heading: Color::Rgb(97, 175, 239),
    accent: Color::Rgb(229, 192, 123),
    dim: Color::Rgb(92, 99, 112),
    good: Color::Rgb(152, 195, 121),
    error: Color::Rgb(224, 108, 117),
    panel_bg: Color::Rgb(33, 37, 43),
    bar_bg: Color::Rgb(51, 56, 66),
    editor_bg: Color::Rgb(40, 44, 52),
    page_bg: Color::Rgb(44, 49, 58),
    page_border: Color::Rgb(62, 68, 81),
    select_bg: Color::Rgb(62, 68, 81),
    select_fg: Color::Rgb(220, 223, 228),
    selection_bg: Color::Rgb(58, 64, 75),
    ai_issue_bg: Color::Rgb(59, 53, 43),
    ai_focus_bg: Color::Rgb(90, 74, 42),
    cursor_fg: Color::Rgb(40, 44, 52),
    cursor_bg: Color::Rgb(171, 178, 191),
};

/// A warm, low-glare light theme. Backgrounds avoid pure white and the text
/// avoids pure black to keep long editing sessions easy on the eyes.
const PAPER: Palette = Palette {
    plain: Color::Rgb(62, 58, 51),
    bold: Color::Rgb(138, 90, 31),
    italic: Color::Rgb(107, 78, 138),
    both: Color::Rgb(163, 74, 99),
    heading: Color::Rgb(47, 107, 107),
    accent: Color::Rgb(154, 107, 31),
    dim: Color::Rgb(124, 117, 104),
    good: Color::Rgb(79, 122, 79),
    error: Color::Rgb(176, 82, 82),
    panel_bg: Color::Rgb(237, 230, 214),
    bar_bg: Color::Rgb(227, 217, 196),
    editor_bg: Color::Rgb(246, 241, 230),
    page_bg: Color::Rgb(251, 247, 238),
    page_border: Color::Rgb(217, 207, 188),
    select_bg: Color::Rgb(220, 210, 190),
    select_fg: Color::Rgb(46, 42, 36),
    selection_bg: Color::Rgb(232, 223, 201),
    ai_issue_bg: Color::Rgb(243, 231, 200),
    ai_focus_bg: Color::Rgb(234, 217, 168),
    cursor_fg: Color::Rgb(251, 247, 238),
    cursor_bg: Color::Rgb(62, 58, 51),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("expected an RGB color");
        };
        (0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64) / 255.0
    }

    #[test]
    fn themes_cycle_forward_and_back() {
        assert_eq!(ThemeName::Monokai.next(), ThemeName::Nord);
        assert_eq!(ThemeName::Nord.next(), ThemeName::OneDark);
        assert_eq!(ThemeName::OneDark.next(), ThemeName::Paper);
        assert_eq!(ThemeName::Paper.next(), ThemeName::Monokai);
        assert_eq!(ThemeName::Monokai.prev(), ThemeName::Paper);
        assert_eq!(ThemeName::Paper.prev(), ThemeName::OneDark);
    }

    #[test]
    fn paper_is_the_only_light_theme() {
        for theme in ThemeName::ALL {
            let palette = theme.palette();
            let bg = luminance(palette.editor_bg);
            let fg = luminance(palette.plain);
            assert_ne!(bg, fg, "{} has invisible text", theme.label());
            if theme == ThemeName::Paper {
                assert!(bg > 0.85, "Paper background should be light");
                assert!(fg < 0.35, "Paper text should be dark");
            } else {
                assert!(bg < 0.25, "{} background should be dark", theme.label());
                assert!(fg > 0.55, "{} text should be light", theme.label());
            }
        }
    }

    #[test]
    fn every_theme_has_distinct_surface_colors() {
        for theme in ThemeName::ALL {
            let p = theme.palette();
            assert_ne!(p.editor_bg, p.panel_bg, "{}", theme.label());
            assert_ne!(p.editor_bg, p.bar_bg, "{}", theme.label());
            assert_ne!(p.panel_bg, p.bar_bg, "{}", theme.label());
            assert_ne!(p.panel_bg, p.page_bg, "{}", theme.label());
            assert_ne!(p.select_bg, p.selection_bg, "{}", theme.label());
        }
    }
}

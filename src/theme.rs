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
    Gruvbox,
    Dracula,
    TokyoNight,
    Catppuccin,
    SolarizedDark,
    SolarizedLight,
}

impl ThemeName {
    pub const ALL: [ThemeName; 10] = [
        ThemeName::Monokai,
        ThemeName::Nord,
        ThemeName::OneDark,
        ThemeName::Paper,
        ThemeName::Gruvbox,
        ThemeName::Dracula,
        ThemeName::TokyoNight,
        ThemeName::Catppuccin,
        ThemeName::SolarizedDark,
        ThemeName::SolarizedLight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Monokai => "Monokai",
            ThemeName::Nord => "Nord",
            ThemeName::OneDark => "One Dark",
            ThemeName::Paper => "Paper (light)",
            ThemeName::Gruvbox => "Gruvbox Dark",
            ThemeName::Dracula => "Dracula",
            ThemeName::TokyoNight => "Tokyo Night",
            ThemeName::Catppuccin => "Catppuccin",
            ThemeName::SolarizedDark => "Solarized Dark",
            ThemeName::SolarizedLight => "Solarized Light",
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
            ThemeName::Gruvbox => GRUVBOX,
            ThemeName::Dracula => DRACULA,
            ThemeName::TokyoNight => TOKYO_NIGHT,
            ThemeName::Catppuccin => CATPPUCCIN,
            ThemeName::SolarizedDark => SOLARIZED_DARK,
            ThemeName::SolarizedLight => SOLARIZED_LIGHT,
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

/// Warm, earthy dark theme with a slightly softer contrast than Monokai.
const GRUVBOX: Palette = Palette {
    plain: Color::Rgb(235, 219, 178),
    bold: Color::Rgb(254, 128, 25),
    italic: Color::Rgb(211, 134, 155),
    both: Color::Rgb(251, 73, 52),
    heading: Color::Rgb(131, 165, 152),
    accent: Color::Rgb(250, 189, 47),
    dim: Color::Rgb(146, 131, 116),
    good: Color::Rgb(184, 187, 38),
    error: Color::Rgb(251, 73, 52),
    panel_bg: Color::Rgb(60, 56, 54),
    bar_bg: Color::Rgb(80, 73, 69),
    editor_bg: Color::Rgb(40, 40, 40),
    page_bg: Color::Rgb(50, 48, 47),
    page_border: Color::Rgb(102, 92, 84),
    select_bg: Color::Rgb(80, 73, 69),
    select_fg: Color::Rgb(235, 219, 178),
    selection_bg: Color::Rgb(69, 64, 61),
    ai_issue_bg: Color::Rgb(66, 60, 56),
    ai_focus_bg: Color::Rgb(90, 74, 47),
    cursor_fg: Color::Rgb(40, 40, 40),
    cursor_bg: Color::Rgb(235, 219, 178),
};

/// Purple-heavy dark theme.
const DRACULA: Palette = Palette {
    plain: Color::Rgb(248, 248, 242),
    bold: Color::Rgb(255, 184, 108),
    italic: Color::Rgb(189, 147, 249),
    both: Color::Rgb(255, 121, 198),
    heading: Color::Rgb(139, 233, 253),
    accent: Color::Rgb(241, 250, 140),
    dim: Color::Rgb(98, 114, 164),
    good: Color::Rgb(80, 250, 123),
    error: Color::Rgb(255, 85, 85),
    panel_bg: Color::Rgb(33, 34, 44),
    bar_bg: Color::Rgb(68, 71, 90),
    editor_bg: Color::Rgb(40, 42, 54),
    page_bg: Color::Rgb(47, 49, 65),
    page_border: Color::Rgb(68, 71, 90),
    select_bg: Color::Rgb(68, 71, 90),
    select_fg: Color::Rgb(248, 248, 242),
    selection_bg: Color::Rgb(58, 61, 79),
    ai_issue_bg: Color::Rgb(56, 58, 73),
    ai_focus_bg: Color::Rgb(74, 63, 94),
    cursor_fg: Color::Rgb(40, 42, 54),
    cursor_bg: Color::Rgb(248, 248, 242),
};

/// Cool blue night theme.
const TOKYO_NIGHT: Palette = Palette {
    plain: Color::Rgb(192, 202, 245),
    bold: Color::Rgb(255, 158, 100),
    italic: Color::Rgb(187, 154, 247),
    both: Color::Rgb(247, 118, 142),
    heading: Color::Rgb(125, 207, 255),
    accent: Color::Rgb(224, 175, 104),
    dim: Color::Rgb(86, 95, 137),
    good: Color::Rgb(158, 206, 106),
    error: Color::Rgb(247, 118, 142),
    panel_bg: Color::Rgb(22, 22, 30),
    bar_bg: Color::Rgb(41, 46, 66),
    editor_bg: Color::Rgb(26, 27, 38),
    page_bg: Color::Rgb(31, 35, 53),
    page_border: Color::Rgb(41, 46, 66),
    select_bg: Color::Rgb(41, 46, 66),
    select_fg: Color::Rgb(192, 202, 245),
    selection_bg: Color::Rgb(50, 56, 79),
    ai_issue_bg: Color::Rgb(42, 42, 60),
    ai_focus_bg: Color::Rgb(58, 53, 80),
    cursor_fg: Color::Rgb(26, 27, 38),
    cursor_bg: Color::Rgb(192, 202, 245),
};

/// Soft pastel dark theme.
const CATPPUCCIN: Palette = Palette {
    plain: Color::Rgb(205, 214, 244),
    bold: Color::Rgb(250, 179, 135),
    italic: Color::Rgb(203, 166, 247),
    both: Color::Rgb(245, 194, 231),
    heading: Color::Rgb(137, 180, 250),
    accent: Color::Rgb(249, 226, 175),
    dim: Color::Rgb(127, 132, 156),
    good: Color::Rgb(166, 227, 161),
    error: Color::Rgb(243, 139, 168),
    panel_bg: Color::Rgb(24, 24, 37),
    bar_bg: Color::Rgb(49, 50, 68),
    editor_bg: Color::Rgb(30, 30, 46),
    page_bg: Color::Rgb(35, 35, 52),
    page_border: Color::Rgb(69, 71, 90),
    select_bg: Color::Rgb(49, 50, 68),
    select_fg: Color::Rgb(205, 214, 244),
    selection_bg: Color::Rgb(69, 71, 90),
    ai_issue_bg: Color::Rgb(42, 42, 60),
    ai_focus_bg: Color::Rgb(62, 56, 82),
    cursor_fg: Color::Rgb(30, 30, 46),
    cursor_bg: Color::Rgb(205, 214, 244),
};

const SOLARIZED_DARK: Palette = Palette {
    plain: Color::Rgb(147, 161, 161),
    bold: Color::Rgb(203, 75, 22),
    italic: Color::Rgb(108, 113, 196),
    both: Color::Rgb(211, 54, 130),
    heading: Color::Rgb(42, 161, 152),
    accent: Color::Rgb(181, 137, 0),
    dim: Color::Rgb(88, 110, 117),
    good: Color::Rgb(133, 153, 0),
    error: Color::Rgb(220, 50, 47),
    panel_bg: Color::Rgb(7, 54, 66),
    bar_bg: Color::Rgb(10, 68, 80),
    editor_bg: Color::Rgb(0, 43, 54),
    page_bg: Color::Rgb(3, 47, 58),
    page_border: Color::Rgb(15, 74, 87),
    select_bg: Color::Rgb(11, 60, 72),
    select_fg: Color::Rgb(147, 161, 161),
    selection_bg: Color::Rgb(16, 64, 76),
    ai_issue_bg: Color::Rgb(18, 58, 66),
    ai_focus_bg: Color::Rgb(51, 69, 42),
    cursor_fg: Color::Rgb(0, 43, 54),
    cursor_bg: Color::Rgb(147, 161, 161),
};

const SOLARIZED_LIGHT: Palette = Palette {
    plain: Color::Rgb(88, 110, 117),
    bold: Color::Rgb(203, 75, 22),
    italic: Color::Rgb(108, 113, 196),
    both: Color::Rgb(211, 54, 130),
    heading: Color::Rgb(38, 139, 210),
    accent: Color::Rgb(181, 137, 0),
    dim: Color::Rgb(131, 148, 150),
    good: Color::Rgb(133, 153, 0),
    error: Color::Rgb(220, 50, 47),
    panel_bg: Color::Rgb(238, 232, 213),
    bar_bg: Color::Rgb(228, 221, 200),
    editor_bg: Color::Rgb(253, 246, 227),
    page_bg: Color::Rgb(247, 240, 220),
    page_border: Color::Rgb(217, 210, 190),
    select_bg: Color::Rgb(230, 223, 203),
    select_fg: Color::Rgb(88, 110, 117),
    selection_bg: Color::Rgb(239, 231, 208),
    ai_issue_bg: Color::Rgb(243, 236, 207),
    ai_focus_bg: Color::Rgb(232, 223, 184),
    cursor_fg: Color::Rgb(253, 246, 227),
    cursor_bg: Color::Rgb(88, 110, 117),
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
        assert_eq!(ThemeName::Paper.next(), ThemeName::Gruvbox);
        assert_eq!(ThemeName::SolarizedLight.next(), ThemeName::Monokai);
        assert_eq!(ThemeName::Monokai.prev(), ThemeName::SolarizedLight);
        assert_eq!(ThemeName::Paper.prev(), ThemeName::OneDark);
    }

    #[test]
    fn themes_have_readable_light_or_dark_contrast() {
        let mut light = 0;
        let mut dark = 0;
        for theme in ThemeName::ALL {
            let palette = theme.palette();
            let bg = luminance(palette.editor_bg);
            let fg = luminance(palette.plain);
            assert_ne!(bg, fg, "{} has invisible text", theme.label());
            assert!(
                (bg - fg).abs() > 0.3,
                "{} background and text are too close",
                theme.label()
            );
            if bg > 0.5 {
                light += 1;
                assert!(bg > 0.75, "{} light background is too dark", theme.label());
                assert!(fg < 0.65, "{} light theme text is too light", theme.label());
            } else {
                dark += 1;
                assert!(bg < 0.35, "{} dark background is too light", theme.label());
                assert!(fg > 0.5, "{} dark theme text is too dark", theme.label());
            }
        }
        assert!(light > 0, "expected at least one light theme");
        assert!(dark > 0, "expected at least one dark theme");
    }

    #[test]
    fn every_theme_has_a_unique_editor_background() {
        for (index, theme) in ThemeName::ALL.iter().enumerate() {
            for other in &ThemeName::ALL[index + 1..] {
                assert_ne!(
                    theme.palette().editor_bg,
                    other.palette().editor_bg,
                    "{} and {} share the same editor background",
                    theme.label(),
                    other.label()
                );
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

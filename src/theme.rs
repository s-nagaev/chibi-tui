//! Tokyo Night inspired dark theme + global syntax-highlighting state.

use ratatui::style::Color;
use syntect::highlighting::Theme as SynTheme;
use syntect::parsing::{SyntaxReference, SyntaxSet};

#[derive(Clone, Copy)]
#[allow(dead_code)] // palette fields are consumed incrementally by the UI layer
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub selection: Color,
    pub fg: Color,
    pub dim: Color,
    pub blue: Color,
    pub cyan: Color,
    pub green: Color,
    pub purple: Color,
    pub orange: Color,
    pub red: Color,
    pub yellow: Color,
    pub code_bg: Color,
    /// Slightly lifted tone used inside fenced code blocks so the block reads
    /// as a distinct panel surface against the chat background.
    pub code_panel_bg: Color,
    /// Muted border color framing code blocks.
    pub code_border: Color,
}

impl Theme {
    pub const fn tokyo_night() -> Self {
        Self {
            bg: Color::Rgb(26, 27, 38),            // #1a1b26
            panel: Color::Rgb(21, 22, 31),         // #15161f
            selection: Color::Rgb(41, 46, 59),     // #292e42
            fg: Color::Rgb(192, 202, 245),         // #c0caf5
            dim: Color::Rgb(86, 95, 137),          // #565f89
            blue: Color::Rgb(122, 162, 247),       // #7aa2f7
            cyan: Color::Rgb(125, 207, 255),       // #7dcfff
            green: Color::Rgb(158, 206, 106),      // #9ece6a
            purple: Color::Rgb(187, 154, 247),     // #bb9af7
            orange: Color::Rgb(255, 158, 100),     // #ff9e64
            red: Color::Rgb(247, 118, 142),        // #f7768e
            yellow: Color::Rgb(224, 175, 104),     // #e0af68
            code_bg: Color::Rgb(17, 18, 26),       // #11121a — inline code chip
            code_panel_bg: Color::Rgb(23, 25, 35), // #171923 — code block panel
            code_border: Color::Rgb(47, 61, 104),  // #2f3d68 — dim Tokyo blue
        }
    }
}

/// Global syntect state (syntax definitions + dark color theme).
pub struct Highlighter {
    pub syntaxes: SyntaxSet,
    pub theme: SynTheme,
}

impl Highlighter {
    fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_nonewlines();
        // "base16-eighties.dark" ships with syntect's default themes; it is a
        // well-established dark palette that sits naturally next to the Tokyo
        // Night accents used everywhere else in this app.
        let defaults = syntect::highlighting::ThemeSet::load_defaults();
        let theme = defaults
            .themes
            .get("base16-eighties.dark")
            .cloned()
            .unwrap_or_default();
        Self { syntaxes, theme }
    }

    /// Resolve a fence info string (`rust`, `py`, `bash`, …) into a syntax
    /// reference. Falls back to plain text when nothing matches.
    pub fn syntax_for(&self, lang: &str) -> &SyntaxReference {
        let l = lang.trim().to_ascii_lowercase();
        if l.is_empty() {
            return self.syntaxes.find_syntax_plain_text();
        }
        self.syntaxes
            .find_syntax_by_token(&l)
            .or_else(|| match l.as_str() {
                "py" | "python3" => self.syntaxes.find_syntax_by_token("python"),
                "sh" | "shell" | "console" => self.syntaxes.find_syntax_by_token("bash"),
                "js" => self.syntaxes.find_syntax_by_token("javascript"),
                "ts" => self.syntaxes.find_syntax_by_token("typescript"),
                "rs" => self.syntaxes.find_syntax_by_token("rust"),
                _ => None,
            })
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
    }
}

static HIGHLIGHTER: std::sync::OnceLock<Highlighter> = std::sync::OnceLock::new();

/// Process-wide highlighter (syntax set loading happens at most once).
pub fn highlighter() -> &'static Highlighter {
    HIGHLIGHTER.get_or_init(Highlighter::new)
}

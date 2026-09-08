//! Tokyo Night inspired dark theme + global syntax-highlighting state.

use ratatui::style::Color;
use syntect::highlighting::Theme as SynTheme;
use syntect::parsing::{SyntaxReference, SyntaxSet};

#[derive(Clone, Copy)]
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
    /// Inline code tone: identical to `bg` on purpose — inline code is marked
    /// by its bold accent text, not by a background patch, so nothing cuts a
    /// dark hole into the chat surface mid-line.
    pub code_bg: Color,
    /// Fenced code block interior: identical to `bg` on purpose — the block
    /// is marked by its `code_border` frame (and language label), not by a
    /// separate panel tone, so the interior blends into the chat surface.
    pub code_panel_bg: Color,
    /// Muted border color framing code blocks.
    pub code_border: Color,
    /// Subtle lifted tone for the input zone (the bottom prompt row, chat
    /// column) so the typing area reads as a distinct surface against both
    /// the sidebar panel and the bare chat background.
    pub input_panel_bg: Color,
    /// Sidebar marker role slot: the dot of the currently selected chat
    ///. A slot, not a raw accent, so a future
    /// theme swap only remaps the role.
    pub active_marker: Color,
    /// Sidebar marker role slot: the dot of an inactive thread with an
    /// unseen background reply. Pairs with a
    /// bold name and no row highlight.
    pub unread_activity: Color,
    /// Sidebar marker role slot: the resting dot of a read, unselected
    /// thread.
    pub dot_default: Color,
    /// Log viewer role slots: one per diagnostics tier that has no base
    /// accent of its own. `TRACE` lines take a very dim tone, one step
    /// darker than the dim slot, so the noisiest chatter sinks into the
    /// background.
    pub log_trace: Color,
    /// Log viewer role slot: `DEBUG` lines take the dim gray, visible but
    /// clearly quieter than the default foreground.
    pub log_debug: Color,
    /// Log viewer search matches: every hit
    /// inside the rendered text takes this slot, kept apart from the level
    /// colors so a match stays readable on any level tint. The match the
    /// cursor currently sits on reuses the slot with reversed colors.
    pub log_match: Color,
}

impl Theme {
    pub const fn tokyo_night() -> Self {
        Self {
            bg: Color::Rgb(32, 34, 46),             // #20222e — half tone above panel
            panel: Color::Rgb(21, 22, 31),          // #15161f
            selection: Color::Rgb(41, 46, 59),      // #292e42
            fg: Color::Rgb(192, 202, 245),          // #c0caf5
            dim: Color::Rgb(86, 95, 137),           // #565f89
            blue: Color::Rgb(122, 162, 247),        // #7aa2f7
            cyan: Color::Rgb(125, 207, 255),        // #7dcfff
            green: Color::Rgb(158, 206, 106),       // #9ece6a
            purple: Color::Rgb(187, 154, 247),      // #bb9af7
            orange: Color::Rgb(255, 158, 100),      // #ff9e64
            red: Color::Rgb(247, 118, 142),         // #f7768e
            yellow: Color::Rgb(224, 175, 104),      // #e0af68
            code_bg: Color::Rgb(32, 34, 46),        // #20222e — same as bg: text marks inline code
            code_panel_bg: Color::Rgb(32, 34, 46), // #20222e — same as bg: the frame marks the block
            code_border: Color::Rgb(47, 61, 104),  // #2f3d68 — dim Tokyo blue
            input_panel_bg: Color::Rgb(36, 38, 51), // #242633 — lifted against chat bg
            // Marker role slots reuse the Tokyo Night accents they always
            // rendered with: the selected dot stayed green, an unseen
            // reply lights yellow, a read thread rests dim.
            active_marker: Color::Rgb(158, 206, 106), // #9ece6a, same as green
            unread_activity: Color::Rgb(224, 175, 104), // #e0af68, same as yellow
            dot_default: Color::Rgb(86, 95, 137),     // #565f89, same as dim
            // Log viewer level slots: the two dim tiers that no base accent
            // covers. INFO/WARNING/ERROR/SUCCESS/CRITICAL reuse the base
            // green/yellow/red accents directly.
            log_trace: Color::Rgb(59, 66, 97), // #3b4261, very dim Tokyo tone
            log_debug: Color::Rgb(86, 95, 137), // #565f89, same as dim
            log_match: Color::Rgb(158, 206, 106), // #9ece6a, same as green
        }
    }

    /// Every bundled theme, in stable order. The log-viewer level table is
    /// tested against this list, so a theme that leaves a level slot
    /// unmapped fails in CI instead of shipping an uncolored tier.
    pub fn bundled() -> Vec<Self> {
        vec![Self::tokyo_night()]
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

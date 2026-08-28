//! feat_model_picker_lite: parser for the backend's textual `/model` listing.
//!
//! The picker popup has NO protocol support — it reuses the plain chat
//! pipeline: a bare `/model` request answers with a line-per-model listing in
//! `result.content`, and `/model <n>` answers with a one-line confirmation.
//! Both formats are captured from the REAL backend (`chibi ide --stdio`,
//! 2026-08-28) and committed as the parser's test fixture
//! (`tests/fixtures/model_listing_captured.txt`):
//!
//! ```text
//! 1. Qwen3.8 Max (Alibaba)
//! 2. Qwen3.5 Plus (Alibaba)
//! ...
//! 104. GLM 4.7 FlashX (ZhipuAI)
//! ```
//!
//! Real-output quirks the parser MUST survive (all verified against the
//! backend source, `chibi/runners/ide_transport.py` + `services/user.py`):
//!
//! * the active model's `display_name` is prefixed with `🟢 ` AND suffixed
//!   with a stray U+FE0F variation selector (`f"🟢 {name}️"`) — both are
//!   presentation noise, stripped into [`ModelEntry::active`];
//! * names themselves may contain `/`, spaces, dots and even parentheses
//!   (e.g. `glm-5.3-flash / z-ai`), so the provider is the LAST
//!   `(...)` group of the row, not the first;
//! * an empty configuration answers `No models available.` — a prose
//!   sentence that must parse to ZERO entries (degradation upstream), never
//!   to a bogus row.
//!
//! The design is deliberately lenient: unparseable lines are skipped, and a
//! listing that yields no rows at all is reported as an empty `Vec` so the
//! caller can degrade gracefully (toast + visible raw exchange) instead of
//! guessing.

/// One parsed row of the `/model` listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    /// The row's own 1-based listing number — sent back verbatim as
    /// `/model <number>` on selection (the backend validates
    /// `1 <= n <= len(models)` against THIS numbering).
    pub number: usize,
    /// Display name with the active-marker noise removed.
    pub name: String,
    /// Text of the trailing `(provider)` group, when present.
    pub provider: Option<String>,
    /// The backend flagged this row as the active model (`🟢 ` marker).
    pub active: bool,
}

/// Active-model marker the backend prepends to the active display name.
const ACTIVE_MARKER: char = '\u{1F7E2}'; // 🟢
/// Stray variation selector the backend appends to the marked display name.
const VARIATION_SELECTOR: char = '\u{FE0F}';

/// Parse a bare `/model` result body into model rows.
///
/// Returns one entry per parseable row, in listing order. Rows that do not
/// start with `<digits>.` / `<digits>)` (after trimming) are skipped; a
/// row with no name after the marker/separator stripping is skipped too.
/// Zero rows means "no usable listing" — the caller degrades.
pub fn parse_model_listing(content: &str) -> Vec<ModelEntry> {
    let mut entries = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        // Leading row number: the longest ASCII-digit run.
        let digits_end = line
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(line.len());
        if digits_end == 0 {
            continue;
        }
        let Ok(number) = line[..digits_end].parse::<usize>() else {
            continue;
        };
        // Real format is `N. `; `N)` is accepted leniently.
        let rest = line[digits_end..].trim_start();
        let Some(rest) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) else {
            continue;
        };
        let mut rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }

        // Presentation-only active marker (🟢 prefix).
        let active = rest.starts_with(ACTIVE_MARKER);
        if active {
            rest = rest[ACTIVE_MARKER.len_utf8()..].trim_start();
            if rest.is_empty() {
                continue;
            }
        }

        // Provider = the LAST `(...)` group; names may contain parens/slashes
        // of their own, so scan from the right.
        let (name, provider) = match split_trailing_group(rest) {
            Some((name, provider)) if !provider.is_empty() => (name, Some(provider.to_owned())),
            _ => (rest, None),
        };

        // Strip the stray variation selector the backend appends to marked
        // names (harmless on unmarked rows — it never occurs naturally).
        let name = name
            .trim_end()
            .trim_end_matches(VARIATION_SELECTOR)
            .trim_end();
        if name.is_empty() {
            continue;
        }

        entries.push(ModelEntry {
            number,
            name: name.to_owned(),
            provider,
            active,
        });
    }
    entries
}

/// Split `name (provider)` at the last `(` whose group closes the string.
/// Returns `(name, provider)` or `None` when the row has no trailing group.
fn split_trailing_group(rest: &str) -> Option<(&str, &str)> {
    if !rest.ends_with(')') {
        return None;
    }
    let open = rest.rfind('(')?;
    if open == 0 {
        return None;
    }
    Some((&rest[..open], &rest[open + 1..rest.len() - 1]))
}

/// Parse a `/model <n>` confirmation into the display label the toast shows.
///
/// The real backend answers `` `Selected model: {display_name} ({provider})` ``
/// (see `ide_transport.py`); everything after the `Selected model: ` prefix is
/// shown verbatim. `None` when the body is not a confirmation — the caller
/// falls back to the raw text (honest degradation, no invention).
pub fn parse_selection_confirmation(content: &str) -> Option<String> {
    content
        .trim()
        .strip_prefix("Selected model: ")
        .map(|tail| tail.trim().to_owned())
        .filter(|tail| !tail.is_empty())
}

/// Render one entry back into its canonical listing row (`N. name (provider)`).
pub fn listing_row_label(entry: &ModelEntry) -> String {
    match &entry.provider {
        Some(provider) => format!("{}. {} ({})", entry.number, entry.name, provider),
        None => format!("{}. {}", entry.number, entry.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The REAL captured `/model` listing (see the module docs for provenance)
    /// is the parser's ground truth.
    const CAPTURED: &str = include_str!("../tests/fixtures/model_listing_captured.txt");

    #[test]
    fn parses_the_real_captured_listing_end_to_end() {
        let entries = parse_model_listing(CAPTURED);
        assert_eq!(entries.len(), 104, "every captured row parses");

        let first = &entries[0];
        assert_eq!(first.number, 1);
        assert_eq!(first.name, "Qwen3.8 Max");
        assert_eq!(first.provider.as_deref(), Some("Alibaba"));
        assert!(!first.active);

        let last = &entries[103];
        assert_eq!(last.number, 104);
        assert_eq!(last.name, "GLM 4.7 FlashX");
        assert_eq!(last.provider.as_deref(), Some("ZhipuAI"));

        // Numbers are the row's own 1-based listing index, strictly increasing.
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.number, i + 1, "row {} kept its own number", i + 1);
        }
    }

    #[test]
    fn keeps_names_with_slashes_and_dots_intact() {
        let entries = parse_model_listing(CAPTURED);
        // `51. glm-5.3-flash / z-ai (OpenRouter)` — the `/` stays in the NAME.
        let openrouter = &entries[50];
        assert_eq!(openrouter.number, 51);
        assert_eq!(openrouter.name, "glm-5.3-flash / z-ai");
        assert_eq!(openrouter.provider.as_deref(), Some("OpenRouter"));
        // `13. Google/Gemini 3.5 Flash Lite (Cheaper Inference)`.
        assert_eq!(entries[12].name, "Google/Gemini 3.5 Flash Lite");
    }

    #[test]
    fn parses_active_marker_with_variation_selector_quirk() {
        // Real marked row shape: `N. 🟢 Name️ (Provider)` — U+FE0F before the
        // space-paren (backend quirk `f"🟢 {name}️"`).
        let listing = "1. Plain Model (OpenAI)\n2. \u{1F7E2} Claude Sonnet 5\u{FE0F} (Anthropic)\n";
        let entries = parse_model_listing(listing);
        assert_eq!(entries.len(), 2);
        assert!(!entries[0].active);
        assert_eq!(entries[0].name, "Plain Model");
        assert!(entries[1].active);
        assert_eq!(entries[1].name, "Claude Sonnet 5");
        assert_eq!(entries[1].provider.as_deref(), Some("Anthropic"));
    }

    #[test]
    fn prose_and_garbage_yield_zero_entries() {
        assert!(parse_model_listing("").is_empty());
        assert!(parse_model_listing("No models available.").is_empty());
        assert!(parse_model_listing("hello world\nthis is not a listing").is_empty());
        assert!(parse_model_listing("\n\n   \n").is_empty());
    }

    #[test]
    fn lenient_parse_skips_garbage_lines_between_rows() {
        let listing = "1. Alpha (A)\ngarbage line\nno-number\nxxx. also garbage\n2. Beta (B)\n";
        let entries = parse_model_listing(listing);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Alpha");
        assert_eq!(entries[1].name, "Beta");
        assert_eq!(entries[1].number, 2);
    }

    #[test]
    fn accepts_paren_separator_and_providerless_rows() {
        // `N)` separator accepted; an EMPTY `()` group is kept leniently as
        // part of the name (provider None) — a weird row beats a lost row.
        let entries = parse_model_listing("1) Bare Model\n2. Nameless ()\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].number, 1);
        assert_eq!(entries[0].name, "Bare Model");
        assert_eq!(entries[0].provider, None);
        assert_eq!(entries[1].name, "Nameless ()");
        assert_eq!(entries[1].provider, None);
    }

    #[test]
    fn nested_parentheses_in_names_keep_the_last_group_as_provider() {
        let entries = parse_model_listing("7. Weird (Inner) Name (Real Provider)\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Weird (Inner) Name");
        assert_eq!(entries[0].provider.as_deref(), Some("Real Provider"));
    }

    #[test]
    fn selection_confirmation_extracts_the_display_tail() {
        assert_eq!(
            parse_selection_confirmation("Selected model: GLM 5.2 (ZhipuAI)").as_deref(),
            Some("GLM 5.2 (ZhipuAI)")
        );
        assert_eq!(
            parse_selection_confirmation("  Selected model: Kimi K3 (Cheaper Inference)  \n")
                .as_deref(),
            Some("Kimi K3 (Cheaper Inference)")
        );
        assert_eq!(parse_selection_confirmation(""), None);
        assert_eq!(parse_selection_confirmation("No models available."), None);
        assert_eq!(parse_selection_confirmation("random chatter"), None);
    }

    #[test]
    fn row_label_roundtrips_the_canonical_shape() {
        let entries = parse_model_listing("12. Glm 5.2 (Cheaper Inference)\n3. Bare\n");
        assert_eq!(
            listing_row_label(&entries[0]),
            "12. Glm 5.2 (Cheaper Inference)"
        );
        assert_eq!(listing_row_label(&entries[1]), "3. Bare");
    }
}

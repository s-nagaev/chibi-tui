//! Markdown rendering: pulldown-cmark events → styled ratatui lines.
//!
//! Supports headings, bold/italic/strikethrough, inline code, fenced code
//! blocks with real syntax highlighting (syntect) rendered as a bordered
//! panel with a language label, ordered/unordered/task lists, block quotes,
//! horizontal rules, links and true column-aligned tables.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthStr;

use crate::theme::{self, Theme};

/// One rendered markdown line.
pub type MdLine = Line<'static>;

/// Renders a markdown string into styled lines.
pub fn render(md: &str, theme: &Theme) -> Vec<MdLine> {
    let parser = Parser::new_ext(md, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES);

    let mut out: Vec<MdLine> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();

    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();

    let mut list_counters: Vec<u64> = Vec::new();
    let mut quote_depth: usize = 0;
    let mut inline_style = Style::new();

    // ---- table collection state ----
    let mut table_cells: Vec<Vec<String>> = Vec::new();
    let mut table_aligns: Vec<Alignment> = Vec::new();
    let mut cur_row: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut link_dest: Option<String> = None;

    let base_fg = theme.fg;

    /// Push accumulated spans as a finished line.
    macro_rules! flush_line {
        () => {{
            out.push(Line::from(std::mem::take(&mut spans)));
        }};
    }

    /// Append text honoring quote depth (splits on newlines into lines).
    macro_rules! push_text {
        ($text:expr, $style:expr) => {{
            let text: &str = $text;
            let style = $style;
            let pieces: Vec<&str> = text.split('\n').collect();
            for (i, piece) in pieces.iter().enumerate() {
                if i > 0 {
                    flush_line!();
                    if quote_depth > 0 {
                        spans.push(Span::styled(
                            "\u{2502} ".repeat(quote_depth),
                            Style::new().fg(theme.green),
                        ));
                    }
                }
                if !piece.is_empty() {
                    spans.push(Span::styled((*piece).to_string(), style));
                }
            }
        }};
    }

    /// Append plain text to the currently open table cell (if any).
    macro_rules! table_push {
        ($text:expr) => {{
            if cur_row.is_empty() {
                cur_row.push(String::new());
            }
            if let Some(cell) = cur_row.last_mut() {
                cell.push_str($text);
            }
        }};
    }

    for event in parser {
        match event {
            // ---------- fenced / indented code blocks ----------
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_line!();
                in_code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    _ => String::new(),
                };
            }
            Event::Text(t) if in_code_block => {
                code_buf.push_str(&t);
            }
            Event::End(TagEnd::CodeBlock) => {
                flush_code_block(&mut out, &code_buf, &code_lang, theme);
                out.push(Line::from(""));
                in_code_block = false;
            }

            // ---------- headings ----------
            Event::Start(Tag::Heading { level, .. }) => {
                flush_line!();
                inline_style = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => {
                        Style::new().fg(theme.blue).add_modifier(Modifier::BOLD)
                    }
                    _ => Style::new().fg(theme.purple).add_modifier(Modifier::BOLD),
                };
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_line!();
                out.push(Line::from(""));
                inline_style = Style::new();
            }

            // ---------- tables ----------
            Event::Start(Tag::Table(aligns)) => {
                flush_line!();
                in_table = true;
                table_cells.clear();
                table_aligns = aligns.to_vec();
                cur_row.clear();
            }
            Event::Text(t) if in_table => table_push!(t.as_ref()),
            Event::Code(code) if in_table => table_push!(&code),
            Event::InlineHtml(html) if in_table => table_push!(html.as_ref()),
            Event::SoftBreak | Event::HardBreak if in_table => {}
            Event::End(TagEnd::TableCell) => {
                cur_row.push(String::new());
            }
            Event::End(TagEnd::TableHead) => {
                if !cur_row.is_empty() {
                    // Drop the sentinel appended when the LAST cell closed,
                    // otherwise a phantom trailing column appears.
                    let popped = cur_row.pop();
                    debug_assert!(popped.is_some_and(|c| c.is_empty()));
                    table_cells.push(std::mem::take(&mut cur_row));
                }
            }
            Event::End(TagEnd::TableRow) => {
                if !cur_row.is_empty() {
                    let popped = cur_row.pop();
                    debug_assert!(popped.is_some_and(|c| c.is_empty()));
                    table_cells.push(std::mem::take(&mut cur_row));
                }
            }
            Event::End(TagEnd::Table) => {
                render_table(&mut out, &table_cells, &table_aligns, theme);
                out.push(Line::from(""));
                in_table = false;
            }

            // ---------- lists ----------
            Event::Start(Tag::List(start)) => {
                flush_line!();
                list_counters.push(start.unwrap_or(1));
            }
            Event::End(TagEnd::List(_)) => {
                list_counters.pop();
                if list_counters.is_empty() {
                    flush_line!();
                }
            }
            Event::Start(Tag::Item) => {
                flush_line!();
                let bullet = match list_counters.last_mut() {
                    Some(n) => {
                        let s = format!("{n}.");
                        *n += 1;
                        s
                    }
                    None => "\u{2022}".to_string(),
                };
                spans.push(Span::styled(
                    format!("{bullet} "),
                    Style::new().fg(theme.orange),
                ));
            }
            Event::End(TagEnd::Item) => {
                flush_line!();
            }

            // ---------- task list markers ----------
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                spans.push(Span::styled(
                    mark.to_string(),
                    Style::new().fg(theme.orange),
                ));
            }

            // ---------- block quotes ----------
            Event::Start(Tag::BlockQuote(_)) => {
                flush_line!();
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush_line!();
                quote_depth -= 1;
            }

            // ---------- horizontal rules ----------
            Event::Rule => {
                flush_line!();
                out.push(Line::from(Span::styled(
                    "\u{2500}".repeat(24),
                    Style::new().fg(theme.selection),
                )));
            }

            // ---------- inline styling ----------
            Event::Start(Tag::Strong) => {
                inline_style = inline_style.add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Strong) => {
                inline_style = inline_style.remove_modifier(Modifier::BOLD);
            }
            Event::Start(Tag::Emphasis) => {
                inline_style = inline_style.add_modifier(Modifier::ITALIC);
            }
            Event::End(TagEnd::Emphasis) => {
                inline_style = inline_style.remove_modifier(Modifier::ITALIC);
            }
            Event::Start(Tag::Strikethrough) => {
                inline_style = inline_style.add_modifier(Modifier::CROSSED_OUT);
            }
            Event::End(TagEnd::Strikethrough) => {
                inline_style = inline_style.remove_modifier(Modifier::CROSSED_OUT);
            }
            Event::Code(code) => {
                if quote_depth > 0 {
                    spans.push(Span::styled(
                        "\u{2502} ".repeat(quote_depth),
                        Style::new().fg(theme.green),
                    ));
                }
                spans.push(Span::styled(
                    format!(" {code} "),
                    Style::new()
                        .fg(theme.green)
                        .bg(theme.code_bg)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                inline_style = Style::new()
                    .fg(theme.cyan)
                    .add_modifier(Modifier::UNDERLINED);
                link_dest = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                if let Some(dest) = link_dest.take() {
                    spans.push(Span::styled(
                        format!(" ({dest})"),
                        Style::new().fg(theme.dim),
                    ));
                }
                inline_style = Style::new();
            }

            // ---------- text flow ----------
            Event::Text(t) => {
                push_text!(&t, inline_style.fg(base_fg));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line!();
                if quote_depth > 0 {
                    spans.push(Span::styled(
                        "\u{2502} ".repeat(quote_depth),
                        Style::new().fg(theme.green),
                    ));
                }
            }
            Event::InlineHtml(html) | Event::Html(html) => {
                push_text!(html.as_ref(), Style::new().fg(theme.dim));
            }
            Event::FootnoteReference(name) => {
                spans.push(Span::styled(
                    format!("[^{name}]"),
                    Style::new().fg(theme.dim),
                ));
            }
            Event::InlineMath(m) | Event::DisplayMath(m) => {
                spans.push(Span::styled(
                    format!(" ${m}$ "),
                    Style::new().fg(theme.purple),
                ));
            }
            _ => {}
        }
    }

    flush_line!();
    out
}

/// Render one fenced code block as a bordered panel with a language label
/// and per-token syntax highlighting via syntect.
fn flush_code_block(out: &mut Vec<MdLine>, body: &str, lang: &str, theme: &Theme) {
    let hl = theme::highlighter();

    // Top border with the language label baked in: ╭─ rust ───╮
    let label = if lang.trim().is_empty() {
        "text".to_string()
    } else {
        lang.trim().to_string()
    };
    let body_width = body
        .lines()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(10)
        .clamp(12, 70);
    // Every row of the panel (borders included) is exactly `width` cells:
    // 2 borders + symmetric 2-column gutters around the widest code line,
    // plus headroom so the label never overflows.
    let width = (body_width + 6).max(label.width() + 8);

    out.push(Line::from(vec![
        Span::styled("\u{256d}", Style::new().fg(theme.code_border)),
        Span::styled("\u{2500} ", Style::new().fg(theme.code_border)),
        Span::styled(label.clone(), Style::new().fg(theme.dim)),
        Span::styled(" ", Style::new().fg(theme.code_border)),
        Span::styled(
            // Fixed overhead around the label: ╭ ─ ␣ ␣ … ╮ → 5 cells.
            "\u{2500}".repeat(width - label.width() - 5),
            Style::new().fg(theme.code_border),
        ),
        Span::styled("\u{256e}", Style::new().fg(theme.code_border)),
    ]));

    // Highlighted body lines, painted on the chat background: the
    // `code_border` frame is the only separator, the interior blends in.
    let syntax = hl.syntax_for(lang);
    let mut hlines = HighlightLines::new(syntax, &hl.theme);
    let panel_bg = Style::new().bg(theme.code_panel_bg);
    let border_style = Style::new().fg(theme.code_border).bg(theme.code_panel_bg);
    let src = body.trim_end_matches('\n');
    // Inner width between the two borders; 2-column gutters on each side.
    let inner_width = width.saturating_sub(2);
    for line in LinesWithEndings::from(src) {
        let Ok(regions) = hlines.highlight_line(line, &hl.syntaxes) else {
            continue;
        };
        // Row layout: `│` + left gutter + tokens + right pad + `│`.
        // The right border span is ALWAYS appended last, so the panel stays
        // a closed rectangle even when an overlong token must be clipped.
        let mut row_spans: Vec<Span<'static>> = vec![Span::styled("\u{2502}", border_style)];
        row_spans.push(Span::styled("  ", panel_bg));
        let mut used = 2usize;
        for (style, chunk) in regions {
            let trimmed = chunk.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue;
            }
            let w = UnicodeWidthStr::width(trimmed);
            if used + w > inner_width {
                break; // line too long: clip here, keep the right border intact
            }
            used += w;
            row_spans.push(Span::styled(
                trimmed.to_string(),
                panel_bg.fg(ratatui_color(style.foreground)),
            ));
        }
        // Right padding seals the line to exactly `width` cells,
        // mirroring the 2-column left gutter.
        if inner_width > used {
            row_spans.push(Span::styled(" ".repeat(inner_width - used), panel_bg));
        }
        row_spans.push(Span::styled("\u{2502}", border_style));
        out.push(Line::from(row_spans));
    }

    // Bottom border: same `width` cells as the top — ╰ + (width-2) dashes + ╯.
    out.push(Line::from(Span::styled(
        format!(
            "\u{2570}{}\u{256f}",
            "\u{2500}".repeat(width.saturating_sub(2))
        ),
        Style::new().fg(theme.code_border),
    )));
}

/// Render a collected table with column borders, a bold header row and
/// per-column alignment.
fn render_table(out: &mut Vec<MdLine>, rows: &[Vec<String>], aligns: &[Alignment], theme: &Theme) {
    if rows.is_empty() {
        return;
    }
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if ncols == 0 {
        return;
    }
    let align_of = |i: usize| -> Alignment { aligns.get(i).copied().unwrap_or(Alignment::None) };

    // Column widths (min 3 so borders never collapse).
    let mut widths = vec![3usize; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
        }
    }

    let hbar = Span::styled("\u{2502}", Style::new().fg(theme.dim));

    let make_row = |cells: &[String], header: bool| -> Line<'static> {
        let mut spans: Vec<Span<'static>> = vec![hbar.clone()];
        for (i, colw) in widths.iter().enumerate() {
            let raw = cells.get(i).map(String::as_str).unwrap_or("");
            let w = UnicodeWidthStr::width(raw);
            let pad = colw.saturating_sub(w);
            let (left, right) = match (header, align_of(i)) {
                (_, Alignment::Right) => (pad, 1),
                (_, Alignment::Center) => (pad / 2, pad - pad / 2 + 1),
                _ => (0, pad + 1), // left-aligned (default): pad on the right
            };
            if left > 0 {
                spans.push(Span::raw(" ".repeat(left)));
            }
            spans.push(if header {
                Span::styled(
                    raw.to_string(),
                    Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(raw.to_string(), Style::new().fg(theme.fg))
            });
            spans.push(Span::raw(" ".repeat(right)));
            spans.push(hbar.clone());
        }
        Line::from(spans)
    };

    // Junction alignment: in `make_row` each column spans `w + 2` cells
    // border-to-border (`w` content + 1 leading pad + 1 trailing pad), and
    // the border/junction character occupies one of those slots. The
    // separator must therefore emit `w + 1` dashes per segment, with `┼`
    // REPLACING the next border. Emitting `w + 2` here added one extra dash
    // per column, so drift accumulated left-to-right (+1, +2, …) and every
    // subsequent `┼`/`┤` landed past the `│` above/below — the reported
    // "table shift".
    let separator = {
        let mut s = String::from("\u{251c}");
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                s.push('\u{253c}');
            }
            s.push_str(&"\u{2500}".repeat(w + 1));
        }
        s.push('\u{2524}');
        Span::styled(s, Style::new().fg(theme.dim))
    };

    let mut iter = rows.iter();
    if let Some(head) = iter.next() {
        out.push(make_row(head, true));
    }
    out.push(Line::from(separator));
    for row in iter {
        out.push(make_row(row, false));
    }
}

/// Map an RGB color to ratatui.
fn ratatui_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use unicode_width::UnicodeWidthChar;

    /// Border/junction characters used by the table renderer.
    const TABLE_BORDERS: &str = "|\u{2502}\u{251c}\u{2524}\u{252c}\u{2534}\u{253c}";

    /// Extract the char positions of all border chars in a rendered line.
    fn border_positions(line: &MdLine) -> Vec<usize> {
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        plain
            .chars()
            .enumerate()
            .filter(|(_, c)| TABLE_BORDERS.contains(*c))
            .map(|(i, _)| i)
            .collect()
    }

    /// Display-column position of every border char in a rendered line.
    /// (Char indices are insufficient once wide cells are present.)
    fn display_border_positions(line: &MdLine) -> Vec<usize> {
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let mut acc = 0usize;
        let mut out = Vec::new();
        for c in plain.chars() {
            if TABLE_BORDERS.contains(c) {
                out.push(acc);
            }
            acc += c.width().unwrap_or(1);
        }
        out
    }

    fn to_plain(lines: &[MdLine]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_commonmark_without_panic() {
        let theme = Theme::tokyo_night();
        let md = "# Title\n\nSome **bold**, *italic*, `code` text.\n\n- item one\n- item two\n\n1. first\n2. second\n\n> quoted thought\n\n```rust\nlet x = 42;\n```\n";
        let lines = render(md, &theme);
        assert!(!lines.is_empty());
        let plain = to_plain(&lines);
        assert!(plain.contains("Title"));
        assert!(plain.contains("item one"));
        assert!(plain.contains("let x = 42;"));
    }

    #[test]
    fn empty_input_yields_single_empty_line() {
        let theme = Theme::tokyo_night();
        assert_eq!(render("", &theme).len(), 1);
    }

    #[test]
    fn code_block_has_label_and_border() {
        let theme = Theme::tokyo_night();
        let md = "```python\ndef f():\n    return 1\n```\n";
        let plain = to_plain(&render(md, &theme));
        assert!(plain.contains("╭─ python"), "top border w/ label: {plain}");
        assert!(plain.contains("╰"), "bottom border missing");
        assert!(plain.contains("def f():"));
    }

    #[test]
    fn table_is_column_aligned_with_borders() {
        let theme = Theme::tokyo_night();
        let md = "| Model | Context |\n|---|---:|\n| gpt-x | 128 |\n| mini | 8 |\n";
        let plain = to_plain(&render(md, &theme));
        assert!(plain.contains('│'), "vertical borders: {plain}");
        assert!(plain.contains('├'), "header separator: {plain}");
        // Header styled bold-blue; numbers must be right aligned per `---:`.
        let rows: Vec<&str> = plain.lines().filter(|l| l.contains('│')).collect();
        assert_eq!(rows.len(), 3, "3 table rows expected: {rows:?}");
        let num_row = rows[2];
        assert!(
            num_row.trim_end().ends_with('│'),
            "right-aligned number touches right border: {num_row}"
        );
    }

    /// Regression for the user-reported table shift: the separator's `┼`
    /// junctions used to drift +N columns left-to-right (one extra dash per
    /// column), so the closing `┤` never met the `│` of header/data rows.
    #[test]
    fn table_separator_junctions_align_with_row_borders() {
        let theme = Theme::tokyo_night();
        // Right-aligned LAST column reproduces the original report exactly.
        let md = "| Model | Provider | Context |\n|---|---|---:|\n\
                  | glm-5.2 | ZhipuAI | 200k |\n\
                  | kimi-k2.7 | MoonshotAI | 256k |\n\
                  | deepseek-v4 | DeepSeek | 128k |\n";
        let lines: Vec<MdLine> = render(md, &theme)
            .into_iter()
            .filter(|l| !border_positions(l).is_empty())
            .collect();
        assert_eq!(lines.len(), 5, "header+sep+3 data rows expected");

        let reference = border_positions(&lines[0]); // header row borders
        for l in lines.iter() {
            assert_eq!(
                border_positions(l),
                reference,
                "border/junction positions diverge between rows"
            );
        }
    }

    /// Wide characters must be measured in display columns, not bytes.
    /// Cyrillic is 2 bytes/1 col, CJK is 3 bytes/2 cols, box-drawing 3
    /// bytes/1 col — byte-based math would misalign every column here.
    #[test]
    fn table_columns_stay_aligned_with_wide_chars() {
        let theme = Theme::tokyo_night();
        let md = "| Модель | 模型 | ┃wide┃ |\n|---|---|---|\n\
                  | кириллица | 中文测试 | ╞═╡ |\n";
        let lines: Vec<MdLine> = render(md, &theme)
            .into_iter()
            .filter(|l| !border_positions(l).is_empty())
            .collect();
        assert_eq!(lines.len(), 3, "header+sep+1 data row expected");

        let reference = display_border_positions(&lines[0]);
        for l in lines.iter() {
            assert_eq!(
                display_border_positions(l),
                reference,
                "display-column positions diverge between rows with wide chars"
            );
        }
    }

    /// Code panel top border, body and bottom border must share one width.
    #[test]
    fn code_panel_borders_share_width() {
        let theme = Theme::tokyo_night();
        let md = "```rust\nlet x = 42;\n```\n";
        let widths: Vec<usize> = render(md, &theme)
            .iter()
            .map(|l| {
                let plain: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                UnicodeWidthStr::width(plain.as_str())
            })
            .filter(|w| *w > 0)
            .collect();
        assert_eq!(widths[0], widths[1], "top border wider/narrower than body");
        assert_eq!(
            widths[0], widths[2],
            "bottom border off-by-one vs top (must be width-1 dashes)"
        );
    }

    #[test]
    fn code_block_two_column_gutters() {
        let theme = Theme::tokyo_night();
        let md = "```rust\nlet some_longer_variable_name = 42;\n```\n";
        let lines: Vec<_> = render(md, &theme)
            .into_iter()
            .filter(|l| {
                let p: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                UnicodeWidthStr::width(p.as_str()) > 0
            })
            .collect();
        let plain: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // Body row layout: `│` + 2-col left gutter + code + right pad + `│`.
        assert!(
            plain.starts_with("\u{2502}  "),
            "row must start with border + 2-col left gutter: {plain:?}"
        );
        assert!(
            plain.ends_with("  \u{2502}"),
            "2-col right gutter then closing border required: {plain:?}"
        );
    }

    /// Regression for iteration-3 diagnosis: body rows never emitted the
    /// right border `│`, leaving every code panel open on the right.
    #[test]
    fn code_panel_body_rows_are_closed() {
        let theme = Theme::tokyo_night();
        let md = "```python\ndef f():\n    return 1\n```\n";
        // `render` opens with an empty flush_line line; the panel follows.
        let rows: Vec<String> = render(md, &theme)
            .iter()
            .skip(1) // leading empty flush_line
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let body: Vec<&String> = rows
            .iter()
            .skip(1) // top border
            .take_while(|r| !r.starts_with('\u{2570}')) // stop at bottom border
            .collect();
        assert!(!body.is_empty(), "panel must have body rows");
        for (i, r) in body.iter().enumerate() {
            assert!(
                r.starts_with('\u{2502}') && r.ends_with('\u{2502}'),
                "body row {i} is not closed by borders: {r:?}"
            );
        }
    }

    /// Overlong code lines must be clipped so the closing `│` survives.
    #[test]
    fn long_code_lines_keep_right_border() {
        let theme = Theme::tokyo_night();
        let md = format!("```text\n{}\nshort\n```\n", "x".repeat(200));
        let widths: Vec<usize> = render(&md, &theme)
            .iter()
            .map(|l| {
                let plain: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                UnicodeWidthStr::width(plain.as_str())
            })
            .filter(|w| *w > 0)
            .collect();
        assert!(widths.len() >= 4);
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "clipped panel must keep uniform width: {widths:?}"
        );
        // `render` opens with an empty flush_line line; the panel follows
        // at index 1 (unfiltered): 0 = blank, 1 = top border, 2 = body.
        let plain: String = render(&md, &theme)[2]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            UnicodeWidthStr::width(plain.trim_end()), // trailing bg pad only
            widths[1],
            "clipped row must reach full width incl. closing border"
        );
        assert!(plain.ends_with('\u{2502}'), "right border lost: {plain:?}");
    }

    /// Code surfaces blend into the chat: both background role slots equal
    /// `theme.bg`, so neither the fenced block interior nor the inline code
    /// chip cuts a dark patch into the chat surface — the `code_border`
    /// frame (and the inline text itself) is the only separator.
    #[test]
    fn code_surfaces_share_chat_background() {
        let theme = Theme::tokyo_night();
        assert_eq!(theme.code_panel_bg, theme.bg, "panel tone must equal bg");
        assert_eq!(theme.code_bg, theme.bg, "inline chip tone must equal bg");

        // Fenced block: interior, gutters, padding and side borders all
        // carry the unified background.
        let md = "```rust\nlet x = 42;\n```\n";
        let rows: Vec<Vec<Span<'static>>> = render(md, &theme)
            .iter()
            .filter(|l| {
                let p: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                p.starts_with('\u{2502}')
            })
            .map(|l| l.spans.clone())
            .collect();
        assert!(!rows.is_empty(), "panel body rows expected");
        for row in &rows {
            for span in row {
                assert_eq!(
                    span.style.bg,
                    Some(theme.bg),
                    "panel span {:?} left the chat background",
                    span.content
                );
            }
        }

        // Inline code chip: no hard cutout mid-line.
        let lines = render("run `make all` now", &theme);
        let chip: Vec<_> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content.contains("make all"))
            .collect();
        assert_eq!(chip.len(), 1, "one inline chip span expected");
        assert_eq!(chip[0].style.bg, Some(theme.bg));
    }
}

#[cfg(test)]
mod diag3 {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn diag_correlation_list() {
        let theme = Theme::tokyo_night();
        let md = "Every request carries correlation metadata:\n\n- `request_id` — unique per request, used for cancel\n- `thread_id` — stable per chat\n- `prompt` — the actual user text\n- `workspace_root` — absolute path, required\n";
        for (i, l) in render(md, &theme).iter().enumerate() {
            let plain: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("[{i}] {plain}");
        }
    }
}

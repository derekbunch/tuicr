//! Rendered markdown for comment bodies and the PR description, used when the
//! `render_markdown` config key is on.
//!
//! `cmark::highlight` colours the source and keeps every marker, so its output
//! maps byte for byte onto what was typed (the comment editor depends on that).
//! This module lays the document out instead: markers are dropped, list items
//! get bullets, block quotes and GitHub alerts get a bar, tables get columns,
//! and prose is word-wrapped. The output is final rows no wider than `width`,
//! so callers neither wrap nor measure it again.
//!
//! Soft line breaks render as line breaks, as GitHub does for comments: review
//! comments are written line by line, and reflowing them would merge separate
//! remarks into one paragraph.

use std::mem;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::cmark::MarkdownPalette;
use super::{HighlightedSpans, SyntaxHighlighter};
use crate::vcs::TAB_WIDTH;

const QUOTE_BAR: &str = "│ ";
const CELL_SEPARATOR: &str = " │ ";
const HEADER_SEPARATOR: &str = "─┼─";
/// Rules stop here, so a body rendered unwrapped (remote review comments) does
/// not draw one as wide as `usize::MAX`.
const RULE_MAX_WIDTH: usize = 80;

/// Lay out `content` as rows at most `width` columns wide.
///
/// With `hl`, rows carry the active theme's markdown and code colours. Without
/// it they are unstyled, which is enough to count rows: colours never change
/// the layout, and height counters have no theme to hand.
pub(super) fn render(
    hl: Option<&SyntaxHighlighter>,
    content: &str,
    width: usize,
) -> Vec<HighlightedSpans> {
    let plain = MarkdownPalette::default();
    let palette = hl.map_or(&plain, |hl| &hl.markdown_palette);
    let mut renderer = MarkdownRenderer::new(hl, palette, width);
    for event in Parser::new_ext(content, options()) {
        renderer.event(event);
    }
    renderer.finish()
}

fn options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    // GitHub alerts: `> [!NOTE]` and friends arrive as a kinded block quote.
    options.insert(Options::ENABLE_GFM);
    options
}

/// A block that prefixes every row inside it.
enum Container {
    /// A block quote or alert: a bar in this style on every row.
    Quote(Style),
    /// A list item: its marker on the item's first row, blanks after.
    Item {
        marker: Option<String>,
        indent: usize,
    },
}

/// A fenced or indented code block being collected.
struct CodeBlock<'a> {
    highlighter: Option<syntect::easy::HighlightLines<'a>>,
    text: String,
}

/// Raw HTML being reduced to its text; `html_action` says what each tag does.
#[derive(Default)]
struct HtmlState {
    /// Inside an HTML block, whose blank lines are leftovers of dropped tags.
    in_block: bool,
    /// Inside a `<!-- -->` comment that has not closed yet.
    in_comment: bool,
    /// A tag cut off at the end of the previous HTML event.
    carry: String,
    /// Open styling tags, with the style-stack depth to restore on close.
    open: Vec<(String, usize)>,
}

/// What a recognised HTML tag turns into. Unknown tags stay literal text, so a
/// bare `Vec<T>` in prose is not swallowed.
enum HtmlAction {
    Style(fn(&MarkdownPalette) -> Style),
    Break,
    AltText,
    Drop,
    Literal,
}

/// The tags GitHub and its bots emit in review bodies (`<details>`,
/// `<summary>`, `<picture>`, `<sub>`, ...), mapped to markdown equivalents.
fn html_action(name: &str) -> HtmlAction {
    match name {
        "b" | "strong" | "summary" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            HtmlAction::Style(|p| p.bold.add_modifier(Modifier::BOLD))
        }
        "em" | "i" => HtmlAction::Style(|p| p.italic.add_modifier(Modifier::ITALIC)),
        "code" | "kbd" | "samp" | "tt" => HtmlAction::Style(|p| p.code),
        "del" | "s" | "strike" => {
            HtmlAction::Style(|p| p.strike.add_modifier(Modifier::CROSSED_OUT))
        }
        "br" => HtmlAction::Break,
        "img" => HtmlAction::AltText,
        "a" | "abbr" | "details" | "div" | "hr" | "ins" | "mark" | "p" | "picture" | "small"
        | "source" | "span" | "sub" | "sup" | "u" => HtmlAction::Drop,
        _ => HtmlAction::Literal,
    }
}

/// A table being collected: cells are inline spans, laid out at the end.
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<HighlightedSpans>>,
    header_rows: usize,
}

struct MarkdownRenderer<'a> {
    hl: Option<&'a SyntaxHighlighter>,
    palette: &'a MarkdownPalette,
    width: usize,
    rows: Vec<HighlightedSpans>,
    containers: Vec<Container>,
    /// Next number for each open list; `None` for a bulleted list.
    lists: Vec<Option<u64>>,
    /// Inline style stack; each entry is already patched onto its parent.
    styles: Vec<Style>,
    /// Inline text of the block being collected, `\n` marking line breaks.
    inline: HighlightedSpans,
    /// Destination and start in `inline` of each open link or image.
    links: Vec<(String, usize)>,
    /// A blank row is owed before the next block.
    gap: bool,
    code: Option<CodeBlock<'a>>,
    table: Option<Table>,
    html: HtmlState,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(hl: Option<&'a SyntaxHighlighter>, palette: &'a MarkdownPalette, width: usize) -> Self {
        Self {
            hl,
            palette,
            width,
            rows: Vec::new(),
            containers: Vec::new(),
            lists: Vec::new(),
            styles: Vec::new(),
            inline: Vec::new(),
            links: Vec::new(),
            gap: false,
            code: None,
            table: None,
            html: HtmlState::default(),
        }
    }

    fn finish(mut self) -> Vec<HighlightedSpans> {
        self.flush_inline();
        // An empty body still occupies one row, like the source view.
        if self.rows.is_empty() {
            self.rows.push(Vec::new());
        }
        self.rows
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Html(html) | Event::InlineHtml(html) => self.html(&html),
            Event::Code(code) | Event::InlineMath(code) | Event::DisplayMath(code) => {
                let style = self.current_style().patch(self.palette.code);
                self.push_inline(style, &code);
            }
            Event::SoftBreak | Event::HardBreak => self.push_inline(self.current_style(), "\n"),
            Event::Rule => self.rule(),
            Event::TaskListMarker(done) => {
                self.push_inline(self.palette.list, if done { "☑ " } else { "☐ " });
            }
            Event::FootnoteReference(name) => {
                self.push_inline(self.palette.link, &format!("[^{name}]"));
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::HtmlBlock => {
                self.open_block();
                self.html.in_block = true;
            }
            Tag::Paragraph => self.open_block(),
            Tag::List(start) => {
                self.open_block();
                // Pay the owed blank row now: the first item clears it.
                self.pay_gap();
                self.lists.push(start);
            }
            Tag::Heading { .. } => {
                self.open_block();
                self.push_style(self.palette.heading.add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(kind) => self.open_quote(kind),
            Tag::CodeBlock(kind) => self.open_code(&kind),
            Tag::Item => self.open_item(),
            Tag::Table(alignments) => {
                self.open_block();
                self.table = Some(Table {
                    alignments,
                    rows: Vec::new(),
                    header_rows: 0,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.push_style(self.palette.italic.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(self.palette.bold.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(self.palette.strike.add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.push_style(self.palette.link.add_modifier(Modifier::UNDERLINED));
                self.links.push((dest_url.to_string(), self.inline.len()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.close_block(),
            TagEnd::HtmlBlock => {
                self.close_block();
                self.html.in_block = false;
            }
            TagEnd::Heading(_) => {
                self.close_block();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.close_block();
                self.containers.pop();
                self.styles.pop();
            }
            TagEnd::CodeBlock => self.close_code(),
            TagEnd::List(_) => {
                self.close_block();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush_inline();
                self.containers.pop();
            }
            TagEnd::Table => self.close_table(),
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableCell => {
                let cell = mem::take(&mut self.inline);
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => self.close_link(),
            _ => {}
        }
    }

    // ---- inline text ----

    fn current_style(&self) -> Style {
        self.styles.last().copied().unwrap_or(self.palette.base)
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.current_style().patch(style));
    }

    fn text(&mut self, text: &str) {
        match self.code.as_mut() {
            Some(code) => code.text.push_str(text),
            None => self.push_inline(self.current_style(), text),
        }
    }

    fn push_inline(&mut self, style: Style, text: &str) {
        push_piece(&mut self.inline, style, &expand_tabs(text));
    }

    /// Show a link's destination after its text, unless the text already is it.
    fn close_link(&mut self) {
        self.styles.pop();
        let Some((url, start)) = self.links.pop() else {
            return;
        };
        let text: String = self.inline[start.min(self.inline.len())..]
            .iter()
            .map(|(_, t)| t.as_str())
            .collect();
        if !url.is_empty() && text.trim() != url {
            let style = self.palette.base.add_modifier(Modifier::DIM);
            push_piece(&mut self.inline, style, &format!(" ({url})"));
        }
    }

    // ---- HTML ----

    /// Reduce raw HTML to its text: tags act per `html_action`, comments go.
    fn html(&mut self, html: &str) {
        let buffer = mem::take(&mut self.html.carry) + html;
        let mut rest = buffer.as_str();
        loop {
            if self.html.in_comment {
                let Some(end) = rest.find("-->") else {
                    return;
                };
                self.html.in_comment = false;
                rest = &rest[end + "-->".len()..];
                continue;
            }
            let Some(open) = rest.find('<') else {
                self.html_text(rest);
                return;
            };
            self.html_text(&rest[..open]);
            rest = &rest[open..];
            // Only `<` followed by a letter, `/` or `!` can start a tag.
            match rest[1..].chars().next() {
                None => {
                    self.html.carry = rest.to_string();
                    return;
                }
                Some(c) if c.is_ascii_alphabetic() || c == '/' || c == '!' => {}
                Some(_) => {
                    self.html_text("<");
                    rest = &rest[1..];
                    continue;
                }
            }
            if let Some(after) = rest.strip_prefix("<!--") {
                self.html.in_comment = true;
                rest = after;
                continue;
            }
            let Some(close) = rest.find('>') else {
                self.html.carry = rest.to_string();
                return;
            };
            self.html_tag(&rest[..=close]);
            rest = &rest[close + 1..];
        }
    }

    fn html_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.push_inline(self.current_style(), &decode_entities(text));
        }
    }

    /// Apply one `<...>` tag.
    fn html_tag(&mut self, tag: &str) {
        let inner = tag[1..tag.len() - 1].trim_end_matches('/');
        let (closing, inner) = inner
            .strip_prefix('/')
            .map_or((false, inner), |i| (true, i));
        let name = inner
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        match html_action(&name) {
            HtmlAction::Style(_) if closing => self.close_html_style(&name),
            HtmlAction::Style(style_of) => {
                let depth = self.styles.len();
                self.push_style(style_of(self.palette));
                self.html.open.push((name, depth));
            }
            HtmlAction::Break => self.push_inline(self.current_style(), "\n"),
            HtmlAction::AltText => {
                if let Some(alt) = attribute(inner, "alt").filter(|alt| !alt.is_empty()) {
                    let style = self.current_style().add_modifier(Modifier::DIM);
                    self.push_inline(style, &decode_entities(alt));
                }
            }
            HtmlAction::Drop => {}
            HtmlAction::Literal => self.push_inline(self.current_style(), tag),
        }
    }

    /// Close the innermost open `name` tag, and anything opened inside it.
    fn close_html_style(&mut self, name: &str) {
        if let Some(index) = self.html.open.iter().rposition(|(open, _)| open == name) {
            self.styles.truncate(self.html.open[index].1);
            self.html.open.truncate(index);
        }
    }

    // ---- blocks ----

    /// Flush pending inline text before a block. The blank row a previous
    /// block owes is paid by `emit`, so a block that draws nothing (an HTML
    /// block of dropped tags) adds no extra spacing.
    fn open_block(&mut self) {
        self.flush_inline();
    }

    fn close_block(&mut self) {
        // A `<` that never closed was text, not a tag.
        let carry = mem::take(&mut self.html.carry);
        self.html_text(&carry);
        // Styling tags do not outlive their block.
        if let Some(&(_, depth)) = self.html.open.first() {
            self.styles.truncate(depth);
            self.html.open.clear();
        }
        self.flush_inline();
        self.gap = true;
    }

    fn open_item(&mut self) {
        self.flush_inline();
        // Items stack tightly; only blocks inside an item are spaced.
        self.gap = false;
        let depth = self
            .containers
            .iter()
            .filter(|c| matches!(c, Container::Item { .. }))
            .count();
        let marker = match self.lists.last_mut() {
            Some(Some(number)) => {
                let marker = format!("{number}. ");
                *number += 1;
                marker
            }
            _ if depth % 2 == 0 => "• ".to_string(),
            _ => "◦ ".to_string(),
        };
        let indent = marker.width();
        self.containers.push(Container::Item {
            marker: Some(marker),
            indent,
        });
    }

    fn open_quote(&mut self, kind: Option<BlockQuoteKind>) {
        self.open_block();
        let Some(kind) = kind else {
            self.containers.push(Container::Quote(self.palette.quote));
            self.push_style(self.palette.quote);
            return;
        };
        let style = Style::default().fg(alert_color(kind));
        self.containers.push(Container::Quote(style));
        // Alert bodies keep the prose style; only the bar and title are tinted.
        self.push_style(Style::default());
        self.emit(vec![(
            style.add_modifier(Modifier::BOLD),
            alert_title(kind).to_string(),
        )]);
    }

    fn open_code(&mut self, kind: &CodeBlockKind<'_>) {
        self.open_block();
        let token = match kind {
            CodeBlockKind::Fenced(lang) => lang.split_whitespace().next().unwrap_or(""),
            CodeBlockKind::Indented => "",
        };
        let highlighter = self.hl.and_then(|hl| {
            (!token.is_empty())
                .then(|| hl.syntax_set.find_syntax_by_token(token))
                .flatten()
                .map(|syntax| syntect::easy::HighlightLines::new(syntax, &hl.theme))
        });
        self.code = Some(CodeBlock {
            highlighter,
            text: String::new(),
        });
    }

    /// Emit a code block line by line, wrapped at any grapheme: code has no
    /// word boundaries worth keeping, and indentation must survive.
    fn close_code(&mut self) {
        let Some(mut code) = self.code.take() else {
            return;
        };
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        let width = self.available();
        for line in text.split('\n') {
            let line = expand_tabs(line);
            let spans = self.code_line_spans(code.highlighter.as_mut(), &line);
            for row in wrap(&spans, width, false) {
                self.emit(row);
            }
        }
        self.gap = true;
    }

    fn code_line_spans(
        &self,
        highlighter: Option<&mut syntect::easy::HighlightLines<'a>>,
        line: &str,
    ) -> HighlightedSpans {
        let plain = || vec![(self.palette.code, line.to_string())];
        let (Some(highlighter), Some(hl)) = (highlighter, self.hl) else {
            return plain();
        };
        // The bundled grammars expect each line to end in `\n`.
        let terminated = format!("{line}\n");
        let Ok(ranges) = highlighter.highlight_line(&terminated, &hl.syntax_set) else {
            return plain();
        };
        let mut spans = HighlightedSpans::new();
        for (style, piece) in ranges {
            let piece = piece.strip_suffix('\n').unwrap_or(piece);
            push_piece(
                &mut spans,
                SyntaxHighlighter::syntect_to_ratatui_style(style),
                piece,
            );
        }
        spans
    }

    fn rule(&mut self) {
        self.open_block();
        let width = self.available().min(RULE_MAX_WIDTH);
        self.emit(vec![(self.palette.quote, "─".repeat(width))]);
        self.gap = true;
    }

    fn close_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0usize; columns];
        for row in &table.rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(spans_width(cell));
            }
        }
        let separators = CELL_SEPARATOR.width() * (columns - 1);
        fit_widths(&mut widths, self.available().saturating_sub(separators));

        let rule_style = self.palette.quote;
        for (index, row) in table.rows.iter().enumerate() {
            let is_header = index < table.header_rows;
            let mut line = HighlightedSpans::new();
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    push_piece(&mut line, rule_style, CELL_SEPARATOR);
                }
                let mut cell = row.get(column).cloned().unwrap_or_default();
                if is_header {
                    for (style, _) in &mut cell {
                        *style = style.add_modifier(Modifier::BOLD);
                    }
                }
                let alignment = table
                    .alignments
                    .get(column)
                    .copied()
                    .unwrap_or(Alignment::None);
                for (style, text) in align_cell(&cell, *width, alignment) {
                    push_piece(&mut line, style, &text);
                }
            }
            trim_trailing_spaces(&mut line);
            self.emit(line);
            if index + 1 == table.header_rows {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                self.emit(vec![(rule_style, rule.join(HEADER_SEPARATOR))]);
            }
        }
        self.gap = true;
    }

    // ---- rows ----

    /// Wrap the pending inline text into rows, one paragraph line at a time.
    fn flush_inline(&mut self) {
        let inline = mem::take(&mut self.inline);
        if inline.is_empty() {
            return;
        }
        let mut lines = split_lines(&inline);
        while lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
            lines.pop();
        }
        if self.html.in_block {
            lines.retain(|line| line.iter().any(|(_, text)| !text.trim().is_empty()));
        }
        let width = self.available();
        for line in lines {
            for row in wrap(&line, width, true) {
                self.emit(row);
            }
        }
    }

    /// Push a row behind its container prefix. The first row after an item
    /// opens takes the item's marker; every later row gets blanks.
    fn emit(&mut self, content: HighlightedSpans) {
        self.pay_gap();
        let mut row = self.prefix(true);
        for (style, text) in content {
            push_piece(&mut row, style, &text);
        }
        self.rows.push(row);
    }

    /// Push the blank row a previous block owes, if any.
    fn pay_gap(&mut self) {
        if mem::take(&mut self.gap) && !self.rows.is_empty() {
            let mut gap = self.prefix(false);
            trim_trailing_spaces(&mut gap);
            self.rows.push(gap);
        }
    }

    fn prefix(&mut self, take_markers: bool) -> HighlightedSpans {
        let list_style = self.palette.list;
        let mut row = HighlightedSpans::new();
        for container in &mut self.containers {
            match container {
                Container::Quote(style) => push_piece(&mut row, *style, QUOTE_BAR),
                Container::Item { marker, indent } => {
                    match marker.take().filter(|_| take_markers) {
                        Some(marker) => push_piece(&mut row, list_style, &marker),
                        None => push_piece(&mut row, list_style, &" ".repeat(*indent)),
                    }
                }
            }
        }
        row
    }

    /// Columns left for content after the container prefix.
    fn available(&self) -> usize {
        let prefix: usize = self
            .containers
            .iter()
            .map(|container| match container {
                Container::Quote(_) => QUOTE_BAR.width(),
                Container::Item { indent, .. } => *indent,
            })
            .sum();
        self.width.saturating_sub(prefix).max(1)
    }
}

fn alert_title(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "Note",
        BlockQuoteKind::Tip => "Tip",
        BlockQuoteKind::Important => "Important",
        BlockQuoteKind::Warning => "Warning",
        BlockQuoteKind::Caution => "Caution",
    }
}

/// GitHub's alert colours, as named terminal colours so they follow the
/// terminal palette in light and dark schemes.
fn alert_color(kind: BlockQuoteKind) -> Color {
    match kind {
        BlockQuoteKind::Note => Color::Blue,
        BlockQuoteKind::Tip => Color::Green,
        BlockQuoteKind::Important => Color::Magenta,
        BlockQuoteKind::Warning => Color::Yellow,
        BlockQuoteKind::Caution => Color::Red,
    }
}

/// Value of attribute `name` in a tag's inner text, quoted or bare.
fn attribute<'t>(tag: &'t str, name: &str) -> Option<&'t str> {
    let mut rest = tag;
    while let Some(at) = rest.find(name) {
        let after = &rest[at + name.len()..];
        if rest[..at].ends_with(char::is_whitespace)
            && let Some(value) = after.trim_start().strip_prefix('=')
        {
            let value = value.trim_start();
            return match value.chars().next()? {
                quote @ ('"' | '\'') => value[1..].split(quote).next(),
                _ => value.split(char::is_whitespace).next(),
            };
        }
        rest = after;
    }
    None
}

/// Decode the entities GitHub bodies use; `&amp;` last so it cannot double-decode.
fn decode_entities(text: &str) -> String {
    const ENTITIES: [(&str, &str); 6] = [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&nbsp;", " "),
        ("&amp;", "&"),
    ];
    ENTITIES
        .iter()
        .fold(text.to_string(), |text, (entity, plain)| {
            text.replace(entity, plain)
        })
}

fn expand_tabs(text: &str) -> String {
    text.replace('\t', &" ".repeat(TAB_WIDTH))
}

/// Append `text` in `style`, merging into the last span when the style matches.
fn push_piece(spans: &mut HighlightedSpans, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some((last, existing)) if *last == style => existing.push_str(text),
        _ => spans.push((style, text.to_string())),
    }
}

fn spans_width(spans: &[(Style, String)]) -> usize {
    spans.iter().map(|(_, text)| text.width()).sum()
}

fn trim_trailing_spaces(spans: &mut HighlightedSpans) {
    while let Some((_, text)) = spans.last_mut() {
        let trimmed = text.trim_end_matches(' ').len();
        text.truncate(trimmed);
        if !text.is_empty() {
            return;
        }
        spans.pop();
    }
}

/// Split spans at each `\n` into one span list per line.
fn split_lines(spans: &[(Style, String)]) -> Vec<HighlightedSpans> {
    let mut lines = vec![HighlightedSpans::new()];
    for (style, text) in spans {
        for (index, piece) in text.split('\n').enumerate() {
            if index > 0 {
                lines.push(HighlightedSpans::new());
            }
            if let Some(line) = lines.last_mut() {
                push_piece(line, *style, piece);
            }
        }
    }
    lines
}

/// One unit the wrapper places whole when it can: a word, a space, or (when
/// not wrapping at words) a single grapheme.
struct WrapToken<'s> {
    graphemes: Vec<(Style, &'s str, usize)>,
    width: usize,
    is_space: bool,
}

fn wrap_tokens(spans: &[(Style, String)], at_words: bool) -> Vec<WrapToken<'_>> {
    let mut tokens: Vec<WrapToken<'_>> = Vec::new();
    for (style, text) in spans {
        for grapheme in text.graphemes(true) {
            let width = grapheme.width();
            let is_space = at_words && grapheme.chars().all(char::is_whitespace);
            let extends_word = at_words && !is_space && tokens.last().is_some_and(|t| !t.is_space);
            match tokens.last_mut() {
                Some(token) if extends_word => {
                    token.graphemes.push((*style, grapheme, width));
                    token.width += width;
                }
                _ => tokens.push(WrapToken {
                    graphemes: vec![(*style, grapheme, width)],
                    width,
                    is_space,
                }),
            }
        }
    }
    tokens
}

/// Greedy wrap of one line of spans into rows at most `width` wide. With
/// `at_words`, breaks fall between words and a word longer than a row is split;
/// otherwise any grapheme may end a row. Always returns at least one row.
fn wrap(spans: &[(Style, String)], width: usize, at_words: bool) -> Vec<HighlightedSpans> {
    let width = width.max(1);
    let mut rows = vec![HighlightedSpans::new()];
    let mut used = 0usize;
    for token in wrap_tokens(spans, at_words) {
        if token.is_space {
            // A space never starts a wrapped row or overflows one.
            if used > 0 && used + token.width <= width {
                let (style, grapheme, w) = token.graphemes[0];
                push_row_piece(&mut rows, style, grapheme);
                used += w;
            }
            continue;
        }
        if used > 0 && used + token.width > width {
            new_row(&mut rows, &mut used);
        }
        for (style, grapheme, w) in token.graphemes {
            if used > 0 && used + w > width {
                new_row(&mut rows, &mut used);
            }
            push_row_piece(&mut rows, style, grapheme);
            used += w;
        }
    }
    if let Some(last) = rows.last_mut() {
        trim_trailing_spaces(last);
    }
    rows
}

fn push_row_piece(rows: &mut [HighlightedSpans], style: Style, text: &str) {
    if let Some(row) = rows.last_mut() {
        push_piece(row, style, text);
    }
}

fn new_row(rows: &mut Vec<HighlightedSpans>, used: &mut usize) {
    if let Some(row) = rows.last_mut() {
        trim_trailing_spaces(row);
    }
    rows.push(HighlightedSpans::new());
    *used = 0;
}

/// Shrink the widest columns one cell at a time until they fit `budget`.
fn fit_widths(widths: &mut [usize], budget: usize) {
    let mut total: usize = widths.iter().sum();
    while total > budget {
        let Some(widest) = widths.iter_mut().max() else {
            return;
        };
        if *widest <= 1 {
            return;
        }
        *widest -= 1;
        total -= 1;
    }
}

/// Pad or truncate `cell` to exactly `width` columns, truncating with `…`.
fn align_cell(cell: &[(Style, String)], width: usize, alignment: Alignment) -> HighlightedSpans {
    let mut content = truncate(cell, width);
    let pad = width.saturating_sub(spans_width(&content));
    let (left, right) = match alignment {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::Left | Alignment::None => (0, pad),
    };
    let base = cell
        .first()
        .map_or_else(Style::default, |(style, _)| *style);
    let mut out = HighlightedSpans::new();
    push_piece(&mut out, base, &" ".repeat(left));
    out.append(&mut content);
    push_piece(&mut out, base, &" ".repeat(right));
    out
}

fn truncate(cell: &[(Style, String)], width: usize) -> HighlightedSpans {
    if spans_width(cell) <= width {
        return cell.to_vec();
    }
    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    let mut out = HighlightedSpans::new();
    let mut last_style = Style::default();
    'cell: for (style, text) in cell {
        last_style = *style;
        for grapheme in text.graphemes(true) {
            let w = grapheme.width();
            if used + w > budget {
                break 'cell;
            }
            push_piece(&mut out, *style, grapheme);
            used += w;
        }
    }
    if width > 0 {
        push_piece(&mut out, last_style, "…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(content: &str, width: usize) -> Vec<String> {
        render(None, content, width)
            .iter()
            .map(|row| row.iter().map(|(_, t)| t.as_str()).collect())
            .collect()
    }

    fn styled(content: &str, width: usize) -> Vec<HighlightedSpans> {
        render(Some(&SyntaxHighlighter::default()), content, width)
    }

    #[test]
    fn should_drop_inline_markers_and_style_the_text() {
        assert_eq!(
            rows("Use **bold** and `code` and ~~gone~~", 80),
            ["Use bold and code and gone"]
        );
        let row = &styled("a **b**", 80)[0];
        let bold = row.iter().find(|(_, t)| t == "b").expect("bold run");
        assert!(bold.0.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn should_keep_soft_breaks_as_line_breaks() {
        assert_eq!(rows("line one\nline two", 80), ["line one", "line two"]);
    }

    #[test]
    fn should_separate_blocks_with_one_blank_row() {
        assert_eq!(
            rows("# Title\n\npara one\n\npara two", 80),
            ["Title", "", "para one", "", "para two"]
        );
    }

    #[test]
    fn should_word_wrap_prose_inside_width() {
        let out = rows("alpha beta gamma delta", 11);
        assert_eq!(out, ["alpha beta", "gamma delta"]);
        assert!(out.iter().all(|row| row.width() <= 11));
    }

    #[test]
    fn should_split_a_word_longer_than_the_row() {
        assert_eq!(rows("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn should_bullet_and_number_list_items_with_hanging_indent() {
        assert_eq!(rows("- one\n- two", 80), ["• one", "• two"]);
        assert_eq!(rows("3. three\n4. four", 80), ["3. three", "4. four"]);
        assert_eq!(rows("- alpha beta gamma", 12), ["• alpha beta", "  gamma"]);
        assert_eq!(rows("- outer\n  - inner", 80), ["• outer", "  ◦ inner"]);
    }

    #[test]
    fn should_mark_tasks() {
        assert_eq!(rows("- [x] done\n- [ ] todo", 80), ["• ☑ done", "• ☐ todo"]);
    }

    #[test]
    fn should_bar_block_quotes() {
        assert_eq!(rows("> quoted\n> more", 80), ["│ quoted", "│ more"]);
    }

    #[test]
    fn should_title_and_tint_github_alerts() {
        assert_eq!(
            rows("> [!WARNING]\n> Mind the gap", 80),
            ["│ Warning", "│ Mind the gap"]
        );
        let out = styled("> [!TIP]\n> body", 80);
        assert_eq!(
            out[0][0].0.fg,
            Some(Color::Green),
            "alert bar tinted: {out:?}"
        );
    }

    #[test]
    fn should_lay_tables_out_in_aligned_columns() {
        let table = "| Key | Value |\n| --- | ---: |\n| a | 1 |\n| long key | 22 |";
        assert_eq!(
            rows(table, 80),
            [
                "Key      │ Value",
                "─────────┼──────",
                "a        │     1",
                "long key │    22"
            ]
        );
    }

    #[test]
    fn should_truncate_tables_wider_than_the_row() {
        let out = rows("| a | b |\n| - | - |\n| abcdefghij | klmnopqrst |", 13);
        assert!(out.iter().all(|row| row.width() <= 13), "{out:?}");
        assert!(out[2].contains('…'), "{out:?}");
    }

    #[test]
    fn should_keep_code_block_indentation_and_highlight_it() {
        let body = "```rust\nfn main() {\n    let x = 1;\n}\n```";
        assert_eq!(rows(body, 80), ["fn main() {", "    let x = 1;", "}"]);
        let out = styled(body, 80);
        let keyword = out[0].iter().find(|(_, t)| t == "fn").expect("`fn` run");
        let punct = out[0]
            .iter()
            .find(|(_, t)| t.contains('('))
            .expect("punctuation run");
        assert_ne!(keyword.0.fg, punct.0.fg, "syntect colours code: {out:?}");
    }

    #[test]
    fn should_show_link_destination_unless_it_is_the_text() {
        assert_eq!(rows("[docs](https://x.y)", 80), ["docs (https://x.y)"]);
        assert_eq!(rows("<https://x.y>", 80), ["https://x.y"]);
    }

    /// The shape of a Copilot review summary: GitHub bots wrap markdown in HTML.
    #[test]
    fn should_reduce_github_html_to_its_text() {
        let body = "<!-- ccr-overview-v2 -->\n\n### Changes\n\n<details open>\n<summary><strong>3 open issues</strong></summary>\n\n- <picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"https://x.y/d.svg\"><img alt=\"medium\" src=\"https://x.y/m.svg\"></picture> First issue\n</details>\n\n<sub>Review effort: Balanced</sub>";
        assert_eq!(
            rows(body, 80),
            [
                "Changes",
                "",
                "3 open issues",
                "",
                "• medium First issue",
                "",
                "Review effort: Balanced"
            ]
        );
        let title = &styled(body, 80)[2];
        assert!(
            title[0].0.add_modifier.contains(Modifier::BOLD),
            "{title:?}"
        );
    }

    #[test]
    fn should_keep_unknown_tags_and_stray_brackets_as_text() {
        assert_eq!(rows("Use Vec<T> here", 80), ["Use Vec<T> here"]);
        assert_eq!(rows("a<br>b &amp; c", 80), ["a", "b & c"]);
        assert_eq!(rows("<div>\nx < y\n</div>", 80), ["x < y"]);
    }

    #[test]
    fn should_drop_multi_line_html_comments() {
        assert_eq!(rows("<!--\nhidden\n-->\n\nshown", 80), ["shown"]);
    }

    #[test]
    fn should_cap_rules_when_unwrapped() {
        assert_eq!(
            rows("a\n\n---\n\nb", usize::MAX)[2].chars().count(),
            RULE_MAX_WIDTH
        );
    }

    #[test]
    fn should_render_an_empty_body_as_one_row() {
        assert_eq!(rows("", 80), [""]);
    }

    #[test]
    fn should_count_the_same_rows_with_and_without_styles() {
        let body = "# T\n\n- a\n- b\n\n| x | y |\n| - | - |\n| 1 | 2 |\n\n> [!NOTE]\n> n\n\n```rust\nlet a = 1;\n```";
        for width in [8, 20, 80] {
            assert_eq!(
                render(None, body, width).len(),
                styled(body, width).len(),
                "width {width}"
            );
        }
    }

    #[test]
    fn should_fit_every_row_inside_the_width() {
        let body = "Some **long** prose that wraps across rows\n\n- an item that also wraps around\n\n> a quote that wraps too";
        for width in [6, 10, 25] {
            for row in rows(body, width) {
                assert!(row.width() <= width, "{row:?} wider than {width}");
            }
        }
    }
}

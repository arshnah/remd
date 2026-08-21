use std::env;
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CEvent, KeyCode};
use notify::{RecursiveMode, Watcher};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::DefaultTerminal;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

struct MdState {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>,
    in_code_block: bool,
    code_buffer: String,
    code_lang: Option<String>,
    heading_level: Option<HeadingLevel>,
    table_alignments: Vec<Alignment>,
    table_header: Vec<String>,
    table_rows: Vec<Vec<String>>,
    table_current_row: Vec<String>,
    in_table_cell: bool,
    table_cell_buf: String,
}

impl MdState {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            style_stack: Vec::new(),
            list_stack: Vec::new(),
            in_code_block: false,
            code_buffer: String::new(),
            code_lang: None,
            heading_level: None,
            table_alignments: Vec::new(),
            table_header: Vec::new(),
            table_rows: Vec::new(),
            table_current_row: Vec::new(),
            in_table_cell: false,
            table_cell_buf: String::new(),
        }
    }

    fn cur_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    fn push_text(&mut self, text: &str, extra: Style) {
        let style = self.cur_style().patch(extra);
        self.current.push(Span::styled(text.to_string(), style));
    }
}

fn highlight_code(buffer: &str, lang: Option<&str>, ps: &SyntaxSet, theme: &Theme) -> Vec<Line<'static>> {
    let syntax = lang
        .and_then(|l| ps.find_syntax_by_token(l))
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();

    for line in LinesWithEndings::from(buffer) {
        let ranges = h.highlight_line(line, ps).unwrap_or_default();
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| {
                let fg = style.foreground;
                let mut rstyle = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                if style.font_style.contains(FontStyle::BOLD) {
                    rstyle = rstyle.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    rstyle = rstyle.add_modifier(Modifier::ITALIC);
                }
                Span::styled(text.trim_end_matches('\n').to_string(), rstyle)
            })
            .collect();
        out.push(Line::from(spans));
    }
    out
}

fn pad_cell(s: &str, width: usize, align: Alignment) -> String {
    let len = s.chars().count();
    let total_pad = width.saturating_sub(len);
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(total_pad), s),
        Alignment::Center => {
            let left = total_pad / 2;
            let right = total_pad - left;
            format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
        }
        _ => format!("{}{}", s, " ".repeat(total_pad)),
    }
}

fn render_table(header: &[String], rows: &[Vec<String>], alignments: &[Alignment]) -> Vec<Line<'static>> {
    let col_count = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    let mut widths = vec![0usize; col_count];
    for (i, h) in header.iter().enumerate() {
        widths[i] = widths[i].max(h.chars().count());
    }
    for row in rows {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }

    let align_for = |i: usize| *alignments.get(i).unwrap_or(&Alignment::None);

    let mut lines = Vec::new();

    if !header.is_empty() {
        let header_line: String = header
            .iter()
            .enumerate()
            .map(|(i, h)| pad_cell(h, widths[i], align_for(i)))
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(Line::from(Span::styled(
            header_line,
            Style::default().add_modifier(Modifier::BOLD),
        )));

        let sep: String = widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-");
        lines.push(Line::from(Span::styled(
            sep,
            Style::default().fg(Color::DarkGray),
        )));
    }

    for row in rows {
        let line: String = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad_cell(c, widths[i], align_for(i)))
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(Line::from(Span::raw(line)));
    }
    lines
}

fn render_markdown(input: &str, ps: &SyntaxSet, theme: &Theme) -> Text<'static> {
    let parser = Parser::new_ext(
        input,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    );
    let mut st = MdState::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    st.heading_level = Some(level);
                }
                Tag::Emphasis => {
                    let s = st.cur_style().add_modifier(Modifier::ITALIC);
                    st.style_stack.push(s);
                }
                Tag::Strong => {
                    let s = st.cur_style().add_modifier(Modifier::BOLD);
                    st.style_stack.push(s);
                }
                Tag::Strikethrough => {
                    let s = st.cur_style().add_modifier(Modifier::CROSSED_OUT);
                    st.style_stack.push(s);
                }
                Tag::CodeBlock(kind) => {
                    st.in_code_block = true;
                    st.code_buffer.clear();
                    st.code_lang = match kind {
                        CodeBlockKind::Fenced(info) => {
                            let lang = info
                                .split(|c: char| c.is_whitespace() || c == ',')
                                .next()
                                .unwrap_or("");
                            (!lang.is_empty()).then(|| lang.to_string())
                        }
                        CodeBlockKind::Indented => None,
                    };
                    st.flush_line();
                }
                Tag::Item => {
                    let depth = st.list_stack.len();
                    let indent = "  ".repeat(depth.saturating_sub(1));
                    let marker = match st.list_stack.last_mut() {
                        Some(Some(n)) => {
                            let s = format!("{n}. ");
                            *n += 1;
                            s
                        }
                        _ => "- ".to_string(),
                    };
                    st.current.push(Span::raw(format!("{indent}{marker}")));
                }
                Tag::List(start) => st.list_stack.push(start),
                Tag::BlockQuote(_) => {
                    st.current
                        .push(Span::styled("> ", Style::default().fg(Color::DarkGray)));
                }
                Tag::Table(alignments) => {
                    st.table_alignments = alignments;
                    st.table_header.clear();
                    st.table_rows.clear();
                }
                Tag::TableHead => {
                    st.table_current_row.clear();
                }
                Tag::TableRow => {
                    st.table_current_row.clear();
                }
                Tag::TableCell => {
                    st.in_table_cell = true;
                    st.table_cell_buf.clear();
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    st.flush_line();
                    st.lines.push(Line::default());
                    st.heading_level = None;
                }
                TagEnd::Paragraph => {
                    st.flush_line();
                    st.lines.push(Line::default());
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    st.style_stack.pop();
                }
                TagEnd::CodeBlock => {
                    st.in_code_block = false;
                    let highlighted =
                        highlight_code(&st.code_buffer, st.code_lang.as_deref(), ps, theme);
                    st.lines.extend(highlighted);
                    st.lines.push(Line::default());
                }
                TagEnd::Item => st.flush_line(),
                TagEnd::List(_) => {
                    st.list_stack.pop();
                    st.lines.push(Line::default());
                }
                TagEnd::TableCell => {
                    st.in_table_cell = false;
                    let cell = std::mem::take(&mut st.table_cell_buf);
                    st.table_current_row.push(cell);
                }
                TagEnd::TableHead => {
                    st.table_header = std::mem::take(&mut st.table_current_row);
                }
                TagEnd::TableRow => {
                    let row = std::mem::take(&mut st.table_current_row);
                    st.table_rows.push(row);
                }
                TagEnd::Table => {
                    let table_lines =
                        render_table(&st.table_header, &st.table_rows, &st.table_alignments);
                    st.lines.extend(table_lines);
                    st.lines.push(Line::default());
                }
                _ => {}
            },
            Event::Text(text) => {
                if st.in_table_cell {
                    st.table_cell_buf.push_str(&text);
                } else if st.in_code_block {
                    st.code_buffer.push_str(&text);
                } else {
                    let extra = match st.heading_level {
                        Some(HeadingLevel::H1) => {
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                        }
                        Some(_) => Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                        None => Style::default(),
                    };
                    st.push_text(&text, extra);
                }
            }
            Event::Code(text) => {
                if st.in_table_cell {
                    st.table_cell_buf.push_str(&text);
                } else {
                    st.push_text(
                        &text,
                        Style::default().fg(Color::Green).bg(Color::Rgb(40, 40, 40)),
                    );
                }
            }
            Event::SoftBreak => st.current.push(Span::raw(" ")),
            Event::HardBreak => st.flush_line(),
            Event::Rule => {
                st.flush_line();
                st.lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
                st.lines.push(Line::default());
            }
            _ => {}
        }
    }
    if !st.current.is_empty() {
        st.flush_line();
    }
    Text::from(st.lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(input: &str) -> Text<'static> {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = &ts.themes["base16-ocean.dark"];
        render_markdown(input, &ps, theme)
    }

    fn lines_as_strings(text: &Text<'static>) -> Vec<String> {
        text.lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn renders_heading_and_paragraph() {
        let text = render("# Title\n\nSome text.\n");
        let rendered = lines_as_strings(&text);
        assert!(rendered.iter().any(|l| l.contains("Title")));
        assert!(rendered.iter().any(|l| l.contains("Some text.")));
    }

    #[test]
    fn renders_list_items_with_markers() {
        let text = render("- a\n- b\n");
        let rendered = lines_as_strings(&text);
        assert!(rendered.iter().any(|l| l.contains("- a")));
        assert!(rendered.iter().any(|l| l.contains("- b")));
    }

    #[test]
    fn renders_code_block_lines_separately() {
        let text = render("```\nline1\nline2\n```\n");
        let rendered = lines_as_strings(&text);
        assert!(rendered.iter().any(|l| l.contains("line1")));
        assert!(rendered.iter().any(|l| l.contains("line2")));
    }

    #[test]
    fn highlights_known_language_without_panicking() {
        let text = render("```rust\nfn main() {}\n```\n");
        let rendered = lines_as_strings(&text);
        assert!(rendered.iter().any(|l| l.contains("fn main()")));
    }

    #[test]
    fn renders_table_with_aligned_columns() {
        let text = render("| a | b |\n|---|---|\n| 1 | 2 |\n");
        let rendered = lines_as_strings(&text);
        assert!(rendered.iter().any(|l| l.contains('a') && l.contains('b')));
        assert!(rendered.iter().any(|l| l.contains('1') && l.contains('2')));
    }

    #[test]
    fn does_not_panic_on_empty_input() {
        let text = render("");
        assert!(text.lines.is_empty());
    }
}

fn load_and_render(path: &str, ps: &SyntaxSet, theme: &Theme) -> std::io::Result<Text<'static>> {
    let content = fs::read_to_string(path)?;
    Ok(render_markdown(&content, ps, theme))
}

fn spawn_watcher(path: &str) -> notify::Result<(notify::RecommendedWatcher, mpsc::Receiver<()>)> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && (event.kind.is_modify() || event.kind.is_create())
        {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(Path::new(path), RecursiveMode::NonRecursive)?;
    Ok((watcher, rx))
}

fn run(
    terminal: &mut DefaultTerminal,
    path: &str,
    ps: &SyntaxSet,
    theme: &Theme,
    rx: &mpsc::Receiver<()>,
) -> std::io::Result<()> {
    let mut text = load_and_render(path, ps, theme)?;
    let mut total_lines = text.lines.len() as u16;
    let mut scroll: u16 = 0;
    let mut last_reload: Option<Instant> = None;

    loop {
        let mut viewport_height = 0u16;
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
            viewport_height = chunks[0].height;

            let paragraph = Paragraph::new(text.clone()).scroll((scroll, 0));
            frame.render_widget(paragraph, chunks[0]);

            let percent = if total_lines <= viewport_height {
                100
            } else {
                let max = total_lines.saturating_sub(viewport_height).max(1);
                (scroll as f32 / max as f32 * 100.0).round() as u16
            };
            let reloaded_tag = match last_reload {
                Some(t) if t.elapsed() < Duration::from_millis(1200) => "  reloaded",
                _ => "",
            };
            let status = format!(" {path}  {percent}%{reloaded_tag} ");
            let status_line = Paragraph::new(Line::from(Span::styled(
                status,
                Style::default().fg(Color::Black).bg(Color::Gray),
            )));
            frame.render_widget(status_line, chunks[1]);
        })?;

        let max_scroll = total_lines.saturating_sub(viewport_height);
        scroll = scroll.min(max_scroll);

        if event::poll(Duration::from_millis(150))?
            && let CEvent::Key(key) = event::read()?
        {
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Down | KeyCode::Char('j') => {
                    scroll = (scroll + 1).min(max_scroll);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    scroll = scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    scroll = (scroll + viewport_height).min(max_scroll);
                }
                KeyCode::PageUp => {
                    scroll = scroll.saturating_sub(viewport_height);
                }
                KeyCode::Char('g') => scroll = 0,
                KeyCode::Char('G') => scroll = max_scroll,
                _ => {}
            }
        }

        if rx.try_recv().is_ok() {
            std::thread::sleep(Duration::from_millis(150));
            while rx.try_recv().is_ok() {}
            if let Ok(new_text) = load_and_render(path, ps, theme) {
                text = new_text;
                total_lines = text.lines.len() as u16;
                last_reload = Some(Instant::now());
            }
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: remd <file.md>");
        std::process::exit(1);
    });

    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let theme = &ts.themes["base16-ocean.dark"];

    let (_watcher, rx) = spawn_watcher(&path).unwrap_or_else(|e| {
        eprintln!("failed to watch {path}: {e}");
        std::process::exit(1);
    });

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &path, &ps, theme, &rx);
    ratatui::restore();
    result
}

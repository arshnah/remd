use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CEvent, KeyCode};
use notify::{RecursiveMode, Watcher};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::DefaultTerminal;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
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
    base_dir: PathBuf,
    in_image: bool,
    image_dest: String,
    image_alt: String,
    images: Vec<(usize, PathBuf, String)>,
}

impl MdState {
    fn new(base_dir: PathBuf) -> Self {
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
            base_dir,
            in_image: false,
            image_dest: String::new(),
            image_alt: String::new(),
            images: Vec::new(),
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

fn render_markdown(
    input: &str,
    ps: &SyntaxSet,
    theme: &Theme,
    base_dir: &Path,
) -> (Text<'static>, Vec<(usize, PathBuf, String)>) {
    let parser = Parser::new_ext(
        input,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    );
    let mut st = MdState::new(base_dir.to_path_buf());

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
                Tag::Image { dest_url, .. } => {
                    st.in_image = true;
                    st.image_dest = dest_url.to_string();
                    st.image_alt.clear();
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
                TagEnd::Image => {
                    st.in_image = false;
                    st.flush_line();
                    let line_index = st.lines.len();
                    let label = if st.image_alt.is_empty() {
                        format!("[image: {}]", st.image_dest)
                    } else {
                        format!("[image: {}]", st.image_alt)
                    };
                    st.lines.push(Line::from(Span::styled(
                        label,
                        Style::default().fg(Color::Magenta),
                    )));
                    st.lines.push(Line::default());

                    if !st.image_dest.starts_with("http://") && !st.image_dest.starts_with("https://") {
                        let path = st.base_dir.join(&st.image_dest);
                        st.images.push((line_index, path, std::mem::take(&mut st.image_alt)));
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if st.in_image {
                    st.image_alt.push_str(&text);
                } else if st.in_table_cell {
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
    (Text::from(st.lines), st.images)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(input: &str) -> Text<'static> {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = &ts.themes["base16-ocean.dark"];
        render_markdown(input, &ps, theme, Path::new(".")).0
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

    #[test]
    fn obsidian_embed_becomes_real_markdown_image() {
        let out = convert_obsidian_embeds("before ![[Pasted image 20260824190711.png|700]] after");
        assert_eq!(out, "before ![Pasted image 20260824190711.png](Pasted image 20260824190711.png) after");
    }

    #[test]
    fn obsidian_embed_without_width_still_converts() {
        let out = convert_obsidian_embeds("![[cat.png]]");
        assert_eq!(out, "![cat.png](cat.png)");
    }

    #[test]
    fn registers_local_image_with_resolved_path() {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = &ts.themes["base16-ocean.dark"];
        let (text, images) = render_markdown(
            "![a cat](cat.png)\n",
            &ps,
            theme,
            Path::new("/docs"),
        );
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].1, Path::new("/docs/cat.png"));
        assert_eq!(images[0].2, "a cat");
        assert!(text.lines.iter().any(|l| l
            .spans
            .iter()
            .any(|s| s.content.contains("a cat"))));
    }

    #[test]
    fn does_not_register_remote_image_urls() {
        let ps = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = &ts.themes["base16-ocean.dark"];
        let (_, images) = render_markdown(
            "![remote](https://example.com/cat.png)\n",
            &ps,
            theme,
            Path::new("/docs"),
        );
        assert!(images.is_empty(), "remote image URLs shouldn't be treated as local files to decode");
    }

    #[test]
    fn wrapped_line_count_splits_long_lines() {
        let text = render(&format!("{}\n", "word ".repeat(40)));
        let width = 40;
        let count = wrapped_line_count(&text, width);
        assert!(
            count > 1,
            "expected a long line to wrap into multiple rows at width {width}, got {count}"
        );
    }

    #[test]
    fn long_line_actually_wraps_in_the_rendered_buffer() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let text = render(&format!("{}\n", "abcdefgh ".repeat(20)));
        let backend = TestBackend::new(30, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let area = frame.area();
                let paragraph = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
                frame.render_widget(paragraph, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut rows_with_content = 0;
        for y in 0..buffer.area.height {
            let row_text: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            if row_text.trim().starts_with("abcdefgh") {
                rows_with_content += 1;
            }
        }
        assert!(
            rows_with_content > 1,
            "expected the long line to wrap across multiple rows in the rendered buffer, only found {rows_with_content}"
        );
    }
}

type RenderedDoc = (Text<'static>, Vec<(usize, PathBuf, String)>);

fn convert_obsidian_embeds(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("![[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let Some(end) = after.find("]]") else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let inner = &after[..end];
        let name = inner.split('|').next().unwrap_or(inner).trim();
        out.push_str(&format!("![{name}]({name})"));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn load_and_render(path: &str, ps: &SyntaxSet, theme: &Theme) -> std::io::Result<RenderedDoc> {
    let content = fs::read_to_string(path)?;
    let content = convert_obsidian_embeds(&content);
    let base_dir = Path::new(path).parent().unwrap_or(Path::new(".")).to_path_buf();
    Ok(render_markdown(&content, ps, theme, &base_dir))
}

fn line_wrapped_rows(line: &Line, width: usize) -> u16 {
    let len: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    (if len == 0 { 1 } else { len.div_ceil(width) }) as u16
}

fn wrapped_line_count(text: &Text, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.lines.iter().map(|l| line_wrapped_rows(l, width)).sum()
}

fn wrapped_row_starts(text: &Text, width: u16) -> Vec<u16> {
    let width = width.max(1) as usize;
    let mut starts = Vec::with_capacity(text.lines.len());
    let mut acc = 0u16;
    for line in &text.lines {
        starts.push(acc);
        acc += line_wrapped_rows(line, width);
    }
    starts
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

const MAX_IMAGE_ROWS: u16 = 15;

fn run(
    terminal: &mut DefaultTerminal,
    path: &str,
    ps: &SyntaxSet,
    theme: &Theme,
    rx: &mpsc::Receiver<()>,
) -> std::io::Result<()> {
    let (mut text, mut images) = load_and_render(path, ps, theme)?;
    let mut scroll: u16 = 0;
    let mut last_reload: Option<Instant> = None;
    let picker = Picker::from_query_stdio().ok().map(|mut p| {
        let is_kitty = std::env::var("KITTY_WINDOW_ID").is_ok() || std::env::var("TERM").map(|t| t.contains("kitty")).unwrap_or(false);
        if is_kitty {
            p.set_protocol_type(ratatui_image::picker::ProtocolType::Kitty);
        }
        p
    });
    let mut image_cache: HashMap<PathBuf, Option<StatefulProtocol>> = HashMap::new();

    loop {
        let mut viewport_height = 0u16;
        let mut wrapped_lines = 0u16;
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
            viewport_height = chunks[0].height;

            let row_starts = wrapped_row_starts(&text, chunks[0].width);
            wrapped_lines = wrapped_line_count(&text, chunks[0].width);

            let paragraph = Paragraph::new(text.clone())
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(paragraph, chunks[0]);

            if let Some(picker) = &picker {
                for (line_index, img_path, _alt) in &images {
                    let Some(&row_start) = row_starts.get(*line_index) else { continue };
                    if row_start < scroll || row_start >= scroll + viewport_height {
                        continue;
                    }
                    let protocol = image_cache.entry(img_path.clone()).or_insert_with(|| {
                        image::ImageReader::open(img_path)
                            .ok()
                            .and_then(|r| r.decode().ok())
                            .map(|img| picker.new_resize_protocol(img))
                    });
                    let Some(protocol) = protocol else { continue };
                    let visual_row = row_start - scroll;
                    let height = MAX_IMAGE_ROWS.min(viewport_height.saturating_sub(visual_row));
                    if height == 0 {
                        continue;
                    }
                    let rect = Rect {
                        x: chunks[0].x,
                        y: chunks[0].y + visual_row,
                        width: chunks[0].width,
                        height,
                    };
                    frame.render_stateful_widget(StatefulImage::<StatefulProtocol>::default(), rect, protocol);
                }
            }

            let percent = if wrapped_lines <= viewport_height {
                100
            } else {
                let max = wrapped_lines.saturating_sub(viewport_height).max(1);
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

        let max_scroll = wrapped_lines.saturating_sub(viewport_height);
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
            if let Ok((new_text, new_images)) = load_and_render(path, ps, theme) {
                text = new_text;
                images = new_images;
                image_cache.clear();
                last_reload = Some(Instant::now());
            }
        }
    }
    Ok(())
}

fn print_help() {
    println!("remd, a terminal markdown pager with live reload");
    println!();
    println!("usage: remd <file.md>");
    println!();
    println!("keys:");
    println!("  j / down       scroll down one line");
    println!("  k / up         scroll up one line");
    println!("  page down      scroll down one page");
    println!("  page up        scroll up one page");
    println!("  g              jump to top");
    println!("  G              jump to bottom");
    println!("  q              quit");
    println!();
    println!("the file is watched and re-rendered automatically on save");
}

fn main() -> std::io::Result<()> {
    let arg = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: remd <file.md>");
        eprintln!("run 'remd --help' for more");
        std::process::exit(1);
    });

    if arg == "-h" || arg == "--help" {
        print_help();
        return Ok(());
    }

    let path = arg;

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

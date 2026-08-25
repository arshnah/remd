# remd

Terminal Markdown pager with live reload. Renders headings, lists,
emphasis, inline code, syntax-highlighted code blocks, tables, and local
images, watches the file, and re-renders as soon as you save. Status bar
shows the file path and scroll position.

## Images

Local `![alt](path)` images render in-terminal via
[ratatui-image](https://github.com/benjajaja/ratatui-image), auto-detecting
whichever graphics protocol your terminal actually supports (Kitty,
iTerm2, Sixel, falling back to unicode half-blocks). Remote (`http(s)://`)
image URLs aren't fetched, they show as a `[image: url]` label instead.
Only the image currently scrolled into view gets decoded and rendered.

## Build

```
cargo build
```

## Run

```
cargo run -- sample.md
```

`j`/`k` or arrows to scroll, `PgUp`/`PgDn`, `g`/`G` for top/bottom, `q` to
quit. Edit the file in another window and the view updates live.

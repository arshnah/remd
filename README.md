# remd

Terminal Markdown pager with live reload. Renders headings, lists,
emphasis, inline code, syntax-highlighted code blocks, and tables, watches
the file, and re-renders as soon as you save. Status bar shows the file
path and scroll position.

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

# Chess TUI

A polished chess game for the terminal, written in Rust. It combines a
responsive Ratatui interface with protocol-aware raster graphics, a dependable
Unicode fallback, local two-player play, and optional human-like AI opponents.

## Highlights

- Adaptive rendering for Kitty graphics (including Ghostty), Sixel, iTerm2
  inline images, true-color half blocks, and protocol-free Unicode
- Antialiased vector pieces for inline images, hand-tuned 6x6 sprites for
  low-resolution paths, and consistent selection, legal-move, capture,
  last-move, cursor, and check highlights
- A board-first layout that uses all 24 rows at common terminal sizes, with
  status, history, and controls collected into a quiet side rail
- Eased 160 ms raster move transitions, including both pieces during castling,
  with latest-frame-wins background composition
- Bounded-resolution, zlib-compressed Kitty uploads in direct terminals, so
  maximized boards stay responsive without sacrificing piece detail
- Full legal chess play: castling, en passant, explicit underpromotion, check,
  checkmate, and stalemate
- Automatic local draw adjudication for threefold repetition, the fifty-move
  rule, and insufficient material
- Non-blocking play against Maia-3 or any external UCI engine, with locally
  validated moves, stale-search protection, and turn-level undo
- SAN move history, undo, board flipping, coordinate toggling, keyboard and
  mouse input, and responsive narrow-terminal layouts
- Alternate-screen cleanup, panic-safe terminal restoration, synchronized
  updates, and background image encoding

## Requirements

- Rust 1.88 or newer
- A terminal with UTF-8 support; true color is recommended
- [`uv`](https://docs.astral.sh/uv/getting-started/installation/) on `PATH` when
  using the managed Maia preset for the first time

Kitty and Ghostty provide the richest path through the Kitty graphics protocol.
Automatic detection can also select Sixel or iTerm2 graphics when supported.
Direct Ghostty sessions use Kitty graphics even when a capability reply is
missed; multiplexed and remote sessions retain the conservative fallback.
Other terminals can use the portable half-block or Unicode renderers.

## Run

```sh
cargo run --release
```

Or install the binary from this checkout:

```sh
cargo install --path .
chess-tui
```

Select a renderer explicitly when useful:

```sh
chess-tui --graphics auto
chess-tui --graphics kitty
chess-tui --graphics halfblocks
chess-tui --graphics unicode
```

| Mode | Behavior |
| --- | --- |
| `auto` | Queries the terminal with an absolute deadline, uses the detected Kitty, Sixel, iTerm2, or half-block protocol, and recognizes direct Ghostty sessions when the query is inconclusive. |
| `kitty` | Uses Kitty graphics in a direct local terminal even when probing is inconclusive; positive contrary capability reports and unconfirmed tmux/SSH sessions still fail safely. |
| `halfblocks` | Uses flat, two-tone pixel pieces encoded as true-color Unicode half blocks; requires no inline-image protocol. |
| `unicode` | Draws multi-cell piece silhouettes directly when space permits, falls back to chess glyphs in cramped cells, and performs no graphics-protocol query. |

Raster modes interpolate moves with a short app-driven animation;
`--no-animation` makes every move immediate. The Unicode renderer is always
immediate. The app does not yet use Kitty's terminal-owned animation objects,
so current transitions work consistently across all raster backends rather
than depending on terminal-native animation support.

The status rail reports whether the negotiated raster backend is still
`preparing` or is actually `ready`, so a cell fallback is no longer mistaken
for active Kitty graphics.

The development profile keeps debug assertions but optimizes the image stack,
so plain `cargo run` is usable for iteration. `cargo run --release` remains the
recommended way to play.

## Play against Maia-3

[Maia-3](https://github.com/CSSLab/maia3) is trained to predict moves humans at
a chosen rating would play. This feels substantially more human than a strong
search engine that occasionally injects an artificial mistake. The CPU-friendly
5M model is the default preset.

Start it directly—there is no repository to clone and no Python command to run:

```sh
chess-tui --engine maia --engine-elo 1500
```

On the first run, chess-tui opens an in-terminal confirmation before downloading
anything. After confirmation it uses `uv` to create an isolated, app-owned
Python environment, installs a pinned archive of the official Maia-3 source,
and prefetches a pinned snapshot of the 5M model. Progress remains visible in
the TUI. Later runs reuse that installation and model automatically.

The initial setup needs network access and may be a large download: the model
itself is comparatively small, but the isolated Python runtime and Maia's
PyTorch dependencies are not. If `uv` is unavailable or setup fails, the TUI
shows the error and lets you retry; the game never silently hangs. Install
`uv`, or point `CHESS_TUI_UV` at its executable. Managed files and caches live
under the operating system's application-data directory, separate from your
global Python installation. Set `CHESS_TUI_DATA_DIR` to choose another
location.

`--engine maia` launches `maia3-uci` with the 5M model and reconstructed move
history. Ratings from 600 through 2600 are supported. Use
`--human-color black` to let Maia open as White. `--engine-temperature 0`
makes its choices deterministic; the default of `1` samples the learned human
move distribution.

The Elo, temperature, and top-p flags belong to the Maia preset. For another
engine, pass its native settings with repeatable `--engine-option=NAME=VALUE`
arguments instead.

Any UCI engine can be used by naming its executable directly:

```sh
chess-tui \
  --engine stockfish \
  --engine-option=UCI_LimitStrength=true \
  --engine-option=UCI_Elo=1600 \
  --engine-movetime-ms 500
```

Custom engines remain external executables and must be available on `PATH` (or
be passed by path); chess-tui only manages installation for the exact `maia`
preset. Engine startup and thinking happen off the UI thread, and every returned
move must pass the local Rust rules engine before it reaches the board. Startup
failures remain visible in the UI and disable moves instead of leaving the board
apparently frozen.

Maia-3 is developed by the [Maia Chess team](https://www.maiachess.com/) and is
installed as separate software from its pinned
[official source](https://github.com/CSSLab/maia3). It is licensed under
[AGPL-3.0](https://github.com/CSSLab/maia3/blob/main/LICENSE).

## Controls

| Input | Action |
| --- | --- |
| Arrow keys or `h` `j` `k` `l` | Move the board cursor |
| `Enter` or `Space` | Select a piece or destination |
| Left click | Select a square |
| `Esc` or right click | Cancel a selection or close a dialog |
| `q` `r` `b` `n` | Choose queen, rook, bishop, or knight during promotion |
| `u` | Undo one move, or the previous human turn in engine games |
| `f` | Flip the board |
| `c` | Toggle coordinates |
| `n` | Start a new game |
| `?` | Toggle the controls overlay |
| `q` or `Ctrl-C` | Quit |

## Development

Run the complete validation set before submitting changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

The rules and application state are deterministic and tested independently of
the terminal. Rendering tests cover board geometry, orientation, hit testing,
overlays, raster output, and terminal buffers.

Generate detailed and low-resolution compositor previews for visual inspection:

```sh
cargo run --example render_preview -- chess-preview.png
cargo run --example render_preview -- chess-pixel-preview.png pixel 480
cargo run --example piece_gallery -- chess-piece-gallery.png 768
```

Render a deterministic terminal-cell layout preview as SVG:

```sh
cargo run --example ui_preview -- chess-ui-preview.svg 80 24
```

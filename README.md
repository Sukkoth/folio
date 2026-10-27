# Folio

A fast, lightweight, polished desktop PDF reader written in pure Rust for Linux.

## Goals

- **Small binary & low RAM**: Standalone binary (~22MB) bundling the MuPDF engine and hardware-accelerated OpenGL renderer.
- **Fast rendering**: Immediate low-res preview followed by crisp full-res rasterization and background prefetching.
- **Modern UI**: Dark theme first (`#0f1115` canvas, `#161a21` 52px toolbar, rounded page cards with soft shadows, smooth opacity animations).
- **Pure Rust app code**: Native Slint GUI, no webview, no JS, no Electron/Tauri.

---

## Architecture Rules

1. **Dedicated Worker Thread**: The UI thread never renders. PDF rasterization takes place on dedicated background worker threads, each opening its own `mupdf::Document` instance lazily.
2. **Decoupled Messaging**: Requests are passed via crossbeam channels (`RenderRequest { page, zoom, generation, is_preview }`), and rendered frames (`RenderedPage { page, rgba, w, h, generation, is_preview }`) are delivered back to the UI thread via `slint::Weak::upgrade_in_event_loop`.
3. **Generation Counter**: Rapid zooming and page flipping increment a generation counter. In-flight and queued render jobs with older generations are immediately dropped to prevent CPU waste and lag.
4. **Lazy / mmap-friendly Loading**: Documents are opened lazily without reading files into memory. Page counts and dimensions are queried cheaply upfront.
5. **HiDPI Aware**: Pages are rendered at display size scaled by the window's physical DPI scale factor for crisp rendering on high-resolution displays.
6. **LRU Page Caching**: An LRU cache (12 entries) keeps recently rendered pages keyed by `(page, zoom_bucket)` for instant navigation.
7. **Prefetching**: When viewing a page, neighboring pages (current ± 1, current ± 2) are automatically prefetched into the cache.
8. **Progressive Display**: Low-resolution previews (0.25x) render almost instantly (<1ms), followed immediately by the full-resolution render.

---

## Prerequisites (Ubuntu / Debian)

Ensure standard build tools and Wayland/X11 development libraries are installed:

```bash
sudo apt update && sudo apt install -y \
    clang \
    pkg-config \
    make \
    cmake \
    libfontconfig1-dev \
    libxkbcommon-dev \
    libxkbcommon-x11-dev \
    libwayland-dev \
    wayland-protocols \
    libx11-dev \
    libgl1-mesa-dev \
    libegl1-mesa-dev
```

---

## Building and Running

### Build in Release Mode
```bash
cargo build --release
```

### Run with a PDF
```bash
cargo run --release -- path/to/document.pdf
```
or run the binary directly:
```bash
./target/release/folio path/to/document.pdf
```

---

## Keyboard & Mouse Shortcuts

| Action | Shortcut |
| :--- | :--- |
| **Next Page** | `Right Arrow`, `Down Arrow`, `PageDown` |
| **Previous Page** | `Left Arrow`, `Up Arrow`, `PageUp` |
| **Toggle View Mode** | `Ctrl + M` or `[ Page \| Scroll ]` toolbar button |
| **Zoom In** | `Ctrl + +` / `Ctrl + =` or `Ctrl + Scroll Up` |
| **Zoom Out** | `Ctrl + -` or `Ctrl + Scroll Down` |
| **Reset Zoom (200%)** | `Ctrl + 0` or click zoom indicator |
| **Fit to Window** | `Fit` button in toolbar |

mod pdf;

use pdf::{CachedRender, PageCache, PdfWorker, RenderRequest, RenderedPage};
use slint::Model;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

slint::include_modules!();

const STATIC_COLORS: &[(&str, u8, u8, u8)] = &[
    ("#facc15", 0xfa, 0xcc, 0x15), // Yellow
    ("#4ade80", 0x4a, 0xde, 0x80), // Green
    ("#60a5fa", 0x60, 0xa5, 0xfa), // Blue
    ("#f472b6", 0xf4, 0x72, 0xb6), // Pink
    ("#fb923c", 0xfb, 0x92, 0x3c), // Orange
];

const HIGHLIGHT_BLEND_COLORS: &[[u8; 3]] = &[
    [254, 240, 138], // Pastel Yellow
    [187, 247, 208], // Pastel Green
    [191, 219, 254], // Pastel Blue
    [251, 207, 232], // Pastel Pink
    [254, 215, 170], // Pastel Orange
];

fn get_highlight_blend_rgb(idx: i32) -> [u8; 3] {
    let i = (idx.max(0) as usize).min(HIGHLIGHT_BLEND_COLORS.len() - 1);
    HIGHLIGHT_BLEND_COLORS[i]
}

fn get_color(idx: i32) -> (String, slint::Color) {
    let i = (idx.max(0) as usize).min(STATIC_COLORS.len() - 1);
    let (hex, r, g, b) = STATIC_COLORS[i];
    (hex.to_string(), slint::Color::from_rgb_u8(r, g, b))
}

fn parse_slint_color(hex: &str) -> Option<slint::Color> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some(slint::Color::from_rgb_u8(r, g, b))
    } else {
        None
    }
}

fn parse_hex_to_rgb(hex: &str) -> Option<(f32, f32, f32)> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some((r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0))
    } else {
        None
    }
}

fn get_highlight_blend_rgb_floats(idx: i32) -> (f32, f32, f32) {
    let rgb = get_highlight_blend_rgb(idx);
    (rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0)
}

fn match_closest_color_index(rgb: (f32, f32, f32)) -> i32 {
    let presets = [
        // Saturated UI button swatches
        (0, (0.98, 0.80, 0.08)), // Yellow (#facc15)
        (1, (0.29, 0.87, 0.50)), // Green  (#4ade80)
        (2, (0.38, 0.65, 0.98)), // Blue   (#60a5fa)
        (3, (0.96, 0.45, 0.71)), // Pink   (#f472b6)
        (4, (0.98, 0.57, 0.24)), // Orange (#fb923c)
        // Soft pastel highlight colors
        (0, (1.00, 0.95, 0.55)), // Pastel Yellow (#fff38c)
        (1, (0.65, 0.95, 0.82)), // Pastel Green  (#a7f3d0)
        (2, (0.73, 0.90, 0.99)), // Pastel Blue   (#bae6fd)
        (3, (0.98, 0.81, 0.91)), // Pastel Pink   (#fbcfe8)
        (4, (1.00, 0.82, 0.61)), // Pastel Orange (#ffd09b)
    ];

    let mut best_idx = 0;
    let mut min_dist = f32::MAX;
    for (idx, (pr, pg, pb)) in presets {
        let dr = rgb.0 - pr;
        let dg = rgb.1 - pg;
        let db = rgb.2 - pb;
        let dist = dr * dr + dg * dg + db * db;
        if dist < min_dist {
            min_dist = dist;
            best_idx = idx;
        }
    }
    best_idx
}

#[derive(Debug, Clone)]
struct AnnotationRecord {
    id: i32,
    page: i32,
    kind: String, // "highlight", "underline", "strikethrough", "note"
    color_index: i32,
    color_hex: String,
    snippet: String,
    comment: String,
    line_rects_pt: Vec<(f32, f32, f32, f32)>, // (x0, y0, x1, y1) in points
    is_from_pdf: bool,
}

struct AppState {
    file_path: String,
    current_page: i32, // 0-indexed
    total_pages: i32,
    zoom_level: f32, // 2.0 = 200% default
    is_scroll_view: bool,
    page_width_pt: f32,
    page_height_pt: f32,
    page_sizes: Vec<(f32, f32)>,
    cache: PageCache,
    worker: PdfWorker,
    annotations: Vec<AnnotationRecord>,
    next_annot_id: i32,
    // Cached character-level text geometry for sub-word selection
    cached_text_page: i32,
    cached_text: pdf::PageTextData,
    sel_start_pt: (f32, f32),
    // Current active selection state in PDF points
    active_selection_page: i32,
    active_selection_snippet: String,
    active_selection_line_rects: Vec<(f32, f32, f32, f32)>,
    is_zooming: bool,
}

impl AppState {
    pub fn get_page_size(&self, page: i32) -> (f32, f32) {
        if page >= 0 && (page as usize) < self.page_sizes.len() {
            self.page_sizes[page as usize]
        } else {
            (self.page_width_pt, self.page_height_pt)
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = AppWindow::new()?;

    let pdf_path_arg = env::args().nth(1);

    match pdf_path_arg {
        None => {
            app.set_file_title("Folio".into());
            app.set_file_path("No PDF file specified".into());
            app.set_has_error(true);
            app.set_error_message(
                "Please specify a PDF file path on the command line:\n\ncargo run --release -- path/to/document.pdf"
                    .into(),
            );
        }
        Some(raw_path) => {
            let path = PathBuf::from(&raw_path);
            let display_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&raw_path)
                .to_string();

            if !path.exists() {
                app.set_file_title(display_name.clone().into());
                app.set_file_path(raw_path.clone().into());
                app.set_has_error(true);
                app.set_error_message(format!("File does not exist:\n{}", path.display()).into());
            } else {
                setup_document(&app, &path, display_name)?;
            }
        }
    }

    app.run()?;
    Ok(())
}

fn setup_document(
    app: &AppWindow,
    path: &Path,
    display_name: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let path_str = path.to_string_lossy().to_string();

    let meta = match pdf::read_pdf_info(&path_str) {
        Ok(m) => m,
        Err(err) => {
            app.set_file_title(display_name.into());
            app.set_file_path(path_str.into());
            app.set_has_error(true);
            app.set_error_message(err.into());
            return Ok(());
        }
    };

    app.set_file_title(display_name.into());
    app.set_file_path(path_str.clone().into());
    app.set_has_error(false);
    app.set_total_pages(meta.page_count);

    // Default zoom value: 200% (2.0)
    let default_zoom = 2.0;

    let (p0_w, p0_h) = meta.page_sizes.first().copied().unwrap_or((meta.initial_width_pt, meta.initial_height_pt));
    let standard_h = if meta.page_sizes.len() > 1 {
        meta.page_sizes[1].1
    } else {
        p0_h
    };
    let card_w = p0_w * default_zoom;
    let card_h = p0_h * default_zoom;
    let scroll_item_h = standard_h * default_zoom + 24.0;

    app.set_page_width(card_w);
    app.set_page_height(card_h);
    app.set_scroll_item_height(scroll_item_h);
    app.set_is_scroll_view(true);

    // Initialize Table of Contents (Outline)
    let has_toc = !meta.toc.is_empty();
    let mut toc_entries = Vec::with_capacity(meta.toc.len());
    for item in &meta.toc {
        toc_entries.push(TocItemData {
            title: item.title.clone().into(),
            page: item.page,
            display_page: format!("{}", item.page + 1).into(),
            depth: item.depth,
        });
    }
    let toc_model = std::rc::Rc::new(slint::VecModel::from(toc_entries));
    app.set_toc_items(toc_model.into());

    // Extract any existing annotations (highlights, underlines, notes) from the PDF
    let existing_annots = pdf::extract_document_annotations(&path_str);
    let mut initial_annotations = Vec::with_capacity(existing_annots.len());
    let mut next_annot_id = 1;

    for ea in existing_annots {
        let color_idx = match_closest_color_index(ea.color_rgb);
        let color_hex = if ea.kind == "highlight" {
            if STATIC_COLORS.iter().any(|(h, ..)| *h == ea.color_hex) {
                let [br, bg, bb] = get_highlight_blend_rgb(color_idx);
                format!("#{br:02x}{bg:02x}{bb:02x}")
            } else {
                ea.color_hex
            }
        } else {
            ea.color_hex
        };

        initial_annotations.push(AnnotationRecord {
            id: next_annot_id,
            page: ea.page,
            kind: ea.kind,
            color_index: color_idx,
            color_hex,
            snippet: ea.snippet,
            comment: ea.comment,
            line_rects_pt: ea.line_rects_pt,
            is_from_pdf: true,
        });
        next_annot_id += 1;
    }

    let has_annotations = !initial_annotations.is_empty();
    app.set_show_sidebar(has_toc || has_annotations);
    if !has_toc && has_annotations {
        app.set_sidebar_tab(1);
    }

    // Initialize the scroll-pages model for continuous scroll view with exact per-page geometry
    let mut items = Vec::with_capacity(meta.page_count as usize);
    let mut cur_offset = 0.0f32;
    for (i, &(w_pt, h_pt)) in meta.page_sizes.iter().enumerate() {
        let pw = w_pt * default_zoom;
        let ph = h_pt * default_zoom;
        items.push(PageItemData {
            page_index: i as i32,
            offset_y: cur_offset,
            width: pw,
            height: ph,
            image: slint::Image::default(),
        });
        cur_offset += ph + 24.0;
    }
    app.set_total_scroll_height(cur_offset + 12.0);
    let model = std::rc::Rc::new(slint::VecModel::from(items));
    app.set_scroll_pages(model.into());

    let state: Arc<Mutex<Option<AppState>>> = Arc::new(Mutex::new(None));
    let state_for_worker = Arc::clone(&state);
    let ui_weak = app.as_weak();

    // Spawn dedicated background worker thread
    let worker = PdfWorker::spawn(path_str.clone(), move |rendered: RenderedPage| {
        let ui_weak = ui_weak.clone();
        let state_lock = Arc::clone(&state_for_worker);

        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            handle_rendered_page(&ui, &state_lock, rendered);
        });
    })
    .map_err(|e| format!("Failed to spawn worker: {e}"))?;

    let app_state = AppState {
        file_path: path_str,
        current_page: 0,
        total_pages: meta.page_count,
        zoom_level: default_zoom,
        is_scroll_view: true,
        page_width_pt: meta.initial_width_pt,
        page_height_pt: meta.initial_height_pt,
        page_sizes: meta.page_sizes,
        cache: PageCache::new(16),
        worker,
        annotations: initial_annotations,
        next_annot_id,
        cached_text_page: -1,
        cached_text: pdf::PageTextData::default(),
        sel_start_pt: (0.0, 0.0),
        active_selection_page: 0,
        active_selection_snippet: String::new(),
        active_selection_line_rects: Vec::new(),
        is_zooming: false,
    };

    if let Ok(mut lock) = state.lock() {
        *lock = Some(app_state);
    }

    // Set up UI callbacks
    setup_callbacks(app, &state);

    // Initial render of page 1 at 200%
    update_view(app, &state);

    Ok(())
}

fn handle_rendered_page(
    ui: &AppWindow,
    state_lock: &Arc<Mutex<Option<AppState>>>,
    rendered: RenderedPage,
) {
    let mut lock = match state_lock.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    let state = match lock.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Drop stale results from older generations
    if rendered.generation != state.worker.current_generation() {
        return;
    }

    if let Some(existing) = state.cache.get(rendered.page, state.zoom_level) {
        if rendered.is_preview && !existing.is_preview {
            return;
        }
    }

    if let Some(image) = rgba_to_image(&rendered.rgba, rendered.w, rendered.h) {
        // Cache full-res renders (and previews as fallbacks)
        state.cache.put(
            rendered.page,
            state.zoom_level,
            CachedRender {
                rgba: rendered.rgba,
                width: rendered.w,
                height: rendered.h,
                is_preview: rendered.is_preview,
            },
        );

        // Update single-page view image if currently active
        if rendered.page == state.current_page {
            ui.set_page_image(image.clone());
            ui.set_page_opacity(1.0);
        }

        // Update scroll-pages model row
        let pages_model = ui.get_scroll_pages();
        let idx = rendered.page as usize;
        if let Some(mut item) = pages_model.row_data(idx) {
            item.image = image;
            pages_model.set_row_data(idx, item);
        }
    }
}

fn ensure_cached_text(state: &mut AppState, page: i32) {
    if state.cached_text_page != page || state.cached_text.lines.is_empty() {
        state.cached_text_page = page;
        state.cached_text = pdf::extract_page_text_data(&state.file_path, page);
    }
}

fn get_page_highlights(state: &AppState, page: i32) -> Vec<pdf::worker::HighlightOverlay> {
    let mut res = Vec::new();
    for a in &state.annotations {
        if a.page == page && a.kind == "highlight" && !a.is_from_pdf {
            let rgb = get_highlight_blend_rgb(a.color_index);
            let adjusted = adjust_line_rects_for_highlight(&a.line_rects_pt);
            res.push(pdf::worker::HighlightOverlay {
                rgb,
                rects_pt: adjusted,
            });
        }
    }
    res
}

use pdf::adjust_line_rects_for_highlight;

fn sync_annotations_to_ui(s: &AppState, ui: &AppWindow) {
    let mut highlights = Vec::new();
    let mut notes = Vec::new();
    let mut visuals = Vec::new();

    for annot in &s.annotations {
        let col = parse_slint_color(&annot.color_hex).unwrap_or_else(|| get_color(annot.color_index).1);
        let item_data = AnnotationItemData {
            id: annot.id,
            page: annot.page,
            kind: annot.kind.clone().into(),
            color: col,
            color_hex: annot.color_hex.clone().into(),
            snippet: annot.snippet.clone().into(),
            comment: annot.comment.clone().into(),
            display_page: format!("{}", annot.page + 1).into(),
        };

        if annot.kind == "note" {
            notes.push(item_data);
        } else {
            highlights.push(item_data);
        }

        // Map line rectangles to visual overlays scaled by zoom.
        // For annotations from PDF, highlights/underlines/strikeouts are already drawn by MuPDF onto the pixmap.
        // We only generate Slint visual overlays for annotations created in this session, or notes from PDF (to show the icon).
        if !annot.is_from_pdf || annot.kind == "note" {
            for rect in &annot.line_rects_pt {
                visuals.push(AnnotationVisual {
                    id: annot.id,
                    page: annot.page,
                    kind: annot.kind.clone().into(),
                    color: col,
                    x: rect.0 * s.zoom_level,
                    y: rect.1 * s.zoom_level,
                    w: (rect.2 - rect.0).max(4.0) * s.zoom_level,
                    h: (rect.3 - rect.1).max(4.0) * s.zoom_level,
                    comment: annot.comment.clone().into(),
                });
            }
        }
    }

    let h_model = std::rc::Rc::new(slint::VecModel::from(highlights));
    ui.set_highlight_items(h_model.into());

    let n_model = std::rc::Rc::new(slint::VecModel::from(notes));
    ui.set_note_items(n_model.into());

    let v_model = std::rc::Rc::new(slint::VecModel::from(visuals));
    ui.set_page_annotations(v_model.into());

    // Update active selection rects according to the current zoom level
    if !s.active_selection_line_rects.is_empty() {
        let mut ui_rects = Vec::new();
        for (rx0, ry0, rx1, ry1) in adjust_line_rects_for_highlight(&s.active_selection_line_rects) {
            ui_rects.push(SelectionRect {
                x: rx0 * s.zoom_level,
                y: ry0 * s.zoom_level,
                width: (rx1 - rx0) * s.zoom_level,
                height: (ry1 - ry0) * s.zoom_level,
            });
        }
        if !ui_rects.is_empty() {
            let first = &ui_rects[0];
            let (pw_pt, _) = s.get_page_size(s.active_selection_page);
            let page_w = pw_pt * s.zoom_level;
            let popup_x = (first.x + (first.width - 250.0) / 2.0).clamp(10.0, (page_w - 260.0).max(10.0));
            let popup_y = (first.y - 48.0).max(8.0);
            ui.set_popup_x(popup_x);
            ui.set_popup_y(popup_y);
        }
        ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::from(ui_rects)).into());
    }
}

fn update_view(ui: &AppWindow, state_lock: &Arc<Mutex<Option<AppState>>>) {
    let mut lock = match state_lock.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    let state = match lock.as_mut() {
        Some(s) => s,
        None => return,
    };

    let scale_factor = ui.window().scale_factor().max(1.0);
    let actual_render_zoom = (state.zoom_level * scale_factor).max(0.1);

    // Update toolbar page indicator: e.g. "1 / 120"
    ui.set_current_page(state.current_page);
    ui.set_page_display(format!("{} / {}", state.current_page + 1, state.total_pages).into());
    ui.set_can_prev(state.current_page > 0);
    ui.set_can_next(state.current_page < state.total_pages - 1);
    ui.set_zoom_display(format!("{}%", (state.zoom_level * 100.0).round() as i32).into());
    ui.set_zoom_level(state.zoom_level);

    // Update displayed page dimensions for single page view with current page's exact geometry
    let (cur_w_pt, cur_h_pt) = state.get_page_size(state.current_page);
    let card_w = cur_w_pt * state.zoom_level;
    let card_h = cur_h_pt * state.zoom_level;
    ui.set_page_width(card_w);
    ui.set_page_height(card_h);

    // Update dimensions in scroll-pages model for continuous scroll view
    let standard_h = if state.page_sizes.len() > 1 {
        state.page_sizes[1].1
    } else {
        cur_h_pt
    };
    let scroll_item_h = standard_h * state.zoom_level + 24.0;
    ui.set_scroll_item_height(scroll_item_h);

    let pages_model = ui.get_scroll_pages();
    let mut cur_offset = 0.0f32;
    for i in 0..state.total_pages as usize {
        let (pw_pt, ph_pt) = state.get_page_size(i as i32);
        let pw = pw_pt * state.zoom_level;
        let ph = ph_pt * state.zoom_level;
        if let Some(mut item) = pages_model.row_data(i) {
            item.offset_y = cur_offset;
            item.width = pw;
            item.height = ph;
            if let Some(cached) = state.cache.get(i as i32, state.zoom_level) {
                if let Some(img) = rgba_to_image(&cached.rgba, cached.width, cached.height) {
                    item.image = img;
                }
            }
            pages_model.set_row_data(i, item);
        }
        cur_offset += ph + 24.0;
    }
    ui.set_total_scroll_height(cur_offset + 12.0);

    // Refresh annotation visual coordinates scaled to current zoom
    sync_annotations_to_ui(state, ui);

    // Bump generation counter
    let next_gen = state.worker.current_generation() + 1;
    state.worker.set_current_generation(next_gen);

    // Check LRU cache for current page
    let cached = state.cache.get(state.current_page, state.zoom_level).cloned();

    if let Some(cached_render) = cached {
        if let Some(img) = rgba_to_image(&cached_render.rgba, cached_render.width, cached_render.height) {
            ui.set_page_image(img.clone());
            ui.set_page_opacity(1.0);

            let idx = state.current_page as usize;
            if let Some(mut item) = pages_model.row_data(idx) {
                item.image = img;
                pages_model.set_row_data(idx, item);
            }
        }
        if !cached_render.is_preview {
            prefetch_neighbors(state, next_gen, actual_render_zoom);
            return;
        }
    }

    // Step 1: Low-res preview first (0.25x)
    let preview_zoom = (actual_render_zoom * 0.25).max(0.1);
    let h_current = get_page_highlights(state, state.current_page);
    state.worker.request_render(RenderRequest {
        page: state.current_page,
        zoom: preview_zoom,
        generation: next_gen,
        is_preview: true,
        highlights: h_current.clone(),
    });

    // Step 2: Full-res render for current page
    state.worker.request_render(RenderRequest {
        page: state.current_page,
        zoom: actual_render_zoom,
        generation: next_gen,
        is_preview: false,
        highlights: h_current,
    });

    // Step 3: Prefetch neighbors
    prefetch_neighbors(state, next_gen, actual_render_zoom);
}

fn prefetch_neighbors(state: &mut AppState, generation: u64, zoom: f32) {
    let neighbors = [
        state.current_page + 1,
        state.current_page - 1,
        state.current_page + 2,
        state.current_page - 2,
    ];

    for &p in &neighbors {
        if p >= 0 && p < state.total_pages {
            if state.cache.get(p, state.zoom_level).is_none() {
                let h_neighbor = get_page_highlights(state, p);
                state.worker.request_render(RenderRequest {
                    page: p,
                    zoom,
                    generation,
                    is_preview: false,
                    highlights: h_neighbor,
                });
            }
        }
    }
}

fn trigger_scroll_page_render(state: &mut AppState, page_idx: i32, ui: &AppWindow) {
    let scale_factor = ui.window().scale_factor().max(1.0);
    let actual_render_zoom = (state.zoom_level * scale_factor).max(0.1);

    let next_gen = state.worker.current_generation() + 1;
    state.worker.set_current_generation(next_gen);

    // If cached, apply immediately
    if let Some(cached) = state.cache.get(page_idx, state.zoom_level).cloned() {
        if let Some(img) = rgba_to_image(&cached.rgba, cached.width, cached.height) {
            let pages_model = ui.get_scroll_pages();
            let idx = page_idx as usize;
            if let Some(mut item) = pages_model.row_data(idx) {
                item.image = img.clone();
                pages_model.set_row_data(idx, item);
            }
            if page_idx == state.current_page {
                ui.set_page_image(img);
                ui.set_page_opacity(1.0);
            }
        }
        if !cached.is_preview {
            prefetch_neighbors(state, next_gen, actual_render_zoom);
            return;
        }
    }

    // Preview then full-res
    let preview_zoom = (actual_render_zoom * 0.25).max(0.1);
    let h_page = get_page_highlights(state, page_idx);
    state.worker.request_render(RenderRequest {
        page: page_idx,
        zoom: preview_zoom,
        generation: next_gen,
        is_preview: true,
        highlights: h_page.clone(),
    });

    state.worker.request_render(RenderRequest {
        page: page_idx,
        zoom: actual_render_zoom,
        generation: next_gen,
        is_preview: false,
        highlights: h_page,
    });

    prefetch_neighbors(state, next_gen, actual_render_zoom);
}

fn trigger_scroll_page_render_direct(state: &mut AppState, page_idx: i32, ui: &AppWindow) {
    let scale_factor = ui.window().scale_factor().max(1.0);
    let actual_render_zoom = (state.zoom_level * scale_factor).max(0.1);

    let next_gen = state.worker.current_generation() + 1;
    state.worker.set_current_generation(next_gen);

    let h_page = get_page_highlights(state, page_idx);
    state.worker.request_render(RenderRequest {
        page: page_idx,
        zoom: actual_render_zoom,
        generation: next_gen,
        is_preview: false,
        highlights: h_page,
    });
}

fn setup_callbacks(app: &AppWindow, state_lock: &Arc<Mutex<Option<AppState>>>) {
    // Toggle Sidebar
    {
        let app_weak = app.as_weak();
        app.on_toggle_sidebar(move || {
            if let Some(ui) = app_weak.upgrade() {
                let cur = ui.get_show_sidebar();
                ui.set_show_sidebar(!cur);
            }
        });
    }

    // Jump to Page (from Table of Contents or Annotations)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_jump_to_page(move |target_page| {
            let mut is_scroll = false;
            let mut total_pages = 0;
            let valid = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        if target_page >= 0 && target_page < s.total_pages {
                            s.current_page = target_page;
                            is_scroll = s.is_scroll_view;
                            total_pages = s.total_pages;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if valid {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_current_page(target_page);
                    ui.set_page_display(format!("{} / {}", target_page + 1, total_pages).into());
                    ui.set_can_prev(target_page > 0);
                    ui.set_can_next(target_page < total_pages - 1);

                    if is_scroll {
                        ui.invoke_scroll_to_page(target_page);
                        if let Ok(mut lock) = state_lock.lock() {
                            if let Some(s) = lock.as_mut() {
                                trigger_scroll_page_render(s, target_page, &ui);
                            }
                        }
                    } else {
                        update_view(&ui, &state_lock);
                    }
                }
            }
        });
    }

    // Selection Started
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_selection_started(move |page_no, local_x, local_y| {
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    let zoom = s.zoom_level.max(0.1);
                    s.active_selection_page = page_no;
                    s.sel_start_pt = (local_x / zoom, local_y / zoom);
                    s.active_selection_snippet.clear();
                    s.active_selection_line_rects.clear();
                    ensure_cached_text(s, page_no);
                }
            }
            if let Some(ui) = app_weak.upgrade() {
                ui.set_has_selection(false);
                ui.set_show_popup(false);
                ui.set_show_note_input(false);
                ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
            }
        });
    }

    // Selection Dragged (live sub-word character level selection)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_selection_dragged(move |page_no, local_x, local_y| {
            let mut line_rects_ui = Vec::new();
            let mut has_sel = false;

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    let zoom = s.zoom_level.max(0.1);
                    let pt_x = local_x / zoom;
                    let pt_y = local_y / zoom;
                    ensure_cached_text(s, page_no);

                    let (snippet, rects_pt) = pdf::compute_text_selection(
                        &s.cached_text,
                        s.sel_start_pt.0,
                        s.sel_start_pt.1,
                        pt_x,
                        pt_y,
                    );

                    s.active_selection_snippet = snippet;
                    s.active_selection_line_rects = rects_pt;

                    if !s.active_selection_line_rects.is_empty() {
                        has_sel = true;
                        for (rx0, ry0, rx1, ry1) in adjust_line_rects_for_highlight(&s.active_selection_line_rects) {
                            line_rects_ui.push(SelectionRect {
                                x: rx0 * zoom,
                                y: ry0 * zoom,
                                width: (rx1 - rx0) * zoom,
                                height: (ry1 - ry0) * zoom,
                            });
                        }
                    }
                }
            }

            if let Some(ui) = app_weak.upgrade() {
                if has_sel {
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::from(line_rects_ui)).into());
                    ui.set_has_selection(true);
                } else {
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
                    ui.set_has_selection(false);
                }
            }
        });
    }

    // Selection Completed (mouse released)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_selection_completed(move |page_no, local_x, local_y| {
            let mut line_rects_ui = Vec::new();
            let mut has_sel = false;
            let mut popup_x = 0.0f32;
            let mut popup_y = 0.0f32;

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    let zoom = s.zoom_level.max(0.1);
                    let pt_x = local_x / zoom;
                    let pt_y = local_y / zoom;
                    ensure_cached_text(s, page_no);

                    let (snippet, rects_pt) = pdf::compute_text_selection(
                        &s.cached_text,
                        s.sel_start_pt.0,
                        s.sel_start_pt.1,
                        pt_x,
                        pt_y,
                    );

                    s.active_selection_snippet = snippet;
                    s.active_selection_line_rects = rects_pt;

                    if !s.active_selection_line_rects.is_empty() && !s.active_selection_snippet.trim().is_empty() {
                        has_sel = true;
                        for (rx0, ry0, rx1, ry1) in adjust_line_rects_for_highlight(&s.active_selection_line_rects) {
                            line_rects_ui.push(SelectionRect {
                                x: rx0 * zoom,
                                y: ry0 * zoom,
                                width: (rx1 - rx0) * zoom,
                                height: (ry1 - ry0) * zoom,
                            });
                        }
                        if let Some(first) = line_rects_ui.first() {
                            let (pw_pt, _) = s.get_page_size(page_no);
                            let page_w = pw_pt * zoom;
                            popup_x = (first.x + (first.width - 250.0) / 2.0).clamp(10.0, (page_w - 260.0).max(10.0));
                            popup_y = (first.y - 48.0).max(8.0);
                        }
                    } else {
                        s.active_selection_line_rects.clear();
                        s.active_selection_snippet.clear();
                    }
                }
            }

            if let Some(ui) = app_weak.upgrade() {
                if has_sel {
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::from(line_rects_ui)).into());
                    ui.set_has_selection(true);
                    ui.set_popup_x(popup_x);
                    ui.set_popup_y(popup_y);
                    ui.set_show_popup(true);
                } else {
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
                    ui.set_has_selection(false);
                    ui.set_show_popup(false);
                    ui.set_show_note_input(false);
                }
            }
        });
    }

    // Create Highlight / Underline / Strikethrough Annotation
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_create_annotation(move |kind, color_idx| {
            let hex = if kind == "highlight" {
                let [r, g, b] = get_highlight_blend_rgb(color_idx);
                format!("#{r:02x}{g:02x}{b:02x}")
            } else {
                get_color(color_idx).0
            };
            let mut need_sync = false;
            let mut highlight_update: Option<(i32, slint::Image)> = None;

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    if !s.active_selection_line_rects.is_empty() {
                        let target_page = s.active_selection_page;
                        let annot = AnnotationRecord {
                            id: s.next_annot_id,
                            page: target_page,
                            kind: kind.to_string(),
                            color_index: color_idx,
                            color_hex: hex,
                            snippet: s.active_selection_snippet.clone(),
                            comment: String::new(),
                            line_rects_pt: s.active_selection_line_rects.clone(),
                            is_from_pdf: false,
                        };
                        s.next_annot_id += 1;

                        if kind == "highlight" {
                            let rgb = get_highlight_blend_rgb(color_idx);
                            let adjusted_rects = adjust_line_rects_for_highlight(&s.active_selection_line_rects);
                            let (pw_pt, _) = s.get_page_size(target_page);

                            if let Some(cached) = s.cache.get_mut(target_page, s.zoom_level) {
                                let w = cached.width;
                                let h = cached.height;
                                let zoom = w as f32 / pw_pt.max(1.0);

                                let hr = rgb[0] as u16;
                                let hg = rgb[1] as u16;
                                let hb = rgb[2] as u16;

                                let mut applied = vec![false; (w * h) as usize];
                                for rect in &adjusted_rects {
                                    let px0 = ((rect.0 * zoom).round() as usize).min(w as usize);
                                    let py0 = ((rect.1 * zoom).round() as usize).min(h as usize);
                                    let px1 = ((rect.2 * zoom).round() as usize).min(w as usize);
                                    let py1 = ((rect.3 * zoom).round() as usize).min(h as usize);

                                    for y in py0..py1 {
                                        let row_offset = y * (w as usize);
                                        for x in px0..px1 {
                                            let p_idx = row_offset + x;
                                            if !applied[p_idx] {
                                                applied[p_idx] = true;
                                                let idx = p_idx * 4;
                                                if idx + 2 < cached.rgba.len() {
                                                    cached.rgba[idx] = ((cached.rgba[idx] as u16 * hr) / 255) as u8;
                                                    cached.rgba[idx + 1] = ((cached.rgba[idx + 1] as u16 * hg) / 255) as u8;
                                                    cached.rgba[idx + 2] = ((cached.rgba[idx + 2] as u16 * hb) / 255) as u8;
                                                }
                                            }
                                        }
                                    }
                                }

                                if let Some(img) = rgba_to_image(&cached.rgba, w, h) {
                                    highlight_update = Some((target_page, img));
                                }
                            }
                        }

                        s.annotations.push(annot);
                        s.active_selection_line_rects.clear();
                        s.active_selection_snippet.clear();
                        need_sync = true;
                    }
                }
            }

            if need_sync {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_dirty(true);
                    ui.set_has_selection(false);
                    ui.set_show_popup(false);
                    ui.set_show_note_input(false);
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());

                    // If a highlight was created and blended directly in-place, update the UI image immediately
                    if let Some((page, img)) = highlight_update {
                        let pages_model = ui.get_scroll_pages();
                        let idx = page as usize;
                        if let Some(mut item) = pages_model.row_data(idx) {
                            item.image = img.clone();
                            pages_model.set_row_data(idx, item);
                        }
                        if page == ui.get_current_page() {
                            ui.set_page_image(img);
                        }
                    }

                    if let Ok(lock) = state_lock.lock() {
                        if let Some(s) = lock.as_ref() {
                            sync_annotations_to_ui(s, &ui);
                        }
                    }
                }
            }
        });
    }

    // Create Note Annotation
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_create_note_annotation(move |comment, color_idx| {
            let (hex, _) = get_color(color_idx);
            let mut need_sync = false;

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    if !s.active_selection_line_rects.is_empty() {
                        let annot = AnnotationRecord {
                            id: s.next_annot_id,
                            page: s.active_selection_page,
                            kind: "note".to_string(),
                            color_index: color_idx,
                            color_hex: hex,
                            snippet: s.active_selection_snippet.clone(),
                            comment: comment.to_string(),
                            line_rects_pt: s.active_selection_line_rects.clone(),
                            is_from_pdf: false,
                        };
                        s.next_annot_id += 1;
                        s.annotations.push(annot);
                        s.active_selection_line_rects.clear();
                        s.active_selection_snippet.clear();
                        need_sync = true;
                    }
                }
            }

            if need_sync {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_dirty(true);
                    ui.set_has_selection(false);
                    ui.set_show_popup(false);
                    ui.set_show_note_input(false);
                    ui.set_current_note_text("".into());
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
                    if let Ok(lock) = state_lock.lock() {
                        if let Some(s) = lock.as_ref() {
                            sync_annotations_to_ui(s, &ui);
                        }
                    }
                }
            }
        });
    }

    // Delete Annotation
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_delete_annotation(move |id| {
            let mut need_sync = false;
            let mut deleted_page = None;
            let mut is_highlight = false;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    if let Some(pos) = s.annotations.iter().position(|a| a.id == id) {
                        let page = s.annotations[pos].page;
                        deleted_page = Some(page);
                        is_highlight = s.annotations[pos].kind == "highlight";
                        s.annotations.remove(pos);
                        need_sync = true;
                        if is_highlight {
                            s.cache.remove_page(page);
                        }
                    }
                }
            }
            if need_sync {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_dirty(true);
                    if let Ok(mut lock) = state_lock.lock() {
                        if let Some(s) = lock.as_mut() {
                            sync_annotations_to_ui(s, &ui);
                            if is_highlight {
                                if let Some(p) = deleted_page {
                                    if s.is_scroll_view {
                                        trigger_scroll_page_render_direct(s, p, &ui);
                                    } else {
                                        update_view(&ui, &state_lock);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Save Document
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_save_document(move || {
            let (file_path, to_save) = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        let to_save: Vec<pdf::SaveAnnotationData> = s
                            .annotations
                            .iter()
                            .map(|a| {
                                let rgb = if a.kind == "highlight" {
                                    if !a.is_from_pdf || STATIC_COLORS.iter().any(|(h, ..)| *h == a.color_hex) {
                                        get_highlight_blend_rgb_floats(a.color_index)
                                    } else if let Some((r, g, b)) = parse_hex_to_rgb(&a.color_hex) {
                                        (r, g, b)
                                    } else {
                                        get_highlight_blend_rgb_floats(a.color_index)
                                    }
                                } else if let Some((r, g, b)) = parse_hex_to_rgb(&a.color_hex) {
                                    (r, g, b)
                                } else {
                                    get_highlight_blend_rgb_floats(a.color_index)
                                };

                                pdf::SaveAnnotationData {
                                    page: a.page,
                                    kind: a.kind.clone(),
                                    color_rgb: rgb,
                                    snippet: a.snippet.clone(),
                                    comment: a.comment.clone(),
                                    line_rects_pt: a.line_rects_pt.clone(),
                                }
                            })
                            .collect();

                        (s.file_path.clone(), to_save)
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_saving(true);
            }

            let save_result = pdf::save_document_annotations(&file_path, &to_save);

            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_saving(false);
                match save_result {
                    Ok(()) => {
                        ui.set_is_dirty(false);
                        ui.set_show_saved_badge(true);

                        let ui_weak = app_weak.clone();
                        slint::Timer::single_shot(
                            std::time::Duration::from_millis(2000),
                            move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    ui.set_show_saved_badge(false);
                                }
                            },
                        );
                    }
                    Err(e) => {
                        eprintln!("Failed to save PDF: {e}");
                    }
                }
            }
        });
    }

    // Clear Selection
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_clear_selection(move || {
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    s.active_selection_line_rects.clear();
                    s.active_selection_snippet.clear();
                }
            }
            if let Some(ui) = app_weak.upgrade() {
                ui.set_has_selection(false);
                ui.set_show_popup(false);
                ui.set_show_note_input(false);
                ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
            }
        });
    }

    // Toggle View Mode (Page vs Scroll)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_set_view_mode(move |is_scroll| {
            let target_page = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        s.is_scroll_view = is_scroll;
                        s.current_page
                    } else {
                        0
                    }
                } else {
                    0
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_scroll_view(is_scroll);
                if is_scroll {
                    ui.invoke_scroll_to_page(target_page);
                } else {
                    update_view(&ui, &state_lock);
                }
            }
        });
    }

    // Visible Page Changed (scrolling in continuous scroll view)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_visible_page_changed(move |new_page| {
            let mut changed = false;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    if s.is_zooming {
                        return;
                    }
                    if s.current_page != new_page && new_page >= 0 && new_page < s.total_pages {
                        s.current_page = new_page;
                        changed = true;
                    }
                }
            }

            if changed {
                if let Some(ui) = app_weak.upgrade() {
                    if let Ok(mut lock) = state_lock.lock() {
                        if let Some(s) = lock.as_mut() {
                            ui.set_current_page(s.current_page);
                            ui.set_page_display(format!("{} / {}", s.current_page + 1, s.total_pages).into());
                            ui.set_can_prev(s.current_page > 0);
                            ui.set_can_next(s.current_page < s.total_pages - 1);
                            trigger_scroll_page_render(s, new_page, &ui);
                        }
                    }
                }
            }
        });
    }

    // Prev Page
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_prev_page(move || {
            let mut is_scroll = false;
            let mut new_page = 0;
            let changed = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        if s.current_page > 0 {
                            s.current_page -= 1;
                            is_scroll = s.is_scroll_view;
                            new_page = s.current_page;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if changed {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_current_page(new_page);
                    if is_scroll {
                        ui.invoke_scroll_to_page(new_page);
                        if let Ok(mut lock) = state_lock.lock() {
                            if let Some(s) = lock.as_mut() {
                                ui.set_page_display(format!("{} / {}", s.current_page + 1, s.total_pages).into());
                                ui.set_can_prev(s.current_page > 0);
                                ui.set_can_next(s.current_page < s.total_pages - 1);
                                trigger_scroll_page_render(s, new_page, &ui);
                            }
                        }
                    } else {
                        update_view(&ui, &state_lock);
                    }
                }
            }
        });
    }

    // Next Page
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_next_page(move || {
            let mut is_scroll = false;
            let mut new_page = 0;
            let changed = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        if s.current_page < s.total_pages - 1 {
                            s.current_page += 1;
                            is_scroll = s.is_scroll_view;
                            new_page = s.current_page;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if changed {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_current_page(new_page);
                    if is_scroll {
                        ui.invoke_scroll_to_page(new_page);
                        if let Ok(mut lock) = state_lock.lock() {
                            if let Some(s) = lock.as_mut() {
                                ui.set_page_display(format!("{} / {}", s.current_page + 1, s.total_pages).into());
                                ui.set_can_prev(s.current_page > 0);
                                ui.set_can_next(s.current_page < s.total_pages - 1);
                                trigger_scroll_page_render(s, new_page, &ui);
                            }
                        }
                    } else {
                        update_view(&ui, &state_lock);
                    }
                }
            }
        });
    }

    // Zoom In
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_in(move || {
            if let Some(ui) = app_weak.upgrade() {
                perform_zoom(&ui, &state_lock, 1.2, None);
            }
        });
    }

    // Zoom Out
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_out(move || {
            if let Some(ui) = app_weak.upgrade() {
                perform_zoom(&ui, &state_lock, 1.0 / 1.2, None);
            }
        });
    }

    // Zoom In At (centered on target page)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_in_at(move |page| {
            if let Some(ui) = app_weak.upgrade() {
                perform_zoom(&ui, &state_lock, 1.2, Some(page));
            }
        });
    }

    // Zoom Out At (centered on target page)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_out_at(move |page| {
            if let Some(ui) = app_weak.upgrade() {
                perform_zoom(&ui, &state_lock, 1.0 / 1.2, Some(page));
            }
        });
    }

    // Zoom Reset (200% default)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_reset(move || {
            if let Some(ui) = app_weak.upgrade() {
                ui.set_suppress_page_change(true);
                let mut is_scroll = false;
                let mut cur_page = 0;
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        s.zoom_level = 2.0;
                        is_scroll = s.is_scroll_view;
                        cur_page = s.current_page;
                    }
                }
                update_view(&ui, &state_lock);
                if is_scroll {
                    ui.invoke_scroll_to_page(cur_page);
                }
                ui.set_suppress_page_change(false);
            }
        });
    }

    // Zoom Fit
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_fit(move || {
            if let Some(ui) = app_weak.upgrade() {
                ui.set_suppress_page_change(true);
                let phys_size = ui.window().size();
                let scale_factor = ui.window().scale_factor().max(1.0);
                let sidebar_w = if ui.get_show_sidebar() { 290.0 } else { 0.0 };
                let avail_w = (phys_size.width as f32 / scale_factor) - 80.0 - sidebar_w;
                let avail_h = (phys_size.height as f32 / scale_factor) - 120.0;

                let factor = if let Ok(lock) = state_lock.lock() {
                    if let Some(s) = lock.as_ref() {
                        let (pw, ph) = s.get_page_size(s.current_page);
                        let fit_w = avail_w / pw.max(1.0);
                        let fit_h = avail_h / ph.max(1.0);
                        let target_zoom = fit_w.min(fit_h).clamp(0.2, 5.0);
                        target_zoom / s.zoom_level
                    } else {
                        1.0
                    }
                } else {
                    1.0
                };
                perform_zoom(&ui, &state_lock, factor, None);
            }
        });
    }
}

fn perform_zoom(
    ui: &AppWindow,
    state_lock: &Arc<Mutex<Option<AppState>>>,
    factor: f32,
    target_page: Option<i32>,
) {
    let mut is_scroll = false;
    let mut old_zoom = 1.0;
    let mut new_zoom = 1.0;

    if let Ok(mut lock) = state_lock.lock() {
        if let Some(s) = lock.as_mut() {
            old_zoom = s.zoom_level;
            new_zoom = (old_zoom * factor).clamp(0.2, 5.0);
            s.zoom_level = new_zoom;
            s.is_zooming = true;
            if let Some(tp) = target_page {
                if tp >= 0 && tp < s.total_pages {
                    s.current_page = tp;
                }
            }
            is_scroll = s.is_scroll_view;
        }
    }

    if is_scroll {
        let old_y = ui.get_scroll_content_y();
        let scale_ratio = new_zoom / old_zoom;
        let new_y = old_y * scale_ratio;

        ui.set_suppress_page_change(true);
        update_view(ui, state_lock);
        ui.set_scroll_content_y(new_y);
    } else {
        update_view(ui, state_lock);
    }

    let state_lock_clone = Arc::clone(state_lock);
    let ui_weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
        if let Ok(mut lock) = state_lock_clone.lock() {
            if let Some(s) = lock.as_mut() {
                s.is_zooming = false;
            }
        }
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_suppress_page_change(false);
        }
    });
}

fn rgba_to_image(rgba: &[u8], w: u32, h: u32) -> Option<slint::Image> {
    if w == 0 || h == 0 || rgba.len() < (w as usize * h as usize * 4) {
        return None;
    }

    let mut pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    let slice = pixel_buffer.make_mut_slice();

    for (dst, chunk) in slice.iter_mut().zip(rgba.chunks_exact(4)) {
        dst.r = chunk[0];
        dst.g = chunk[1];
        dst.b = chunk[2];
        dst.a = chunk[3];
    }

    Some(slint::Image::from_rgba8(pixel_buffer))
}

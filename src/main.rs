mod export;
mod history;
mod pdf;

use pdf::{CachedRender, PageCache, PdfWorker, RenderRequest, RenderedPage};
use slint::Model;
use std::collections::VecDeque;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

slint::include_modules!();

const MAX_UNDO_OPERATIONS: usize = 100;

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

#[derive(Debug, Clone)]
enum UndoAction {
    Created(AnnotationRecord),
    Deleted {
        index: usize,
        record: AnnotationRecord,
    },
    ColorChanged {
        id: i32,
        old_color_index: i32,
        old_color_hex: String,
        new_color_index: i32,
        new_color_hex: String,
    },
    NoteChanged {
        id: i32,
        old_comment: String,
        new_comment: String,
    },
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
    undo_stack: VecDeque<UndoAction>,
    redo_stack: VecDeque<UndoAction>,
    // Cached character-level text geometry for sub-word selection
    cached_text_page: i32,
    cached_text: pdf::PageTextData,
    sel_start_pt: (f32, f32),
    // Current active selection state in PDF points
    active_selection_page: i32,
    active_selection_snippet: String,
    active_selection_line_rects: Vec<(f32, f32, f32, f32)>,
    is_zooming: bool,
    // Search state
    search_query: String,
    search_matches: Vec<pdf::SearchHitResult>,
    active_search_index: i32,
    is_searching: bool,
    search_match_case: bool,
    search_match_whole_word: bool,
    pub toc: Vec<pdf::TocItem>,
}

impl AppState {
    pub fn push_undo(&mut self, action: UndoAction) {
        if self.undo_stack.len() >= MAX_UNDO_OPERATIONS {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(action);
        self.redo_stack.clear();
    }

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

    let app_weak = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
        if let Some(app) = app_weak.upgrade() {
            app.invoke_grab_focus();
        }
    });

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

    // Restore saved session history if available
    let saved_session = history::load_document_session(&path_str);
    let (initial_page, initial_zoom, initial_is_scroll, initial_scroll_y) = if let Some(ref s) = saved_session {
        let p = s.page.clamp(0, (meta.page_count - 1).max(0));
        let z = if s.zoom_level >= 0.5 && s.zoom_level <= 5.0 { s.zoom_level } else { default_zoom };
        (p, z, s.is_scroll_view, s.scroll_y)
    } else {
        (0, default_zoom, true, 0.0)
    };

    let (p0_w, p0_h) = meta.page_sizes.first().copied().unwrap_or((meta.initial_width_pt, meta.initial_height_pt));
    let standard_h = if meta.page_sizes.len() > 1 {
        meta.page_sizes[1].1
    } else {
        p0_h
    };
    let card_w = p0_w * initial_zoom;
    let card_h = p0_h * initial_zoom;
    let scroll_item_h = standard_h * initial_zoom + 24.0;

    app.set_page_width(card_w);
    app.set_page_height(card_h);
    app.set_scroll_item_height(scroll_item_h);
    app.set_is_scroll_view(initial_is_scroll);
    app.set_current_page(initial_page);
    app.set_zoom_level(initial_zoom);
    app.set_zoom_display(format!("{}%", (initial_zoom * 100.0).round() as i32).into());
    app.set_page_display(format!("{} / {}", initial_page + 1, meta.page_count).into());
    app.set_can_prev(initial_page > 0);
    app.set_can_next(initial_page < meta.page_count - 1);

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
        let pw = w_pt * initial_zoom;
        let ph = h_pt * initial_zoom;
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
        current_page: initial_page,
        total_pages: meta.page_count,
        zoom_level: initial_zoom,
        is_scroll_view: initial_is_scroll,
        page_width_pt: meta.initial_width_pt,
        page_height_pt: meta.initial_height_pt,
        page_sizes: meta.page_sizes,
        cache: PageCache::new(16),
        worker,
        annotations: initial_annotations,
        next_annot_id,
        undo_stack: VecDeque::new(),
        redo_stack: VecDeque::new(),
        cached_text_page: -1,
        cached_text: pdf::PageTextData::default(),
        sel_start_pt: (0.0, 0.0),
        active_selection_page: 0,
        active_selection_snippet: String::new(),
        active_selection_line_rects: Vec::new(),
        is_zooming: false,
        search_query: String::new(),
        search_matches: Vec::new(),
        active_search_index: -1,
        is_searching: false,
        search_match_case: false,
        search_match_whole_word: false,
        toc: meta.toc,
    };

    if let Ok(mut lock) = state.lock() {
        *lock = Some(app_state);
    }

    // Set up UI callbacks
    setup_callbacks(app, &state);

    // Initial render of page
    update_view(app, &state);

    // Restore exact scroll position or jump to page
    if initial_is_scroll {
        if initial_scroll_y != 0.0 {
            app.set_scroll_content_y(initial_scroll_y);
        } else if initial_page > 0 {
            app.invoke_scroll_to_page(initial_page);
        }
        if let Ok(mut lock) = state.lock() {
            if let Some(s) = lock.as_mut() {
                trigger_scroll_page_render_direct(s, initial_page, app);
            }
        }
    }

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
        if a.page == page && a.kind == "highlight" {
            let rgb = if let Some((r, g, b)) = parse_hex_to_rgb(&a.color_hex) {
                [
                    (r * 255.0).round().clamp(0.0, 255.0) as u8,
                    (g * 255.0).round().clamp(0.0, 255.0) as u8,
                    (b * 255.0).round().clamp(0.0, 255.0) as u8,
                ]
            } else {
                get_highlight_blend_rgb(a.color_index)
            };
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
        let swatch_col = get_color(annot.color_index).1;
        let col = if annot.kind == "highlight" {
            swatch_col
        } else {
            parse_slint_color(&annot.color_hex).unwrap_or(swatch_col)
        };
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

        let has_note = annot.kind == "note" || !annot.comment.trim().is_empty();
        if has_note {
            notes.push(item_data.clone());
        }
        if annot.kind != "note" {
            highlights.push(item_data);
        }

        // All annotations (including highlights) provide visual hitboxes for hover & click interaction
        let num_rects = annot.line_rects_pt.len();
        for (idx, rect) in annot.line_rects_pt.iter().enumerate() {
            let is_last_rect = idx + 1 == num_rects;
            visuals.push(AnnotationVisual {
                id: annot.id,
                page: annot.page,
                kind: annot.kind.clone().into(),
                color: col,
                color_index: annot.color_index,
                x: rect.0 * s.zoom_level,
                y: rect.1 * s.zoom_level,
                w: (rect.2 - rect.0).max(4.0) * s.zoom_level,
                h: (rect.3 - rect.1).max(4.0) * s.zoom_level,
                comment: annot.comment.clone().into(),
                show_note_icon: has_note && is_last_rect,
            });
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

    ui.set_can_undo(!s.undo_stack.is_empty());
    ui.set_can_redo(!s.redo_stack.is_empty());
}

fn sync_search_to_ui(s: &AppState, ui: &AppWindow) {
    let total = s.search_matches.len();
    ui.set_total_search_matches(total as i32);
    ui.set_active_search_index(s.active_search_index);
    ui.set_is_searching(s.is_searching);

    if total == 0 {
        if s.search_query.trim().is_empty() {
            ui.set_search_match_display("".into());
        } else if s.is_searching {
            ui.set_search_match_display("Searching...".into());
        } else {
            ui.set_search_match_display("0 results".into());
        }
        ui.set_search_results(std::rc::Rc::new(slint::VecModel::default()).into());
        ui.set_page_search_highlights(std::rc::Rc::new(slint::VecModel::default()).into());
        ui.set_scroll_search_highlights(std::rc::Rc::new(slint::VecModel::default()).into());
        return;
    }

    let display = format!("{} of {}", s.active_search_index + 1, total);
    ui.set_search_match_display(display.into());

    let mut result_items = Vec::new();
    let mut page_highlight_items = Vec::new();
    let mut scroll_highlight_items = Vec::new();

    // Precalculate scroll page offsets and widths
    let mut page_offsets: Vec<(f32, f32)> = Vec::with_capacity(s.total_pages as usize);
    let mut cur_offset = 0.0f32;
    for i in 0..s.total_pages {
        let (pw_pt, ph_pt) = s.get_page_size(i);
        let pw = pw_pt * s.zoom_level;
        let ph = ph_pt * s.zoom_level;
        page_offsets.push((cur_offset, pw));
        cur_offset += ph + 24.0;
    }

    for (idx, m) in s.search_matches.iter().enumerate() {
        let is_active = (idx as i32) == s.active_search_index;
        result_items.push(SearchResultItem {
            id: idx as i32,
            page: m.page,
            display_page: m.page + 1,
            snippet: m.snippet.clone().into(),
            match_text: m.match_text.clone().into(),
            is_active,
        });

        let (page_offset_y, page_w) = page_offsets
            .get(m.page as usize)
            .copied()
            .unwrap_or((0.0, 595.0 * s.zoom_level));

        for rect in &m.rects_pt {
            let rx = rect.0 * s.zoom_level;
            let ry = rect.1 * s.zoom_level;
            let rw = (rect.2 - rect.0).max(2.0) * s.zoom_level;
            let rh = (rect.3 - rect.1).max(2.0) * s.zoom_level;

            // Single page view highlight (only if current page)
            if m.page == s.current_page {
                page_highlight_items.push(SearchHighlight {
                    page: m.page,
                    x: rx,
                    y: ry,
                    width: rw,
                    height: rh,
                    is_current: is_active,
                    page_width: page_w,
                });
            }

            // Continuous scroll view highlight (positioned in scroll content coordinates)
            scroll_highlight_items.push(SearchHighlight {
                page: m.page,
                x: rx,
                y: page_offset_y + 12.0 + ry,
                width: rw,
                height: rh,
                is_current: is_active,
                page_width: page_w,
            });
        }
    }

    ui.set_search_results(std::rc::Rc::new(slint::VecModel::from(result_items)).into());
    ui.set_page_search_highlights(std::rc::Rc::new(slint::VecModel::from(page_highlight_items)).into());
    ui.set_scroll_search_highlights(std::rc::Rc::new(slint::VecModel::from(scroll_highlight_items)).into());
}

fn jump_to_search_match(
    s: &mut AppState,
    ui: &AppWindow,
    match_idx: i32,
) {
    if s.search_matches.is_empty() {
        return;
    }
    let total = s.search_matches.len() as i32;
    let clamped_idx = match_idx.rem_euclid(total);
    s.active_search_index = clamped_idx;

    let target_page = s.search_matches[clamped_idx as usize].page;
    let target_first_rect = s.search_matches[clamped_idx as usize].rects_pt.first().cloned();

    if s.is_scroll_view {
        let mut target_scroll_y = 0.0f32;
        if let Some(pages) = ui.get_scroll_pages().as_any().downcast_ref::<slint::VecModel<PageItemData>>() {
            if let Some(p_item) = pages.row_data(target_page as usize) {
                target_scroll_y = p_item.offset_y;
                if let Some(rect) = target_first_rect {
                    target_scroll_y += rect.1 * s.zoom_level - 120.0;
                }
            }
        }
        ui.set_scroll_content_y(-target_scroll_y.max(0.0));
        ui.set_current_page(target_page);
        s.current_page = target_page;
        trigger_scroll_page_render_direct(s, target_page, ui);
        sync_search_to_ui(s, ui);
    } else {
        if s.current_page != target_page {
            s.current_page = target_page;
            ui.set_current_page(target_page);
            update_view_with_state(s, ui);
        } else {
            sync_search_to_ui(s, ui);
        }
    }
    save_current_session_direct(s, ui);
}

fn save_current_session_direct(s: &AppState, ui: &AppWindow) {
    if !s.file_path.is_empty() {
        let session = history::DocumentSession {
            page: s.current_page,
            scroll_y: ui.get_scroll_content_y(),
            zoom_level: s.zoom_level,
            is_scroll_view: s.is_scroll_view,
            last_opened_at: 0,
        };
        history::save_document_session(&s.file_path, session);
    }
}

fn save_current_session(state_lock: &Arc<Mutex<Option<AppState>>>, ui: &AppWindow) {
    if let Ok(lock) = state_lock.lock() {
        if let Some(s) = lock.as_ref() {
            save_current_session_direct(s, ui);
        }
    }
}

fn trigger_search(
    query: String,
    state_lock: Arc<Mutex<Option<AppState>>>,
    ui_weak: slint::Weak<AppWindow>,
    search_generation: Arc<AtomicU64>,
) {
    let query_trimmed = query.trim().to_string();
    let current_gen = search_generation.fetch_add(1, Ordering::SeqCst) + 1;

    if query_trimmed.is_empty() {
        if let Ok(mut lock) = state_lock.lock() {
            if let Some(s) = lock.as_mut() {
                s.search_query.clear();
                s.search_matches.clear();
                s.active_search_index = -1;
                s.is_searching = false;
                if let Some(ui) = ui_weak.upgrade() {
                    sync_search_to_ui(s, &ui);
                }
            }
        }
        return;
    }

    let (file_path, total_pages, match_case, match_whole_word) = if let Ok(mut lock) = state_lock.lock() {
        if let Some(s) = lock.as_mut() {
            s.search_query = query_trimmed.clone();
            s.is_searching = true;
            if let Some(ui) = ui_weak.upgrade() {
                sync_search_to_ui(s, &ui);
            }
            (
                s.file_path.clone(),
                s.total_pages,
                s.search_match_case,
                s.search_match_whole_word,
            )
        } else {
            return;
        }
    } else {
        return;
    };

    let search_gen_clone = Arc::clone(&search_generation);
    std::thread::Builder::new()
        .name("pdf-search".to_string())
        .spawn(move || {
            let hits = pdf::search_document(
                &file_path,
                total_pages,
                &query_trimmed,
                match_case,
                match_whole_word,
                Some(Arc::clone(&search_gen_clone)),
                current_gen,
            );

            if search_gen_clone.load(Ordering::SeqCst) != current_gen {
                return;
            }

            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                if search_gen_clone.load(Ordering::SeqCst) != current_gen {
                    return;
                }
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        s.is_searching = false;
                        s.search_matches = hits;
                        if !s.search_matches.is_empty() {
                            let cur_page = s.current_page;
                            let first_on_cur = s.search_matches.iter().position(|m| m.page == cur_page);
                            s.active_search_index = first_on_cur.unwrap_or(0) as i32;
                            jump_to_search_match(s, &ui, s.active_search_index);
                        } else {
                            s.active_search_index = -1;
                            sync_search_to_ui(s, &ui);
                        }
                    }
                }
            });
        })
        .ok();
}

fn update_view(ui: &AppWindow, state_lock: &Arc<Mutex<Option<AppState>>>) {
    let mut lock = match state_lock.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if let Some(state) = lock.as_mut() {
        update_view_with_state(state, ui);
    }
}

fn update_view_with_state(state: &mut AppState, ui: &AppWindow) {

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
    sync_search_to_ui(state, ui);

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

fn collect_annotations_to_save(s: &AppState) -> Vec<pdf::SaveAnnotationData> {
    s.annotations
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
        .collect()
}

fn prompt_save_as(default_path: &str) -> Option<std::path::PathBuf> {
    let current_path = std::path::Path::new(default_path);
    let file_name = current_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document.pdf");

    let initial_dir = current_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));

    // 1. Try rfd native file dialog
    let res = rfd::FileDialog::new()
        .set_directory(initial_dir)
        .set_file_name(file_name)
        .add_filter("PDF document (*.pdf)", &["pdf"])
        .save_file();

    if let Some(mut path) = res {
        if path.extension().is_none() {
            path.set_extension("pdf");
        }
        return Some(path);
    }

    // 2. Fallback to zenity if available
    if let Ok(output) = std::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--save",
            "--confirm-overwrite",
            &format!("--filename={}", default_path),
            "--file-filter=PDF documents (*.pdf) | *.pdf",
            "--file-filter=All files | *",
        ])
        .output()
    {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path_str.is_empty() {
                let mut path = std::path::PathBuf::from(path_str);
                if path.extension().is_none() {
                    path.set_extension("pdf");
                }
                return Some(path);
            }
        }
    }

    None
}

fn perform_undo(s: &mut AppState) -> Option<(bool, Option<i32>)> {
    let action = s.undo_stack.pop_back()?;
    let mut highlight_page_to_rerender = None;

    match &action {
        UndoAction::Created(annot) => {
            // Undoing a creation: remove the created annotation
            if let Some(pos) = s.annotations.iter().position(|a| a.id == annot.id) {
                let page = s.annotations[pos].page;
                let is_hl = s.annotations[pos].kind == "highlight";
                s.annotations.remove(pos);
                if is_hl {
                    s.cache.remove_page(page);
                    highlight_page_to_rerender = Some(page);
                }
            }
        }
        UndoAction::Deleted { index, record } => {
            // Undoing a deletion: re-insert the annotation at its previous position
            let page = record.page;
            let is_hl = record.kind == "highlight";
            let pos = (*index).min(s.annotations.len());
            s.annotations.insert(pos, record.clone());
            if is_hl {
                s.cache.remove_page(page);
                highlight_page_to_rerender = Some(page);
            }
        }
        UndoAction::ColorChanged {
            id,
            old_color_index,
            old_color_hex,
            ..
        } => {
            // Undoing a color change: revert to old color
            if let Some(pos) = s.annotations.iter().position(|a| a.id == *id) {
                let page = s.annotations[pos].page;
                let is_hl = s.annotations[pos].kind == "highlight";
                s.annotations[pos].color_index = *old_color_index;
                s.annotations[pos].color_hex = old_color_hex.clone();
                s.annotations[pos].is_from_pdf = false;
                if is_hl {
                    s.cache.remove_page(page);
                    highlight_page_to_rerender = Some(page);
                }
            }
        }
        UndoAction::NoteChanged {
            id,
            old_comment,
            ..
        } => {
            // Undoing a note change: revert to old comment
            if let Some(pos) = s.annotations.iter().position(|a| a.id == *id) {
                s.annotations[pos].comment = old_comment.clone();
                s.annotations[pos].is_from_pdf = false;
            }
        }
    }

    if s.redo_stack.len() >= MAX_UNDO_OPERATIONS {
        s.redo_stack.pop_front();
    }
    s.redo_stack.push_back(action);

    Some((true, highlight_page_to_rerender))
}

fn perform_redo(s: &mut AppState) -> Option<(bool, Option<i32>)> {
    let action = s.redo_stack.pop_back()?;
    let mut highlight_page_to_rerender = None;

    match &action {
        UndoAction::Created(annot) => {
            // Redoing a creation: re-insert the annotation
            let page = annot.page;
            let is_hl = annot.kind == "highlight";
            s.annotations.push(annot.clone());
            if is_hl {
                s.cache.remove_page(page);
                highlight_page_to_rerender = Some(page);
            }
        }
        UndoAction::Deleted { record, .. } => {
            // Redoing a deletion: remove the annotation again
            if let Some(pos) = s.annotations.iter().position(|a| a.id == record.id) {
                let page = s.annotations[pos].page;
                let is_hl = s.annotations[pos].kind == "highlight";
                s.annotations.remove(pos);
                if is_hl {
                    s.cache.remove_page(page);
                    highlight_page_to_rerender = Some(page);
                }
            }
        }
        UndoAction::ColorChanged {
            id,
            new_color_index,
            new_color_hex,
            ..
        } => {
            // Redoing a color change: apply new color again
            if let Some(pos) = s.annotations.iter().position(|a| a.id == *id) {
                let page = s.annotations[pos].page;
                let is_hl = s.annotations[pos].kind == "highlight";
                s.annotations[pos].color_index = *new_color_index;
                s.annotations[pos].color_hex = new_color_hex.clone();
                s.annotations[pos].is_from_pdf = false;
                if is_hl {
                    s.cache.remove_page(page);
                    highlight_page_to_rerender = Some(page);
                }
            }
        }
        UndoAction::NoteChanged {
            id,
            new_comment,
            ..
        } => {
            // Redoing a note change: apply new comment again
            if let Some(pos) = s.annotations.iter().position(|a| a.id == *id) {
                s.annotations[pos].comment = new_comment.clone();
                s.annotations[pos].is_from_pdf = false;
            }
        }
    }

    if s.undo_stack.len() >= MAX_UNDO_OPERATIONS {
        s.undo_stack.pop_front();
    }
    s.undo_stack.push_back(action);

    Some((true, highlight_page_to_rerender))
}

fn trigger_toast(app: &AppWindow, message: &str) {
    app.set_toast_message(message.into());
    app.set_show_toast(true);
    let app_weak = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(2200), move || {
        if let Some(app) = app_weak.upgrade() {
            app.set_show_toast(false);
        }
    });
}

fn build_annotations_markdown_from_state(s: &AppState) -> String {
    let doc_title = Path::new(&s.file_path)
        .file_stem()
        .and_then(|st| st.to_str())
        .unwrap_or("Document");

    let export_annots: Vec<export::ExportAnnotation> = s
        .annotations
        .iter()
        .map(|a| {
            let min_y = a.line_rects_pt.first().map(|r| r.1).unwrap_or(0.0);
            export::ExportAnnotation {
                page: a.page,
                kind: a.kind.clone(),
                snippet: a.snippet.clone(),
                comment: a.comment.clone(),
                min_y_pt: min_y,
            }
        })
        .collect();

    export::generate_annotations_markdown(doc_title, &export_annots, &s.toc)
}

fn setup_callbacks(app: &AppWindow, state_lock: &Arc<Mutex<Option<AppState>>>) {
    let search_generation = Arc::new(AtomicU64::new(0));
    let search_debounce: std::rc::Rc<std::cell::RefCell<Option<slint::Timer>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    let session_save_timer: std::rc::Rc<std::cell::RefCell<Option<slint::Timer>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    let make_session_saver = {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        let session_timer = std::rc::Rc::clone(&session_save_timer);
        move || {
            let state_lock_c = Arc::clone(&state_lock);
            let app_weak_c = app_weak.clone();
            let session_timer_c = std::rc::Rc::clone(&session_timer);
            move || {
                *session_timer_c.borrow_mut() = None;
                let timer = slint::Timer::default();
                let state_lock_inner = Arc::clone(&state_lock_c);
                let app_weak_inner = app_weak_c.clone();
                timer.start(
                    slint::TimerMode::SingleShot,
                    std::time::Duration::from_millis(400),
                    move || {
                        if let Some(ui) = app_weak_inner.upgrade() {
                            save_current_session(&state_lock_inner, &ui);
                        }
                    },
                );
                *session_timer_c.borrow_mut() = Some(timer);
            }
        }
    };

    // Toggle Search Bar
    {
        let app_weak = app.as_weak();
        let state_lock = Arc::clone(state_lock);
        let search_generation = Arc::clone(&search_generation);
        let search_debounce = std::rc::Rc::clone(&search_debounce);
        app.on_toggle_search(move || {
            if let Some(ui) = app_weak.upgrade() {
                let currently_showing = ui.get_show_search();
                if currently_showing {
                    *search_debounce.borrow_mut() = None;
                    ui.set_show_search(false);
                    if ui.get_sidebar_tab() == 3 {
                        ui.set_sidebar_tab(0);
                    }
                    ui.invoke_grab_focus();
                } else {
                    ui.set_show_search(true);
                    ui.set_show_sidebar(true);
                    ui.set_sidebar_tab(3); // Switch to Search tab
                    let cur_query = ui.get_search_query().to_string();
                    if !cur_query.trim().is_empty() {
                        *search_debounce.borrow_mut() = None;
                        trigger_search(
                            cur_query,
                            Arc::clone(&state_lock),
                            app_weak.clone(),
                            Arc::clone(&search_generation),
                        );
                    }
                }
            }
        });
    }

    // Close Search Bar
    {
        let app_weak = app.as_weak();
        let search_debounce = std::rc::Rc::clone(&search_debounce);
        app.on_close_search(move || {
            *search_debounce.borrow_mut() = None;
            if let Some(ui) = app_weak.upgrade() {
                ui.set_show_search(false);
                if ui.get_sidebar_tab() == 3 {
                    ui.set_sidebar_tab(0);
                }
                ui.invoke_grab_focus();
            }
        });
    }

    // Search Query Changed (debounced by 250ms to prevent spamming search threads while typing)
    {
        let app_weak = app.as_weak();
        let state_lock = Arc::clone(state_lock);
        let search_generation = Arc::clone(&search_generation);
        let search_debounce = std::rc::Rc::clone(&search_debounce);
        app.on_search_query_changed(move |query| {
            // Cancel any pending debounce timer
            *search_debounce.borrow_mut() = None;

            let query_str = query.to_string();
            let query_trimmed = query_str.trim().to_string();

            if query_trimmed.is_empty() {
                trigger_search(
                    String::new(),
                    Arc::clone(&state_lock),
                    app_weak.clone(),
                    Arc::clone(&search_generation),
                );
                return;
            }

            // Immediately display "Searching..." so the UI responds instantly to keystrokes
            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_searching(true);
                ui.set_search_match_display("Searching...".into());
            }

            // Start 250ms debounce
            let timer = slint::Timer::default();
            let app_weak_c = app_weak.clone();
            let state_lock_c = Arc::clone(&state_lock);
            let search_gen_c = Arc::clone(&search_generation);
            timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(250),
                move || {
                    trigger_search(
                        query_str.clone(),
                        Arc::clone(&state_lock_c),
                        app_weak_c.clone(),
                        Arc::clone(&search_gen_c),
                    );
                },
            );
            *search_debounce.borrow_mut() = Some(timer);
        });
    }

    // Search Next (cycle forward, or trigger immediate search if Enter pressed before debounce)
    {
        let app_weak = app.as_weak();
        let state_lock = Arc::clone(state_lock);
        let search_generation = Arc::clone(&search_generation);
        let search_debounce = std::rc::Rc::clone(&search_debounce);
        app.on_search_next(move || {
            if let Some(ui) = app_weak.upgrade() {
                let cur_input = ui.get_search_query().to_string();
                let cur_trimmed = cur_input.trim().to_string();

                let needs_immediate_search = {
                    if let Ok(lock) = state_lock.lock() {
                        if let Some(s) = lock.as_ref() {
                            s.search_query != cur_trimmed || s.search_matches.is_empty()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if needs_immediate_search && !cur_trimmed.is_empty() {
                    // User pressed Enter: bypass debounce and trigger search immediately
                    *search_debounce.borrow_mut() = None;
                    trigger_search(
                        cur_input,
                        Arc::clone(&state_lock),
                        app_weak.clone(),
                        Arc::clone(&search_generation),
                    );
                    return;
                }

                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        if !s.search_matches.is_empty() {
                            let next_idx = s.active_search_index + 1;
                            jump_to_search_match(s, &ui, next_idx);
                        }
                    }
                }
            }
        });
    }

    // Search Previous (cycle backward, or trigger immediate search if Shift+Enter pressed)
    {
        let app_weak = app.as_weak();
        let state_lock = Arc::clone(state_lock);
        let search_generation = Arc::clone(&search_generation);
        let search_debounce = std::rc::Rc::clone(&search_debounce);
        app.on_search_prev(move || {
            if let Some(ui) = app_weak.upgrade() {
                let cur_input = ui.get_search_query().to_string();
                let cur_trimmed = cur_input.trim().to_string();

                let needs_immediate_search = {
                    if let Ok(lock) = state_lock.lock() {
                        if let Some(s) = lock.as_ref() {
                            s.search_query != cur_trimmed || s.search_matches.is_empty()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if needs_immediate_search && !cur_trimmed.is_empty() {
                    // User pressed Shift+Enter: bypass debounce and trigger search immediately
                    *search_debounce.borrow_mut() = None;
                    trigger_search(
                        cur_input,
                        Arc::clone(&state_lock),
                        app_weak.clone(),
                        Arc::clone(&search_generation),
                    );
                    return;
                }

                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        if !s.search_matches.is_empty() {
                            let prev_idx = s.active_search_index - 1;
                            jump_to_search_match(s, &ui, prev_idx);
                        }
                    }
                }
            }
        });
    }

    // Select Search Result (from Sidebar)
    {
        let app_weak = app.as_weak();
        let state_lock = Arc::clone(state_lock);
        app.on_select_search_result(move |match_id| {
            if let Some(ui) = app_weak.upgrade() {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        jump_to_search_match(s, &ui, match_id);
                    }
                }
            }
        });
    }

    // Toggle Search Match Case (Aa)
    {
        let app_weak = app.as_weak();
        let state_lock = Arc::clone(state_lock);
        let search_generation = Arc::clone(&search_generation);
        app.on_toggle_search_match_case(move || {
            let (new_val, query) = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        s.search_match_case = !s.search_match_case;
                        (s.search_match_case, s.search_query.clone())
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                ui.set_search_match_case(new_val);
                if !query.is_empty() {
                    trigger_search(
                        query,
                        Arc::clone(&state_lock),
                        app_weak.clone(),
                        Arc::clone(&search_generation),
                    );
                }
            }
        });
    }

    // Toggle Search Match Whole Word ([W])
    {
        let app_weak = app.as_weak();
        let state_lock = Arc::clone(state_lock);
        let search_generation = Arc::clone(&search_generation);
        app.on_toggle_search_match_whole_word(move || {
            let (new_val, query) = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        s.search_match_whole_word = !s.search_match_whole_word;
                        (s.search_match_whole_word, s.search_query.clone())
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                ui.set_search_match_whole_word(new_val);
                if !query.is_empty() {
                    trigger_search(
                        query,
                        Arc::clone(&state_lock),
                        app_weak.clone(),
                        Arc::clone(&search_generation),
                    );
                }
            }
        });
    }

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
        let save_session = make_session_saver();
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
                    save_session();
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
                ui.set_selected_annotation_id(-1);
                ui.set_hovered_annotation_id(-1);
                ui.set_current_note_text("".into());
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
                    ui.set_selected_annotation_id(-1);
                    ui.set_popup_x(popup_x);
                    ui.set_popup_y(popup_y);
                    ui.set_show_popup(true);
                } else {
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
                    ui.set_has_selection(false);
                    ui.set_show_popup(false);
                    ui.set_show_note_input(false);
                    ui.set_current_note_text("".into());
                    ui.set_selected_annotation_id(-1);
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
                        s.push_undo(UndoAction::Created(annot.clone()));

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
                    ui.set_current_note_text("".into());
                    ui.set_selected_annotation_id(-1);
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
                        s.push_undo(UndoAction::Created(annot.clone()));
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
                    ui.set_selected_annotation_id(-1);
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
                        let deleted_record = s.annotations[pos].clone();
                        s.push_undo(UndoAction::Deleted {
                            index: pos,
                            record: deleted_record,
                        });
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
                    ui.set_selected_annotation_id(-1);
                    ui.set_hovered_annotation_id(-1);
                    ui.set_show_popup(false);
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

    // Update Annotation Color
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_update_annotation_color(move |id, new_color_idx| {
            let mut need_sync = false;
            let mut update_page = None;
            let mut is_highlight = false;

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    if let Some(pos) = s.annotations.iter().position(|a| a.id == id) {
                        let page = s.annotations[pos].page;
                        let old_index = s.annotations[pos].color_index;
                        let old_hex = s.annotations[pos].color_hex.clone();
                        let (hex, _) = get_color(new_color_idx);
                        if old_index != new_color_idx {
                            s.push_undo(UndoAction::ColorChanged {
                                id,
                                old_color_index: old_index,
                                old_color_hex: old_hex,
                                new_color_index: new_color_idx,
                                new_color_hex: hex.clone(),
                            });
                        }
                        update_page = Some(page);
                        is_highlight = s.annotations[pos].kind == "highlight";
                        s.annotations[pos].color_index = new_color_idx;
                        s.annotations[pos].color_hex = hex;
                        s.annotations[pos].is_from_pdf = false;
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
                                if let Some(p) = update_page {
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

    // Update Annotation Note
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_update_annotation_note(move |id, comment_text| {
            let comment_str = comment_text.to_string();
            let mut need_sync = false;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    if let Some(pos) = s.annotations.iter().position(|a| a.id == id) {
                        let old_comment = s.annotations[pos].comment.clone();
                        if old_comment != comment_str {
                            s.push_undo(UndoAction::NoteChanged {
                                id,
                                old_comment,
                                new_comment: comment_str.clone(),
                            });
                            s.annotations[pos].comment = comment_str;
                            s.annotations[pos].is_from_pdf = false;
                            need_sync = true;
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
                        }
                    }
                }
            }
        });
    }

    // Undo
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_undo(move || {
            let mut undo_result = None;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    undo_result = perform_undo(s);
                }
            }

            if let Some((_, hl_page)) = undo_result {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_dirty(true);
                    ui.set_show_popup(false);
                    ui.set_selected_annotation_id(-1);
                    ui.set_hovered_annotation_id(-1);
                    if let Ok(mut lock) = state_lock.lock() {
                        if let Some(s) = lock.as_mut() {
                            sync_annotations_to_ui(s, &ui);
                            if let Some(p) = hl_page {
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
        });
    }

    // Redo
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_redo(move || {
            let mut redo_result = None;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    redo_result = perform_redo(s);
                }
            }

            if let Some((_, hl_page)) = redo_result {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_dirty(true);
                    ui.set_show_popup(false);
                    ui.set_selected_annotation_id(-1);
                    ui.set_hovered_annotation_id(-1);
                    if let Ok(mut lock) = state_lock.lock() {
                        if let Some(s) = lock.as_mut() {
                            sync_annotations_to_ui(s, &ui);
                            if let Some(p) = hl_page {
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
                        let to_save = collect_annotations_to_save(s);
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
                        save_current_session(&state_lock, &ui);

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

    // Save Document As (Ctrl + Shift + S)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_save_document_as(move || {
            let (file_path, to_save) = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        let to_save = collect_annotations_to_save(s);
                        (s.file_path.clone(), to_save)
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            let chosen_path = match prompt_save_as(&file_path) {
                Some(p) => p,
                None => return, // User cancelled
            };

            let new_path_str = chosen_path.to_string_lossy().to_string();

            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_saving(true);
            }

            // If saving to a new path, copy the base file over first if they differ
            if new_path_str != file_path {
                if let Err(e) = std::fs::copy(&file_path, &new_path_str) {
                    eprintln!("Failed to copy PDF for Save As: {e}");
                    if let Some(ui) = app_weak.upgrade() {
                        ui.set_is_saving(false);
                    }
                    return;
                }
            }

            let save_result = pdf::save_document_annotations(&new_path_str, &to_save);

            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_saving(false);
                match save_result {
                    Ok(()) => {
                        let new_file_name = std::path::Path::new(&new_path_str)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&new_path_str)
                            .to_string();

                        ui.set_is_dirty(false);
                        ui.set_file_path(new_path_str.clone().into());
                        ui.set_file_title(new_file_name.into());
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

                        if let Ok(mut lock) = state_lock.lock() {
                            if let Some(s) = lock.as_mut() {
                                s.file_path = new_path_str.clone();
                                s.cached_text_page = -1;
                                s.worker.switch_document(new_path_str);
                            }
                        }
                        save_current_session(&state_lock, &ui);
                    }
                    Err(e) => {
                        eprintln!("Failed to save PDF as: {e}");
                    }
                }
            }
        });
    }

    // Copy Annotations Markdown (Clipboard)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_copy_annotations_markdown(move || {
            let (md, is_empty) = {
                if let Ok(lock) = state_lock.lock() {
                    if let Some(s) = lock.as_ref() {
                        if s.annotations.is_empty() {
                            (String::new(), true)
                        } else {
                            (build_annotations_markdown_from_state(s), false)
                        }
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                if is_empty {
                    trigger_toast(&ui, "No annotations to export");
                    return;
                }
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(md)) {
                    Ok(_) => trigger_toast(&ui, "✓ Copied annotations to clipboard"),
                    Err(e) => trigger_toast(&ui, &format!("Failed to copy: {}", e)),
                }
            }
        });
    }

    // Export Annotations Markdown (.md File)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_export_annotations_markdown(move || {
            let (md, is_empty, default_filename) = {
                if let Ok(lock) = state_lock.lock() {
                    if let Some(s) = lock.as_ref() {
                        if s.annotations.is_empty() {
                            (String::new(), true, String::new())
                        } else {
                            let stem = Path::new(&s.file_path)
                                .file_stem()
                                .and_then(|st| st.to_str())
                                .unwrap_or("document");
                            (
                                build_annotations_markdown_from_state(s),
                                false,
                                format!("{}_annotations.md", stem),
                            )
                        }
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                if is_empty {
                    trigger_toast(&ui, "No annotations to export");
                    return;
                }

                if let Some(save_path) = rfd::FileDialog::new()
                    .set_file_name(&default_filename)
                    .add_filter("Markdown", &["md"])
                    .save_file()
                {
                    match std::fs::write(&save_path, md) {
                        Ok(_) => {
                            let display_name = save_path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("annotations.md");
                            trigger_toast(&ui, &format!("✓ Saved to {}", display_name));
                        }
                        Err(e) => trigger_toast(&ui, &format!("Failed to save: {}", e)),
                    }
                }
            }
        });
    }

    // Zen Mode Toggle (F11)
    {
        let app_weak = app.as_weak();
        app.on_toggle_zen_mode(move || {
            if let Some(ui) = app_weak.upgrade() {
                let was_zen = ui.get_is_zen_mode();
                let new_zen = !was_zen;

                if new_zen {
                    let was_sidebar = ui.get_show_sidebar();
                    ui.set_sidebar_was_open(was_sidebar);
                    if was_sidebar {
                        ui.set_show_sidebar(false);
                    }
                    ui.set_is_zen_mode(true);
                    ui.window().set_fullscreen(true);
                    trigger_toast(&ui, "Zen Mode: Press F11 or Esc to exit");
                } else {
                    ui.set_is_zen_mode(false);
                    ui.window().set_fullscreen(false);
                    if ui.get_sidebar_was_open() {
                        ui.set_show_sidebar(true);
                    }
                    trigger_toast(&ui, "Exited Zen Mode");
                }
            }
        });
    }

    // Exit Zen Mode (Esc)
    {
        let app_weak = app.as_weak();
        app.on_exit_zen_mode(move || {
            if let Some(ui) = app_weak.upgrade() {
                if ui.get_is_zen_mode() {
                    ui.set_is_zen_mode(false);
                    ui.window().set_fullscreen(false);
                    if ui.get_sidebar_was_open() {
                        ui.set_show_sidebar(true);
                    }
                    trigger_toast(&ui, "Exited Zen Mode");
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
                ui.set_selected_annotation_id(-1);
                ui.set_hovered_annotation_id(-1);
                ui.set_current_note_text("".into());
                ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
            }
        });
    }

    // Toggle View Mode (Page vs Scroll)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        let save_session = make_session_saver();
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
                save_session();
            }
        });
    }

    // Visible Page Changed (scrolling in continuous scroll view)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        let save_session = make_session_saver();
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
                    save_session();
                }
            }
        });
    }

    // Prev Page
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        let save_session = make_session_saver();
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
                    save_session();
                }
            }
        });
    }

    // Next Page
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        let save_session = make_session_saver();
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
                    save_session();
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
    ui.set_show_popup(false);
    ui.set_selected_annotation_id(-1);
    ui.set_hovered_annotation_id(-1);
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
            save_current_session(&state_lock_clone, &ui);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_state() -> (AppState, std::path::PathBuf) {
        let temp_dir = std::env::temp_dir();
        let test_pdf_path = temp_dir.join("folio_undo_test.pdf");
        let path_str = test_pdf_path.to_str().unwrap();

        // Create a blank 1-page PDF
        {
            let mut doc = mupdf::pdf::PdfDocument::new();
            doc.new_page((500.0, 700.0)).unwrap();
            doc.save(path_str).unwrap();
        }

        let worker = PdfWorker::spawn(path_str.to_string(), |_| {}).unwrap();

        let state = AppState {
            file_path: path_str.to_string(),
            current_page: 0,
            total_pages: 1,
            zoom_level: 1.0,
            is_scroll_view: false,
            page_width_pt: 500.0,
            page_height_pt: 700.0,
            page_sizes: vec![(500.0, 700.0)],
            cache: PageCache::new(4),
            worker,
            annotations: Vec::new(),
            next_annot_id: 1,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            cached_text_page: -1,
            cached_text: pdf::PageTextData::default(),
            sel_start_pt: (0.0, 0.0),
            active_selection_page: 0,
            active_selection_snippet: String::new(),
            active_selection_line_rects: Vec::new(),
            is_zooming: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            active_search_index: -1,
            is_searching: false,
            search_match_case: false,
            search_match_whole_word: false,
            toc: Vec::new(),
        };

        (state, test_pdf_path)
    }

    #[test]
    fn test_undo_redo_create() {
        let (mut state, pdf_path) = setup_test_state();

        let annot = AnnotationRecord {
            id: 1,
            page: 0,
            kind: "highlight".to_string(),
            color_index: 0,
            color_hex: "#facc15".to_string(),
            snippet: "Test snippet".to_string(),
            comment: "".to_string(),
            line_rects_pt: vec![(10.0, 20.0, 100.0, 30.0)],
            is_from_pdf: false,
        };

        // Create
        state.annotations.push(annot.clone());
        state.push_undo(UndoAction::Created(annot));
        assert_eq!(state.annotations.len(), 1);
        assert_eq!(state.undo_stack.len(), 1);
        assert_eq!(state.redo_stack.len(), 0);

        // Undo -> Annotation should be removed
        let res = perform_undo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations.len(), 0);
        assert_eq!(state.undo_stack.len(), 0);
        assert_eq!(state.redo_stack.len(), 1);

        // Redo -> Annotation should be restored
        let res = perform_redo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations.len(), 1);
        assert_eq!(state.annotations[0].id, 1);
        assert_eq!(state.undo_stack.len(), 1);
        assert_eq!(state.redo_stack.len(), 0);

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_undo_redo_delete() {
        let (mut state, pdf_path) = setup_test_state();

        let annot = AnnotationRecord {
            id: 1,
            page: 0,
            kind: "underline".to_string(),
            color_index: 1,
            color_hex: "#4ade80".to_string(),
            snippet: "Underlined text".to_string(),
            comment: "".to_string(),
            line_rects_pt: vec![(10.0, 20.0, 100.0, 30.0)],
            is_from_pdf: false,
        };

        state.annotations.push(annot.clone());

        // Delete
        let deleted = state.annotations.remove(0);
        state.push_undo(UndoAction::Deleted {
            index: 0,
            record: deleted,
        });
        assert_eq!(state.annotations.len(), 0);

        // Undo -> Restores deleted annotation
        let res = perform_undo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations.len(), 1);
        assert_eq!(state.annotations[0].id, 1);
        assert_eq!(state.annotations[0].kind, "underline");

        // Redo -> Deletes it again
        let res = perform_redo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations.len(), 0);

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_undo_redo_color_change() {
        let (mut state, pdf_path) = setup_test_state();

        let annot = AnnotationRecord {
            id: 1,
            page: 0,
            kind: "highlight".to_string(),
            color_index: 0,
            color_hex: "#facc15".to_string(),
            snippet: "Colored text".to_string(),
            comment: "".to_string(),
            line_rects_pt: vec![(10.0, 20.0, 100.0, 30.0)],
            is_from_pdf: false,
        };

        state.annotations.push(annot);

        // Change color from Yellow (0) to Blue (2)
        state.push_undo(UndoAction::ColorChanged {
            id: 1,
            old_color_index: 0,
            old_color_hex: "#facc15".to_string(),
            new_color_index: 2,
            new_color_hex: "#60a5fa".to_string(),
        });
        state.annotations[0].color_index = 2;
        state.annotations[0].color_hex = "#60a5fa".to_string();

        assert_eq!(state.annotations[0].color_index, 2);

        // Undo -> Reverts to Yellow (0)
        let res = perform_undo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations[0].color_index, 0);
        assert_eq!(state.annotations[0].color_hex, "#facc15");

        // Redo -> Re-applies Blue (2)
        let res = perform_redo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations[0].color_index, 2);
        assert_eq!(state.annotations[0].color_hex, "#60a5fa");

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_undo_redo_note_change() {
        let (mut state, pdf_path) = setup_test_state();

        let annot = AnnotationRecord {
            id: 1,
            page: 0,
            kind: "highlight".to_string(),
            color_index: 0,
            color_hex: "#facc15".to_string(),
            snippet: "Important passage".to_string(),
            comment: "".to_string(),
            line_rects_pt: vec![(10.0, 20.0, 100.0, 30.0)],
            is_from_pdf: false,
        };

        state.annotations.push(annot);

        // Add note to highlight
        state.push_undo(UndoAction::NoteChanged {
            id: 1,
            old_comment: "".to_string(),
            new_comment: "Review this later".to_string(),
        });
        state.annotations[0].comment = "Review this later".to_string();

        assert_eq!(state.annotations[0].comment, "Review this later");

        // Undo -> Reverts to empty comment
        let res = perform_undo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations[0].comment, "");

        // Redo -> Re-applies comment
        let res = perform_redo(&mut state);
        assert!(res.is_some());
        assert_eq!(state.annotations[0].comment, "Review this later");

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_undo_stack_capacity_cap_and_redo_clearing() {
        let (mut state, pdf_path) = setup_test_state();

        // Push 105 actions
        for i in 0..105 {
            let annot = AnnotationRecord {
                id: i,
                page: 0,
                kind: "note".to_string(),
                color_index: 0,
                color_hex: "#facc15".to_string(),
                snippet: format!("Note {i}"),
                comment: "".to_string(),
                line_rects_pt: vec![],
                is_from_pdf: false,
            };
            state.push_undo(UndoAction::Created(annot));
        }

        // Must be capped at MAX_UNDO_OPERATIONS (100)
        assert_eq!(state.undo_stack.len(), 100);

        // Undo 3 times -> redo stack has 3 items
        perform_undo(&mut state);
        perform_undo(&mut state);
        perform_undo(&mut state);
        assert_eq!(state.redo_stack.len(), 3);

        // Performing a new action must clear the redo stack
        let new_annot = AnnotationRecord {
            id: 999,
            page: 0,
            kind: "note".to_string(),
            color_index: 0,
            color_hex: "#facc15".to_string(),
            snippet: "New Note".to_string(),
            comment: "".to_string(),
            line_rects_pt: vec![],
            is_from_pdf: false,
        };
        state.push_undo(UndoAction::Created(new_annot));
        assert_eq!(state.redo_stack.len(), 0);

        let _ = std::fs::remove_file(pdf_path);
    }
}

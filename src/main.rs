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
    ("#c084fc", 0xc0, 0x84, 0xfc), // Purple
];

const HIGHLIGHT_BLEND_COLORS: &[[u8; 3]] = &[
    [254, 240, 138], // Pastel Yellow
    [187, 247, 208], // Pastel Green
    [191, 219, 254], // Pastel Blue
    [251, 207, 232], // Pastel Pink
    [254, 215, 170], // Pastel Orange
    [233, 213, 255], // Pastel Purple
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
        (5, (0.75, 0.52, 0.99)), // Purple (#c084fc)
        // Soft pastel highlight colors
        (0, (1.00, 0.95, 0.55)), // Pastel Yellow (#fff38c)
        (1, (0.65, 0.95, 0.82)), // Pastel Green  (#a7f3d0)
        (2, (0.73, 0.90, 0.99)), // Pastel Blue   (#bae6fd)
        (3, (0.98, 0.81, 0.91)), // Pastel Pink   (#fbcfe8)
        (4, (1.00, 0.82, 0.61)), // Pastel Orange (#ffd09b)
        (5, (0.91, 0.84, 1.00)), // Pastel Purple (#e9d5ff)
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
pub(crate) struct AnnotationRecord {
    id: i32,
    page: i32,
    kind: String, // "highlight", "underline", "strikethrough", "note"
    color_index: i32,
    color_hex: String,
    snippet: String,
    comment: String,
    line_rects_pt: Vec<(f32, f32, f32, f32)>, // (x0, y0, x1, y1) in points
    is_from_pdf: bool,
    creation_date: String,
    mod_date: String,
    author: String,
}

fn get_current_author() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "Folio".to_string())
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

#[derive(Clone, Debug, PartialEq)]
pub struct PageSelection {
    pub page: i32,
    pub snippet: String,
    pub line_rects_pt: Vec<(f32, f32, f32, f32)>,
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
    page_text_cache: std::collections::HashMap<i32, pdf::PageTextData>,
    sel_start_page: i32,
    sel_start_pt: (f32, f32),
    // Current active selection state in PDF points
    active_selections: Vec<PageSelection>,
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
    pub collapsed_toc_ids: std::collections::HashSet<i32>,
    pub page_links_cache: std::collections::HashMap<i32, Vec<pdf::PageLink>>,
    pub has_logical_pages: bool,
    pub page_labels: Vec<String>,
    pub rotation: i32, // 0, 90, 180, 270
    pub is_dual_page: bool,
    pub paper_theme: pdf::PaperTheme,
    pub doc_properties: pdf::DocumentProperties,
    pub annotation_sort_mode: u8,
}

pub fn transform_rect_for_rotation(
    rect: (f32, f32, f32, f32),
    page_w_pt: f32,
    page_h_pt: f32,
    rotation: i32,
) -> (f32, f32, f32, f32) {
    match rotation {
        90 => (
            page_h_pt - rect.3,
            rect.0,
            page_h_pt - rect.1,
            rect.2,
        ),
        180 => (
            page_w_pt - rect.2,
            page_h_pt - rect.3,
            page_w_pt - rect.0,
            page_h_pt - rect.1,
        ),
        270 => (
            rect.1,
            page_w_pt - rect.2,
            rect.3,
            page_w_pt - rect.0,
        ),
        _ => rect,
    }
}

impl AppState {
    pub fn push_undo(&mut self, action: UndoAction) {
        if self.undo_stack.len() >= MAX_UNDO_OPERATIONS {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(action);
        self.redo_stack.clear();
    }

    pub fn get_unrotated_page_size(&self, page: i32) -> (f32, f32) {
        if page >= 0 && (page as usize) < self.page_sizes.len() {
            self.page_sizes[page as usize]
        } else {
            (self.page_width_pt, self.page_height_pt)
        }
    }

    pub fn get_or_load_page_links(&mut self, page: i32) -> &[pdf::PageLink] {
        if !self.page_links_cache.contains_key(&page) {
            let links = pdf::extract_page_links(&self.file_path, page);
            self.page_links_cache.insert(page, links);
        }
        self.page_links_cache.get(&page).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn get_page_size(&self, page: i32) -> (f32, f32) {
        let (w, h) = self.get_unrotated_page_size(page);
        if self.rotation == 90 || self.rotation == 270 {
            (h, w)
        } else {
            (w, h)
        }
    }

    pub fn to_unrotated_pt(&self, page: i32, rx: f32, ry: f32) -> (f32, f32) {
        let (w, h) = self.get_unrotated_page_size(page);
        match self.rotation {
            90 => (ry, h - rx),
            180 => (w - rx, h - ry),
            270 => (w - ry, rx),
            _ => (rx, ry),
        }
    }
}

pub fn parse_primary_monitor_scale(xml: &str) -> Option<f32> {
    let mut first_scale: Option<f32> = None;

    // Split by <logicalmonitor> blocks
    for block in xml.split("<logicalmonitor>") {
        if let Some(end_idx) = block.find("</logicalmonitor>") {
            let lm_body = &block[..end_idx];
            let mut block_scale = None;
            let mut block_primary = false;

            if let Some(s_start) = lm_body.find("<scale>") {
                let rest = &lm_body[s_start + 7..];
                if let Some(s_end) = rest.find("</scale>") {
                    if let Ok(val) = rest[..s_end].trim().parse::<f32>() {
                        block_scale = Some(val);
                    }
                }
            }

            if lm_body.contains("<primary>yes</primary>") {
                block_primary = true;
            }

            if first_scale.is_none() && block_scale.is_some() {
                first_scale = block_scale;
            }

            if block_primary && block_scale.is_some() {
                return block_scale;
            }
        }
    }

    first_scale
}

fn detect_system_scale_factor() -> Option<f32> {
    // 1. If explicit environment variable is already set, respect it
    if let Ok(val) = env::var("SLINT_SCALE_FACTOR") {
        if let Ok(scale) = val.parse::<f32>() {
            if scale > 0.1 {
                return Some(scale);
            }
        }
    }

    // 2. Check GDK_SCALE / QT_SCALE_FACTOR
    for var in &["GDK_SCALE", "QT_SCALE_FACTOR"] {
        if let Ok(val) = env::var(var) {
            if let Ok(scale) = val.parse::<f32>() {
                if scale > 0.1 {
                    return Some(scale);
                }
            }
        }
    }

    // 3. GNOME / Wayland / Mutter monitor configuration (~/.config/monitors.xml)
    if let Ok(home) = env::var("HOME") {
        let monitors_path = Path::new(&home).join(".config/monitors.xml");
        if let Ok(content) = std::fs::read_to_string(monitors_path) {
            if let Some(scale) = parse_primary_monitor_scale(&content) {
                if scale > 0.1 {
                    return Some(scale);
                }
            }
        }
    }

    None
}

fn init_display_environment() {
    // 1. On Wayland desktops, ensure winit connects directly via Wayland backend
    if env::var("WAYLAND_DISPLAY").is_ok() && env::var("WINIT_UNIX_BACKEND").is_err() {
        // Safe: called synchronously at program startup before any worker threads are created.
        unsafe {
            env::set_var("WINIT_UNIX_BACKEND", "wayland");
        }
    }

    // 2. Auto-detect display scaling (e.g. 1.25 on fractional Wayland displays)
    // and set SLINT_SCALE_FACTOR before the window backend initializes.
    if env::var("SLINT_SCALE_FACTOR").is_err() {
        if let Some(scale) = detect_system_scale_factor() {
            if (scale - 1.0).abs() > 0.01 {
                // Safe: called synchronously at program startup before any worker threads are created.
                unsafe {
                    env::set_var("SLINT_SCALE_FACTOR", scale.to_string());
                }
            }
        }
    }
}

pub fn compute_actual_render_zoom(zoom_level: f32, scale_factor: f32) -> f32 {
    // 96 DPI / 72 DPI = 1.3333 scale for native 100% display density.
    // Multiplying by monitor scale_factor ensures native physical pixel resolution.
    // Clamping to [0.2, 8.0] ensures crisp, high-resolution rendering all the way up to 500% zoom.
    (zoom_level * (96.0 / 72.0) * scale_factor).clamp(0.2, 8.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_display_environment();
    let app = AppWindow::new()?;

    // Load persistent config and apply to UI
    let config = history::load_app_config();
    app.set_dark_mode(config.dark_mode);
    app.set_paper_theme(config.paper_theme as i32);

    // Register global UI callbacks (Theme toggle & Open document)
    {
        let app_weak = app.as_weak();
        app.on_toggle_theme(move || {
            if let Some(ui) = app_weak.upgrade() {
                let current = ui.get_dark_mode();
                let next = !current;
                ui.set_dark_mode(next);
                history::update_app_config(|c| c.dark_mode = next);
            }
        });
    }

    {
        let app_weak = app.as_weak();
        app.on_open_document(move || {
            let app_weak = app_weak.clone();
            std::thread::spawn(move || {
                let res = rfd::FileDialog::new()
                    .add_filter("PDF document (*.pdf)", &["pdf"])
                    .pick_file();

                if let Some(path) = res {
                    let _ = app_weak.upgrade_in_event_loop(move |ui| {
                        let display_name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("document.pdf")
                            .to_string();
                        let _ = setup_document(&ui, &path, display_name);
                        update_recent_books_model(&ui);
                    });
                }
            });
        });
    }

    {
        let app_weak = app.as_weak();
        app.on_open_recent_book(move |path_str| {
            if let Some(ui) = app_weak.upgrade() {
                ui.set_show_recent_books(false);
                let path = PathBuf::from(path_str.as_str());
                if path.exists() {
                    let display_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("document.pdf")
                        .to_string();
                    let _ = setup_document(&ui, &path, display_name);
                    update_recent_books_model(&ui);
                } else {
                    trigger_toast(&ui, "File not found");
                }
            }
        });
    }

    update_recent_books_model(&app);

    let pdf_path_arg = env::args().nth(1);

    match pdf_path_arg {
        None => {
            app.set_file_title("Folio".into());
            app.set_file_path("No PDF file specified".into());
            app.set_has_error(true);
            app.set_error_message(
                "Open a PDF document to begin reading with rich annotations and dual-page view."
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

pub fn resolve_initial_paper_theme(active_theme: i32, config_theme: u8) -> pdf::PaperTheme {
    if active_theme >= 0 && active_theme <= 3 {
        pdf::PaperTheme::from(active_theme)
    } else {
        pdf::PaperTheme::from(config_theme as i32)
    }
}

fn to_slint_doc_info(props: &pdf::DocumentProperties) -> DocInfoData {
    DocInfoData {
        file_name: props.file_name.clone().into(),
        file_path: props.file_path.clone().into(),
        file_size: props.file_size_display.clone().into(),
        page_count: props.page_count_display.clone().into(),
        page_size: props.page_dimensions_display.clone().into(),
        title: props.title.clone().into(),
        author: props.author.clone().into(),
        subject: props.subject.clone().into(),
        keywords: props.keywords.clone().into(),
        creator: props.creator.clone().into(),
        producer: props.producer.clone().into(),
        creation_date: props.creation_date.clone().into(),
        mod_date: props.mod_date.clone().into(),
        pdf_version: props.pdf_version.clone().into(),
        encryption: props.encryption.clone().into(),
    }
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

    let doc_props = pdf::extract_document_properties(&path_str).unwrap_or_else(|_| pdf::DocumentProperties {
        file_name: display_name.clone(),
        file_path: path_str.clone(),
        ..Default::default()
    });
    app.set_doc_info(to_slint_doc_info(&doc_props));

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

    // Maintain active paper theme across files: either from the currently running UI or global config
    let config = history::load_app_config();
    let initial_paper_theme = resolve_initial_paper_theme(app.get_paper_theme(), config.paper_theme);

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
    app.set_paper_theme(initial_paper_theme as i32);

    // Table of Contents (Outline) presence
    let has_toc = !meta.toc.is_empty();

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
            creation_date: ea.creation_date,
            mod_date: ea.mod_date,
            author: ea.author,
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

    let initial_collapsed_ids: std::collections::HashSet<i32> = meta
        .toc
        .iter()
        .filter(|item| item.has_children)
        .map(|item| item.id)
        .collect();

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
        page_text_cache: std::collections::HashMap::new(),
        sel_start_page: 0,
        sel_start_pt: (0.0, 0.0),
        active_selections: Vec::new(),
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
        collapsed_toc_ids: initial_collapsed_ids,
        page_links_cache: std::collections::HashMap::new(),
        has_logical_pages: meta.has_logical_pages,
        page_labels: meta.page_labels,
        rotation: 0,
        is_dual_page: false,
        paper_theme: initial_paper_theme,
        doc_properties: doc_props,
        annotation_sort_mode: config.annotation_sort_mode,
    };

    update_page_indicator(&app_state, app);
    rebuild_toc_model(&app_state, app);

    app.set_rotation(0);
    app.set_is_dual_page(false);
    app.set_has_secondary_page(false);

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
                trigger_scroll_page_render(s, initial_page, app);
            }
        }
    }

    if let Ok(lock) = state.lock() {
        if let Some(s) = lock.as_ref() {
            save_current_session_direct(s, app);
        }
    }
    update_recent_books_model(app);

    Ok(())
}

fn update_recent_books_model(ui: &AppWindow) {
    let recents = history::get_recent_documents(10);
    let items: Vec<RecentBookItem> = recents
        .into_iter()
        .map(|r| RecentBookItem {
            filename: slint::SharedString::from(r.filename),
            path: slint::SharedString::from(r.path),
            page: r.page + 1,
        })
        .collect();
    ui.set_recent_books(slint::ModelRc::new(slint::VecModel::from(items)));
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

    // Drop stale results from different rotation
    if rendered.rotation != state.rotation {
        return;
    }

    // Drop stale results from different paper theme
    if rendered.theme != state.paper_theme {
        return;
    }

    // Drop stale preview from older generation
    if rendered.is_preview && rendered.generation != state.worker.current_generation() {
        return;
    }

    if let Some(existing) = state.cache.get(rendered.page, state.zoom_level, state.rotation, state.paper_theme) {
        if rendered.is_preview && !existing.is_preview {
            return;
        }
    }

    if let Some(image) = rgba_to_image(&rendered.rgba, rendered.w, rendered.h) {
        // Cache full-res renders (and previews as fallbacks)
        state.cache.put(
            rendered.page,
            state.zoom_level,
            state.rotation,
            rendered.theme,
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

        // Update secondary page if in dual-page spread
        if state.is_dual_page && !state.is_scroll_view && rendered.page == state.current_page + 1 {
            ui.set_secondary_page_image(image.clone());
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
    if !state.page_text_cache.contains_key(&page) {
        let extracted = pdf::extract_page_text_data(&state.file_path, page);
        state.page_text_cache.insert(page, extracted);
    }
    if state.cached_text_page != page {
        state.cached_text_page = page;
        if let Some(t) = state.page_text_cache.get(&page) {
            state.cached_text = t.clone();
        }
    }
}

fn get_cached_or_load_text<'a>(
    page_text_cache: &'a mut std::collections::HashMap<i32, pdf::PageTextData>,
    file_path: &str,
    page: i32,
) -> &'a pdf::PageTextData {
    page_text_cache.entry(page).or_insert_with(|| pdf::extract_page_text_data(file_path, page))
}

fn resolve_scroll_drag_position(state: &AppState, origin_page: i32, local_x: f32, local_y: f32) -> (i32, f32, f32) {
    if !state.is_scroll_view || state.total_pages <= 1 {
        return (origin_page, local_x, local_y);
    }

    let mut page_offsets: Vec<(f32, f32)> = Vec::with_capacity(state.total_pages as usize);
    let mut cur_offset = 0.0f32;
    for i in 0..state.total_pages {
        let (pw_pt, ph_pt) = state.get_page_size(i);
        let pw = pw_pt * state.zoom_level;
        let ph = ph_pt * state.zoom_level;
        page_offsets.push((cur_offset, pw));
        cur_offset += ph + 24.0;
    }

    let origin_top = page_offsets.get(origin_page as usize).map(|o| o.0).unwrap_or(0.0) + 12.0;
    let global_y = origin_top + local_y;

    for i in 0..state.total_pages {
        let (cur_top, pw) = page_offsets[i as usize];
        let card_top = cur_top + 12.0;
        let ph = state.get_page_size(i).1 * state.zoom_level;
        let card_bottom = card_top + ph;

        if i == 0 && global_y < card_top {
            return (0, local_x.clamp(0.0, pw), 0.0);
        }

        if global_y >= card_top && global_y <= card_bottom {
            return (i, local_x.clamp(0.0, pw), global_y - card_top);
        }

        if global_y > card_bottom {
            if i == state.total_pages - 1 {
                return (i, local_x.clamp(0.0, pw), ph);
            }
            let next_card_top = page_offsets[(i + 1) as usize].0 + 12.0;
            if global_y < next_card_top {
                // In the 24px gap between page i and page i + 1
                if global_y - card_bottom < (next_card_top - card_bottom) / 2.0 {
                    return (i, local_x.clamp(0.0, pw), ph);
                } else {
                    let next_pw = page_offsets[(i + 1) as usize].1;
                    return (i + 1, local_x.clamp(0.0, next_pw), 0.0);
                }
            }
        }
    }

    let last_idx = state.total_pages - 1;
    let (pw, ph) = (
        state.get_page_size(last_idx).0 * state.zoom_level,
        state.get_page_size(last_idx).1 * state.zoom_level,
    );
    (last_idx, local_x.clamp(0.0, pw), ph)
}

fn compute_multipage_selection(
    state: &mut AppState,
    start_page: i32,
    start_pt: (f32, f32),
    end_page: i32,
    end_pt: (f32, f32),
) -> Vec<PageSelection> {
    let mut results = Vec::new();

    if start_page == end_page {
        let text = get_cached_or_load_text(&mut state.page_text_cache, &state.file_path, start_page);
        let (snippet, rects) = pdf::compute_text_selection(text, start_pt.0, start_pt.1, end_pt.0, end_pt.1);
        if !rects.is_empty() && !snippet.trim().is_empty() {
            results.push(PageSelection {
                page: start_page,
                snippet,
                line_rects_pt: rects,
            });
        }
    } else if start_page < end_page {
        // Page start_page: from start_pt to EndOfPage
        let p_start_text = get_cached_or_load_text(&mut state.page_text_cache, &state.file_path, start_page);
        let (s_snip, s_rects) = pdf::compute_page_selection(
            p_start_text,
            pdf::PageBoundary::Point(start_pt.0, start_pt.1),
            pdf::PageBoundary::EndOfPage,
        );
        if !s_rects.is_empty() {
            results.push(PageSelection {
                page: start_page,
                snippet: s_snip,
                line_rects_pt: s_rects,
            });
        }

        // Intermediate full pages
        for p in (start_page + 1)..end_page {
            let p_text = get_cached_or_load_text(&mut state.page_text_cache, &state.file_path, p);
            let (mid_snip, mid_rects) = pdf::compute_page_selection(
                p_text,
                pdf::PageBoundary::StartOfPage,
                pdf::PageBoundary::EndOfPage,
            );
            if !mid_rects.is_empty() {
                results.push(PageSelection {
                    page: p,
                    snippet: mid_snip,
                    line_rects_pt: mid_rects,
                });
            }
        }

        // Page end_page: from StartOfPage to end_pt
        let p_end_text = get_cached_or_load_text(&mut state.page_text_cache, &state.file_path, end_page);
        let (e_snip, e_rects) = pdf::compute_page_selection(
            p_end_text,
            pdf::PageBoundary::StartOfPage,
            pdf::PageBoundary::Point(end_pt.0, end_pt.1),
        );
        if !e_rects.is_empty() {
            results.push(PageSelection {
                page: end_page,
                snippet: e_snip,
                line_rects_pt: e_rects,
            });
        }
    } else {
        // Dragging upwards: end_page < start_page
        let p_end_text = get_cached_or_load_text(&mut state.page_text_cache, &state.file_path, end_page);
        let (e_snip, e_rects) = pdf::compute_page_selection(
            p_end_text,
            pdf::PageBoundary::Point(end_pt.0, end_pt.1),
            pdf::PageBoundary::EndOfPage,
        );
        if !e_rects.is_empty() {
            results.push(PageSelection {
                page: end_page,
                snippet: e_snip,
                line_rects_pt: e_rects,
            });
        }

        for p in (end_page + 1)..start_page {
            let p_text = get_cached_or_load_text(&mut state.page_text_cache, &state.file_path, p);
            let (mid_snip, mid_rects) = pdf::compute_page_selection(
                p_text,
                pdf::PageBoundary::StartOfPage,
                pdf::PageBoundary::EndOfPage,
            );
            if !mid_rects.is_empty() {
                results.push(PageSelection {
                    page: p,
                    snippet: mid_snip,
                    line_rects_pt: mid_rects,
                });
            }
        }

        let p_start_text = get_cached_or_load_text(&mut state.page_text_cache, &state.file_path, start_page);
        let (s_snip, s_rects) = pdf::compute_page_selection(
            p_start_text,
            pdf::PageBoundary::StartOfPage,
            pdf::PageBoundary::Point(start_pt.0, start_pt.1),
        );
        if !s_rects.is_empty() {
            results.push(PageSelection {
                page: start_page,
                snippet: s_snip,
                line_rects_pt: s_rects,
            });
        }
    }

    results
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

fn get_annotation_min_y(a: &AnnotationRecord) -> f32 {
    let mut min_y = f32::INFINITY;
    for r in &a.line_rects_pt {
        let top = r.1.min(r.3);
        if top < min_y {
            min_y = top;
        }
    }
    if min_y.is_infinite() {
        0.0
    } else {
        min_y
    }
}

pub(crate) fn build_sidebar_annotation_items(
    records: &[AnnotationRecord],
    is_notes: bool,
    toc: &[pdf::TocItem],
    sort_mode: u8, // 0 = Document Order (chapters), 1 = Newest First, 2 = Oldest First
) -> Vec<AnnotationItemData> {
    // 1. Filter
    let mut filtered: Vec<&AnnotationRecord> = records
        .iter()
        .filter(|a| {
            if is_notes {
                a.kind == "note" || !a.comment.trim().is_empty()
            } else {
                a.kind != "note"
            }
        })
        .collect();

    let get_chapter_heading = |a: &AnnotationRecord| -> String {
        if !toc.is_empty() {
            export::resolve_toc_heading(a.page, toc)
                .unwrap_or_else(|| format!("Page {}", a.page + 1))
        } else {
            format!("Page {}", a.page + 1)
        }
    };

    // 2. Sort
    filtered.sort_by(|a, b| {
        match sort_mode {
            1 => {
                // Newest First
                let raw_a = if !a.creation_date.is_empty() { &a.creation_date } else { &a.mod_date };
                let raw_b = if !b.creation_date.is_empty() { &b.creation_date } else { &b.mod_date };
                raw_b.cmp(raw_a).then_with(|| b.id.cmp(&a.id))
            }
            2 => {
                // Oldest First
                let raw_a = if !a.creation_date.is_empty() { &a.creation_date } else { &a.mod_date };
                let raw_b = if !b.creation_date.is_empty() { &b.creation_date } else { &b.mod_date };
                raw_a.cmp(raw_b).then_with(|| a.id.cmp(&b.id))
            }
            _ => {
                // Document Order: page, min_y, id
                let y_a = get_annotation_min_y(a);
                let y_b = get_annotation_min_y(b);
                let y_cmp = y_a.partial_cmp(&y_b).unwrap_or(std::cmp::Ordering::Equal);
                a.page.cmp(&b.page).then(y_cmp).then_with(|| a.id.cmp(&b.id))
            }
        }
    });

    // 3. Assign group headers and construct AnnotationItemData
    let mut items = Vec::with_capacity(filtered.len());
    let mut last_group = String::new();

    for annot in filtered {
        let swatch_col = get_color(annot.color_index).1;
        let col = parse_slint_color(&annot.color_hex).unwrap_or(swatch_col);
        let raw_date = if !annot.mod_date.is_empty() {
            &annot.mod_date
        } else {
            &annot.creation_date
        };
        let display_date = pdf::format_pdf_date(raw_date);

        // In Document Order (0), group under chapter/page headers; in date modes (1, 2), flat list
        let group_header = if sort_mode == 0 {
            let heading = get_chapter_heading(annot);
            if heading != last_group {
                last_group = heading.clone();
                heading
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        items.push(AnnotationItemData {
            id: annot.id,
            page: annot.page,
            kind: annot.kind.clone().into(),
            color: col,
            color_hex: annot.color_hex.clone().into(),
            snippet: annot.snippet.clone().into(),
            comment: annot.comment.clone().into(),
            display_page: format!("{}", annot.page + 1).into(),
            display_date: display_date.into(),
            author: annot.author.clone().into(),
            group_header: group_header.into(),
        });
    }

    items
}

fn sync_annotations_to_ui(s: &AppState, ui: &AppWindow) {
    let highlights = build_sidebar_annotation_items(
        &s.annotations,
        false,
        &s.toc,
        s.annotation_sort_mode,
    );

    let notes = build_sidebar_annotation_items(
        &s.annotations,
        true,
        &s.toc,
        s.annotation_sort_mode,
    );

    let mut visuals = Vec::new();

    for annot in &s.annotations {
        let swatch_col = get_color(annot.color_index).1;
        let col = parse_slint_color(&annot.color_hex).unwrap_or(swatch_col);
        let has_note = annot.kind == "note" || !annot.comment.trim().is_empty();

        // All annotations (including highlights) provide visual hitboxes for hover & click interaction
        let (unrot_w, unrot_h) = s.get_unrotated_page_size(annot.page);
        let num_rects = annot.line_rects_pt.len();
        for (idx, rect) in annot.line_rects_pt.iter().enumerate() {
            let is_last_rect = idx + 1 == num_rects;
            let tr = transform_rect_for_rotation(*rect, unrot_w, unrot_h, s.rotation);
            visuals.push(AnnotationVisual {
                id: annot.id,
                page: annot.page,
                kind: annot.kind.clone().into(),
                color: col,
                color_index: annot.color_index,
                x: tr.0 * s.zoom_level,
                y: tr.1 * s.zoom_level,
                w: (tr.2 - tr.0).max(4.0) * s.zoom_level,
                h: (tr.3 - tr.1).max(4.0) * s.zoom_level,
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

    ui.set_annotation_sort_mode(s.annotation_sort_mode as i32);

    // Update active selection rects according to the current zoom level
    if !s.active_selections.is_empty() {
        let mut ui_rects = Vec::new();
        for sel in &s.active_selections {
            let (unrot_w, unrot_h) = s.get_unrotated_page_size(sel.page);
            for orig_r in adjust_line_rects_for_highlight(&sel.line_rects_pt) {
                let tr = transform_rect_for_rotation(orig_r, unrot_w, unrot_h, s.rotation);
                ui_rects.push(SelectionRect {
                    page: sel.page,
                    x: tr.0 * s.zoom_level,
                    y: tr.1 * s.zoom_level,
                    width: (tr.2 - tr.0) * s.zoom_level,
                    height: (tr.3 - tr.1) * s.zoom_level,
                });
            }
        }
        if let Some(first) = ui_rects.first() {
            let (pw_pt, _) = s.get_page_size(first.page);
            let page_w = pw_pt * s.zoom_level;
            let popup_x = ((first.x + (first.width - 296.0) / 2.0).clamp(10.0, (page_w - 306.0).max(10.0))).round();
            let popup_y = ((first.y - 58.0).max(8.0)).round();
            ui.set_popup_page(first.page);
            ui.set_popup_x(popup_x);
            ui.set_popup_y(popup_y);
        }
        ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::from(ui_rects)).into());
    } else if !s.active_selection_line_rects.is_empty() {
        let (unrot_w, unrot_h) = s.get_unrotated_page_size(s.active_selection_page);
        let mut ui_rects = Vec::new();
        for orig_r in adjust_line_rects_for_highlight(&s.active_selection_line_rects) {
            let tr = transform_rect_for_rotation(orig_r, unrot_w, unrot_h, s.rotation);
            ui_rects.push(SelectionRect {
                page: s.active_selection_page,
                x: tr.0 * s.zoom_level,
                y: tr.1 * s.zoom_level,
                width: (tr.2 - tr.0) * s.zoom_level,
                height: (tr.3 - tr.1) * s.zoom_level,
            });
        }
        if !ui_rects.is_empty() {
            let first = &ui_rects[0];
            let (pw_pt, _) = s.get_page_size(s.active_selection_page);
            let page_w = pw_pt * s.zoom_level;
            let popup_x = ((first.x + (first.width - 296.0) / 2.0).clamp(10.0, (page_w - 306.0).max(10.0))).round();
            let popup_y = ((first.y - 58.0).max(8.0)).round();
            ui.set_popup_page(first.page);
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

        let (unrot_w, unrot_h) = s.get_unrotated_page_size(m.page);
        for rect in &m.rects_pt {
            let tr = transform_rect_for_rotation(*rect, unrot_w, unrot_h, s.rotation);
            let rx = tr.0 * s.zoom_level;
            let ry = tr.1 * s.zoom_level;
            let rw = (tr.2 - tr.0).max(2.0) * s.zoom_level;
            let rh = (tr.3 - tr.1).max(2.0) * s.zoom_level;

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

pub fn compute_visible_toc_items(
    toc: &[pdf::TocItem],
    collapsed_ids: &std::collections::HashSet<i32>,
    page_labels: &[String],
) -> Vec<TocItemData> {
    let mut visible_entries = Vec::new();
    let mut skip_until_depth: Option<i32> = None;

    for item in toc {
        if let Some(skip_depth) = skip_until_depth {
            if item.depth > skip_depth {
                continue;
            } else {
                skip_until_depth = None;
            }
        }

        let is_expanded = !collapsed_ids.contains(&item.id);
        let display_page = if !page_labels.is_empty() {
            page_labels
                .get(item.page as usize)
                .cloned()
                .unwrap_or_else(|| format!("{}", item.page + 1))
        } else {
            format!("{}", item.page + 1)
        };

        visible_entries.push(TocItemData {
            id: item.id,
            title: item.title.clone().into(),
            page: item.page,
            display_page: display_page.into(),
            depth: item.depth,
            has_children: item.has_children,
            is_expanded,
        });

        if item.has_children && !is_expanded {
            skip_until_depth = Some(item.depth);
        }
    }
    visible_entries
}

fn rebuild_toc_model(state: &AppState, ui: &AppWindow) {
    let visible_entries = compute_visible_toc_items(
        &state.toc,
        &state.collapsed_toc_ids,
        &state.page_labels,
    );
    let toc_model = std::rc::Rc::new(slint::VecModel::from(visible_entries));
    ui.set_toc_items(toc_model.into());
}

pub fn resolve_page_number(
    input: &str,
    total_pages: i32,
    has_logical_pages: bool,
    page_labels: &[String],
) -> Option<i32> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }

    // Explicit sheet prefix: e.g. "#15" or "p15" or "p.15" jumps directly to sheet 15
    if let Some(rest) = raw
        .strip_prefix('#')
        .or_else(|| raw.strip_prefix("p."))
        .or_else(|| raw.strip_prefix('p'))
        .or_else(|| raw.strip_prefix('P'))
    {
        let rest_trimmed = rest.trim();
        if let Ok(paper_num) = rest_trimmed.parse::<i32>() {
            if paper_num >= 1 && paper_num <= total_pages {
                return Some(paper_num - 1);
            }
        }
    }

    if has_logical_pages {
        // First priority: match exact logical page label (e.g. "ix", "1", "29", "Cover")
        if let Some(pos) = page_labels
            .iter()
            .position(|lbl| lbl.eq_ignore_ascii_case(raw))
        {
            return Some(pos as i32);
        }
        // Second priority: check if user typed a 1-based paper sheet number
        if let Ok(paper_num) = raw.parse::<i32>() {
            if paper_num >= 1 && paper_num <= total_pages {
                return Some(paper_num - 1);
            }
        }
    } else {
        // Document without custom labels: standard 1-based paper number
        if let Ok(paper_num) = raw.parse::<i32>() {
            if paper_num >= 1 && paper_num <= total_pages {
                return Some(paper_num - 1);
            }
        }
    }

    None
}

fn resolve_page_input(input: &str, state: &AppState) -> Option<i32> {
    resolve_page_number(
        input,
        state.total_pages,
        state.has_logical_pages,
        &state.page_labels,
    )
}

fn update_page_indicator(state: &AppState, ui: &AppWindow) {
    if state.is_dual_page && state.current_page + 1 < state.total_pages {
        let p1_paper = state.current_page + 1;
        let p2_paper = state.current_page + 2;
        let p1_label = if state.has_logical_pages {
            state
                .page_labels
                .get(state.current_page as usize)
                .cloned()
                .unwrap_or_else(|| format!("{p1_paper}"))
        } else {
            format!("{p1_paper}")
        };
        let p2_label = if state.has_logical_pages {
            state
                .page_labels
                .get((state.current_page + 1) as usize)
                .cloned()
                .unwrap_or_else(|| format!("{p2_paper}"))
        } else {
            format!("{p2_paper}")
        };

        let cur_input = format!("{}-{}", p1_label, p2_label);
        let has_different = state.has_logical_pages
            && (p1_label != format!("{p1_paper}") || p2_label != format!("{p2_paper}"));

        let suffix = if has_different {
            format!("({}-{} of {})", p1_paper, p2_paper, state.total_pages)
        } else {
            format!("/ {}", state.total_pages)
        };

        ui.set_current_page_input(cur_input.clone().into());
        ui.set_page_suffix_display(suffix.clone().into());
        ui.set_page_display(format!("{} {}", cur_input, suffix).into());
    } else {
        let paper = state.current_page + 1;
        let label = if state.has_logical_pages {
            state
                .page_labels
                .get(state.current_page as usize)
                .cloned()
                .unwrap_or_else(|| format!("{paper}"))
        } else {
            format!("{paper}")
        };

        let has_different = state.has_logical_pages && label != format!("{paper}");
        let suffix = if has_different {
            format!("({} of {})", paper, state.total_pages)
        } else {
            format!("/ {}", state.total_pages)
        };

        ui.set_current_page_input(label.clone().into());
        ui.set_page_suffix_display(suffix.clone().into());
        ui.set_page_display(format!("{} {}", label, suffix).into());
    }

    ui.set_can_prev(state.current_page > 0);
    ui.set_can_next(state.current_page < state.total_pages - 1);
}

pub fn compute_transformed_links(
    links: &[pdf::PageLink],
    page: i32,
    unrot_w: f32,
    unrot_h: f32,
    rotation: i32,
    zoom_level: f32,
) -> Vec<PdfLinkData> {
    let mut ui_links = Vec::with_capacity(links.len());
    for link in links {
        let tr = transform_rect_for_rotation(link.bounds_pt, unrot_w, unrot_h, rotation);
        let x = tr.0 * zoom_level;
        let y = tr.1 * zoom_level;
        let width = (tr.2 - tr.0).max(2.0) * zoom_level;
        let height = (tr.3 - tr.1).max(2.0) * zoom_level;

        ui_links.push(PdfLinkData {
            page,
            x,
            y,
            width,
            height,
            target_page: link.target_page,
            uri: link.uri.clone().into(),
        });
    }
    ui_links
}

fn sync_links_to_ui(state: &mut AppState, ui: &AppWindow) {
    let pages_to_sync: Vec<i32> = if state.is_scroll_view {
        let min_p = (state.current_page - 2).max(0);
        let max_p = (state.current_page + 3).min(state.total_pages - 1);
        (min_p..=max_p).collect()
    } else if state.is_dual_page {
        if state.current_page + 1 < state.total_pages {
            vec![state.current_page, state.current_page + 1]
        } else {
            vec![state.current_page]
        }
    } else {
        vec![state.current_page]
    };

    let mut ui_links = Vec::new();
    for &page in &pages_to_sync {
        let links = state.get_or_load_page_links(page).to_vec();
        let (unrot_w, unrot_h) = state.get_unrotated_page_size(page);
        ui_links.extend(compute_transformed_links(
            &links,
            page,
            unrot_w,
            unrot_h,
            state.rotation,
            state.zoom_level,
        ));
    }

    let link_model = std::rc::Rc::new(slint::VecModel::from(ui_links));
    ui.set_page_links(link_model.into());
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
            paper_theme: s.paper_theme as u8,
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
    let actual_render_zoom = compute_actual_render_zoom(state.zoom_level, scale_factor);

    // Update toolbar page indicator: e.g. "1 / 120" or "1-2 / 120"
    ui.set_current_page(state.current_page);
    ui.set_rotation(state.rotation);
    let is_dual = state.is_dual_page && !state.is_scroll_view;
    let has_secondary = is_dual && (state.current_page + 1 < state.total_pages);
    ui.set_is_dual_page(state.is_dual_page);
    ui.set_has_secondary_page(has_secondary);

    update_page_indicator(state, ui);
    ui.set_zoom_display(format!("{}%", (state.zoom_level * 100.0).round() as i32).into());
    ui.set_zoom_level(state.zoom_level);

    // Update displayed page dimensions for single page view with current page's exact geometry
    let (cur_w_pt, cur_h_pt) = state.get_page_size(state.current_page);
    let card_w = cur_w_pt * state.zoom_level;
    let card_h = cur_h_pt * state.zoom_level;
    ui.set_page_width(card_w);
    ui.set_page_height(card_h);

    if has_secondary {
        let (sec_w_pt, sec_h_pt) = state.get_page_size(state.current_page + 1);
        ui.set_secondary_page_width(sec_w_pt * state.zoom_level);
        ui.set_secondary_page_height(sec_h_pt * state.zoom_level);
    }

    // Update dimensions in scroll-pages model for continuous scroll view
    let standard_h = if state.page_sizes.len() > 1 {
        state.get_page_size(1).1
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
            let geom_changed = (item.offset_y - cur_offset).abs() > 0.5
                || (item.width - pw).abs() > 0.5
                || (item.height - ph).abs() > 0.5;
            item.offset_y = cur_offset;
            item.width = pw;
            item.height = ph;
            let mut image_changed = false;
            if let Some(cached) = state.cache.get(i as i32, state.zoom_level, state.rotation, state.paper_theme) {
                if let Some(img) = rgba_to_image(&cached.rgba, cached.width, cached.height) {
                    item.image = img;
                    image_changed = true;
                }
            }
            if geom_changed || image_changed {
                pages_model.set_row_data(i, item);
            }
        }
        cur_offset += ph + 24.0;
    }
    ui.set_total_scroll_height(cur_offset + 12.0);

    // Refresh annotation visual coordinates scaled to current zoom
    sync_annotations_to_ui(state, ui);
    sync_search_to_ui(state, ui);
    sync_links_to_ui(state, ui);

    // Bump generation counter
    let next_gen = state.worker.current_generation() + 1;
    state.worker.set_current_generation(next_gen);

    // Check LRU cache for current page
    let cached = state.cache.get(state.current_page, state.zoom_level, state.rotation, state.paper_theme).cloned();

    let mut primary_needs_render = true;
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
            primary_needs_render = false;
        }
    }

    if primary_needs_render {
        // Step 1: Low-res preview first (0.25x)
        let preview_zoom = (actual_render_zoom * 0.25).max(0.1);
        let h_current = get_page_highlights(state, state.current_page);
        state.worker.request_render(RenderRequest {
            page: state.current_page,
            zoom: preview_zoom,
            generation: next_gen,
            is_preview: true,
            highlights: h_current.clone(),
            rotation: state.rotation,
            theme: state.paper_theme,
        });

        // Step 2: Full-res render for current page
        state.worker.request_render(RenderRequest {
            page: state.current_page,
            zoom: actual_render_zoom,
            generation: next_gen,
            is_preview: false,
            highlights: h_current,
            rotation: state.rotation,
            theme: state.paper_theme,
        });
    }

    // If dual-page spread, check and request companion page
    if has_secondary {
        let sec_page = state.current_page + 1;
        let cached_sec = state.cache.get(sec_page, state.zoom_level, state.rotation, state.paper_theme).cloned();
        let mut sec_needs_render = true;
        if let Some(cached_render) = cached_sec {
            if let Some(img) = rgba_to_image(&cached_render.rgba, cached_render.width, cached_render.height) {
                ui.set_secondary_page_image(img);
            }
            if !cached_render.is_preview {
                sec_needs_render = false;
            }
        }

        if sec_needs_render {
            let h_sec = get_page_highlights(state, sec_page);
            state.worker.request_render(RenderRequest {
                page: sec_page,
                zoom: actual_render_zoom,
                generation: next_gen,
                is_preview: false,
                highlights: h_sec,
                rotation: state.rotation,
                theme: state.paper_theme,
            });
        }
    }

    // Step 3: Prefetch neighbors
    prefetch_neighbors(state, next_gen, actual_render_zoom);
}

fn prefetch_neighbors(state: &mut AppState, generation: u64, zoom: f32) {
    let neighbors = if state.zoom_level > 3.0 {
        vec![state.current_page + 1, state.current_page - 1]
    } else {
        vec![
            state.current_page + 1,
            state.current_page - 1,
            state.current_page + 2,
            state.current_page - 2,
        ]
    };

    for &p in &neighbors {
        if p >= 0 && p < state.total_pages {
            if state.cache.get(p, state.zoom_level, state.rotation, state.paper_theme).is_none() {
                let h_neighbor = get_page_highlights(state, p);
                state.worker.request_render(RenderRequest {
                    page: p,
                    zoom,
                    generation,
                    is_preview: false,
                    highlights: h_neighbor,
                    rotation: state.rotation,
                    theme: state.paper_theme,
                });
            }
        }
    }
}

fn trigger_scroll_page_render(state: &mut AppState, page_idx: i32, ui: &AppWindow) {
    let scale_factor = ui.window().scale_factor().max(1.0);
    let actual_render_zoom = compute_actual_render_zoom(state.zoom_level, scale_factor);

    let next_gen = state.worker.current_generation() + 1;
    state.worker.set_current_generation(next_gen);

    // If cached, apply immediately
    if let Some(cached) = state.cache.get(page_idx, state.zoom_level, state.rotation, state.paper_theme).cloned() {
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
        rotation: state.rotation,
        theme: state.paper_theme,
    });

    state.worker.request_render(RenderRequest {
        page: page_idx,
        zoom: actual_render_zoom,
        generation: next_gen,
        is_preview: false,
        highlights: h_page,
        rotation: state.rotation,
        theme: state.paper_theme,
    });

    prefetch_neighbors(state, next_gen, actual_render_zoom);
}

fn trigger_scroll_page_render_direct(state: &mut AppState, page_idx: i32, ui: &AppWindow) {
    let scale_factor = ui.window().scale_factor().max(1.0);
    let actual_render_zoom = compute_actual_render_zoom(state.zoom_level, scale_factor);

    let next_gen = state.worker.current_generation() + 1;
    state.worker.set_current_generation(next_gen);

    let h_page = get_page_highlights(state, page_idx);
    state.worker.request_render(RenderRequest {
        page: page_idx,
        zoom: actual_render_zoom,
        generation: next_gen,
        is_preview: false,
        highlights: h_page,
        rotation: state.rotation,
        theme: state.paper_theme,
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
                creation_date: a.creation_date.clone(),
                mod_date: a.mod_date.clone(),
                author: a.author.clone(),
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
                let currently_showing = ui.get_show_search() && ui.get_show_sidebar() && ui.get_sidebar_tab() == 3;
                if currently_showing {
                    *search_debounce.borrow_mut() = None;
                    ui.set_show_search(false);
                    ui.set_show_sidebar(false);
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
                ui.set_show_sidebar(false);
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
            let valid = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        if target_page >= 0 && target_page < s.total_pages {
                            s.current_page = target_page;
                            is_scroll = s.is_scroll_view;
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
                    if let Ok(lock) = state_lock.lock() {
                        if let Some(s) = lock.as_ref() {
                            update_page_indicator(s, &ui);
                        }
                    }

                    if is_scroll {
                        ui.invoke_scroll_to_page(target_page);
                        if let Ok(mut lock) = state_lock.lock() {
                            if let Some(s) = lock.as_mut() {
                                trigger_scroll_page_render(s, target_page, &ui);
                                sync_links_to_ui(s, &ui);
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

    // Go to Page (Direct Input in top toolbar)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        let save_session = make_session_saver();
        app.on_go_to_page(move |input| {
            let mut target_page: Option<i32> = None;
            let mut is_scroll = false;

            if let Ok(lock) = state_lock.lock() {
                if let Some(s) = lock.as_ref() {
                    target_page = resolve_page_input(input.as_str(), s);
                    is_scroll = s.is_scroll_view;
                }
            }

            if let Some(ui) = app_weak.upgrade() {
                if let Some(target) = target_page {
                    if let Ok(mut lock) = state_lock.lock() {
                        if let Some(s) = lock.as_mut() {
                            s.current_page = target;
                            update_page_indicator(s, &ui);
                            if is_scroll {
                                trigger_scroll_page_render(s, target, &ui);
                                sync_links_to_ui(s, &ui);
                            }
                        }
                    }
                    if is_scroll {
                        ui.invoke_scroll_to_page(target);
                    } else {
                        update_view(&ui, &state_lock);
                    }
                    save_session();
                } else {
                    // Invalid page input: reset input to current page
                    if let Ok(lock) = state_lock.lock() {
                        if let Some(s) = lock.as_ref() {
                            update_page_indicator(s, &ui);
                        }
                    }
                }
            }
        });
    }

    // Reset Page Input (Cancel edit on Escape)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_reset_page_input(move || {
            if let Some(ui) = app_weak.upgrade() {
                if let Ok(lock) = state_lock.lock() {
                    if let Some(s) = lock.as_ref() {
                        update_page_indicator(s, &ui);
                    }
                }
            }
        });
    }

    // Toggle Table of Contents Item Collapse/Expand
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_toggle_toc_collapse(move |id| {
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    if s.collapsed_toc_ids.contains(&id) {
                        s.collapsed_toc_ids.remove(&id);
                    } else {
                        s.collapsed_toc_ids.insert(id);
                    }
                    if let Some(ui) = app_weak.upgrade() {
                        rebuild_toc_model(s, &ui);
                    }
                }
            }
        });
    }

    // Open External Document URL in default browser
    {
        app.on_open_external_url(move |uri| {
            let uri_str = uri.as_str().trim();
            if uri_str.starts_with("http://") || uri_str.starts_with("https://") || uri_str.starts_with("mailto:") {
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("xdg-open").arg(uri_str).spawn();
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(uri_str).spawn();
                #[cfg(target_os = "windows")]
                let _ = std::process::Command::new("cmd").args(["/C", "start", "", uri_str]).spawn();
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
                    s.sel_start_page = page_no;
                    s.sel_start_pt = s.to_unrotated_pt(page_no, local_x / zoom, local_y / zoom);
                    s.active_selections.clear();
                    s.active_selection_page = page_no;
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
                ui.set_selected_annotation_kind("".into());
                ui.set_hovered_annotation_id(-1);
                ui.set_current_note_text("".into());
                ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());
            }
        });
    }

    // Selection Dragged (live sub-word character level selection across lines & pages)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_selection_dragged(move |page_no, local_x, local_y| {
            let mut line_rects_ui = Vec::new();
            let mut has_sel = false;

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    let (target_page, curr_x, curr_y) = resolve_scroll_drag_position(s, page_no, local_x, local_y);
                    let zoom = s.zoom_level.max(0.1);
                    let (pt_x, pt_y) = s.to_unrotated_pt(target_page, curr_x / zoom, curr_y / zoom);

                    let selections = compute_multipage_selection(
                        s,
                        s.sel_start_page,
                        s.sel_start_pt,
                        target_page,
                        (pt_x, pt_y),
                    );

                    s.active_selections = selections;
                    s.active_selection_page = s.active_selections.first().map(|p| p.page).unwrap_or(page_no);
                    s.active_selection_snippet = s.active_selections.iter().map(|p| p.snippet.as_str()).collect::<Vec<_>>().join("\n");
                    s.active_selection_line_rects = s.active_selections.first().map(|p| p.line_rects_pt.clone()).unwrap_or_default();

                    if !s.active_selections.is_empty() {
                        has_sel = true;
                        for sel in &s.active_selections {
                            let (unrot_w, unrot_h) = s.get_unrotated_page_size(sel.page);
                            for orig_r in adjust_line_rects_for_highlight(&sel.line_rects_pt) {
                                let tr = transform_rect_for_rotation(orig_r, unrot_w, unrot_h, s.rotation);
                                line_rects_ui.push(SelectionRect {
                                    page: sel.page,
                                    x: tr.0 * zoom,
                                    y: tr.1 * zoom,
                                    width: (tr.2 - tr.0) * zoom,
                                    height: (tr.3 - tr.1) * zoom,
                                });
                            }
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
            let mut popup_page = page_no;
            let mut popup_x = 0.0f32;
            let mut popup_y = 0.0f32;

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    let (target_page, curr_x, curr_y) = resolve_scroll_drag_position(s, page_no, local_x, local_y);
                    let zoom = s.zoom_level.max(0.1);
                    let (pt_x, pt_y) = s.to_unrotated_pt(target_page, curr_x / zoom, curr_y / zoom);

                    let selections = compute_multipage_selection(
                        s,
                        s.sel_start_page,
                        s.sel_start_pt,
                        target_page,
                        (pt_x, pt_y),
                    );

                    s.active_selections = selections;
                    s.active_selection_page = s.active_selections.first().map(|p| p.page).unwrap_or(page_no);
                    s.active_selection_snippet = s.active_selections.iter().map(|p| p.snippet.as_str()).collect::<Vec<_>>().join("\n");
                    s.active_selection_line_rects = s.active_selections.first().map(|p| p.line_rects_pt.clone()).unwrap_or_default();

                    if !s.active_selections.is_empty() && !s.active_selection_snippet.trim().is_empty() {
                        has_sel = true;
                        for sel in &s.active_selections {
                            let (unrot_w, unrot_h) = s.get_unrotated_page_size(sel.page);
                            for orig_r in adjust_line_rects_for_highlight(&sel.line_rects_pt) {
                                let tr = transform_rect_for_rotation(orig_r, unrot_w, unrot_h, s.rotation);
                                line_rects_ui.push(SelectionRect {
                                    page: sel.page,
                                    x: tr.0 * zoom,
                                    y: tr.1 * zoom,
                                    width: (tr.2 - tr.0) * zoom,
                                    height: (tr.3 - tr.1) * zoom,
                                });
                            }
                        }
                        if let Some(first) = line_rects_ui.first() {
                            let (pw_pt, _) = s.get_page_size(first.page);
                            let page_w = pw_pt * zoom;
                            popup_page = first.page;
                            popup_x = ((first.x + (first.width - 296.0) / 2.0).clamp(10.0, (page_w - 306.0).max(10.0))).round();
                            popup_y = ((first.y - 58.0).max(8.0)).round();
                        }
                    } else {
                        s.active_selections.clear();
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
                ui.set_selected_annotation_kind("".into());
                    ui.set_popup_page(popup_page);
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
                ui.set_selected_annotation_kind("".into());
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
            let mut highlight_updates: Vec<(i32, slint::Image)> = Vec::new();

            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    let selections = if !s.active_selections.is_empty() {
                        s.active_selections.clone()
                    } else if !s.active_selection_line_rects.is_empty() {
                        vec![PageSelection {
                            page: s.active_selection_page,
                            snippet: s.active_selection_snippet.clone(),
                            line_rects_pt: s.active_selection_line_rects.clone(),
                        }]
                    } else {
                        Vec::new()
                    };

                    let now_date = pdf::now_pdf_date();
                    let author = get_current_author();
                    for sel in selections {
                        if sel.line_rects_pt.is_empty() {
                            continue;
                        }
                        let target_page = sel.page;
                        let annot = AnnotationRecord {
                            id: s.next_annot_id,
                            page: target_page,
                            kind: kind.to_string(),
                            color_index: color_idx,
                            color_hex: hex.clone(),
                            snippet: sel.snippet.clone(),
                            comment: String::new(),
                            line_rects_pt: sel.line_rects_pt.clone(),
                            is_from_pdf: false,
                            creation_date: now_date.clone(),
                            mod_date: now_date.clone(),
                            author: author.clone(),
                        };
                        s.next_annot_id += 1;
                        s.push_undo(UndoAction::Created(annot.clone()));

                        if kind == "highlight" {
                            let rgb = get_highlight_blend_rgb(color_idx);
                            let adjusted_rects = adjust_line_rects_for_highlight(&sel.line_rects_pt);
                            let (pw_pt, _) = s.get_page_size(target_page);
                            let (unrot_w, unrot_h) = s.get_unrotated_page_size(target_page);
                            let rotation = s.rotation;

                            if let Some(cached) = s.cache.get_mut(target_page, s.zoom_level, rotation, s.paper_theme) {
                                let w = cached.width as usize;
                                let h = cached.height as usize;
                                let zoom = w as f32 / pw_pt.max(1.0);

                                let h_overlay = pdf::worker::HighlightOverlay {
                                    rgb,
                                    rects_pt: adjusted_rects,
                                };

                                pdf::worker::apply_highlights_to_rgba(
                                    &mut cached.rgba,
                                    w,
                                    h,
                                    &[h_overlay],
                                    s.paper_theme,
                                    unrot_w,
                                    unrot_h,
                                    rotation,
                                    zoom,
                                );

                                if let Some(img) = rgba_to_image(&cached.rgba, cached.width, cached.height) {
                                    highlight_updates.push((target_page, img));
                                }
                            }
                        }

                        s.annotations.push(annot);
                        need_sync = true;
                    }

                    s.active_selections.clear();
                    s.active_selection_line_rects.clear();
                    s.active_selection_snippet.clear();
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
                ui.set_selected_annotation_kind("".into());
                    ui.set_selection_rects(std::rc::Rc::new(slint::VecModel::default()).into());

                    for (target_page, img) in highlight_updates {
                        let pages_model = ui.get_scroll_pages();
                        let idx = target_page as usize;
                        if let Some(mut item) = pages_model.row_data(idx) {
                            item.image = img.clone();
                            pages_model.set_row_data(idx, item);
                        }
                        if target_page == ui.get_current_page() {
                            ui.set_page_image(img.clone());
                        }
                        if ui.get_is_dual_page() && target_page == ui.get_current_page() + 1 {
                            ui.set_secondary_page_image(img);
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
                    let (target_page, snippet, line_rects) = if !s.active_selections.is_empty() {
                        let primary = &s.active_selections[0];
                        (primary.page, s.active_selection_snippet.clone(), primary.line_rects_pt.clone())
                    } else if !s.active_selection_line_rects.is_empty() {
                        (s.active_selection_page, s.active_selection_snippet.clone(), s.active_selection_line_rects.clone())
                    } else {
                        (-1, String::new(), Vec::new())
                    };

                    if target_page >= 0 && !line_rects.is_empty() {
                        let now_date = pdf::now_pdf_date();
                        let author = get_current_author();
                        let annot = AnnotationRecord {
                            id: s.next_annot_id,
                            page: target_page,
                            kind: "note".to_string(),
                            color_index: color_idx,
                            color_hex: hex,
                            snippet,
                            comment: comment.to_string(),
                            line_rects_pt: line_rects,
                            is_from_pdf: false,
                            creation_date: now_date.clone(),
                            mod_date: now_date,
                            author,
                        };
                        s.next_annot_id += 1;
                        s.push_undo(UndoAction::Created(annot.clone()));
                        s.annotations.push(annot);
                        s.active_selections.clear();
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
                ui.set_selected_annotation_kind("".into());
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
                ui.set_selected_annotation_kind("".into());
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
                        is_highlight = s.annotations[pos].kind == "highlight";
                        let hex = if is_highlight {
                            let [r, g, b] = get_highlight_blend_rgb(new_color_idx);
                            format!("#{r:02x}{g:02x}{b:02x}")
                        } else {
                            get_color(new_color_idx).0
                        };
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
                        s.annotations[pos].color_index = new_color_idx;
                        s.annotations[pos].color_hex = hex;
                        s.annotations[pos].mod_date = pdf::now_pdf_date();
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
                            s.annotations[pos].mod_date = pdf::now_pdf_date();
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
                ui.set_selected_annotation_kind("".into());
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
                ui.set_selected_annotation_kind("".into());
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

            let app_weak = app_weak.clone();
            let state_lock = Arc::clone(&state_lock);
            std::thread::spawn(move || {
                let chosen_path = match prompt_save_as(&file_path) {
                    Some(p) => p,
                    None => return, // User cancelled
                };

                let new_path_str = chosen_path.to_string_lossy().to_string();

                let _ = app_weak.upgrade_in_event_loop({
                    let new_path_str = new_path_str.clone();
                    let file_path = file_path.clone();
                    let to_save = to_save.clone();
                    let state_lock = Arc::clone(&state_lock);
                    let app_weak = app_weak.clone();
                    move |ui| {
                        ui.set_is_saving(true);

                        // If saving to a new path, copy the base file over first if they differ
                        if new_path_str != file_path {
                            if let Err(e) = std::fs::copy(&file_path, &new_path_str) {
                                eprintln!("Failed to copy PDF for Save As: {e}");
                                ui.set_is_saving(false);
                                return;
                            }
                        }

                        let save_result = pdf::save_document_annotations(&new_path_str, &to_save);
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
            });
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

            if is_empty {
                if let Some(ui) = app_weak.upgrade() {
                    trigger_toast(&ui, "No annotations to export");
                }
                return;
            }

            let app_weak = app_weak.clone();
            std::thread::spawn(move || {
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
                                .unwrap_or("annotations.md")
                                .to_string();
                            let _ = app_weak.upgrade_in_event_loop(move |ui| {
                                trigger_toast(&ui, &format!("✓ Saved to {}", display_name));
                            });
                        }
                        Err(e) => {
                            let err_msg = format!("Failed to save: {}", e);
                            let _ = app_weak.upgrade_in_event_loop(move |ui| {
                                trigger_toast(&ui, &err_msg);
                            });
                        }
                    }
                }
            });
        });
    }

    // Set Annotation Sort Mode
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_set_annotation_sort_mode(move |sort_mode| {
            let sm = (sort_mode.clamp(0, 2)) as u8;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    s.annotation_sort_mode = sm;
                    history::update_app_config(|cfg| {
                        cfg.annotation_sort_mode = sm;
                    });
                    if let Some(ui) = app_weak.upgrade() {
                        sync_annotations_to_ui(s, &ui);
                    }
                }
            }
        });
    }

    // Copy to clipboard (Doc Info property value or arbitrary string)
    {
        let app_weak = app.as_weak();
        app.on_copy_to_clipboard(move |text| {
            let s = text.to_string();
            if s.is_empty() || s == "—" {
                return;
            }
            if let Some(ui) = app_weak.upgrade() {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(s)) {
                    Ok(_) => trigger_toast(&ui, "✓ Copied to clipboard"),
                    Err(e) => trigger_toast(&ui, &format!("Failed to copy: {}", e)),
                }
            }
        });
    }

    // Copy All Document Properties Markdown to Clipboard
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_copy_all_doc_info(move || {
            let md = {
                if let Ok(lock) = state_lock.lock() {
                    if let Some(s) = lock.as_ref() {
                        pdf::format_document_properties_markdown(&s.doc_properties)
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(md)) {
                    Ok(_) => trigger_toast(&ui, "✓ Copied document properties to clipboard"),
                    Err(e) => trigger_toast(&ui, &format!("Failed to copy: {}", e)),
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
                    s.active_selections.clear();
                    s.active_selection_line_rects.clear();
                    s.active_selection_snippet.clear();
                }
            }
            if let Some(ui) = app_weak.upgrade() {
                ui.set_has_selection(false);
                ui.set_show_popup(false);
                ui.set_show_note_input(false);
                ui.set_selected_annotation_id(-1);
                ui.set_selected_annotation_kind("".into());
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
                        if is_scroll {
                            s.is_dual_page = false;
                        }
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
                    ui.set_is_dual_page(false);
                    ui.invoke_scroll_to_page(target_page);
                } else {
                    update_view(&ui, &state_lock);
                }
                save_session();
            }
        });
    }

    // Dual Page Mode Toggle
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        let save_session = make_session_saver();
        app.on_set_dual_page(move |is_dual| {
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    s.is_dual_page = is_dual;
                    if is_dual {
                        s.is_scroll_view = false;
                    }
                }
            }

            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_dual_page(is_dual);
                if is_dual {
                    ui.set_is_scroll_view(false);
                }
                update_view(&ui, &state_lock);
                save_session();
            }
        });
    }

    // Rotate Clockwise
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_rotate_clockwise(move || {
            let (new_rot, current_page, is_scroll) = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        s.rotation = (s.rotation + 90) % 360;
                        s.cache.clear();
                        (s.rotation, s.current_page, s.is_scroll_view)
                    } else {
                        (0, 0, false)
                    }
                } else {
                    (0, 0, false)
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                ui.set_rotation(new_rot);
                update_view(&ui, &state_lock);
                if is_scroll {
                    ui.set_suppress_page_change(true);
                    ui.invoke_scroll_to_page(current_page);
                    ui.set_suppress_page_change(false);
                    if let Ok(mut lock) = state_lock.lock() {
                        if let Some(s) = lock.as_mut() {
                            trigger_scroll_page_render(s, current_page, &ui);
                        }
                    }
                }
            }
        });
    }

    // Cycle Paper Theme (Light -> Sepia -> Scientific Dark -> Light)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_cycle_paper_theme(move || {
            let (new_theme, current_page, is_scroll) = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        let next = match s.paper_theme {
                            pdf::PaperTheme::Light => pdf::PaperTheme::Sepia,
                            pdf::PaperTheme::Sepia => pdf::PaperTheme::ScientificDark,
                            pdf::PaperTheme::ScientificDark => pdf::PaperTheme::Light,
                            pdf::PaperTheme::HighContrastDark => pdf::PaperTheme::Light,
                        };
                        s.paper_theme = next;
                        (next, s.current_page, s.is_scroll_view)
                    } else {
                        (pdf::PaperTheme::Light, 0, false)
                    }
                } else {
                    (pdf::PaperTheme::Light, 0, false)
                }
            };

            if let Some(ui) = app_weak.upgrade() {
                ui.set_paper_theme(new_theme as i32);
                history::update_app_config(|c| c.paper_theme = new_theme as u8);
                update_view(&ui, &state_lock);
                if is_scroll {
                    if let Ok(mut lock) = state_lock.lock() {
                        if let Some(s) = lock.as_mut() {
                            trigger_scroll_page_render(s, current_page, &ui);
                        }
                    }
                }
                save_current_session(&state_lock, &ui);
            }
        });
    }

    // Set Paper Theme directly
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_set_paper_theme(move |theme_idx| {
            let (new_theme, changed, current_page, is_scroll) = {
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        let target = pdf::PaperTheme::from(theme_idx);
                        if s.paper_theme != target {
                            s.paper_theme = target;
                            (target, true, s.current_page, s.is_scroll_view)
                        } else {
                            (target, false, s.current_page, s.is_scroll_view)
                        }
                    } else {
                        (pdf::PaperTheme::Light, false, 0, false)
                    }
                } else {
                    (pdf::PaperTheme::Light, false, 0, false)
                }
            };

            if changed {
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_paper_theme(new_theme as i32);
                    history::update_app_config(|c| c.paper_theme = new_theme as u8);
                    update_view(&ui, &state_lock);
                    if is_scroll {
                        if let Ok(mut lock) = state_lock.lock() {
                            if let Some(s) = lock.as_mut() {
                                trigger_scroll_page_render(s, current_page, &ui);
                            }
                        }
                    }
                    save_current_session(&state_lock, &ui);
                }
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
                            update_page_indicator(s, &ui);
                            trigger_scroll_page_render(s, new_page, &ui);
                            sync_links_to_ui(s, &ui);
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
                        let step = if s.is_dual_page && !s.is_scroll_view { 2 } else { 1 };
                        if s.current_page > 0 {
                            s.current_page = (s.current_page - step).max(0);
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
                                update_page_indicator(s, &ui);
                                trigger_scroll_page_render(s, new_page, &ui);
                                sync_links_to_ui(s, &ui);
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
                        let step = if s.is_dual_page && !s.is_scroll_view { 2 } else { 1 };
                        if s.current_page + step < s.total_pages {
                            s.current_page += step;
                            is_scroll = s.is_scroll_view;
                            new_page = s.current_page;
                            true
                        } else if s.current_page < s.total_pages - 1 {
                            s.current_page = s.total_pages - 1;
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
                                update_page_indicator(s, &ui);
                                trigger_scroll_page_render(s, new_page, &ui);
                                sync_links_to_ui(s, &ui);
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

static LAST_ZOOM_INSTANT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn perform_zoom(
    ui: &AppWindow,
    state_lock: &Arc<Mutex<Option<AppState>>>,
    factor: f32,
    target_page: Option<i32>,
) {
    {
        if let Ok(mut last_zoom) = LAST_ZOOM_INSTANT.lock() {
            let now = std::time::Instant::now();
            if let Some(prev) = *last_zoom {
                if now.duration_since(prev).as_millis() < 40 {
                    return;
                }
            }
            *last_zoom = Some(now);
        }
    }

    ui.set_show_popup(false);
    ui.set_selected_annotation_id(-1);
    ui.set_selected_annotation_kind("".into());
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
        ui.set_suppress_page_change(false);
    } else {
        update_view(ui, state_lock);
    }

    let state_lock_clone = Arc::clone(state_lock);
    let ui_weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
        if let Ok(mut lock) = state_lock_clone.lock() {
            if let Some(s) = lock.as_mut() {
                s.is_zooming = false;
            }
        }
        if let Some(ui) = ui_weak.upgrade() {
            save_current_session(&state_lock_clone, &ui);
        }
    });
}

fn rgba_to_image(rgba: &[u8], w: u32, h: u32) -> Option<slint::Image> {
    let expected_len = (w as usize) * (h as usize) * 4;
    if w == 0 || h == 0 || rgba.len() < expected_len {
        return None;
    }

    let mut pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    {
        let slice = pixel_buffer.make_mut_slice();
        unsafe {
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), slice.as_mut_ptr() as *mut u8, expected_len);
        }
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
            page_text_cache: std::collections::HashMap::new(),
            sel_start_page: 0,
            sel_start_pt: (0.0, 0.0),
            active_selections: Vec::new(),
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
            collapsed_toc_ids: std::collections::HashSet::new(),
            page_links_cache: std::collections::HashMap::new(),
            has_logical_pages: false,
            page_labels: vec!["1".to_string()],
            rotation: 0,
            is_dual_page: false,
            paper_theme: pdf::PaperTheme::Light,
            doc_properties: pdf::DocumentProperties::default(),
            annotation_sort_mode: 0,
        };

        (state, test_pdf_path)
    }

    #[test]
    fn test_theme_toggle_and_defaults() {
        let app = AppWindow::new().unwrap();
        assert_eq!(app.get_dark_mode(), true);
        app.set_dark_mode(false);
        assert_eq!(app.get_dark_mode(), false);
        app.set_dark_mode(true);
        assert_eq!(app.get_dark_mode(), true);
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
            creation_date: String::new(),
            mod_date: String::new(),
            author: String::new(),
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
            creation_date: String::new(),
            mod_date: String::new(),
            author: String::new(),
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
            creation_date: String::new(),
            mod_date: String::new(),
            author: String::new(),
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
            creation_date: String::new(),
            mod_date: String::new(),
            author: String::new(),
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
                creation_date: String::new(),
                mod_date: String::new(),
                author: String::new(),
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
            creation_date: String::new(),
            mod_date: String::new(),
            author: String::new(),
        };
        state.push_undo(UndoAction::Created(new_annot));
        assert_eq!(state.redo_stack.len(), 0);

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_page_size_rotation_swaps_paper_canvas() {
        let (mut state, pdf_path) = setup_test_state();
        // Base size 500x700
        assert_eq!(state.get_page_size(0), (500.0, 700.0));

        // 90 deg -> Paper rotates to landscape: 700x500
        state.rotation = 90;
        assert_eq!(state.get_page_size(0), (700.0, 500.0));

        // 180 deg -> Paper returns to portrait: 500x700
        state.rotation = 180;
        assert_eq!(state.get_page_size(0), (500.0, 700.0));

        // 270 deg -> Paper rotates to landscape: 700x500
        state.rotation = 270;
        assert_eq!(state.get_page_size(0), (700.0, 500.0));

        // 360/0 deg -> 500x700
        state.rotation = 0;
        assert_eq!(state.get_page_size(0), (500.0, 700.0));

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_transform_rect_and_coordinate_roundtrip() {
        let (mut state, pdf_path) = setup_test_state();
        let (w, h) = state.get_unrotated_page_size(0); // 500 x 700
        let orig_rect = (50.0, 100.0, 150.0, 120.0);

        // At 0 deg: identical
        assert_eq!(transform_rect_for_rotation(orig_rect, w, h, 0), orig_rect);

        // At 90 deg: width becomes height, rect rotates
        state.rotation = 90;
        let rot_rect_90 = transform_rect_for_rotation(orig_rect, w, h, 90);
        // x = 700 - 120 = 580, y = 50, x1 = 700 - 100 = 600, y1 = 150
        assert_eq!(rot_rect_90, (580.0, 50.0, 600.0, 150.0));

        // Coordinate inverse: center of rot_rect_90 maps back to center of orig_rect
        let (cx_rot, cy_rot) = ((rot_rect_90.0 + rot_rect_90.2) / 2.0, (rot_rect_90.1 + rot_rect_90.3) / 2.0);
        let (un_x, un_y) = state.to_unrotated_pt(0, cx_rot, cy_rot);
        let (orig_cx, orig_cy) = ((orig_rect.0 + orig_rect.2) / 2.0, (orig_rect.1 + orig_rect.3) / 2.0);
        assert!((un_x - orig_cx).abs() < 0.001);
        assert!((un_y - orig_cy).abs() < 0.001);

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_parse_primary_monitor_scale() {
        let sample_xml = r#"<monitors version="2">
  <configuration>
    <logicalmonitor>
      <x>0</x>
      <y>0</y>
      <scale>1.25</scale>
      <primary>yes</primary>
      <monitor>
        <monitorspec>
          <connector>eDP-1</connector>
        </monitorspec>
      </monitor>
    </logicalmonitor>
  </configuration>
</monitors>"#;
        assert_eq!(super::parse_primary_monitor_scale(sample_xml), Some(1.25));

        // Multi-monitor scenario: second monitor is primary
        let multi_xml = r#"<monitors version="2">
  <configuration>
    <logicalmonitor>
      <x>0</x>
      <y>0</y>
      <scale>1.0</scale>
      <primary>no</primary>
    </logicalmonitor>
    <logicalmonitor>
      <x>1920</x>
      <y>0</y>
      <scale>1.5</scale>
      <primary>yes</primary>
    </logicalmonitor>
  </configuration>
</monitors>"#;
        assert_eq!(super::parse_primary_monitor_scale(multi_xml), Some(1.5));

        // Invalid XML or empty
        assert_eq!(super::parse_primary_monitor_scale(""), None);
    }

    #[test]
    fn test_compute_actual_render_zoom() {
        // Standard 100% zoom with 1.25 fractional scale
        let z = super::compute_actual_render_zoom(1.0, 1.25);
        let expected = (1.0 * (96.0 / 72.0) * 1.25f32).clamp(0.2, 8.0);
        assert!((z - expected).abs() < 0.001);

        // Clamping check: very small zoom
        let z_min = super::compute_actual_render_zoom(0.01, 1.0);
        assert_eq!(z_min, 0.2);

        // Clamping check: very large zoom
        let z_max = super::compute_actual_render_zoom(10.0, 2.0);
        assert_eq!(z_max, 8.0);
    }

    #[test]
    fn test_resolve_scroll_drag_position_across_pages() {
        let (mut state, pdf_path) = setup_test_state();
        state.is_scroll_view = true;
        state.total_pages = 3;
        state.page_sizes = vec![(500.0, 700.0), (500.0, 700.0), (500.0, 700.0)];
        state.zoom_level = 1.0;

        // 1. Dragging within page 0
        let (p, _x, y) = super::resolve_scroll_drag_position(&state, 0, 100.0, 250.0);
        assert_eq!(p, 0);
        assert!((y - 250.0).abs() < 0.01);

        // 2. Dragging down past page 0 (height 700 + gap 24) into page 1: local_y = 800.0
        // global_y = 12.0 + 800.0 = 812.0
        // page 1 card_top = (700 + 24) + 12 = 736.0
        // target page is 1, local_y = 812 - 736 = 76.0
        let (p, _x, y) = super::resolve_scroll_drag_position(&state, 0, 100.0, 800.0);
        assert_eq!(p, 1);
        assert!((y - 76.0).abs() < 0.01);

        // 3. Dragging further down into page 2: local_y = 1600.0 from page 0
        // global_y = 12 + 1600 = 1612
        // page 2 card_top = 2 * (700 + 24) + 12 = 1460
        // target page is 2, local_y = 1612 - 1460 = 152.0
        let (p, _x, y) = super::resolve_scroll_drag_position(&state, 0, 100.0, 1600.0);
        assert_eq!(p, 2);
        assert!((y - 152.0).abs() < 0.01);

        let _ = std::fs::remove_file(pdf_path);
    }

    #[test]
    fn test_highlight_color_consistency_between_create_and_update() {
        // Green highlight (index 1)
        let green_idx = 1;
        let [r, g, b] = super::get_highlight_blend_rgb(green_idx);
        let create_hex = format!("#{r:02x}{g:02x}{b:02x}");

        let is_highlight = true;
        let update_hex = if is_highlight {
            let [r, g, b] = super::get_highlight_blend_rgb(green_idx);
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            super::get_color(green_idx).0
        };

        assert_eq!(create_hex, "#bbf7d0");
        assert_eq!(update_hex, "#bbf7d0");
        assert_eq!(create_hex, update_hex);
    }

    #[test]
    fn test_rebuild_toc_model_collapse_expand() {
        // Structure:
        // 0: Chapter 1 (id 0, depth 0, has_children: true)
        // 1:   Section 1.1 (id 1, depth 1, has_children: true)
        // 2:     Sub 1.1.1 (id 2, depth 2, has_children: false)
        // 3:   Section 1.2 (id 3, depth 1, has_children: false)
        // 4: Chapter 2 (id 4, depth 0, has_children: false)
        let toc = vec![
            pdf::TocItem { id: 0, title: "Chapter 1".into(), page: 0, depth: 0, has_children: true },
            pdf::TocItem { id: 1, title: "Section 1.1".into(), page: 1, depth: 1, has_children: true },
            pdf::TocItem { id: 2, title: "Sub 1.1.1".into(), page: 2, depth: 2, has_children: false },
            pdf::TocItem { id: 3, title: "Section 1.2".into(), page: 3, depth: 1, has_children: false },
            pdf::TocItem { id: 4, title: "Chapter 2".into(), page: 4, depth: 0, has_children: false },
        ];

        // 1. By default all nodes with children start collapsed
        let mut collapsed: std::collections::HashSet<i32> = toc
            .iter()
            .filter(|item| item.has_children)
            .map(|item| item.id)
            .collect();

        let visible = super::compute_visible_toc_items(&toc, &collapsed, &[]);
        // Initially visible: Chapter 1, Chapter 2 (all sub-levels collapsed)
        assert_eq!(visible.len(), 2);
        let titles: Vec<String> = visible.iter().map(|item| item.title.to_string()).collect();
        assert_eq!(titles, vec!["Chapter 1", "Chapter 2"]);

        // 2. Expand Chapter 1 (id 0 removed from collapsed)
        collapsed.remove(&0);
        let visible = super::compute_visible_toc_items(&toc, &collapsed, &[]);
        // Section 1.1 is still collapsed, so: Chapter 1, Section 1.1, Section 1.2, Chapter 2
        assert_eq!(visible.len(), 4);
        let titles: Vec<String> = visible.iter().map(|item| item.title.to_string()).collect();
        assert_eq!(titles, vec!["Chapter 1", "Section 1.1", "Section 1.2", "Chapter 2"]);

        // 3. Expand Section 1.1 (id 1 removed)
        collapsed.remove(&1);
        let visible = super::compute_visible_toc_items(&toc, &collapsed, &[]);
        // All items now visible
        assert_eq!(visible.len(), 5);

        // 4. Re-collapse Section 1.1 (id 1)
        collapsed.insert(1);
        let visible = super::compute_visible_toc_items(&toc, &collapsed, &[]);
        assert_eq!(visible.len(), 4);

        // 5. Re-collapse Chapter 1 (id 0)
        collapsed.insert(0);
        let visible = super::compute_visible_toc_items(&toc, &collapsed, &[]);
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn test_sync_links_to_ui_rotation_and_zoom() {
        // Mock 1 link on page 0
        let links = vec![
            pdf::PageLink {
                bounds_pt: (10.0, 20.0, 110.0, 40.0), // width = 100, height = 20
                target_page: 5,
                uri: String::new(),
            }
        ];

        let unrot_w = 500.0;
        let unrot_h = 700.0;
        let zoom = 2.0;

        // Rotation = 0
        let transformed = super::compute_transformed_links(&links, 0, unrot_w, unrot_h, 0, zoom);
        assert_eq!(transformed.len(), 1);
        let link0 = &transformed[0];
        assert_eq!(link0.page, 0);
        assert_eq!(link0.target_page, 5);
        assert!((link0.x - 20.0).abs() < 0.01);
        assert!((link0.y - 40.0).abs() < 0.01);
        assert!((link0.width - 200.0).abs() < 0.01);
        assert!((link0.height - 40.0).abs() < 0.01);

        // Rotation = 90
        // page size is 500 x 700 (w x h)
        // at 90 deg: x = 700 - 40 = 660, y = 10, width = 20, height = 100
        let transformed_90 = super::compute_transformed_links(&links, 0, unrot_w, unrot_h, 90, zoom);
        assert_eq!(transformed_90.len(), 1);
        let link_rot = &transformed_90[0];
        assert!((link_rot.x - 660.0 * 2.0).abs() < 0.01);
        assert!((link_rot.y - 10.0 * 2.0).abs() < 0.01);
        assert!((link_rot.width - 40.0).abs() < 0.01);
        assert!((link_rot.height - 200.0).abs() < 0.01);
    }

    #[test]
    fn test_resolve_page_number_logical_and_paper() {
        // Mock 5-page book with logical labels:
        // Page 0: "Cover"
        // Page 1: "i"
        // Page 2: "ii"
        // Page 3: "1"
        // Page 4: "2"
        let labels = vec![
            "Cover".to_string(),
            "i".to_string(),
            "ii".to_string(),
            "1".to_string(),
            "2".to_string(),
        ];
        let total = 5;

        // 1. Exact logical labels
        assert_eq!(super::resolve_page_number("Cover", total, true, &labels), Some(0));
        assert_eq!(super::resolve_page_number("cover", total, true, &labels), Some(0));
        assert_eq!(super::resolve_page_number("i", total, true, &labels), Some(1));
        assert_eq!(super::resolve_page_number("I", total, true, &labels), Some(1));
        assert_eq!(super::resolve_page_number("ii", total, true, &labels), Some(2));
        assert_eq!(super::resolve_page_number("1", total, true, &labels), Some(3));
        assert_eq!(super::resolve_page_number("2", total, true, &labels), Some(4));
        assert_eq!(super::resolve_page_number("99", total, true, &labels), None);

        // 2. Explicit sheet prefixes (#, p, p.) jump directly to physical sheet
        assert_eq!(super::resolve_page_number("#1", total, true, &labels), Some(0));
        assert_eq!(super::resolve_page_number("#4", total, true, &labels), Some(3));
        assert_eq!(super::resolve_page_number("p1", total, true, &labels), Some(0));
        assert_eq!(super::resolve_page_number("p.4", total, true, &labels), Some(3));

        // 3. Document without logical labels
        assert_eq!(super::resolve_page_number("1", 10, false, &[]), Some(0));
        assert_eq!(super::resolve_page_number("10", 10, false, &[]), Some(9));
        assert_eq!(super::resolve_page_number("0", 10, false, &[]), None);
        assert_eq!(super::resolve_page_number("11", 10, false, &[]), None);
        assert_eq!(super::resolve_page_number("abc", 10, false, &[]), None);
    }

    #[test]
    fn test_settings_survival_across_file_changes_and_dialog_opens() {
        let temp_dir = std::env::temp_dir();
        let test_config_path = temp_dir.join("folio_test_settings_survival.json");
        let _ = std::fs::remove_file(&test_config_path);

        // 1. Verify that custom settings update and survive in config
        crate::history::save_app_config_to(
            &crate::history::AppConfig {
                dark_mode: false,
                paper_theme: 2, // Scientific Dark
                annotation_sort_mode: 1,
            },
            &test_config_path,
        );

        let loaded = crate::history::load_app_config_from(&test_config_path);
        assert_eq!(loaded.dark_mode, false);
        assert_eq!(loaded.paper_theme, 2);
        assert_eq!(loaded.annotation_sort_mode, 1);

        // 2. Cold start: active UI theme is not yet set (-1 or unset) -> resolves to config theme (Scientific Dark = 2)
        let cold_start_theme = super::resolve_initial_paper_theme(-1, loaded.paper_theme);
        assert_eq!(cold_start_theme, pdf::PaperTheme::ScientificDark);

        // 3. User switches to Sepia (1), then opens another file via dialog or recent books -> resolves to active theme (Sepia = 1)
        let dialog_open_theme = super::resolve_initial_paper_theme(1, loaded.paper_theme);
        assert_eq!(dialog_open_theme, pdf::PaperTheme::Sepia);

        // 4. User switches to Scientific Dark (2), then opens a recent book -> resolves to active theme (Scientific Dark = 2)
        let recent_change_theme = super::resolve_initial_paper_theme(2, loaded.paper_theme);
        assert_eq!(recent_change_theme, pdf::PaperTheme::ScientificDark);

        // 5. Update config in place and verify persistence
        crate::history::update_app_config_at(&test_config_path, |c| {
            c.dark_mode = true;
            c.paper_theme = 1;
            c.annotation_sort_mode = 2;
        });

        let updated = crate::history::load_app_config_from(&test_config_path);
        assert_eq!(updated.dark_mode, true);
        assert_eq!(updated.paper_theme, 1);
        assert_eq!(updated.annotation_sort_mode, 2);

        let _ = std::fs::remove_file(&test_config_path);
    }

    #[test]
    fn test_sidebar_annotation_sorting_and_grouping() {
        let toc = vec![
            pdf::TocItem {
                id: 1,
                title: "Chapter 1".to_string(),
                page: 0,
                depth: 0,
                has_children: false,
            },
            pdf::TocItem {
                id: 2,
                title: "Chapter 2".to_string(),
                page: 2,
                depth: 0,
                has_children: false,
            },
        ];

        let annots = vec![
            AnnotationRecord {
                id: 1,
                page: 0,
                kind: "highlight".to_string(),
                color_index: 0,
                color_hex: "#facc15".to_string(),
                snippet: "Snippet 1".to_string(),
                comment: "".to_string(),
                line_rects_pt: vec![(10.0, 100.0, 100.0, 120.0)],
                is_from_pdf: false,
                creation_date: "D:20261031100000Z".to_string(),
                mod_date: "".to_string(),
                author: "User".to_string(),
            },
            AnnotationRecord {
                id: 2,
                page: 0,
                kind: "highlight".to_string(),
                color_index: 0,
                color_hex: "#facc15".to_string(),
                snippet: "Snippet 2".to_string(),
                comment: "".to_string(),
                line_rects_pt: vec![(10.0, 200.0, 100.0, 220.0)],
                is_from_pdf: false,
                creation_date: "D:20261031120000Z".to_string(),
                mod_date: "".to_string(),
                author: "User".to_string(),
            },
            AnnotationRecord {
                id: 3,
                page: 2,
                kind: "highlight".to_string(),
                color_index: 0,
                color_hex: "#facc15".to_string(),
                snippet: "Snippet 3".to_string(),
                comment: "".to_string(),
                line_rects_pt: vec![(10.0, 50.0, 100.0, 70.0)],
                is_from_pdf: false,
                creation_date: "D:20261031080000Z".to_string(),
                mod_date: "".to_string(),
                author: "User".to_string(),
            },
        ];

        // 1. Mode 0: Document Order (groups by outline chapter / page, sorted page -> y -> id)
        let items = super::build_sidebar_annotation_items(&annots, false, &toc, 0);
        let ids: Vec<i32> = items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(items.len(), 3);
        assert!(items[0].group_header.contains("Chapter 1"));
        assert_eq!(items[1].group_header.as_str(), "");
        assert!(items[2].group_header.contains("Chapter 2"));

        // 2. Mode 1: Newest First (flat list sorted by creation/mod date descending)
        let items = super::build_sidebar_annotation_items(&annots, false, &toc, 1);
        let ids: Vec<i32> = items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![2, 1, 3]);
        assert!(items.iter().all(|i| i.group_header.as_str().is_empty()));

        // 3. Mode 2: Oldest First (flat list sorted by creation/mod date ascending)
        let items = super::build_sidebar_annotation_items(&annots, false, &toc, 2);
        let ids: Vec<i32> = items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![3, 1, 2]);
        assert!(items.iter().all(|i| i.group_header.as_str().is_empty()));
    }

    #[test]
    fn test_six_highlight_colors_and_presets() {
        assert_eq!(super::STATIC_COLORS.len(), 6);
        assert_eq!(super::HIGHLIGHT_BLEND_COLORS.len(), 6);

        // Indices: 0=Yellow, 1=Green, 2=Blue, 3=Pink, 4=Orange, 5=Purple
        let expected_static_hex = [
            "#facc15", "#4ade80", "#60a5fa", "#f472b6", "#fb923c", "#c084fc",
        ];
        let expected_blend_rgb = [
            [254, 240, 138], // Yellow
            [187, 247, 208], // Green
            [191, 219, 254], // Blue
            [251, 207, 232], // Pink
            [254, 215, 170], // Orange
            [233, 213, 255], // Purple
        ];

        for i in 0..6 {
            let (hex, _) = super::get_color(i);
            assert_eq!(hex, expected_static_hex[i as usize]);
            let rgb = super::get_highlight_blend_rgb(i);
            assert_eq!(rgb, expected_blend_rgb[i as usize]);
        }

        // Test purple index matching
        let purple_vibrant_rgb = (192.0 / 255.0, 132.0 / 255.0, 252.0 / 255.0);
        assert_eq!(super::match_closest_color_index(purple_vibrant_rgb), 5);

        let purple_pastel_rgb = (233.0 / 255.0, 213.0 / 255.0, 255.0 / 255.0);
        assert_eq!(super::match_closest_color_index(purple_pastel_rgb), 5);
    }
}

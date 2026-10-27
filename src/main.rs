mod pdf;

use pdf::{CachedRender, PageCache, PdfWorker, RenderRequest, RenderedPage};
use slint::Model;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

slint::include_modules!();

struct AppState {
    #[allow(dead_code)]
    file_path: String,
    current_page: i32, // 0-indexed
    total_pages: i32,
    zoom_level: f32, // 2.0 = 200% default
    is_scroll_view: bool,
    page_width_pt: f32,
    page_height_pt: f32,
    cache: PageCache,
    worker: PdfWorker,
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

    let card_w = meta.initial_width_pt * default_zoom;
    let card_h = meta.initial_height_pt * default_zoom;
    let scroll_item_h = card_h + 24.0;

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
    // Auto-open sidebar if document contains an outline
    app.set_show_sidebar(has_toc);

    // Initialize the scroll-pages model for continuous scroll view
    let mut items = Vec::with_capacity(meta.page_count as usize);
    for i in 0..meta.page_count {
        items.push(PageItemData {
            page_index: i,
            width: card_w,
            height: card_h,
            image: slint::Image::default(),
        });
    }
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
        cache: PageCache::new(16),
        worker,
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

    // Update displayed page dimensions for single page view
    let card_w = state.page_width_pt * state.zoom_level;
    let card_h = state.page_height_pt * state.zoom_level;
    ui.set_page_width(card_w);
    ui.set_page_height(card_h);

    // Update dimensions in scroll-pages model for continuous scroll view
    let scroll_item_h = card_h + 24.0;
    ui.set_scroll_item_height(scroll_item_h);

    let pages_model = ui.get_scroll_pages();
    for i in 0..state.total_pages as usize {
        if let Some(mut item) = pages_model.row_data(i) {
            item.width = card_w;
            item.height = card_h;
            if let Some(cached) = state.cache.get(i as i32, state.zoom_level) {
                if let Some(img) = rgba_to_image(&cached.rgba, cached.width, cached.height) {
                    item.image = img;
                }
            }
            pages_model.set_row_data(i, item);
        }
    }

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
    state.worker.request_render(RenderRequest {
        page: state.current_page,
        zoom: preview_zoom,
        generation: next_gen,
        is_preview: true,
    });

    // Step 2: Full-res render for current page
    state.worker.request_render(RenderRequest {
        page: state.current_page,
        zoom: actual_render_zoom,
        generation: next_gen,
        is_preview: false,
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
                state.worker.request_render(RenderRequest {
                    page: p,
                    zoom,
                    generation,
                    is_preview: false,
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
    state.worker.request_render(RenderRequest {
        page: page_idx,
        zoom: preview_zoom,
        generation: next_gen,
        is_preview: true,
    });

    state.worker.request_render(RenderRequest {
        page: page_idx,
        zoom: actual_render_zoom,
        generation: next_gen,
        is_preview: false,
    });

    prefetch_neighbors(state, next_gen, actual_render_zoom);
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

    // Jump to Page (from Table of Contents)
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
            let mut is_scroll = false;
            let mut cur_page = 0;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    s.zoom_level = (s.zoom_level * 1.2).min(5.0);
                    is_scroll = s.is_scroll_view;
                    cur_page = s.current_page;
                }
            }
            if let Some(ui) = app_weak.upgrade() {
                update_view(&ui, &state_lock);
                if is_scroll {
                    ui.invoke_scroll_to_page(cur_page);
                }
            }
        });
    }

    // Zoom Out
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_out(move || {
            let mut is_scroll = false;
            let mut cur_page = 0;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    s.zoom_level = (s.zoom_level / 1.2).max(0.2);
                    is_scroll = s.is_scroll_view;
                    cur_page = s.current_page;
                }
            }
            if let Some(ui) = app_weak.upgrade() {
                update_view(&ui, &state_lock);
                if is_scroll {
                    ui.invoke_scroll_to_page(cur_page);
                }
            }
        });
    }

    // Zoom Reset (200% default)
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_reset(move || {
            let mut is_scroll = false;
            let mut cur_page = 0;
            if let Ok(mut lock) = state_lock.lock() {
                if let Some(s) = lock.as_mut() {
                    s.zoom_level = 2.0;
                    is_scroll = s.is_scroll_view;
                    cur_page = s.current_page;
                }
            }
            if let Some(ui) = app_weak.upgrade() {
                update_view(&ui, &state_lock);
                if is_scroll {
                    ui.invoke_scroll_to_page(cur_page);
                }
            }
        });
    }

    // Zoom Fit
    {
        let state_lock = Arc::clone(state_lock);
        let app_weak = app.as_weak();
        app.on_zoom_fit(move || {
            if let Some(ui) = app_weak.upgrade() {
                let phys_size = ui.window().size();
                let scale_factor = ui.window().scale_factor().max(1.0);
                let sidebar_w = if ui.get_show_sidebar() { 280.0 } else { 0.0 };
                let avail_w = (phys_size.width as f32 / scale_factor) - 80.0 - sidebar_w;
                let avail_h = (phys_size.height as f32 / scale_factor) - 120.0;

                let mut is_scroll = false;
                let mut cur_page = 0;
                if let Ok(mut lock) = state_lock.lock() {
                    if let Some(s) = lock.as_mut() {
                        let fit_w = avail_w / s.page_width_pt.max(1.0);
                        let fit_h = avail_h / s.page_height_pt.max(1.0);
                        s.zoom_level = fit_w.min(fit_h).clamp(0.2, 5.0);
                        is_scroll = s.is_scroll_view;
                        cur_page = s.current_page;
                    }
                }
                update_view(&ui, &state_lock);
                if is_scroll {
                    ui.invoke_scroll_to_page(cur_page);
                }
            }
        });
    }
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

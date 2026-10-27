use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[derive(Debug, Clone)]
pub struct HighlightOverlay {
    pub rgb: [u8; 3],
    pub rects_pt: Vec<(f32, f32, f32, f32)>,
}

#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub page: i32,
    pub zoom: f32,
    pub generation: u64,
    pub is_preview: bool,
    pub highlights: Vec<HighlightOverlay>,
}

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub page: i32,
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub generation: u64,
    pub is_preview: bool,
}

#[derive(Debug, Clone)]
pub enum WorkerMessage {
    Render(RenderRequest),
    #[allow(dead_code)]
    Reload,
    SwitchDocument(String),
}

pub struct PdfWorker {
    sender: Sender<WorkerMessage>,
    current_generation: Arc<AtomicU64>,
}

impl PdfWorker {
    pub fn spawn<F>(file_path: String, on_rendered: F) -> Result<Self, String>
    where
        F: Fn(RenderedPage) + Send + Sync + 'static,
    {
        let (sender, receiver): (Sender<WorkerMessage>, Receiver<WorkerMessage>) =
            crossbeam_channel::unbounded();
        let current_generation = Arc::new(AtomicU64::new(0));
        let worker_gen = Arc::clone(&current_generation);
        let callback = Arc::new(on_rendered);

        thread::Builder::new()
            .name("pdf-worker".to_string())
            .spawn(move || {
                worker_loop(file_path, receiver, worker_gen, callback);
            })
            .map_err(|e| format!("Failed to spawn worker thread: {e}"))?;

        Ok(Self {
            sender,
            current_generation,
        })
    }

    pub fn request_render(&self, req: RenderRequest) {
        let _ = self.sender.send(WorkerMessage::Render(req));
    }

    pub fn switch_document(&self, new_path: String) {
        let _ = self.sender.send(WorkerMessage::SwitchDocument(new_path));
    }

    #[allow(dead_code)]
    pub fn reload_document(&self) {
        let _ = self.sender.send(WorkerMessage::Reload);
    }

    pub fn set_current_generation(&self, generation: u64) {
        self.current_generation.store(generation, Ordering::Relaxed);
    }

    pub fn current_generation(&self) -> u64 {
        self.current_generation.load(Ordering::Relaxed)
    }
}

fn worker_loop(
    file_path: String,
    receiver: Receiver<WorkerMessage>,
    current_generation: Arc<AtomicU64>,
    on_rendered: Arc<dyn Fn(RenderedPage) + Send + Sync + 'static>,
) {
    let mut file_path = file_path;
    let mut doc = match mupdf::Document::open(&file_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("PDF worker failed to open {file_path}: {e}");
            return;
        }
    };

    while let Ok(msg) = receiver.recv() {
        match msg {
            WorkerMessage::SwitchDocument(new_path) => {
                if let Ok(new_doc) = mupdf::Document::open(&new_path) {
                    doc = new_doc;
                    file_path = new_path;
                }
            }
            WorkerMessage::Reload => {
                if let Ok(new_doc) = mupdf::Document::open(&file_path) {
                    doc = new_doc;
                }
            }
            WorkerMessage::Render(req) => {
                // Fast-path drop if request generation is already superseded
                if req.generation < current_generation.load(Ordering::Relaxed) {
                    continue;
                }

                match render_page(&doc, req.page, req.zoom, &req.highlights) {
                    Ok((rgba, w, h)) => {
                        if req.generation >= current_generation.load(Ordering::Relaxed) {
                            on_rendered(RenderedPage {
                                page: req.page,
                                rgba,
                                w,
                                h,
                                generation: req.generation,
                                is_preview: req.is_preview,
                            });
                        }
                    }
                    Err(e) => {
                        eprintln!("PDF worker error rendering page {}: {e}", req.page);
                    }
                }
            }
        }
    }
}

fn render_page(
    doc: &mupdf::Document,
    page_no: i32,
    zoom: f32,
    highlights: &[HighlightOverlay],
) -> Result<(Vec<u8>, u32, u32), String> {
    let page = doc
        .load_page(page_no)
        .map_err(|e| format!("Failed to load page {page_no}: {e}"))?;

    let matrix = mupdf::Matrix::new_scale(zoom, zoom);
    // Render with alpha=false onto white paper canvas, show_extras=false so Folio manages all annotations uniformly
    let pixmap = page
        .to_pixmap(&matrix, &mupdf::Colorspace::device_rgb(), false, false)
        .map_err(|e| format!("Failed to rasterize page {page_no}: {e}"))?;

    let w = pixmap.width();
    let h = pixmap.height();
    if w == 0 || h == 0 {
        return Err("Rasterized pixmap has zero dimension".to_string());
    }

    let stride = pixmap.stride() as usize;
    let samples = pixmap.samples();
    let row_len = (w as usize) * 3;

    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let row_start = y * stride;
        if row_start + row_len <= samples.len() {
            let row = &samples[row_start..row_start + row_len];
            for chunk in row.chunks_exact(3) {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
        } else {
            return Err("Pixmap row exceeds sample buffer".to_string());
        }
    }

    // Apply optical Multiply blending for highlights:
    // White paper (255,255,255) becomes the highlight color.
    // Text pixels (0,0,0) remain 100% pure black (0 * color / 255 = 0), never discolored or washed out!
    // A pixel coverage mask ensures overlapping line boundaries are NEVER multiplied twice.
    let mut applied = vec![false; (w * h) as usize];

    for h_over in highlights {
        let hr = h_over.rgb[0] as u16;
        let hg = h_over.rgb[1] as u16;
        let hb = h_over.rgb[2] as u16;

        for rect in &h_over.rects_pt {
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
                        rgba[idx] = ((rgba[idx] as u16 * hr) / 255) as u8;
                        rgba[idx + 1] = ((rgba[idx + 1] as u16 * hg) / 255) as u8;
                        rgba[idx + 2] = ((rgba[idx + 2] as u16 * hb) / 255) as u8;
                    }
                }
            }
        }
    }

    Ok((rgba, w, h))
}

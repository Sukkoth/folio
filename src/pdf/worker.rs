use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub page: i32,
    pub zoom: f32,
    pub generation: u64,
    pub is_preview: bool,
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

pub struct PdfWorker {
    sender: Sender<RenderRequest>,
    current_generation: Arc<AtomicU64>,
}

impl PdfWorker {
    pub fn spawn<F>(file_path: String, on_rendered: F) -> Result<Self, String>
    where
        F: Fn(RenderedPage) + Send + Sync + 'static,
    {
        let (sender, receiver): (Sender<RenderRequest>, Receiver<RenderRequest>) =
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
        let _ = self.sender.send(req);
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
    receiver: Receiver<RenderRequest>,
    current_generation: Arc<AtomicU64>,
    on_rendered: Arc<dyn Fn(RenderedPage) + Send + Sync + 'static>,
) {
    let doc = match mupdf::Document::open(&file_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("PDF worker failed to open {file_path}: {e}");
            return;
        }
    };

    while let Ok(req) = receiver.recv() {
        // Fast-path drop if request generation is already superseded
        if req.generation < current_generation.load(Ordering::Relaxed) {
            continue;
        }

        match render_page(&doc, req.page, req.zoom) {
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

fn render_page(
    doc: &mupdf::Document,
    page_no: i32,
    zoom: f32,
) -> Result<(Vec<u8>, u32, u32), String> {
    let page = doc
        .load_page(page_no)
        .map_err(|e| format!("Failed to load page {page_no}: {e}"))?;

    let matrix = mupdf::Matrix::new_scale(zoom, zoom);
    // Render with alpha=false so page content is rendered onto an opaque white paper canvas
    let pixmap = page
        .to_pixmap(&matrix, &mupdf::Colorspace::device_rgb(), false, true)
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

    Ok((rgba, w, h))
}

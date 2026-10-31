use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum PaperTheme {
    #[default]
    Light = 0,
    Sepia = 1,
    ScientificDark = 2,
    HighContrastDark = 3,
}

impl From<i32> for PaperTheme {
    fn from(v: i32) -> Self {
        match v {
            1 => PaperTheme::Sepia,
            2 => PaperTheme::ScientificDark,
            3 => PaperTheme::HighContrastDark,
            _ => PaperTheme::Light,
        }
    }
}

impl From<PaperTheme> for i32 {
    fn from(t: PaperTheme) -> Self {
        t as i32
    }
}

#[derive(Debug, Clone)]
pub struct HighlightOverlay {
    pub rgb: [u8; 3],
    pub rects_pt: Vec<(f32, f32, f32, f32)>,
}

#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub page: i32,
    pub zoom: f32,
    pub rotation: i32,
    pub generation: u64,
    pub is_preview: bool,
    pub highlights: Vec<HighlightOverlay>,
    pub theme: PaperTheme,
}

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub page: i32,
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub rotation: i32,
    pub generation: u64,
    pub is_preview: bool,
    pub theme: PaperTheme,
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

                match render_page(&doc, req.page, req.zoom, req.rotation, &req.highlights, req.theme) {
                    Ok((rgba, w, h)) => {
                        if req.generation >= current_generation.load(Ordering::Relaxed) {
                            on_rendered(RenderedPage {
                                page: req.page,
                                rgba,
                                w,
                                h,
                                rotation: req.rotation,
                                generation: req.generation,
                                is_preview: req.is_preview,
                                theme: req.theme,
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

pub fn transform_rect_to_pixels(
    rect: (f32, f32, f32, f32),
    pw_pt: f32,
    ph_pt: f32,
    rotation: i32,
    zoom: f32,
    w: usize,
    h: usize,
) -> (usize, usize, usize, usize) {
    let (rx0, ry0, rx1, ry1) = match rotation.rem_euclid(360) {
        90 => (ph_pt - rect.3, rect.0, ph_pt - rect.1, rect.2),
        180 => (pw_pt - rect.2, ph_pt - rect.3, pw_pt - rect.0, ph_pt - rect.1),
        270 => (rect.1, pw_pt - rect.2, rect.3, pw_pt - rect.0),
        _ => (rect.0, rect.1, rect.2, rect.3),
    };
    let min_x = rx0.min(rx1);
    let max_x = rx0.max(rx1);
    let min_y = ry0.min(ry1);
    let max_y = ry0.max(ry1);

    let px0 = ((min_x * zoom).floor().max(0.0) as usize).min(w);
    let py0 = ((min_y * zoom).floor().max(0.0) as usize).min(h);
    let px1 = ((max_x * zoom).ceil().max(0.0) as usize).min(w);
    let py1 = ((max_y * zoom).ceil().max(0.0) as usize).min(h);

    (px0, py0, px1, py1)
}

pub fn apply_highlights_to_rgba(
    rgba: &mut [u8],
    w: usize,
    h: usize,
    highlights: &[HighlightOverlay],
    theme: PaperTheme,
    pw_pt: f32,
    ph_pt: f32,
    rotation: i32,
    zoom: f32,
) {
    if highlights.is_empty() {
        return;
    }

    let mut applied = vec![false; w * h];

    for h_over in highlights {
        let hr = h_over.rgb[0];
        let hg = h_over.rgb[1];
        let hb = h_over.rgb[2];

        for rect in &h_over.rects_pt {
            let (px0, py0, px1, py1) = transform_rect_to_pixels(*rect, pw_pt, ph_pt, rotation, zoom, w, h);

            for y in py0..py1 {
                let row_offset = y * w;
                for x in px0..px1 {
                    let p_idx = row_offset + x;
                    if !applied[p_idx] {
                        applied[p_idx] = true;
                        let idx = p_idx * 4;
                        if idx + 2 < rgba.len() {
                            match theme {
                                PaperTheme::Light => {
                                    rgba[idx] = ((rgba[idx] as u16 * hr as u16) / 255) as u8;
                                    rgba[idx + 1] = ((rgba[idx + 1] as u16 * hg as u16) / 255) as u8;
                                    rgba[idx + 2] = ((rgba[idx + 2] as u16 * hb as u16) / 255) as u8;
                                }
                                PaperTheme::Sepia => {
                                    rgba[idx] = ((rgba[idx] as u16 * hr as u16) / 255) as u8;
                                    rgba[idx + 1] = ((rgba[idx + 1] as u16 * hg as u16) / 255) as u8;
                                    rgba[idx + 2] = ((rgba[idx + 2] as u16 * hb as u16) / 255) as u8;
                                }
                                PaperTheme::ScientificDark | PaperTheme::HighContrastDark => {
                                    let r = rgba[idx] as f32;
                                    let g = rgba[idx + 1] as f32;
                                    let b = rgba[idx + 2] as f32;
                                    let lum = (54.0 * r + 182.0 * g + 19.0 * b) / 255.0;

                                    // Smoothly separate dark background paper from light text:
                                    // t_text = 0.0 for dark paper background, 1.0 for light text
                                    let t_text = ((lum - 40.0) / 100.0).clamp(0.0, 1.0);

                                    // For background: blend highlight color with ~42% alpha over dark slate
                                    let bg_r = r * 0.58 + (hr as f32) * 0.42;
                                    let bg_g = g * 0.58 + (hg as f32) * 0.42;
                                    let bg_b = b * 0.58 + (hb as f32) * 0.42;

                                    // For text: preserve brightness with a soft highlight tint
                                    let txt_r = (r * 0.86 + (hr as f32) * 0.14).min(255.0);
                                    let txt_g = (g * 0.86 + (hg as f32) * 0.14).min(255.0);
                                    let txt_b = (b * 0.86 + (hb as f32) * 0.14).min(255.0);

                                    rgba[idx] = (bg_r + t_text * (txt_r - bg_r)).round() as u8;
                                    rgba[idx + 1] = (bg_g + t_text * (txt_g - bg_g)).round() as u8;
                                    rgba[idx + 2] = (bg_b + t_text * (txt_b - bg_b)).round() as u8;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn render_page(
    doc: &mupdf::Document,
    page_no: i32,
    zoom: f32,
    rotation: i32,
    highlights: &[HighlightOverlay],
    theme: PaperTheme,
) -> Result<(Vec<u8>, u32, u32), String> {
    let page = doc
        .load_page(page_no)
        .map_err(|e| format!("Failed to load page {page_no}: {e}"))?;

    let bounds = page.bounds().unwrap_or(mupdf::Rect { x0: 0.0, y0: 0.0, x1: 595.0, y1: 842.0 });
    let pw_pt = (bounds.x1 - bounds.x0).abs();
    let ph_pt = (bounds.y1 - bounds.y0).abs();

    // In dark themes, extract image/figure blocks to protect them from negative inversion
    let mut image_rects_pt = Vec::new();
    if theme == PaperTheme::ScientificDark || theme == PaperTheme::HighContrastDark {
        if let Ok(tp) = page.to_text_page(mupdf::TextPageFlags::ACCURATE_BBOXES) {
            for block in tp.blocks() {
                if block.r#type() == mupdf::text_page::TextBlockType::Image {
                    let b = block.bounds();
                    image_rects_pt.push((b.x0, b.y0, b.x1, b.y1));
                }
            }
        }
    }

    let mut matrix = mupdf::Matrix::new_scale(zoom, zoom);
    if rotation != 0 {
        matrix.rotate(rotation as f32);
    }

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

    let image_rects_px: Vec<(usize, usize, usize, usize)> = image_rects_pt
        .into_iter()
        .map(|r| transform_rect_to_pixels(r, pw_pt, ph_pt, rotation, zoom, w as usize, h as usize))
        .collect();

    // 1. First, apply paper theme to base PDF canvas (paper, ink, diagrams)
    apply_paper_theme(&mut rgba, w as usize, h as usize, theme, &image_rects_px);

    // 2. Second, apply highlights onto themed paper with theme-aware blending
    // Highlights are NEVER subjected to negative inversion or double-sepia filtering!
    apply_highlights_to_rgba(
        &mut rgba,
        w as usize,
        h as usize,
        highlights,
        theme,
        pw_pt,
        ph_pt,
        rotation,
        zoom,
    );

    Ok((rgba, w, h))
}

pub fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;

    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;

    let l = (max + min) / 2.0;

    if delta < 1e-4 {
        return (0.0, 0.0, l);
    }

    let s = if l < 0.5 {
        delta / (max + min)
    } else {
        delta / (2.0 - max - min)
    };

    let mut h = if (max - rf).abs() < 1e-4 {
        ((gf - bf) / delta).rem_euclid(6.0)
    } else if (max - gf).abs() < 1e-4 {
        (bf - rf) / delta + 2.0
    } else {
        (rf - gf) / delta + 4.0
    };
    h *= 60.0;
    if h < 0.0 {
        h += 360.0;
    }

    (h, s, l)
}

pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s < 1e-4 {
        let v = (l * 255.0).round().clamp(0.0, 255.0) as u8;
        return (v, v, v);
    }

    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
    let m = l - c / 2.0;

    let (r1, g1, b1) = if (0.0..1.0).contains(&h_prime) {
        (c, x, 0.0)
    } else if (1.0..2.0).contains(&h_prime) {
        (x, c, 0.0)
    } else if (2.0..3.0).contains(&h_prime) {
        (0.0, c, x)
    } else if (3.0..4.0).contains(&h_prime) {
        (0.0, x, c)
    } else if (4.0..5.0).contains(&h_prime) {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    (
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// Applies scientific paper themes directly to the raster RGBA buffer.
/// - Light: Unmodified original rendering.
/// - Sepia: Warm cream paper (#F4ECD8), soft umber text (#382E25), blue light attenuation.
/// - ScientificDark: Perceptual luminance tone mapping (#18181A background, #D6D4CD text),
///   preserving hue of colored highlights/links/plots, and preserving photographic images.
/// - HighContrastDark: Pure black/white inversion (#000000 background, #FFFFFF text).
pub fn apply_paper_theme(
    rgba: &mut [u8],
    w: usize,
    h: usize,
    theme: PaperTheme,
    image_rects: &[(usize, usize, usize, usize)],
) {
    if theme == PaperTheme::Light {
        return;
    }

    let total_pixels = w * h;
    if rgba.len() < total_pixels * 4 {
        return;
    }

    // Build image mask if any image blocks are present
    let image_mask = if !image_rects.is_empty() {
        let mut mask = vec![false; total_pixels];
        for &(px0, py0, px1, py1) in image_rects {
            let y_end = py1.min(h);
            let x_end = px1.min(w);
            for y in py0..y_end {
                let row_start = y * w;
                for x in px0..x_end {
                    mask[row_start + x] = true;
                }
            }
        }
        Some(mask)
    } else {
        None
    };

    // Precompute 256-entry lookup tables for neutral pixels (chroma <= 12)
    let mut lut_r = [0u8; 256];
    let mut lut_g = [0u8; 256];
    let mut lut_b = [0u8; 256];

    match theme {
        PaperTheme::Sepia => {
            // Paper: (244, 236, 216), Ink: (56, 46, 37)
            for y in 0..256 {
                let ny = y as f32 / 255.0;
                lut_r[y] = (56.0 + ny * (244.0 - 56.0)).round() as u8;
                lut_g[y] = (46.0 + ny * (236.0 - 46.0)).round() as u8;
                lut_b[y] = (37.0 + ny * (216.0 - 37.0)).round() as u8;
            }
        }
        PaperTheme::ScientificDark => {
            // Slate: (24, 24, 26), Soft stone ink: (214, 212, 205)
            // Original white (y=255) maps to slate (24, 24, 26)
            // Original black (y=0) maps to stone (214, 212, 205)
            for y in 0..256 {
                let y_inv = 1.0 - (y as f32 / 255.0);
                lut_r[y] = (24.0 + y_inv * (214.0 - 24.0)).round() as u8;
                lut_g[y] = (24.0 + y_inv * (212.0 - 24.0)).round() as u8;
                lut_b[y] = (26.0 + y_inv * (205.0 - 26.0)).round() as u8;
            }
        }
        PaperTheme::HighContrastDark => {
            for y in 0..256 {
                let inv = (255 - y) as u8;
                lut_r[y] = inv;
                lut_g[y] = inv;
                lut_b[y] = inv;
            }
        }
        PaperTheme::Light => unreachable!(),
    }

    for p_idx in 0..total_pixels {
        // If this pixel is part of a photo/figure, protect it in dark themes
        if let Some(ref mask) = image_mask {
            if mask[p_idx] {
                if theme == PaperTheme::ScientificDark || theme == PaperTheme::HighContrastDark {
                    let idx = p_idx * 4;
                    // Slightly dampen 100% white glare in figures by 8% without inverting photographic colors
                    rgba[idx] = ((rgba[idx] as u16 * 235) / 255) as u8;
                    rgba[idx + 1] = ((rgba[idx + 1] as u16 * 235) / 255) as u8;
                    rgba[idx + 2] = ((rgba[idx + 2] as u16 * 235) / 255) as u8;
                    continue;
                }
            }
        }

        let idx = p_idx * 4;
        let r = rgba[idx];
        let g = rgba[idx + 1];
        let b = rgba[idx + 2];

        let max_v = r.max(g).max(b);
        let min_v = r.min(g).min(b);
        let chroma = max_v - min_v;

        // Rec. 709 integer luminance approximation: (54*r + 182*g + 19*b) >> 8
        let lum = ((54 * r as u32 + 182 * g as u32 + 19 * b as u32) >> 8).min(255) as usize;

        if chroma <= 12 {
            // Fast path: Neutral pixels (text, paper, anti-aliased font edges)
            rgba[idx] = lut_r[lum];
            rgba[idx + 1] = lut_g[lum];
            rgba[idx + 2] = lut_b[lum];
        } else {
            // Saturated pixel: Colored text, highlight, link, plot line
            match theme {
                PaperTheme::Sepia => {
                    let nr = r as f32 / 255.0;
                    let ng = g as f32 / 255.0;
                    let nb = b as f32 / 255.0;
                    let sat_r = (56.0 * (1.0 - nr) + nr * 244.0).clamp(0.0, 255.0);
                    let sat_g = (46.0 * (1.0 - ng) + ng * 236.0).clamp(0.0, 255.0);
                    let sat_b = (37.0 * (1.0 - nb) + nb * 216.0).clamp(0.0, 255.0);

                    let t = ((chroma as f32 - 12.0) / 18.0).clamp(0.0, 1.0);
                    let neut_r = lut_r[lum] as f32;
                    let neut_g = lut_g[lum] as f32;
                    let neut_b = lut_b[lum] as f32;

                    rgba[idx] = (neut_r + t * (sat_r - neut_r)).round() as u8;
                    rgba[idx + 1] = (neut_g + t * (sat_g - neut_g)).round() as u8;
                    rgba[idx + 2] = (neut_b + t * (sat_b - neut_b)).round() as u8;
                }
                PaperTheme::ScientificDark | PaperTheme::HighContrastDark => {
                    // Hue-preserving luminance remapping
                    let (h, s, l) = rgb_to_hsl(r, g, b);
                    let new_l = if theme == PaperTheme::HighContrastDark {
                        1.0 - l
                    } else {
                        (0.82 - 0.52 * l).clamp(0.28, 0.85)
                    };
                    let new_s = (s * 1.05).clamp(0.0, 1.0);
                    let (sat_r, sat_g, sat_b) = hsl_to_rgb(h, new_s, new_l);

                    let t = ((chroma as f32 - 12.0) / 20.0).clamp(0.0, 1.0);
                    let neut_r = lut_r[lum] as f32;
                    let neut_g = lut_g[lum] as f32;
                    let neut_b = lut_b[lum] as f32;

                    rgba[idx] = (neut_r + t * (sat_r as f32 - neut_r)).round() as u8;
                    rgba[idx + 1] = (neut_g + t * (sat_g as f32 - neut_g)).round() as u8;
                    rgba[idx + 2] = (neut_b + t * (sat_b as f32 - neut_b)).round() as u8;
                }
                PaperTheme::Light => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paper_theme_conversions() {
        assert_eq!(PaperTheme::from(0), PaperTheme::Light);
        assert_eq!(PaperTheme::from(1), PaperTheme::Sepia);
        assert_eq!(PaperTheme::from(2), PaperTheme::ScientificDark);
        assert_eq!(PaperTheme::from(3), PaperTheme::HighContrastDark);
        assert_eq!(PaperTheme::from(99), PaperTheme::Light);

        assert_eq!(i32::from(PaperTheme::Light), 0);
        assert_eq!(i32::from(PaperTheme::Sepia), 1);
        assert_eq!(i32::from(PaperTheme::ScientificDark), 2);
        assert_eq!(i32::from(PaperTheme::HighContrastDark), 3);
    }

    #[test]
    fn test_paper_theme_light_unmodified() {
        let mut rgba = vec![255, 255, 255, 255, 0, 0, 0, 255];
        let original = rgba.clone();
        apply_paper_theme(&mut rgba, 2, 1, PaperTheme::Light, &[]);
        assert_eq!(rgba, original);
    }

    #[test]
    fn test_paper_theme_sepia_tone() {
        let mut rgba = vec![
            255, 255, 255, 255, // White paper
            0, 0, 0, 255,       // Black text
        ];
        apply_paper_theme(&mut rgba, 2, 1, PaperTheme::Sepia, &[]);

        // White paper should map to warm cream (~244, 236, 216)
        assert!((rgba[0] as i32 - 244).abs() <= 2);
        assert!((rgba[1] as i32 - 236).abs() <= 2);
        assert!((rgba[2] as i32 - 216).abs() <= 2);

        // Black text should map to warm umber (~56, 46, 37)
        assert!((rgba[4] as i32 - 56).abs() <= 2);
        assert!((rgba[5] as i32 - 46).abs() <= 2);
        assert!((rgba[6] as i32 - 37).abs() <= 2);
    }

    #[test]
    fn test_paper_theme_scientific_dark_inversion() {
        let mut rgba = vec![
            255, 255, 255, 255, // White paper
            0, 0, 0, 255,       // Black text
        ];
        apply_paper_theme(&mut rgba, 2, 1, PaperTheme::ScientificDark, &[]);

        // White paper should map to dark slate (#18181A -> ~24, 24, 26)
        assert!((rgba[0] as i32 - 24).abs() <= 2);
        assert!((rgba[1] as i32 - 24).abs() <= 2);
        assert!((rgba[2] as i32 - 26).abs() <= 2);

        // Black text should map to soft stone (#D6D4CD -> ~214, 212, 205)
        assert!((rgba[4] as i32 - 214).abs() <= 2);
        assert!((rgba[5] as i32 - 212).abs() <= 2);
        assert!((rgba[6] as i32 - 205).abs() <= 2);
    }

    #[test]
    fn test_paper_theme_scientific_dark_hue_preservation() {
        // Yellow highlight color: (254, 240, 138)
        let mut rgba = vec![254, 240, 138, 255];
        apply_paper_theme(&mut rgba, 1, 1, PaperTheme::ScientificDark, &[]);

        // Assert that hue is preserved in yellow range (40 - 70 degrees) and NOT inverted to blue (200 - 260 degrees)
        let (h, _s, _l) = rgb_to_hsl(rgba[0], rgba[1], rgba[2]);
        assert!(
            (40.0..=75.0).contains(&h),
            "Expected yellow hue (40..75) but got {h} deg for RGB {:?}",
            &rgba[0..3]
        );
        // Red channel must remain dominant over blue
        assert!(rgba[0] > rgba[2], "Yellow highlight must have Red > Blue: {:?}", &rgba[0..3]);
    }

    #[test]
    fn test_paper_theme_image_rect_preservation() {
        let mut rgba = vec![
            255, 255, 255, 255, // Outside image (paper)
            120, 180, 220, 255, // Inside image (photograph pixel)
        ];
        let image_rects = vec![(1, 0, 2, 1)]; // Pixel at x=1 is inside image
        apply_paper_theme(&mut rgba, 2, 1, PaperTheme::ScientificDark, &image_rects);

        // Outside pixel is inverted to dark slate
        assert!((rgba[0] as i32 - 24).abs() <= 2);

        // Inside photograph pixel is NOT color-inverted (only slightly de-glared)
        assert!((rgba[4] as i32 - 110).abs() <= 5);
        assert!((rgba[5] as i32 - 165).abs() <= 5);
        assert!((rgba[6] as i32 - 202).abs() <= 5);
    }

    #[test]
    fn test_highlight_color_consistency_across_all_themes() {
        let yellow_rgb = [254, 240, 138];
        let h_overlay = HighlightOverlay {
            rgb: yellow_rgb,
            rects_pt: vec![(0.0, 0.0, 10.0, 10.0)],
        };

        // 1. Light Theme
        let mut rgba_light = vec![255, 255, 255, 255]; // 1x1 white pixel
        apply_highlights_to_rgba(&mut rgba_light, 1, 1, &[h_overlay.clone()], PaperTheme::Light, 10.0, 10.0, 0, 0.1);
        assert_eq!(rgba_light[0], 254);
        assert_eq!(rgba_light[1], 240);
        assert_eq!(rgba_light[2], 138);

        // 2. Sepia Theme
        let mut rgba_sepia = vec![244, 236, 216, 255]; // 1x1 sepia paper pixel
        apply_highlights_to_rgba(&mut rgba_sepia, 1, 1, &[h_overlay.clone()], PaperTheme::Sepia, 10.0, 10.0, 0, 0.1);
        let (h_sep, _, _) = rgb_to_hsl(rgba_sepia[0], rgba_sepia[1], rgba_sepia[2]);
        assert!((40.0..=75.0).contains(&h_sep), "Sepia highlight must have yellow hue, got {h_sep}");
        assert!(rgba_sepia[0] > rgba_sepia[2], "Sepia highlight must keep red > blue");

        // 3. Scientific Dark Theme: Paper background pixel
        let mut rgba_dark_bg = vec![24, 24, 26, 255]; // 1x1 slate paper pixel
        apply_highlights_to_rgba(&mut rgba_dark_bg, 1, 1, &[h_overlay.clone()], PaperTheme::ScientificDark, 10.0, 10.0, 0, 0.1);
        let (h_dark, _, l_dark) = rgb_to_hsl(rgba_dark_bg[0], rgba_dark_bg[1], rgba_dark_bg[2]);
        assert!((40.0..=75.0).contains(&h_dark), "Dark theme highlight must have yellow hue, got {h_dark}");
        assert!(rgba_dark_bg[0] > rgba_dark_bg[2], "Dark theme highlight must keep red > blue");
        assert!(rgba_dark_bg[0] > 90, "Dark theme highlight must be visibly luminous, got {}", rgba_dark_bg[0]);

        // 4. Scientific Dark Theme: Light text pixel
        let mut rgba_dark_text = vec![214, 212, 205, 255]; // 1x1 light stone text pixel
        apply_highlights_to_rgba(&mut rgba_dark_text, 1, 1, &[h_overlay], PaperTheme::ScientificDark, 10.0, 10.0, 0, 0.1);
        let (_, _, l_text) = rgb_to_hsl(rgba_dark_text[0], rgba_dark_text[1], rgba_dark_text[2]);
        assert!(l_text > 0.75, "Text must remain bright on dark highlight, got l={l_text}");
        assert!(l_text > l_dark + 0.25, "Text contrast must exceed highlight background contrast");
    }
}

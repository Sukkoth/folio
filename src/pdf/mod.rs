pub mod cache;
pub mod worker;

pub use cache::{CachedRender, PageCache};
pub use worker::{PdfWorker, RenderRequest, RenderedPage};

#[derive(Debug, Clone)]
pub struct PdfMetadata {
    pub page_count: i32,
    pub initial_width_pt: f32,
    pub initial_height_pt: f32,
}

/// Lazily inspects page count and dimensions cheaply without loading the whole file
pub fn read_pdf_info(file_path: &str) -> Result<PdfMetadata, String> {
    let doc = mupdf::Document::open(file_path)
        .map_err(|e| format!("Failed to open PDF document: {e}"))?;
    let page_count = doc
        .page_count()
        .map_err(|e| format!("Failed to read page count: {e}"))?;
    if page_count <= 0 {
        return Err("PDF file contains zero pages".to_string());
    }

    let first_page = doc
        .load_page(0)
        .map_err(|e| format!("Failed to inspect page bounds: {e}"))?;
    let bounds = first_page
        .bounds()
        .map_err(|e| format!("Failed to obtain page geometry: {e}"))?;

    let width_pt = (bounds.x1 - bounds.x0).abs().max(10.0);
    let height_pt = (bounds.y1 - bounds.y0).abs().max(10.0);

    Ok(PdfMetadata {
        page_count,
        initial_width_pt: width_pt,
        initial_height_pt: height_pt,
    })
}

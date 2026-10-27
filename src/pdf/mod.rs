pub mod cache;
pub mod worker;

pub use cache::{CachedRender, PageCache};
pub use worker::{PdfWorker, RenderRequest, RenderedPage};

#[derive(Debug, Clone)]
pub struct TocItem {
    pub title: String,
    pub page: i32,
    pub depth: i32,
}

#[derive(Debug, Clone)]
pub struct PdfMetadata {
    pub page_count: i32,
    pub initial_width_pt: f32,
    pub initial_height_pt: f32,
    pub toc: Vec<TocItem>,
}

/// Lazily inspects page count, dimensions, and outline cheaply without loading the whole file
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

    // Extract table of contents (outline) if available
    let toc = match doc.outlines() {
        Ok(outlines) => {
            let mut list = Vec::new();
            extract_toc(&doc, &outlines, 0, &mut list);
            list
        }
        Err(_) => Vec::new(),
    };

    Ok(PdfMetadata {
        page_count,
        initial_width_pt: width_pt,
        initial_height_pt: height_pt,
        toc,
    })
}

fn extract_toc(
    doc: &mupdf::Document,
    outlines: &[mupdf::Outline],
    depth: i32,
    result: &mut Vec<TocItem>,
) {
    for item in outlines {
        let mut target_page = None;
        if let Some(dest) = &item.dest {
            target_page = Some(dest.loc.page_number as i32);
        } else if let Some(uri) = &item.uri {
            if let Ok(Some(dest)) = doc.resolve_link(uri) {
                target_page = Some(dest.loc.page_number as i32);
            }
        }

        let title = item.title.trim().to_string();
        if !title.is_empty() {
            result.push(TocItem {
                title,
                page: target_page.unwrap_or(0),
                depth,
            });
        }

        if !item.down.is_empty() {
            extract_toc(doc, &item.down, depth + 1, result);
        }
    }
}

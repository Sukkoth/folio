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
    pub page_sizes: Vec<(f32, f32)>,
    pub toc: Vec<TocItem>,
}

/// Lazily inspects page count, per-page dimensions, and outline cheaply without loading whole file
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

    let mut page_sizes = Vec::with_capacity(page_count as usize);
    for p in 0..page_count {
        if let Ok(page) = doc.load_page(p) {
            if let Ok(b) = page.bounds() {
                let w = (b.x1 - b.x0).abs().max(10.0);
                let h = (b.y1 - b.y0).abs().max(10.0);
                page_sizes.push((w, h));
                continue;
            }
        }
        page_sizes.push((width_pt, height_pt));
    }

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
        page_sizes,
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

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct PageChar {
    pub ch: char,
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
    pub baseline_y: f32,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct PageLine {
    pub y0: f32,
    pub y1: f32,
    pub x0: f32,
    pub x1: f32,
    pub baseline_y: f32,
    pub chars: Vec<PageChar>,
}

#[derive(Clone, Debug, Default)]
pub struct PageTextData {
    pub lines: Vec<PageLine>,
}

/// Lazily extracts character-level text geometry for a specific page using MuPDF
pub fn extract_page_text_data(file_path: &str, page_no: i32) -> PageTextData {
    let doc = match mupdf::Document::open(file_path) {
        Ok(d) => d,
        Err(_) => return PageTextData::default(),
    };
    let page = match doc.load_page(page_no) {
        Ok(p) => p,
        Err(_) => return PageTextData::default(),
    };
    let flags = mupdf::TextPageFlags::ACCURATE_BBOXES | mupdf::TextPageFlags::ACCURATE_ASCENDERS;
    let tp = match page.to_text_page(flags) {
        Ok(t) => t,
        Err(_) => return PageTextData::default(),
    };

    let mut lines = Vec::new();

    for block in tp.blocks() {
        if block.r#type() != mupdf::text_page::TextBlockType::Text {
            continue;
        }
        for line in block.lines() {
            let mut chars = Vec::new();
            let mut line_x0 = f32::INFINITY;
            let mut line_x1 = f32::NEG_INFINITY;
            let mut line_y0 = f32::INFINITY;
            let mut line_y1 = f32::NEG_INFINITY;
            let mut line_base = 0.0f32;

            for ch_item in line.chars() {
                if let Some(c) = ch_item.char() {
                    let q = ch_item.quad();
                    let origin = ch_item.origin();
                    let cx0 = q.ul.x.min(q.ll.x);
                    let cx1 = q.ur.x.max(q.lr.x);
                    let cy0 = q.ul.y.min(q.ur.y);
                    let cy1 = q.ll.y.max(q.lr.y);

                    line_x0 = line_x0.min(cx0);
                    line_x1 = line_x1.max(cx1);
                    line_y0 = line_y0.min(cy0);
                    line_y1 = line_y1.max(cy1);
                    line_base = origin.y;

                    chars.push(PageChar {
                        ch: c,
                        x0: cx0,
                        x1: cx1,
                        y0: cy0,
                        y1: cy1,
                        baseline_y: origin.y,
                    });
                }
            }

            if !chars.is_empty() {
                lines.push(PageLine {
                    x0: line_x0,
                    x1: line_x1,
                    y0: line_y0,
                    y1: line_y1,
                    baseline_y: line_base,
                    chars,
                });
            }
        }
    }

    PageTextData { lines }
}

/// Computes selected text snippet and exact sub-word per-line bounding boxes in PDF points
pub fn compute_text_selection(
    text_data: &PageTextData,
    x_s: f32,
    y_s: f32,
    x_e: f32,
    y_e: f32,
) -> (String, Vec<(f32, f32, f32, f32)>) {
    if text_data.lines.is_empty() {
        return (String::new(), Vec::new());
    }

    let lines = &text_data.lines;

    // Helper to find nearest line to a point (x, y)
    let find_line = |_px: f32, py: f32| -> Option<usize> {
        // Direct vertical hit (with 5pt tolerance)
        for (i, l) in lines.iter().enumerate() {
            if py >= l.y0 - 5.0 && py <= l.y1 + 5.0 {
                return Some(i);
            }
        }
        // Vertical proximity
        let mut best = None;
        let mut min_d = f32::MAX;
        for (i, l) in lines.iter().enumerate() {
            let mid_y = (l.y0 + l.y1) / 2.0;
            let d = (py - mid_y).abs();
            if d < min_d {
                min_d = d;
                best = Some(i);
            }
        }
        if min_d <= 50.0 {
            best
        } else {
            None
        }
    };

    let start_line = find_line(x_s, y_s);
    let end_line = find_line(x_e, y_e);

    // Selected characters grouped by line index
    let mut selected_per_line: Vec<(usize, Vec<&PageChar>)> = Vec::new();

    if let (Some(l_s), Some(l_e)) = (start_line, end_line) {
        if l_s == l_e {
            let line = &lines[l_s];
            let x_min = x_s.min(x_e);
            let x_max = x_s.max(x_e);
            if (x_max - x_min) >= 1.5 {
                let mut line_chars = Vec::new();
                for ch in &line.chars {
                    // Character overlaps drag range if character spans into [x_min, x_max]
                    if ch.x1 >= x_min && ch.x0 <= x_max {
                        line_chars.push(ch);
                    }
                }
                if !line_chars.is_empty() {
                    selected_per_line.push((l_s, line_chars));
                }
            }
        } else if l_s < l_e {
            // Dragging downwards across lines
            let start = &lines[l_s];
            let mut start_chars = Vec::new();
            for ch in &start.chars {
                if ch.x1 >= x_s {
                    start_chars.push(ch);
                }
            }
            if !start_chars.is_empty() {
                selected_per_line.push((l_s, start_chars));
            }

            for mid_idx in (l_s + 1)..l_e {
                let mid = &lines[mid_idx];
                selected_per_line.push((mid_idx, mid.chars.iter().collect()));
            }

            let end = &lines[l_e];
            let mut end_chars = Vec::new();
            for ch in &end.chars {
                if ch.x0 <= x_e {
                    end_chars.push(ch);
                }
            }
            if !end_chars.is_empty() {
                selected_per_line.push((l_e, end_chars));
            }
        } else {
            // Dragging upwards across lines
            let top = &lines[l_e];
            let mut top_chars = Vec::new();
            for ch in &top.chars {
                if ch.x1 >= x_e {
                    top_chars.push(ch);
                }
            }
            if !top_chars.is_empty() {
                selected_per_line.push((l_e, top_chars));
            }

            for mid_idx in (l_e + 1)..l_s {
                let mid = &lines[mid_idx];
                selected_per_line.push((mid_idx, mid.chars.iter().collect()));
            }

            let bottom = &lines[l_s];
            let mut bottom_chars = Vec::new();
            for ch in &bottom.chars {
                if ch.x0 <= x_s {
                    bottom_chars.push(ch);
                }
            }
            if !bottom_chars.is_empty() {
                selected_per_line.push((l_s, bottom_chars));
            }
        }
    }

    // Fallback: 2D bounding box intersection if linear selection found no characters
    if selected_per_line.is_empty() {
        let x_min = x_s.min(x_e);
        let x_max = x_s.max(x_e);
        let y_min = y_s.min(y_e);
        let y_max = y_s.max(y_e);
        if (x_max - x_min) >= 3.0 && (y_max - y_min) >= 3.0 {
            for (l_idx, line) in lines.iter().enumerate() {
                let mut line_chars = Vec::new();
                for ch in &line.chars {
                    if ch.x0 < x_max && ch.x1 > x_min && ch.y0 < y_max && ch.y1 > y_min {
                        line_chars.push(ch);
                    }
                }
                if !line_chars.is_empty() {
                    selected_per_line.push((l_idx, line_chars));
                }
            }
        }
    }

    if selected_per_line.is_empty() {
        return (String::new(), Vec::new());
    }

    // Compute exact character-level line bounding boxes and text snippet
    let mut line_rects: Vec<(f32, f32, f32, f32)> = Vec::new();
    let mut snippet_parts: Vec<String> = Vec::new();

    for (l_idx, chars) in selected_per_line {
        if chars.is_empty() {
            continue;
        }
        let line = &lines[l_idx];
        let rx0 = chars.iter().map(|c| c.x0).fold(f32::INFINITY, f32::min);
        let rx1 = chars.iter().map(|c| c.x1).fold(f32::NEG_INFINITY, f32::max);
        line_rects.push((rx0, line.y0, rx1, line.y1));

        let part: String = chars.iter().map(|c| c.ch).collect();
        snippet_parts.push(part);
    }

    let snippet = snippet_parts.join(" ");
    (snippet, line_rects)
}

/// Extracts the underlying text snippet covered by a set of bounding boxes on a page.
pub fn extract_text_for_rects(
    text_data: &PageTextData,
    rects: &[(f32, f32, f32, f32)],
) -> String {
    if text_data.lines.is_empty() || rects.is_empty() {
        return String::new();
    }

    let mut line_texts = Vec::new();

    for line in &text_data.lines {
        let mut matched_chars: Vec<char> = Vec::new();

        for ch in &line.chars {
            let cx = (ch.x0 + ch.x1) * 0.5;
            let cy = (ch.y0 + ch.y1) * 0.5;

            let in_rect = rects.iter().any(|r| {
                cx >= (r.0 - 2.0) && cx <= (r.2 + 2.0) && cy >= (r.1 - 4.0) && cy <= (r.3 + 4.0)
            });

            if in_rect {
                matched_chars.push(ch.ch);
            }
        }

        if !matched_chars.is_empty() {
            let s: String = matched_chars.into_iter().collect();
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                line_texts.push(trimmed.to_string());
            }
        }
    }

    line_texts.join(" ")
}

#[derive(Debug, Clone)]
pub struct ExtractedAnnotation {
    pub page: i32,
    pub kind: String, // "highlight", "underline", "strikethrough", "note"
    pub color_rgb: (f32, f32, f32),
    pub color_hex: String,
    pub snippet: String,
    pub comment: String,
    pub line_rects_pt: Vec<(f32, f32, f32, f32)>,
}

/// Extracts all existing annotations (highlights, underlines, strikethroughs, notes) from the PDF
pub fn extract_document_annotations(file_path: &str) -> Vec<ExtractedAnnotation> {
    let pdf_doc = match mupdf::pdf::PdfDocument::open(file_path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let page_count = match pdf_doc.page_count() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut annotations = Vec::new();

    for p in 0..page_count {
        let page = match pdf_doc.load_pdf_page(p) {
            Ok(page) => page,
            Err(_) => continue,
        };

        let mut page_text: Option<PageTextData> = None;

        for annot in page.annotations() {
            let annot_type = match annot.r#type() {
                Ok(t) => t,
                Err(_) => continue,
            };

            let kind = match annot_type {
                mupdf::pdf::PdfAnnotationType::Highlight => "highlight",
                mupdf::pdf::PdfAnnotationType::Underline | mupdf::pdf::PdfAnnotationType::Squiggly => "underline",
                mupdf::pdf::PdfAnnotationType::StrikeOut => "strikethrough",
                mupdf::pdf::PdfAnnotationType::Text | mupdf::pdf::PdfAnnotationType::FreeText => "note",
                _ => continue,
            };

            let mut line_rects_pt = Vec::new();
            if let Ok(quads) = annot.quads() {
                for q in quads {
                    let rx0 = q.ul.x.min(q.ur.x).min(q.ll.x).min(q.lr.x);
                    let ry0 = q.ul.y.min(q.ur.y).min(q.ll.y).min(q.lr.y);
                    let rx1 = q.ul.x.max(q.ur.x).max(q.ll.x).max(q.lr.x);
                    let ry1 = q.ul.y.max(q.ur.y).max(q.ll.y).max(q.lr.y);
                    if (rx1 - rx0) > 0.5 && (ry1 - ry0) > 0.5 {
                        line_rects_pt.push((rx0, ry0, rx1, ry1));
                    }
                }
            }

            if line_rects_pt.is_empty() {
                if let Ok(rect) = annot.rect() {
                    let rx0 = rect.x0.min(rect.x1);
                    let ry0 = rect.y0.min(rect.y1);
                    let rx1 = rect.x0.max(rect.x1);
                    let ry1 = rect.y0.max(rect.y1);
                    if (rx1 - rx0) > 0.5 && (ry1 - ry0) > 0.5 {
                        line_rects_pt.push((rx0, ry0, rx1, ry1));
                    }
                }
            }

            let (r, g, b): (f32, f32, f32) = match annot.color() {
                Ok(Some(mupdf::color::AnnotationColor::Rgb { red, green, blue })) => (red, green, blue),
                Ok(Some(mupdf::color::AnnotationColor::Gray(gray))) => (gray, gray, gray),
                Ok(Some(mupdf::color::AnnotationColor::Cmyk { cyan, magenta, yellow, key })) => (
                    (1.0 - cyan) * (1.0 - key),
                    (1.0 - magenta) * (1.0 - key),
                    (1.0 - yellow) * (1.0 - key),
                ),
                _ => {
                    if kind == "note" {
                        (0.23, 0.51, 0.96)
                    } else {
                        (0.98, 0.80, 0.08)
                    }
                }
            };

            let color_hex = format!(
                "#{:02x}{:02x}{:02x}",
                (r * 255.0f32).round().clamp(0.0, 255.0) as u8,
                (g * 255.0f32).round().clamp(0.0, 255.0) as u8,
                (b * 255.0f32).round().clamp(0.0, 255.0) as u8,
            );

            let comment = annot.contents().ok().flatten().unwrap_or("").trim().to_string();

            let mut snippet = String::new();
            if kind == "note" {
                snippet = comment.clone();
            } else {
                if page_text.is_none() {
                    page_text = Some(extract_page_text_data(file_path, p));
                }
                if let Some(ref pt) = page_text {
                    snippet = extract_text_for_rects(pt, &line_rects_pt);
                }
                if snippet.is_empty() && !comment.is_empty() {
                    snippet = comment.clone();
                }
                if snippet.is_empty() {
                    let kind_capitalized = match kind {
                        "highlight" => "Highlight",
                        "underline" => "Underline",
                        "strikethrough" => "Strikethrough",
                        _ => "Annotation",
                    };
                    snippet = format!("{} on Page {}", kind_capitalized, p + 1);
                }
            }

            annotations.push(ExtractedAnnotation {
                page: p,
                kind: kind.to_string(),
                color_rgb: (r, g, b),
                color_hex,
                snippet,
                comment,
                line_rects_pt,
            });
        }
    }

    annotations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_annotations() {
        let path = "/home/twof/Documents/The Message of Ephesians (John R. W. Stott) (z-library.sk, 1lib.sk, z-lib.sk).pdf";
        if std::path::Path::new(path).exists() {
            let annots = extract_document_annotations(path);
            println!("Extracted {} annotations from PDF", annots.len());
            for a in &annots {
                println!("- Page {}: [{}] '{}' (color: {})", a.page + 1, a.kind, a.snippet, a.color_hex);
            }
        }
    }
}

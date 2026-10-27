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

/// Adjusts raw line rectangles for highlights with vertical ascender/descender padding
/// and clean constant spacing (2.4pt) between consecutive lines.
pub fn adjust_line_rects_for_highlight(
    rects: &[(f32, f32, f32, f32)],
) -> Vec<(f32, f32, f32, f32)> {
    let n = rects.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        let (rx0, ry0, rx1, ry1) = rects[0];
        let line_h = (ry1 - ry0).max(1.0);
        let pad = (line_h * 0.16).clamp(1.2, 2.0);
        return vec![(rx0, (ry0 - pad).max(0.0), rx1, ry1 + pad)];
    }

    // Auto-detect: if rects already touch or overlap (e.g. from an app using continuous highlights with no gaps),
    // we preserve the continuous block (gap = 0). Otherwise, we use Folio's spaced highlight (gap = 2.4pt).
    let is_already_continuous = (0..n - 1).all(|i| {
        let y1_curr = rects[i].3;
        let y0_next = rects[i + 1].1;
        (y0_next - y1_curr) <= 0.5
    });

    let gap = if is_already_continuous { 0.0 } else { 2.4 };
    let mut adjusted_y = Vec::with_capacity(n);
    for i in 0..n {
        adjusted_y.push((rects[i].1, rects[i].3));
    }

    // Between each consecutive pair of lines (i, i+1):
    for i in 0..(n - 1) {
        let y1_curr = rects[i].3;
        let y0_next = rects[i + 1].1;
        let raw_gap = y0_next - y1_curr;

        // Normal consecutive lines in a text block
        if raw_gap < 12.0 {
            let mid = (y1_curr + y0_next) * 0.5;
            adjusted_y[i].1 = mid - gap * 0.5;
            adjusted_y[i + 1].0 = mid + gap * 0.5;
        } else {
            // Paragraph break
            let pad = ((rects[i].3 - rects[i].1) * 0.16).clamp(1.2, 2.0);
            adjusted_y[i].1 = y1_curr + pad;
            let next_pad = ((rects[i + 1].3 - rects[i + 1].1) * 0.16).clamp(1.2, 2.0);
            adjusted_y[i + 1].0 = (y0_next - next_pad).max(0.0);
        }
    }

    // Top of first line
    let first_h = (rects[0].3 - rects[0].1).max(1.0);
    let top_pad = (first_h * 0.16).clamp(1.2, 2.0);
    adjusted_y[0].0 = (rects[0].1 - top_pad).max(0.0);

    // Bottom of last line
    let last_h = (rects[n - 1].3 - rects[n - 1].1).max(1.0);
    let bot_pad = (last_h * 0.16).clamp(1.2, 2.0);
    adjusted_y[n - 1].1 = rects[n - 1].3 + bot_pad;

    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (rx0, _, rx1, _) = rects[i];
        let (ay0, ay1) = adjusted_y[i];
        out.push((rx0, ay0, rx1, ay1));
    }
    out
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

            let is_dup = annotations.iter().any(|existing: &ExtractedAnnotation| {
                if existing.page != p || existing.kind != kind {
                    return false;
                }
                if !snippet.is_empty() && existing.snippet == snippet {
                    return true;
                }
                if !line_rects_pt.is_empty() && existing.line_rects_pt.len() == line_rects_pt.len() {
                    let all_match = existing.line_rects_pt.iter().zip(&line_rects_pt).all(|(r1, r2)| {
                        (r1.0 - r2.0).abs() < 1.0
                            && (r1.1 - r2.1).abs() < 1.0
                            && (r1.2 - r2.2).abs() < 1.0
                            && (r1.3 - r2.3).abs() < 1.0
                    });
                    if all_match {
                        return true;
                    }
                }
                false
            });

            if !is_dup {
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
    }

    annotations
}

#[derive(Debug, Clone)]
pub struct SaveAnnotationData {
    pub page: i32,
    pub kind: String, // "highlight", "underline", "strikethrough", "note"
    pub color_rgb: (f32, f32, f32),
    pub snippet: String,
    pub comment: String,
    pub line_rects_pt: Vec<(f32, f32, f32, f32)>,
}

/// Saves annotations permanently into the PDF file
pub fn save_document_annotations(
    file_path: &str,
    annotations: &[SaveAnnotationData],
) -> Result<(), String> {
    let doc = mupdf::pdf::PdfDocument::open(file_path)
        .map_err(|e| format!("Failed to open PDF document for saving: {e}"))?;

    let page_count = doc
        .page_count()
        .map_err(|e| format!("Failed to read PDF page count: {e}"))?;

    for p in 0..page_count {
        let mut page = match doc.load_pdf_page(p) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // 1. Remove existing markup annotations from in-memory page structure
        let existing: Vec<_> = page
            .annotations()
            .filter(|a| {
                matches!(
                    a.r#type(),
                    Ok(mupdf::pdf::PdfAnnotationType::Highlight)
                        | Ok(mupdf::pdf::PdfAnnotationType::Underline)
                        | Ok(mupdf::pdf::PdfAnnotationType::Squiggly)
                        | Ok(mupdf::pdf::PdfAnnotationType::StrikeOut)
                        | Ok(mupdf::pdf::PdfAnnotationType::Text)
                        | Ok(mupdf::pdf::PdfAnnotationType::FreeText)
                )
            })
            .collect();

        for a in existing {
            let _ = page.delete_annotation(a);
        }

        // 2. Clean the page dictionary's Annots array directly.
        // In MuPDF, direct annotation dictionaries are not removed from the page's Annots array
        // by pdf_delete_annot because their dictionary lengths differ once loaded.
        let page_obj = page.object();
        if let Ok(Some(mut annots_arr)) = page_obj.get_dict("Annots") {
            let len = annots_arr.len().unwrap_or(0);
            for i in (0..len).rev() {
                if let Ok(Some(item)) = annots_arr.get_array(i as i32) {
                    if let Ok(Some(subtype)) = item.get_dict("Subtype") {
                        let name_bytes = subtype.as_name().unwrap_or_default();
                        let name_str = String::from_utf8_lossy(&name_bytes);
                        if matches!(
                            name_str.as_ref(),
                            "Highlight" | "Underline" | "StrikeOut" | "Squiggly" | "Text" | "FreeText"
                        ) {
                            let _ = annots_arr.array_delete(i as i32);
                        }
                    }
                }
            }
        }

        // 3. Add current annotations for this page
        for a in annotations.iter().filter(|a| a.page == p) {
            let color = mupdf::color::AnnotationColor::Rgb {
                red: a.color_rgb.0,
                green: a.color_rgb.1,
                blue: a.color_rgb.2,
            };

            let quads: Vec<mupdf::Quad> = a
                .line_rects_pt
                .iter()
                .filter_map(|&(x0, y0, x1, y1)| {
                    let rx0 = x0.min(x1);
                    let ry0 = y0.min(y1);
                    let rx1 = x0.max(x1);
                    let ry1 = y0.max(y1);
                    if (rx1 - rx0) > 0.5 && (ry1 - ry0) > 0.5 {
                        Some(mupdf::Rect::new(rx0, ry0, rx1, ry1).quad())
                    } else {
                        None
                    }
                })
                .collect();

            match a.kind.as_str() {
                "highlight" => {
                    let adjusted_rects = adjust_line_rects_for_highlight(&a.line_rects_pt);
                    let quads: Vec<mupdf::Quad> = adjusted_rects
                        .iter()
                        .filter_map(|&(x0, y0, x1, y1)| {
                            let rx0 = x0.min(x1);
                            let ry0 = y0.min(y1);
                            let rx1 = x0.max(x1);
                            let ry1 = y0.max(y1);
                            if (rx1 - rx0) > 0.5 && (ry1 - ry0) > 0.5 {
                                Some(mupdf::Rect::new(rx0, ry0, rx1, ry1).quad())
                            } else {
                                None
                            }
                        })
                        .collect();

                    if !quads.is_empty() {
                        if let Ok(mut annot) = page.add_highlight_annotation(quads) {
                            let _ = annot.set_color(color);
                            if !a.snippet.is_empty() {
                                let _ = annot.set_contents(&a.snippet);
                            }
                            let _ = annot.update();
                            set_crisp_rectangular_highlight_appearance(&mut annot, a.color_rgb);
                        }
                    }
                }
                "underline" => {
                    if !quads.is_empty() {
                        if let Ok(mut annot) = page.add_underline_annotation(quads) {
                            let _ = annot.set_color(color);
                            if !a.snippet.is_empty() {
                                let _ = annot.set_contents(&a.snippet);
                            }
                            let _ = annot.update();
                        }
                    }
                }
                "strikethrough" => {
                    if !quads.is_empty() {
                        if let Ok(mut annot) = page.add_strikeout_annotation(quads) {
                            let _ = annot.set_color(color);
                            if !a.snippet.is_empty() {
                                let _ = annot.set_contents(&a.snippet);
                            }
                            let _ = annot.update();
                        }
                    }
                }
                "note" => {
                    let (rx0, ry0, _, _) = a
                        .line_rects_pt
                        .first()
                        .copied()
                        .unwrap_or((50.0, 50.0, 74.0, 74.0));
                    let rect = mupdf::Rect::new(rx0, ry0, rx0 + 24.0, ry0 + 24.0);
                    let text_content = if !a.comment.is_empty() {
                        &a.comment
                    } else {
                        &a.snippet
                    };
                    if let Ok(mut annot) = page.add_text_annotation(rect, text_content) {
                        let _ = annot.set_color(color);
                        let _ = annot.update();
                    }
                }
                _ => {}
            }
        }
    }

    // Save atomically: write to temp file then rename
    let tmp_path = format!("{}.folio_tmp", file_path);
    let mut options = mupdf::pdf::PdfWriteOptions::default();
    options.set_incremental(false);
    options.set_garbage(true);
    doc.save_with_options(&tmp_path, options)
        .map_err(|e| format!("Failed to write modified PDF to disk: {e}"))?;

    std::fs::rename(&tmp_path, file_path)
        .map_err(|e| format!("Failed to replace PDF file: {e}"))?;

    Ok(())
}

/// Overwrites MuPDF's default Adobe-style curved/rounded appearance stream with
/// clean, modern, crisp rectangular paths (re f) matching Folio's live reading design.
fn set_crisp_rectangular_highlight_appearance(
    annot: &mut mupdf::pdf::PdfAnnotation,
    (r, g, b): (f32, f32, f32),
) {
    let obj = annot.object();
    let qp = match obj.get_dict("QuadPoints") {
        Ok(Some(qp)) => qp,
        _ => return,
    };
    let qp_len = match qp.len() {
        Ok(len) if len >= 8 && len % 8 == 0 => len,
        _ => return,
    };

    let mut rect_ops = String::new();
    let mut min_all_x = f32::INFINITY;
    let mut min_all_y = f32::INFINITY;
    let mut max_all_x = f32::NEG_INFINITY;
    let mut max_all_y = f32::NEG_INFINITY;

    for i in (0..qp_len).step_by(8) {
        let ul_x = qp.get_array(i as i32 + 0).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);
        let ul_y = qp.get_array(i as i32 + 1).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);
        let ur_x = qp.get_array(i as i32 + 2).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);
        let ur_y = qp.get_array(i as i32 + 3).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);
        let ll_x = qp.get_array(i as i32 + 4).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);
        let ll_y = qp.get_array(i as i32 + 5).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);
        let lr_x = qp.get_array(i as i32 + 6).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);
        let lr_y = qp.get_array(i as i32 + 7).ok().flatten().and_then(|o| o.as_float().ok()).unwrap_or(0.0);

        let rx0 = ul_x.min(ll_x);
        let rx1 = ur_x.max(lr_x);
        let ry0 = ll_y.min(lr_y);
        let ry1 = ul_y.max(ur_y);
        let rw = (rx1 - rx0).max(0.1);
        let rh = (ry1 - ry0).max(0.1);

        min_all_x = min_all_x.min(rx0);
        min_all_y = min_all_y.min(ry0);
        max_all_x = max_all_x.max(rx1);
        max_all_y = max_all_y.max(ry1);

        rect_ops.push_str(&format!("{rx0:.3} {ry0:.3} {rw:.3} {rh:.3} re\n"));
    }

    if rect_ops.is_empty() {
        return;
    }

    let stream_content = format!("/H gs\n{r:.4} {g:.4} {b:.4} rg\n{rect_ops}f\n");

    if let Ok(Some(ap)) = obj.get_dict("AP") {
        if let Ok(Some(mut n_stream)) = ap.get_dict("N") {
            let _ = n_stream.write_stream_string(&stream_content);
            if let Ok(Some(mut bbox_arr)) = n_stream.get_dict("BBox") {
                let _ = bbox_arr.array_put(0, mupdf::pdf::PdfObject::new_real(min_all_x).unwrap());
                let _ = bbox_arr.array_put(1, mupdf::pdf::PdfObject::new_real(min_all_y).unwrap());
                let _ = bbox_arr.array_put(2, mupdf::pdf::PdfObject::new_real(max_all_x).unwrap());
                let _ = bbox_arr.array_put(3, mupdf::pdf::PdfObject::new_real(max_all_y).unwrap());
            }
        }
    }

    if let Ok(Some(mut rect_arr)) = obj.get_dict("Rect") {
        let _ = rect_arr.array_put(0, mupdf::pdf::PdfObject::new_real(min_all_x).unwrap());
        let _ = rect_arr.array_put(1, mupdf::pdf::PdfObject::new_real(min_all_y).unwrap());
        let _ = rect_arr.array_put(2, mupdf::pdf::PdfObject::new_real(max_all_x).unwrap());
        let _ = rect_arr.array_put(3, mupdf::pdf::PdfObject::new_real(max_all_y).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inspect_annot_ap() {
        let mut doc = mupdf::pdf::PdfDocument::new();
        let mut page = doc.new_page((500.0, 700.0)).unwrap();
        let quad = mupdf::Rect::new(50.0, 100.0, 250.0, 120.0).quad();
        let mut annot = page.add_highlight_annotation(vec![quad]).unwrap();
        annot.set_color(mupdf::color::AnnotationColor::Rgb { red: 1.0, green: 0.8, blue: 0.2 }).unwrap();
        annot.update().unwrap();
        set_crisp_rectangular_highlight_appearance(&mut annot, (1.0, 0.8, 0.2));

        let tmp = "/tmp/folio_test_rect_highlight.pdf";
        doc.save(tmp).unwrap();

        // Render page using mupdf to verify it renders cleanly
        let doc2 = mupdf::Document::open(tmp).unwrap();
        let page2 = doc2.load_page(0).unwrap();
        let pixmap = page2.to_pixmap(&mupdf::Matrix::new_scale(1.0, 1.0), &mupdf::Colorspace::device_rgb(), false, true).unwrap();
        let quads = annot.quads().unwrap();
        println!("annot.quads(): {:?}", quads);
        let samples = pixmap.samples();
        let stride = pixmap.stride() as usize;

        // Check pixel at (48, 110) - outside rect: should be pure white (255, 255, 255)
        let p_out = (110 * stride) + (48 * 3);
        println!("Outside pixel (48, 110): {:?}", &samples[p_out..p_out+3]);

        // Check pixel at (52, 110) - inside rect: should be highlighted
        let p_in = (110 * stride) + (52 * 3);
        println!("Inside pixel (52, 110): {:?}", &samples[p_in..p_in+3]);

        // Check top corner pixel at (50, 100): should be highlighted (flat rectangular edge, NOT curved away!)
        let p_corner = (100 * stride) + (50 * 3);
        println!("Corner pixel (50, 100): {:?}", &samples[p_corner..p_corner+3]);

        assert_eq!(&samples[p_out..p_out+3], &[255, 255, 255]);
        assert_ne!(&samples[p_in..p_in+3], &[255, 255, 255]);
        assert_ne!(&samples[p_corner..p_corner+3], &[255, 255, 255]);
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn test_extract_and_deduplicate() {
        let path = "/home/twof/Documents/The Message of Ephesians (John R. W. Stott) (z-library.sk, 1lib.sk, z-lib.sk).pdf";
        if std::path::Path::new(path).exists() {
            let annots = extract_document_annotations(path);
            println!("Extracted {} deduplicated annotations from PDF", annots.len());
            for a in &annots {
                println!("- Page {}: [{}] '{}' (color: {})", a.page + 1, a.kind, a.snippet, a.color_hex);
            }
            // Verify there are no duplicate snippets on the same page
            let mut seen = std::collections::HashSet::new();
            for a in &annots {
                let key = (a.page, a.kind.clone(), a.snippet.clone());
                assert!(seen.insert(key), "Found duplicate annotation: Page {} [{}] '{}'", a.page + 1, a.kind, a.snippet);
            }

            // Test saving and reloading: count must stay identical (no duplicates!)
            let tmp_copy = "/tmp/folio_ephesians_test_roundtrip.pdf";
            std::fs::copy(path, tmp_copy).unwrap();

            let to_save: Vec<SaveAnnotationData> = annots.iter().map(|a| {
                let rgb = if a.kind == "highlight" {
                    // Test pastel orange for "Most commentators"
                    if a.snippet.contains("Most commentators") {
                        (1.0, 0.8157, 0.6078)
                    } else {
                        a.color_rgb
                    }
                } else {
                    a.color_rgb
                };
                SaveAnnotationData {
                    page: a.page,
                    kind: a.kind.clone(),
                    color_rgb: rgb,
                    snippet: a.snippet.clone(),
                    comment: a.comment.clone(),
                    line_rects_pt: a.line_rects_pt.clone(),
                }
            }).collect();

            save_document_annotations(tmp_copy, &to_save).unwrap();
            let reloaded1 = extract_document_annotations(tmp_copy);
            assert_eq!(reloaded1.len(), annots.len(), "Reloaded count must match exactly");

            // Save a second time: count must still remain exactly identical!
            save_document_annotations(tmp_copy, &to_save).unwrap();
            let reloaded2 = extract_document_annotations(tmp_copy);
            assert_eq!(reloaded2.len(), annots.len(), "Second save must still match exactly");

            // Check color of "Most commentators" is pastel peach/orange
            let most_comm = reloaded2.iter().find(|a| a.snippet.contains("Most commentators")).unwrap();
            assert_eq!(most_comm.color_hex, "#ffd09b", "Color must be soft pastel peach/orange");

            let _ = std::fs::remove_file(tmp_copy);
        }
    }

    #[test]
    fn test_save_and_extract_roundtrip() {
        let temp_dir = std::env::temp_dir();
        let test_pdf_path = temp_dir.join("folio_test_annot.pdf");
        let path_str = test_pdf_path.to_str().unwrap();

        // Create a blank PDF with 1 page
        {
            let mut doc = mupdf::pdf::PdfDocument::new();
            doc.new_page((500.0, 700.0)).unwrap();
            doc.save(path_str).unwrap();
        }

        // Save a highlight and a note
        let to_save = vec![
            SaveAnnotationData {
                page: 0,
                kind: "highlight".to_string(),
                color_rgb: (0.98, 0.80, 0.08),
                snippet: "Testing Folio Highlight Saving".to_string(),
                comment: String::new(),
                line_rects_pt: vec![(50.0, 100.0, 250.0, 120.0)],
            },
            SaveAnnotationData {
                page: 0,
                kind: "note".to_string(),
                color_rgb: (0.23, 0.51, 0.96),
                snippet: String::new(),
                comment: "Important user note".to_string(),
                line_rects_pt: vec![(60.0, 60.0, 84.0, 84.0)],
            },
        ];

        save_document_annotations(path_str, &to_save).unwrap();

        // Extract and verify
        let extracted = extract_document_annotations(path_str);
        assert_eq!(extracted.len(), 2);
        assert!(extracted.iter().any(|a| a.kind == "highlight"));
        assert!(extracted.iter().any(|a| a.kind == "note" && a.comment == "Important user note"));

        // Save again with the same 2 annotations: verify count stays 2 (no duplicates created)
        save_document_annotations(path_str, &to_save).unwrap();
        let extracted2 = extract_document_annotations(path_str);
        assert_eq!(extracted2.len(), 2, "Re-saving must not create duplicate annotations");

        let _ = std::fs::remove_file(test_pdf_path);
    }
}

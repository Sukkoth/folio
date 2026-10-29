pub mod cache;
pub mod worker;

pub use cache::{CachedRender, PageCache};
pub use worker::{PdfWorker, RenderRequest, RenderedPage};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct TocItem {
    pub id: i32,
    pub title: String,
    pub page: i32,
    pub depth: i32,
    pub has_children: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageLink {
    pub bounds_pt: (f32, f32, f32, f32), // (x0, y0, x1, y1) in unrotated PDF points
    pub target_page: i32,                // -1 if external URI
    pub uri: String,
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
            let mut next_id = 0;
            extract_toc(&doc, &outlines, 0, &mut next_id, &mut list);
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
    next_id: &mut i32,
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
        let has_children = !item.down.is_empty();
        let id = *next_id;
        *next_id += 1;

        if !title.is_empty() {
            result.push(TocItem {
                id,
                title,
                page: target_page.unwrap_or(0),
                depth,
                has_children,
            });
        }

        if has_children {
            extract_toc(doc, &item.down, depth + 1, next_id, result);
        }
    }
}

pub fn extract_page_links(file_path: &str, page_no: i32) -> Vec<PageLink> {
    let doc = match mupdf::Document::open(file_path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    extract_page_links_from_doc(&doc, page_no)
}

pub fn extract_page_links_from_doc(doc: &mupdf::Document, page_no: i32) -> Vec<PageLink> {
    let page = match doc.load_page(page_no) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let links_iter = match page.links() {
        Ok(iter) => iter,
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::new();
    for link in links_iter {
        let mut target_page = -1;
        if let Some(dest) = &link.dest {
            target_page = dest.loc.page_number as i32;
        } else if !link.uri.is_empty() {
            if let Ok(Some(dest)) = doc.resolve_link(&link.uri) {
                target_page = dest.loc.page_number as i32;
            }
        }

        result.push(PageLink {
            bounds_pt: (link.bounds.x0, link.bounds.y0, link.bounds.x1, link.bounds.y1),
            target_page,
            uri: link.uri,
        });
    }

    result
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

            let has_visible_text = chars.iter().any(|c| !c.ch.is_whitespace());
            if has_visible_text {
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

    // Sort lines in natural reading order: top-to-bottom, left-to-right
    lines.sort_by(|a, b| {
        let a_mid = (a.y0 + a.y1) / 2.0;
        let b_mid = (b.y0 + b.y1) / 2.0;
        let v_overlap = a.y0.max(b.y0) < a.y1.min(b.y1);
        if v_overlap {
            a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            a_mid.partial_cmp(&b_mid).unwrap_or(std::cmp::Ordering::Equal)
        }
    });

    PageTextData { lines }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PageBoundary {
    Point(f32, f32),
    StartOfPage,
    EndOfPage,
}

/// Computes selected text snippet and exact sub-word per-line bounding boxes in PDF points
/// according to natural reading stream order (never 2D rectangular slicing).
pub fn compute_page_selection(
    text_data: &PageTextData,
    from: PageBoundary,
    to: PageBoundary,
) -> (String, Vec<(f32, f32, f32, f32)>) {
    if text_data.lines.is_empty() {
        return (String::new(), Vec::new());
    }

    let lines = &text_data.lines;

    // Helper to map any (x, y) point to (line_idx, char_idx) in text stream
    let find_line_and_char = |px: f32, py: f32| -> (usize, usize) {
        if py < lines[0].y0 {
            return (0, 0);
        }
        if py > lines.last().unwrap().y1 {
            let last_idx = lines.len() - 1;
            return (last_idx, lines[last_idx].chars.len());
        }

        // Find line with minimum vertical distance
        let mut best_line = 0;
        let mut min_dist_y = f32::MAX;
        let mut min_dist_x = f32::MAX;

        for (i, l) in lines.iter().enumerate() {
            let dist_y = if py < l.y0 {
                l.y0 - py
            } else if py > l.y1 {
                py - l.y1
            } else {
                0.0
            };

            let dist_x = if px < l.x0 {
                l.x0 - px
            } else if px > l.x1 {
                px - l.x1
            } else {
                0.0
            };

            if dist_y < min_dist_y - 0.5 {
                min_dist_y = dist_y;
                min_dist_x = dist_x;
                best_line = i;
            } else if (dist_y - min_dist_y).abs() <= 0.5 && dist_x < min_dist_x {
                min_dist_x = dist_x;
                best_line = i;
            }
        }

        // Now find char on best_line
        let line = &lines[best_line];
        if line.chars.is_empty() || px <= line.chars[0].x0 {
            return (best_line, 0);
        }
        if px >= line.chars.last().unwrap().x1 {
            return (best_line, line.chars.len());
        }

        for (j, ch) in line.chars.iter().enumerate() {
            let mid_x = (ch.x0 + ch.x1) / 2.0;
            if px <= mid_x {
                return (best_line, j);
            }
        }

        (best_line, line.chars.len())
    };

    let (l_s, c_s) = match from {
        PageBoundary::StartOfPage => (0, 0),
        PageBoundary::EndOfPage => {
            let last_idx = lines.len() - 1;
            (last_idx, lines[last_idx].chars.len())
        }
        PageBoundary::Point(x, y) => find_line_and_char(x, y),
    };

    let (l_e, c_e) = match to {
        PageBoundary::StartOfPage => (0, 0),
        PageBoundary::EndOfPage => {
            let last_idx = lines.len() - 1;
            (last_idx, lines[last_idx].chars.len())
        }
        PageBoundary::Point(x, y) => find_line_and_char(x, y),
    };

    // Determine stream direction
    let is_backward = (l_s > l_e) || (l_s == l_e && c_s > c_e);
    let (start_line, start_char, end_line, end_char) = if is_backward {
        (l_e, c_e, l_s, c_s)
    } else {
        (l_s, c_s, l_e, c_e)
    };

    if start_line == end_line && start_char == end_char {
        return (String::new(), Vec::new());
    }

    let mut selected_per_line: Vec<(usize, Vec<&PageChar>)> = Vec::new();

    if start_line == end_line {
        let line = &lines[start_line];
        let end_idx = end_char.min(line.chars.len());
        let start_idx = start_char.min(end_idx);
        let slice = line.chars[start_idx..end_idx].iter().collect::<Vec<_>>();
        if !slice.is_empty() {
            selected_per_line.push((start_line, slice));
        }
    } else {
        // First line: from start_char to end of line
        let first_line = &lines[start_line];
        let s_idx = start_char.min(first_line.chars.len());
        let first_slice = first_line.chars[s_idx..].iter().collect::<Vec<_>>();
        if !first_slice.is_empty() {
            selected_per_line.push((start_line, first_slice));
        }

        // Full intermediate lines
        for mid_idx in (start_line + 1)..end_line {
            let mid_line = &lines[mid_idx];
            let mid_slice = mid_line.chars.iter().collect::<Vec<_>>();
            if !mid_slice.is_empty() {
                selected_per_line.push((mid_idx, mid_slice));
            }
        }

        // Last line: from beginning to end_char
        let last_line = &lines[end_line];
        let e_idx = end_char.min(last_line.chars.len());
        let last_slice = last_line.chars[..e_idx].iter().collect::<Vec<_>>();
        if !last_slice.is_empty() {
            selected_per_line.push((end_line, last_slice));
        }
    }

    if selected_per_line.is_empty() {
        return (String::new(), Vec::new());
    }

    // Compute exact character-level line bounding boxes and text snippet
    let mut line_rects: Vec<(f32, f32, f32, f32)> = Vec::new();
    let mut snippet_parts: Vec<String> = Vec::new();

    for (l_idx, mut chars) in selected_per_line {
        // Trim leading and trailing whitespace characters so we only highlight actual text
        while let Some(first) = chars.first() {
            if first.ch.is_whitespace() {
                chars.remove(0);
            } else {
                break;
            }
        }
        while let Some(last) = chars.last() {
            if last.ch.is_whitespace() {
                chars.pop();
            } else {
                break;
            }
        }
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

/// Computes selected text snippet and exact sub-word per-line bounding boxes in PDF points
pub fn compute_text_selection(
    text_data: &PageTextData,
    x_s: f32,
    y_s: f32,
    x_e: f32,
    y_e: f32,
) -> (String, Vec<(f32, f32, f32, f32)>) {
    compute_page_selection(
        text_data,
        PageBoundary::Point(x_s, y_s),
        PageBoundary::Point(x_e, y_e),
    )
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
pub struct SearchHitResult {
    pub page: i32,
    pub snippet: String,
    pub match_text: String,
    pub rects_pt: Vec<(f32, f32, f32, f32)>,
}

fn merge_quads_to_line_rects(quads: &[mupdf::Quad]) -> Vec<(f32, f32, f32, f32)> {
    if quads.is_empty() {
        return Vec::new();
    }

    let mut line_rects: Vec<(f32, f32, f32, f32)> = Vec::new();

    for q in quads {
        let qx0 = q.ul.x.min(q.ll.x);
        let qx1 = q.ur.x.max(q.lr.x);
        let qy0 = q.ul.y.min(q.ur.y);
        let qy1 = q.ll.y.max(q.lr.y);

        let mut merged = false;
        if let Some(last) = line_rects.last_mut() {
            let last_mid_y = (last.1 + last.3) * 0.5;
            let q_mid_y = (qy0 + qy1) * 0.5;
            let line_h = (last.3 - last.1).max(qy1 - qy0);
            if (last_mid_y - q_mid_y).abs() <= line_h * 0.6 {
                last.0 = last.0.min(qx0);
                last.1 = last.1.min(qy0);
                last.2 = last.2.max(qx1);
                last.3 = last.3.max(qy1);
                merged = true;
            }
        }

        if !merged {
            line_rects.push((qx0, qy0, qx1, qy1));
        }
    }

    line_rects
}

/// Searches the entire PDF document for a query string, returning all matches with exact bounding rects
/// and context snippets. Searches pages in parallel across worker threads and supports cooperative cancellation,
/// case-sensitivity (`match_case`), and whole-word matching (`match_whole_word`).
pub fn search_document(
    file_path: &str,
    total_pages: i32,
    needle: &str,
    match_case: bool,
    match_whole_word: bool,
    cancel_gen: Option<Arc<AtomicU64>>,
    current_gen: u64,
) -> Vec<SearchHitResult> {
    let needle_trimmed = needle.trim();
    if needle_trimmed.is_empty() {
        return Vec::new();
    }

    let num_pages = if total_pages > 0 {
        total_pages
    } else {
        match mupdf::Document::open(file_path) {
            Ok(d) => d.page_count().unwrap_or(0),
            Err(_) => return Vec::new(),
        }
    };

    if num_pages <= 0 {
        return Vec::new();
    }

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);

    if num_pages <= 4 || num_threads <= 1 {
        return search_document_range(
            file_path,
            0,
            num_pages,
            needle_trimmed,
            match_case,
            match_whole_word,
            cancel_gen.as_ref(),
            current_gen,
        );
    }

    let chunk_size = (num_pages as usize + num_threads - 1) / num_threads;
    let mut handles = Vec::new();

    for t in 0..num_threads {
        let start_p = (t * chunk_size) as i32;
        let end_p = ((t + 1) * chunk_size).min(num_pages as usize) as i32;
        if start_p >= end_p {
            break;
        }

        let file_path = file_path.to_string();
        let needle = needle_trimmed.to_string();
        let cancel_gen = cancel_gen.clone();

        if let Ok(handle) = std::thread::Builder::new()
            .name(format!("pdf-search-{t}"))
            .spawn(move || {
                search_document_range(
                    &file_path,
                    start_p,
                    end_p,
                    &needle,
                    match_case,
                    match_whole_word,
                    cancel_gen.as_ref(),
                    current_gen,
                )
            })
        {
            handles.push(handle);
        }
    }

    let mut results = Vec::new();
    for handle in handles {
        if let Ok(thread_hits) = handle.join() {
            results.extend(thread_hits);
        }
    }

    if let Some(ref cg) = cancel_gen {
        if cg.load(Ordering::Relaxed) != current_gen {
            return Vec::new();
        }
    }

    // Sort results by page and then by vertical position
    results.sort_by(|a, b| {
        if a.page != b.page {
            a.page.cmp(&b.page)
        } else {
            let ya = a.rects_pt.first().map(|r| r.1).unwrap_or(0.0);
            let yb = b.rects_pt.first().map(|r| r.1).unwrap_or(0.0);
            ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal)
        }
    });

    results
}

struct SearchLineInfo {
    y0: f32,
    y1: f32,
    text: String,
    chars: Vec<(char, f32, f32, f32)>, // (c, cx0, cx1, cy)
}

fn search_document_range(
    file_path: &str,
    start_page: i32,
    end_page: i32,
    needle_trimmed: &str,
    match_case: bool,
    match_whole_word: bool,
    cancel_gen: Option<&Arc<AtomicU64>>,
    current_gen: u64,
) -> Vec<SearchHitResult> {
    let doc = match mupdf::Document::open(file_path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut results = Vec::new();

    for p in start_page..end_page {
        if let Some(cg) = cancel_gen {
            if cg.load(Ordering::Relaxed) != current_gen {
                return Vec::new();
            }
        }

        let page = match doc.load_page(p) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let tp = match page.to_text_page(mupdf::TextPageFlags::ACCURATE_BBOXES) {
            Ok(tp) => tp,
            Err(_) => continue,
        };

        let mut page_hits: Vec<Vec<(f32, f32, f32, f32)>> = Vec::new();
        let _ = tp.search_cb(needle_trimmed, &mut page_hits, |acc, quads| {
            let line_rects = merge_quads_to_line_rects(quads);
            if !line_rects.is_empty() {
                acc.push(line_rects);
            }
            mupdf::text_page::SearchHitResponse::ContinueSearch
        });

        // Fast path: if this page has 0 matches, skip expensive block/line/char parsing!
        if page_hits.is_empty() {
            continue;
        }

        // Only parse text blocks and lines when matches exist on this page
        let mut lines_info: Vec<SearchLineInfo> = Vec::new();
        for block in tp.blocks() {
            if block.r#type() != mupdf::text_page::TextBlockType::Text {
                continue;
            }
            for line in block.lines() {
                let mut line_str = String::new();
                let b = line.bounds();
                let mut line_chars = Vec::new();
                for ch in line.chars() {
                    if let Some(c) = ch.char() {
                        line_str.push(c);
                        let q = ch.quad();
                        let cx0 = q.ul.x.min(q.ll.x);
                        let cx1 = q.ur.x.max(q.lr.x);
                        let cy0 = q.ul.y.min(q.ur.y);
                        let cy1 = q.ll.y.max(q.lr.y);
                        line_chars.push((c, cx0, cx1, (cy0 + cy1) * 0.5));
                    }
                }
                let trimmed = line_str.trim().to_string();
                if !trimmed.is_empty() {
                    lines_info.push(SearchLineInfo {
                        y0: b.y0,
                        y1: b.y1,
                        text: trimmed,
                        chars: line_chars,
                    });
                }
            }
        }

        for hit_rects in page_hits {
            if hit_rects.is_empty() {
                continue;
            }

            let mid_y = (hit_rects[0].1 + hit_rects[0].3) * 0.5;
            let hit_x0 = hit_rects[0].0;
            let mut best_snippet = String::new();
            let mut min_dist = f32::MAX;
            let mut best_line_idx = None;

            for (idx, line) in lines_info.iter().enumerate() {
                if mid_y >= (line.y0 - 2.0) && mid_y <= (line.y1 + 2.0) {
                    best_snippet = line.text.clone();
                    best_line_idx = Some(idx);
                    break;
                }
                let dist = if mid_y < line.y0 { line.y0 - mid_y } else { mid_y - line.y1 };
                if dist < min_dist {
                    min_dist = dist;
                    best_snippet = line.text.clone();
                    best_line_idx = Some(idx);
                }
            }

            if best_snippet.is_empty() {
                best_snippet = needle_trimmed.to_string();
            }

            let needle_char_count = needle_trimmed.chars().count();
            let mut matched_text = needle_trimmed.to_string();
            let mut matched_line_and_range = None;

            if let Some(line_idx) = best_line_idx {
                let line = &lines_info[line_idx];
                // Find character in line closest to hit_x0
                let mut best_char_idx = None;
                let mut min_x_diff = f32::MAX;

                for (ci, &(_c, cx0, _, _)) in line.chars.iter().enumerate() {
                    let diff = (cx0 - hit_x0).abs();
                    if diff < min_x_diff {
                        min_x_diff = diff;
                        best_char_idx = Some(ci);
                    }
                }

                if let Some(start_ci) = best_char_idx {
                    if start_ci + needle_char_count <= line.chars.len() {
                        let candidate: String = line.chars[start_ci..start_ci + needle_char_count]
                            .iter()
                            .map(|c| c.0)
                            .collect();
                        if candidate.eq_ignore_ascii_case(needle_trimmed) {
                            matched_text = candidate;
                            matched_line_and_range = Some((line_idx, start_ci, start_ci + needle_char_count));
                        }
                    }
                }
            }

            // Check Case Sensitivity modifier
            if match_case && matched_text != needle_trimmed {
                continue;
            }

            // Check Whole Word modifier
            if match_whole_word {
                if let Some((line_idx, start_ci, end_ci)) = matched_line_and_range {
                    let line = &lines_info[line_idx];
                    // Preceding character check
                    if start_ci > 0 {
                        let prev_c = line.chars[start_ci - 1].0;
                        if prev_c.is_alphanumeric() || prev_c == '_' {
                            continue;
                        }
                    }
                    // Trailing character check
                    if end_ci < line.chars.len() {
                        let next_c = line.chars[end_ci].0;
                        if next_c.is_alphanumeric() || next_c == '_' {
                            continue;
                        }
                    }
                }
            }

            results.push(SearchHitResult {
                page: p,
                snippet: best_snippet,
                match_text: matched_text,
                rects_pt: hit_rects,
            });
        }
    }

    results
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
                            let text = if !a.comment.is_empty() { &a.comment } else { &a.snippet };
                            if !text.is_empty() {
                                let _ = annot.set_contents(text);
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
                            let text = if !a.comment.is_empty() { &a.comment } else { &a.snippet };
                            if !text.is_empty() {
                                let _ = annot.set_contents(text);
                            }
                            let _ = annot.update();
                        }
                    }
                }
                "strikethrough" => {
                    if !quads.is_empty() {
                        if let Ok(mut annot) = page.add_strikeout_annotation(quads) {
                            let _ = annot.set_color(color);
                            let text = if !a.comment.is_empty() { &a.comment } else { &a.snippet };
                            if !text.is_empty() {
                                let _ = annot.set_contents(text);
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
    fn test_stream_based_selection_downward() {
        fn make_test_line(text: &str, y0: f32, y1: f32, start_x: f32) -> PageLine {
            let mut chars = Vec::new();
            let mut cur_x = start_x;
            for c in text.chars() {
                let char_w = 6.0;
                chars.push(PageChar {
                    ch: c,
                    x0: cur_x,
                    x1: cur_x + char_w,
                    y0,
                    y1,
                    baseline_y: y1 - 2.0,
                });
                cur_x += char_w;
            }
            PageLine {
                x0: start_x,
                x1: cur_x,
                y0,
                y1,
                baseline_y: y1 - 2.0,
                chars,
            }
        }

        let l0 = make_test_line("which is paralleled exactly in Colossians 1:4. Every Christian both believes", 100.0, 115.0, 50.0);
        let l1 = make_test_line("and loves. Faith and love are basic Christian graces, as also is hope, the", 120.0, 135.0, 50.0);
        let l2 = make_test_line("third member of the triad, which has already been mentioned in verse 12", 140.0, 155.0, 50.0);
        let l3 = make_test_line("too, in this case Jews and Gentiles without distinction).", 160.0, 175.0, 50.0);
        let l4 = make_test_line("1 Barth, Ephesians, I, p. 146.", 220.0, 235.0, 50.0); // Footnote

        let text_data = PageTextData {
            lines: vec![l0, l1, l2, l3, l4],
        };

        // User starts at "which" (x = 52.0, y = 105.0) and drags STRAIGHT DOWN to "Jews and" (x = 210.0, y = 168.0)
        let (snippet, rects) = compute_text_selection(&text_data, 52.0, 105.0, 210.0, 168.0);

        // 1. Footnote (line 4) must NOT be selected
        assert_eq!(rects.len(), 4);
        assert!(!snippet.contains("Barth"));

        // 2. Line 0 must be selected through to the end of the line
        assert!(snippet.contains("in Colossians 1:4"));
        assert!(snippet.contains("both believes"));

        // 3. Middle lines (1 and 2) must be 100% fully selected
        assert!(snippet.contains("and loves. Faith and love are basic Christian graces, as also is hope, the"));
        assert!(snippet.contains("third member of the triad, which has already been mentioned in verse 12"));

        // 4. Line 3 must end at "Jews and" and NOT include "without distinction"
        assert!(snippet.contains("too, in this case Jews and"));
        assert!(!snippet.contains("without distinction"));
    }

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

            // Check color of "Most commentators" if present
            if let Some(most_comm) = reloaded2.iter().find(|a| a.snippet.contains("Most commentators")) {
                assert_eq!(most_comm.color_hex, "#ffd09b", "Color must be soft pastel peach/orange");
            }

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

    #[test]
    fn test_search_document() {
        let test_pdf_path = "/tmp/folio_search_test.pdf";
        let path_str = test_pdf_path;
        {
            let mut doc = mupdf::pdf::PdfDocument::new();
            doc.new_page((500.0, 700.0)).unwrap();
            doc.save(path_str).unwrap();
        }

        let hits = search_document(path_str, 1, "test", false, false, None, 0);
        assert_eq!(hits.len(), 0);

        let ephesians_path = "/home/twof/Documents/The Message of Ephesians (John R. W. Stott) (z-library.sk, 1lib.sk, z-lib.sk).pdf";
        if std::path::Path::new(ephesians_path).exists() {
            let start = std::time::Instant::now();
            let ephesians_hits = search_document(ephesians_path, 273, "Ephesians", false, false, None, 0);
            let elapsed = start.elapsed();
            println!("Found {} occurrences of 'Ephesians' across all 273 pages in {:?}", ephesians_hits.len(), elapsed);
            assert!(!ephesians_hits.is_empty(), "Should find 'Ephesians'");
            assert!(!ephesians_hits[0].snippet.is_empty());
            assert_eq!(ephesians_hits[0].rects_pt.len(), 1, "Word on one line must be merged into 1 single bounding block");

            // Test case-sensitive search
            let case_hits = search_document(ephesians_path, 273, "ephesians", true, false, None, 0);
            assert_eq!(case_hits.len(), 20, "Should find 20 lowercase 'ephesians' occurrences");
            for h in &case_hits {
                assert_eq!(h.match_text, "ephesians");
            }

            // Test whole-word search vs substring
            let whole_hits = search_document(ephesians_path, 273, "plan", false, true, None, 0);
            let sub_hits = search_document(ephesians_path, 273, "plan", false, false, None, 0);
            assert_eq!(whole_hits.len(), 28, "Should find 28 whole-word 'plan' occurrences");
            assert_eq!(sub_hits.len(), 51, "Should find 51 substring 'plan' occurrences");
            assert!(whole_hits.len() <= sub_hits.len(), "Whole word matches should be a subset of substring matches");
        }

        let _ = std::fs::remove_file(test_pdf_path);
    }

    #[test]
    fn test_page_rotation() {
        let ephesians_path = "/home/twof/Documents/The Message of Ephesians (John R. W. Stott) (z-library.sk, 1lib.sk, z-lib.sk).pdf";
        if std::path::Path::new(ephesians_path).exists() {
            let doc = mupdf::Document::open(ephesians_path).unwrap();
            let page = doc.load_page(0).unwrap();

            let m0 = mupdf::Matrix::new_scale(1.0, 1.0);
            let pix0 = page.to_pixmap(&m0, &mupdf::Colorspace::device_rgb(), false, false).unwrap();
            let (w0, h0) = (pix0.width(), pix0.height());
            println!("0 deg dimensions: ({w0}, {h0})");

            let mut m90 = mupdf::Matrix::new_scale(1.0, 1.0);
            m90.rotate(90.0);
            let pix90 = page.to_pixmap(&m90, &mupdf::Colorspace::device_rgb(), false, false).unwrap();
            let (w90, h90) = (pix90.width(), pix90.height());
            println!("90 deg dimensions: ({w90}, {h90})");

            let mut m180 = mupdf::Matrix::new_scale(1.0, 1.0);
            m180.rotate(180.0);
            let pix180 = page.to_pixmap(&m180, &mupdf::Colorspace::device_rgb(), false, false).unwrap();
            let (w180, h180) = (pix180.width(), pix180.height());
            println!("180 deg dimensions: ({w180}, {h180})");

            let mut m270 = mupdf::Matrix::new_scale(1.0, 1.0);
            m270.rotate(270.0);
            let pix270 = page.to_pixmap(&m270, &mupdf::Colorspace::device_rgb(), false, false).unwrap();
            let (w270, h270) = (pix270.width(), pix270.height());
            println!("270 deg dimensions: ({w270}, {h270})");

            assert_eq!(w90, h0, "90 deg rotation width must equal 0 deg height");
            assert_eq!(h90, w0, "90 deg rotation height must equal 0 deg width");
            assert_eq!(w180, w0, "180 deg rotation width must equal 0 deg width");
            assert_eq!(h180, h0, "180 deg rotation height must equal 0 deg height");
            assert_eq!(w270, h0, "270 deg rotation width must equal 0 deg height");
            assert_eq!(h270, w0, "270 deg rotation height must equal 0 deg width");
        }
    }
}

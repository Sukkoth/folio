use crate::pdf::TocItem;

#[derive(Debug, Clone)]
pub struct ExportAnnotation {
    pub page: i32, // 0-indexed
    pub kind: String, // "highlight", "underline", "strikethrough", "note"
    pub snippet: String,
    pub comment: String,
    pub min_y_pt: f32,
}

/// Formats a kind identifier into a clean type word.
fn format_kind_word(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "underline" => "Underline",
        "strikethrough" => "Strikethrough",
        "note" => "Note",
        _ => "Highlight",
    }
}

/// Finds the most relevant TOC heading for a given page.
/// If the active item is a child (depth > 0), resolves the parent name to produce "Chapter — Section".
fn resolve_toc_heading(page: i32, toc: &[TocItem]) -> Option<String> {
    if toc.is_empty() {
        return None;
    }

    // Find the last item whose starting page <= page
    let active_idx = toc.iter().rposition(|item| item.page <= page)?;
    let active_item = &toc[active_idx];

    if active_item.depth == 0 {
        Some(format!("{} (p. {})", active_item.title, active_item.page + 1))
    } else {
        // Look backwards for the parent with lower depth
        let mut parent_title = None;
        for item in toc[..active_idx].iter().rev() {
            if item.depth < active_item.depth {
                parent_title = Some(item.title.as_str());
                break;
            }
        }

        if let Some(parent) = parent_title {
            Some(format!(
                "{} — {} (p. {})",
                parent,
                active_item.title,
                active_item.page + 1
            ))
        } else {
            Some(format!("{} (p. {})", active_item.title, active_item.page + 1))
        }
    }
}

/// Generates a Markdown document grouping annotations by outline chapter/section and page.
pub fn generate_annotations_markdown(
    doc_title: &str,
    annotations: &[ExportAnnotation],
    toc: &[TocItem],
) -> String {
    let mut out = String::new();
    let title = if doc_title.trim().is_empty() {
        "Untitled Document"
    } else {
        doc_title.trim()
    };

    out.push_str(&format!("# {}\n\n", title));

    if annotations.is_empty() {
        out.push_str("*No annotations found.*\n");
        return out;
    }

    // Sort annotations by page ascending, then by vertical position (top to bottom)
    let mut sorted_annots = annotations.to_vec();
    sorted_annots.sort_by(|a, b| {
        a.page
            .cmp(&b.page)
            .then_with(|| a.min_y_pt.partial_cmp(&b.min_y_pt).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut current_heading: Option<String> = None;
    let mut current_page: Option<i32> = None;

    for annot in &sorted_annots {
        // Check if TOC heading changed
        if !toc.is_empty() {
            let heading_for_annot = resolve_toc_heading(annot.page, toc);
            if heading_for_annot != current_heading {
                current_heading = heading_for_annot;
                if let Some(ref heading) = current_heading {
                    out.push_str(&format!("## {}\n\n", heading));
                }
                // Reset page header tracking when chapter changes
                current_page = None;
            }
        }

        // Check if Page header changed
        if current_page != Some(annot.page) {
            current_page = Some(annot.page);
            out.push_str(&format!("### Page {}\n", annot.page + 1));
        }

        let type_word = format_kind_word(&annot.kind);
        let trimmed_snippet = annot.snippet.trim();
        let trimmed_comment = annot.comment.trim();
        let is_note_kind = annot.kind.eq_ignore_ascii_case("note");

        if is_note_kind {
            let note_text = if !trimmed_comment.is_empty() {
                trimmed_comment
            } else {
                trimmed_snippet
            };

            // If snippet exists and is distinct from the note comment, show snippet in quote and note underneath
            if !trimmed_snippet.is_empty() && !trimmed_comment.is_empty() && trimmed_snippet != trimmed_comment {
                let mut lines = trimmed_snippet.lines();
                if let Some(first) = lines.next() {
                    out.push_str(&format!("> [{}] {}\n", type_word, first));
                    for line in lines {
                        out.push_str(&format!("> {}\n", line));
                    }
                }
                out.push_str(&format!("Note: {}\n", trimmed_comment));
            } else {
                // Standalone note: output ONLY the note text in the blockquote, no duplicate "Note:" line
                let mut lines = note_text.lines();
                if let Some(first) = lines.next() {
                    out.push_str(&format!("> [{}] {}\n", type_word, first));
                    for line in lines {
                        out.push_str(&format!("> {}\n", line));
                    }
                } else {
                    out.push_str(&format!("> [{}]\n", type_word));
                }
            }
        } else {
            // Highlight, underline, strikethrough
            let mut lines = trimmed_snippet.lines();
            if let Some(first) = lines.next() {
                out.push_str(&format!("> [{}] {}\n", type_word, first));
                for line in lines {
                    out.push_str(&format!("> {}\n", line));
                }
            } else {
                out.push_str(&format!("> [{}]\n", type_word));
            }

            if !trimmed_comment.is_empty() && trimmed_comment != trimmed_snippet {
                out.push_str(&format!("Note: {}\n", trimmed_comment));
            }
        }

        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_export_empty_annotations() {
        let md = generate_annotations_markdown("Sample Document", &[], &[]);
        assert_eq!(md, "# Sample Document\n\n*No annotations found.*\n");
    }

    #[test]
    fn test_export_without_toc() {
        let annots = vec![
            ExportAnnotation {
                page: 13,
                kind: "highlight".to_string(),
                snippet: "Paul's focus in Ephesians is God's plan.".to_string(),
                comment: "Important thesis.".to_string(),
                min_y_pt: 100.0,
            },
            ExportAnnotation {
                page: 41,
                kind: "underline".to_string(),
                snippet: "For by grace you have been saved.".to_string(),
                comment: "".to_string(),
                min_y_pt: 50.0,
            },
        ];

        let md = generate_annotations_markdown("The Message of Ephesians", &annots, &[]);
        let expected = "\
# The Message of Ephesians

### Page 14
> [Highlight] Paul's focus in Ephesians is God's plan.
Note: Important thesis.

### Page 42
> [Underline] For by grace you have been saved.

";
        assert_eq!(md, expected);
    }

    #[test]
    fn test_export_with_toc_and_chapter_grouping() {
        let toc = vec![
            TocItem {
                id: 1,
                title: "Chapter 1: The New Society".to_string(),
                page: 0,
                depth: 0,
                has_children: false,
            },
            TocItem {
                id: 2,
                title: "Chapter 2: One New Humanity".to_string(),
                page: 25,
                depth: 0,
                has_children: false,
            },
        ];

        let annots = vec![
            ExportAnnotation {
                page: 13, // Page 14
                kind: "highlight".to_string(),
                snippet: "Paul's focus in Ephesians is God's cosmic plan to unite all things in Christ.".to_string(),
                comment: "Crucial summary for Chapter 1 thesis.".to_string(),
                min_y_pt: 200.0,
            },
            ExportAnnotation {
                page: 41, // Page 42
                kind: "underline".to_string(),
                snippet: "For by grace you have been saved through faith.".to_string(),
                comment: "".to_string(),
                min_y_pt: 150.0,
            },
        ];

        let md = generate_annotations_markdown("The Message of Ephesians", &annots, &toc);
        let expected = "\
# The Message of Ephesians

## Chapter 1: The New Society (p. 1)

### Page 14
> [Highlight] Paul's focus in Ephesians is God's cosmic plan to unite all things in Christ.
Note: Crucial summary for Chapter 1 thesis.

## Chapter 2: One New Humanity (p. 26)

### Page 42
> [Underline] For by grace you have been saved through faith.

";
        assert_eq!(md, expected);
    }

    #[test]
    fn test_export_nested_toc_hierarchy() {
        let toc = vec![
            TocItem {
                id: 1,
                title: "Part I".to_string(),
                page: 0,
                depth: 0,
                has_children: true,
            },
            TocItem {
                id: 2,
                title: "Section 1.1 Foundations".to_string(),
                page: 4,
                depth: 1,
                has_children: false,
            },
        ];

        let annots = vec![ExportAnnotation {
            page: 5,
            kind: "note".to_string(),
            snippet: "Early doctrine".to_string(),
            comment: "Verify Greek text".to_string(),
            min_y_pt: 50.0,
        }];

        let md = generate_annotations_markdown("Book Title", &annots, &toc);
        assert!(md.contains("## Part I — Section 1.1 Foundations (p. 5)"));
        assert!(md.contains("### Page 6"));
        assert!(md.contains("> [Note] Early doctrine\nNote: Verify Greek text"));
    }

    #[test]
    fn test_export_sorting_order_and_multiline() {
        let annots = vec![
            ExportAnnotation {
                page: 2,
                kind: "highlight".to_string(),
                snippet: "Lower down on page 3\nSecond line".to_string(),
                comment: "".to_string(),
                min_y_pt: 300.0,
            },
            ExportAnnotation {
                page: 2,
                kind: "underline".to_string(),
                snippet: "Higher up on page 3".to_string(),
                comment: "First note".to_string(),
                min_y_pt: 120.0,
            },
            ExportAnnotation {
                page: 0,
                kind: "highlight".to_string(),
                snippet: "Page 1 quote".to_string(),
                comment: "".to_string(),
                min_y_pt: 200.0,
            },
        ];

        let md = generate_annotations_markdown("Doc", &annots, &[]);
        let expected = "\
# Doc

### Page 1
> [Highlight] Page 1 quote

### Page 3
> [Underline] Higher up on page 3
Note: First note

> [Highlight] Lower down on page 3
> Second line

";
        assert_eq!(md, expected);
    }

    #[test]
    fn test_export_standalone_note_without_snippet() {
        let annots = vec![ExportAnnotation {
            page: 4,
            kind: "note".to_string(),
            snippet: "".to_string(),
            comment: "Remember to review this argument later.".to_string(),
            min_y_pt: 100.0,
        }];

        let md = generate_annotations_markdown("Doc", &annots, &[]);
        let expected = "\
# Doc

### Page 5
> [Note] Remember to review this argument later.

";
        assert_eq!(md, expected);
    }

    #[test]
    fn test_export_standalone_note_where_snippet_equals_comment() {
        let annots = vec![
            ExportAnnotation {
                page: 14,
                kind: "note".to_string(),
                snippet: "Then another".to_string(),
                comment: "Then another".to_string(),
                min_y_pt: 100.0,
            },
            ExportAnnotation {
                page: 14,
                kind: "note".to_string(),
                snippet: "then another".to_string(),
                comment: "then another".to_string(),
                min_y_pt: 150.0,
            },
        ];

        let md = generate_annotations_markdown("Doc", &annots, &[]);
        let expected = "\
# Doc

### Page 15
> [Note] Then another

> [Note] then another

";
        assert_eq!(md, expected);
    }
}

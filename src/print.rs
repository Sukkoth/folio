use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct PrintJobOptions {
    pub printer: Option<String>,
    pub page_mode: i32, // 0 = all, 1 = current page, 2 = custom range
    pub current_page: i32, // 0-indexed
    pub page_range: String, // e.g. "1-3, 5, 8"
    pub copies: i32,
}

pub fn list_available_printers() -> Vec<String> {
    if let Ok(output) = Command::new("lpstat").arg("-e").output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let printers: Vec<String> = text
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !printers.is_empty() {
                return printers;
            }
        }
    }

    if let Ok(output) = Command::new("lpstat").arg("-p").output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut printers = Vec::new();
            for line in text.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[0] == "printer" {
                    printers.push(parts[1].to_string());
                }
            }
            if !printers.is_empty() {
                return printers;
            }
        }
    }

    Vec::new()
}

pub fn build_lp_args(
    file_path: &str,
    options: &PrintJobOptions,
) -> Result<Vec<String>, String> {
    let mut args = Vec::new();

    if let Some(ref p) = options.printer {
        let trimmed = p.trim();
        if !trimmed.is_empty() && trimmed != "Default System Printer" && trimmed != "Default Printer" {
            args.push("-d".to_string());
            args.push(trimmed.to_string());
        }
    }

    let copies = options.copies.max(1);
    if copies > 1 {
        args.push("-n".to_string());
        args.push(copies.to_string());
    }

    match options.page_mode {
        0 => {
            // All pages
        }
        1 => {
            // Current page (1-indexed for CUPS lp)
            let page_num = options.current_page + 1;
            args.push("-P".to_string());
            args.push(page_num.to_string());
        }
        2 => {
            // Custom page range
            let range = options.page_range.replace(' ', "");
            if range.is_empty() {
                return Err("Please specify a page range (e.g. 1-3, 5)".to_string());
            }
            if !range.chars().all(|c| c.is_ascii_digit() || c == '-' || c == ',') {
                return Err("Invalid page range format. Use numbers, commas, and hyphens (e.g. 1-3, 5)".to_string());
            }
            args.push("-P".to_string());
            args.push(range);
        }
        _ => {}
    }

    args.push(file_path.to_string());
    Ok(args)
}

pub fn execute_print_job(
    file_path: &str,
    password: Option<&str>,
    options: &PrintJobOptions,
) -> Result<String, String> {
    if !std::path::Path::new(file_path).exists() {
        return Err(format!("Document file not found: {file_path}"));
    }

    // If document is password-protected, create a temporary decrypted copy for CUPS lp
    let (target_path, temp_cleanup) = if let Some(pw) = password {
        match crate::pdf::open_pdf_document(file_path, Some(pw)) {
            Ok(doc) => {
                let temp_path = std::env::temp_dir().join(format!("folio_print_{}.pdf", std::process::id()));
                let opts = mupdf::pdf::PdfWriteOptions::default();
                if let Err(e) = doc.save_with_options(&temp_path.to_string_lossy(), opts) {
                    return Err(format!("Failed to prepare encrypted document for printing: {e}"));
                }
                (temp_path.to_string_lossy().to_string(), Some(temp_path))
            }
            Err(_) => (file_path.to_string(), None),
        }
    } else {
        (file_path.to_string(), None)
    };

    let args = match build_lp_args(&target_path, options) {
        Ok(a) => a,
        Err(e) => {
            if let Some(temp) = temp_cleanup {
                let _ = std::fs::remove_file(temp);
            }
            return Err(e);
        }
    };

    let output = Command::new("lp")
        .args(&args)
        .output()
        .map_err(|e| {
            if let Some(ref temp) = temp_cleanup {
                let _ = std::fs::remove_file(temp);
            }
            format!("Failed to invoke printing system (lp): {e}")
        })?;

    // Cleanup temp file if created
    if let Some(temp) = temp_cleanup {
        let _ = std::fs::remove_file(temp);
    }

    if output.status.success() {
        let msg = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if msg.is_empty() {
            Ok("Print job submitted successfully".to_string())
        } else {
            Ok(format!("Print job submitted: {msg}"))
        }
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("lp failed with exit status {:?}", output.status.code())
        } else {
            err
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_lp_args_all_pages() {
        let opts = PrintJobOptions {
            printer: Some("OfficeJet".to_string()),
            page_mode: 0,
            current_page: 2,
            page_range: "".to_string(),
            copies: 1,
        };
        let args = build_lp_args("/tmp/doc.pdf", &opts).unwrap();
        assert_eq!(args, vec!["-d", "OfficeJet", "/tmp/doc.pdf"]);
    }

    #[test]
    fn test_build_lp_args_current_page_and_copies() {
        let opts = PrintJobOptions {
            printer: Some("Default System Printer".to_string()),
            page_mode: 1,
            current_page: 4, // 0-indexed -> page 5
            page_range: "".to_string(),
            copies: 3,
        };
        let args = build_lp_args("/tmp/doc.pdf", &opts).unwrap();
        assert_eq!(args, vec!["-n", "3", "-P", "5", "/tmp/doc.pdf"]);
    }

    #[test]
    fn test_build_lp_args_custom_range() {
        let opts = PrintJobOptions {
            printer: None,
            page_mode: 2,
            current_page: 0,
            page_range: "1-3, 5, 8".to_string(),
            copies: 1,
        };
        let args = build_lp_args("/tmp/doc.pdf", &opts).unwrap();
        assert_eq!(args, vec!["-P", "1-3,5,8", "/tmp/doc.pdf"]);
    }

    #[test]
    fn test_build_lp_args_invalid_range() {
        let opts = PrintJobOptions {
            printer: None,
            page_mode: 2,
            current_page: 0,
            page_range: "1-3; rm -rf /".to_string(),
            copies: 1,
        };
        let res = build_lp_args("/tmp/doc.pdf", &opts);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("Invalid page range"));
    }
}

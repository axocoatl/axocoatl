//! File-content extractors for uploaded attachments.
//!
//! Runs **once at upload time** inside `FileStore::store_with`. The extracted
//! text is cached on `FileEntry` so subsequent chat turns inline it directly
//! without re-parsing.
//!
//! Each extractor is best-effort: any failure logs a warning and returns
//! `None`. The original bytes always survive on disk, so we can re-extract
//! later if we change parsers.

use std::io::Write;

/// Main entry point: dispatch on MIME (and fall through to the file
/// extension if MIME is generic). Returns `(extracted_text, ocr_text)`.
///
/// - `extracted_text`: parsed content for inlining into the prompt
///   (PDF body, CSV/XLSX cells as TSV, plain-text contents).
/// - `ocr_text`: tesseract output for images; `None` if tesseract isn't on
///   PATH or yielded nothing useful.
pub fn extract(bytes: &[u8], mime: &str, original_name: &str) -> (Option<String>, Option<String>) {
    let lower_mime = mime.to_lowercase();
    let ext = std::path::Path::new(original_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // PDF
    if lower_mime == "application/pdf" || ext == "pdf" {
        return (extract_pdf(bytes), None);
    }

    // CSV — handled with a tiny parser, since calamine's in-memory
    // auto-sniff only covers binary spreadsheet formats.
    if ext == "csv" || lower_mime == "text/csv" {
        return (extract_csv(bytes), None);
    }

    // Other spreadsheet formats (xlsx, xls, xlsb, ods) via calamine.
    if matches!(ext.as_str(), "xlsx" | "xlsm" | "xlsb" | "xls" | "ods")
        || lower_mime.contains("spreadsheet")
    {
        return (extract_spreadsheet(bytes, &ext), None);
    }

    // Plain text — store the contents directly (within a reasonable cap so
    // a giant log file doesn't balloon the metadata sidecar).
    if lower_mime.starts_with("text/") || matches!(ext.as_str(), "md" | "rst" | "txt" | "log") {
        if let Ok(s) = std::str::from_utf8(bytes) {
            let s = if s.len() > 256 * 1024 {
                format!("{}\n\n…[truncated at 256 KB]", &s[..256 * 1024])
            } else {
                s.to_string()
            };
            return (Some(s), None);
        }
    }

    // Images — attempt OCR via the `tesseract` CLI if it's on PATH.
    // Tesseract isn't a hard dep; missing binary just means no OCR text.
    if lower_mime.starts_with("image/") {
        return (None, ocr_image(bytes, &ext));
    }

    (None, None)
}

/// PDF text extraction via the pure-Rust `pdf-extract` crate.
fn extract_pdf(bytes: &[u8]) -> Option<String> {
    // pdf-extract writes to a buffer; some malformed PDFs make it panic, so
    // catch_unwind keeps the daemon healthy if a user uploads a bad file.
    let result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match result {
        Ok(Ok(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "pdf-extract failed");
            None
        }
        Err(_) => {
            tracing::warn!("pdf-extract panicked on input");
            None
        }
    }
}

/// Minimal CSV → TSV-style text conversion. Handles quoted fields, escaped
/// quotes, and embedded commas. Not RFC 4180 perfect — good enough to feed
/// an LLM, which is the only consumer here.
fn extract_csv(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut out = String::from("## Sheet: csv\n");
    let mut field = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                row.push(std::mem::take(&mut field));
            }
            '\n' if !in_quotes => {
                row.push(std::mem::take(&mut field));
                out.push_str(&row.join("\t"));
                out.push('\n');
                row.clear();
            }
            '\r' if !in_quotes => {}
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        out.push_str(&row.join("\t"));
        out.push('\n');
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// xlsx/xls/ods → TSV-style text, one sheet at a time with a header.
fn extract_spreadsheet(bytes: &[u8], ext: &str) -> Option<String> {
    use calamine::{open_workbook_auto_from_rs, Data, Reader};
    let cursor = std::io::Cursor::new(bytes);
    let mut wb = match open_workbook_auto_from_rs(cursor) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, ext, "calamine open_workbook failed");
            return None;
        }
    };
    let sheet_names = wb.sheet_names().to_vec();
    let mut out = String::new();
    for name in sheet_names {
        let Ok(range) = wb.worksheet_range(&name) else {
            continue;
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("## Sheet: {name}\n"));
        for row in range.rows() {
            let line: Vec<String> = row
                .iter()
                .map(|c| match c {
                    Data::Empty => String::new(),
                    Data::String(s) => s.clone(),
                    Data::Float(f) => format!("{f}"),
                    Data::Int(i) => format!("{i}"),
                    Data::Bool(b) => b.to_string(),
                    Data::DateTime(d) => format!("{d:?}"),
                    Data::DurationIso(s) => s.clone(),
                    Data::DateTimeIso(s) => s.clone(),
                    Data::Error(e) => format!("#{e:?}"),
                })
                .collect();
            out.push_str(&line.join("\t"));
            out.push('\n');
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Run `tesseract` over the image bytes. We feed it stdin so we don't pollute
/// the filesystem with temp images. Returns None if tesseract isn't on PATH,
/// the binary fails, or the output is empty.
fn ocr_image(bytes: &[u8], ext: &str) -> Option<String> {
    // We write to a temp file because tesseract reads images by path; piping
    // to stdin works for stdout-text-mode but is finicky across versions.
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!(
        "axo-ocr-{}.{}",
        std::process::id(),
        if ext.is_empty() { "img" } else { ext }
    ));
    let mut f = match std::fs::File::create(&tmp_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "ocr: tempfile create failed");
            return None;
        }
    };
    if let Err(e) = f.write_all(bytes) {
        tracing::warn!(error = %e, "ocr: tempfile write failed");
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }
    drop(f);
    // `tesseract <path> stdout -l eng quiet`
    let out = std::process::Command::new("tesseract")
        .arg(&tmp_path)
        .arg("stdout")
        .arg("-l")
        .arg("eng")
        .arg("quiet")
        .output();
    let _ = std::fs::remove_file(&tmp_path);
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Ok(o) => {
            tracing::debug!(stderr = %String::from_utf8_lossy(&o.stderr), "tesseract returned non-zero");
            None
        }
        Err(e) => {
            // ENOENT just means tesseract isn't installed — log once at info,
            // not every upload, to keep the noise down.
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::debug!("tesseract not on PATH; OCR skipped");
            } else {
                tracing::warn!(error = %e, "tesseract invocation failed");
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_extracts_inline() {
        let (text, ocr) = extract(b"hello world", "text/plain", "note.txt");
        assert_eq!(text.as_deref(), Some("hello world"));
        assert!(ocr.is_none());
    }

    #[test]
    fn markdown_is_treated_as_text() {
        let (text, _) = extract(b"# Title\n\nBody", "application/octet-stream", "readme.md");
        assert!(text.unwrap().contains("Title"));
    }

    #[test]
    fn very_large_text_truncates() {
        let big = "a".repeat(300_000);
        let (text, _) = extract(big.as_bytes(), "text/plain", "big.txt");
        let t = text.unwrap();
        assert!(t.contains("[truncated"));
        assert!(t.len() < big.len() + 100);
    }

    #[test]
    fn csv_extracts_to_tsv() {
        let csv = b"a,b,c\n1,2,3\n4,5,6";
        let (text, _) = extract(csv, "text/csv", "data.csv");
        let t = text.unwrap();
        assert!(t.contains("Sheet:"));
        assert!(t.contains("1\t2\t3") || t.contains("a\tb\tc"));
    }

    #[test]
    fn unknown_binary_yields_nothing() {
        let (text, ocr) = extract(&[0xFFu8, 0xFE, 0xFD], "application/octet-stream", "x.bin");
        assert!(text.is_none() && ocr.is_none());
    }
}

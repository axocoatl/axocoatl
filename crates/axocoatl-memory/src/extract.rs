//! Bounded file-content extractors for uploaded context.
//!
//! Extraction is best-effort and runs once per content-addressed blob. Every
//! textual result is capped before it is persisted so a PDF, spreadsheet, or
//! OCR result cannot make its metadata sidecar or later model context grow
//! without bound. The original bytes remain available for future re-extraction.

use crate::files::{ExtractionMetadata, ExtractionStatus, TextExtractionMetadata};
use std::io::{Read, Write};
#[cfg(unix)]
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(unix)]
use uuid::Uuid;

pub const EXTRACTION_VERSION: u32 = 1;
pub const DEFAULT_TEXT_LIMIT_BYTES: usize = 256 * 1024;
pub const DEFAULT_OCR_LIMIT_BYTES: usize = 256 * 1024;
pub const DEFAULT_OCR_TIMEOUT: Duration = Duration::from_secs(30);

/// Limits used for cached representations. These are byte limits, but the
/// truncator always backs up to a valid UTF-8 character boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionLimits {
    pub extracted_text_bytes: usize,
    pub ocr_text_bytes: usize,
    pub ocr_timeout: Duration,
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            extracted_text_bytes: DEFAULT_TEXT_LIMIT_BYTES,
            ocr_text_bytes: DEFAULT_OCR_LIMIT_BYTES,
            ocr_timeout: DEFAULT_OCR_TIMEOUT,
        }
    }
}

/// Cached extraction payload plus the facts needed to show truncation
/// honestly in a session context chip or transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractionOutput {
    pub extracted_text: Option<String>,
    pub ocr_text: Option<String>,
    pub metadata: ExtractionMetadata,
}

impl ExtractionOutput {
    /// Compatibility adapter for extractors used by the retained Chat API.
    /// The caller receives bounded data even when its closure returns an
    /// unexpectedly large string.
    pub fn from_legacy(value: (Option<String>, Option<String>)) -> Self {
        Self {
            extracted_text: value.0,
            ocr_text: value.1,
            metadata: ExtractionMetadata::default(),
        }
        .bounded(ExtractionLimits::default())
    }

    pub fn bounded(mut self, limits: ExtractionLimits) -> Self {
        let previous_status = self.metadata.status;
        let (extracted_text, extracted_meta) = bound_optional_text(
            self.extracted_text.take(),
            limits.extracted_text_bytes,
            self.metadata.extracted_text.take(),
        );
        let (ocr_text, ocr_meta) = bound_optional_text(
            self.ocr_text.take(),
            limits.ocr_text_bytes,
            self.metadata.ocr_text.take(),
        );
        self.extracted_text = extracted_text;
        self.ocr_text = ocr_text;
        let has_text = self.extracted_text.is_some() || self.ocr_text.is_some();
        self.metadata = ExtractionMetadata {
            version: EXTRACTION_VERSION,
            status: if has_text {
                ExtractionStatus::Complete
            } else if previous_status == ExtractionStatus::Complete {
                ExtractionStatus::Unavailable
            } else {
                previous_status
            },
            extracted_text: extracted_meta,
            ocr_text: ocr_meta,
        };
        self
    }
}

/// Compatibility entry point. New callers that need truncation metadata should
/// use [`extract_with_limits`].
pub fn extract(bytes: &[u8], mime: &str, original_name: &str) -> (Option<String>, Option<String>) {
    let output = extract_with_limits(bytes, mime, original_name, ExtractionLimits::default());
    (output.extracted_text, output.ocr_text)
}

/// Extract a bounded textual representation and return explicit metadata.
pub fn extract_with_limits(
    bytes: &[u8],
    mime: &str,
    original_name: &str,
    limits: ExtractionLimits,
) -> ExtractionOutput {
    let lower_mime = mime.to_lowercase();
    let ext = std::path::Path::new(original_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let applicable = lower_mime == "application/pdf"
        || ext == "pdf"
        || ext == "csv"
        || lower_mime == "text/csv"
        || matches!(ext.as_str(), "xlsx" | "xlsm" | "xlsb" | "xls" | "ods")
        || lower_mime.contains("spreadsheet")
        || lower_mime.starts_with("text/")
        || matches!(ext.as_str(), "md" | "rst" | "txt" | "log")
        || lower_mime.starts_with("image/");
    let (extracted_text, ocr_text) = if lower_mime == "application/pdf" || ext == "pdf" {
        (extract_pdf(bytes), None)
    } else if ext == "csv" || lower_mime == "text/csv" {
        (extract_csv(bytes, limits.extracted_text_bytes), None)
    } else if matches!(ext.as_str(), "xlsx" | "xlsm" | "xlsb" | "xls" | "ods")
        || lower_mime.contains("spreadsheet")
    {
        (
            extract_spreadsheet(bytes, &ext, limits.extracted_text_bytes),
            None,
        )
    } else if lower_mime.starts_with("text/")
        || matches!(ext.as_str(), "md" | "rst" | "txt" | "log")
    {
        (std::str::from_utf8(bytes).ok().map(str::to_owned), None)
    } else if lower_mime.starts_with("image/") {
        (
            None,
            ocr_image(bytes, &ext, limits.ocr_timeout, limits.ocr_text_bytes),
        )
    } else {
        (None, None)
    };

    let has_text = extracted_text.is_some() || ocr_text.is_some();
    ExtractionOutput {
        extracted_text,
        ocr_text,
        metadata: ExtractionMetadata {
            version: EXTRACTION_VERSION,
            status: if has_text {
                ExtractionStatus::Complete
            } else if applicable {
                ExtractionStatus::Unavailable
            } else {
                ExtractionStatus::NotApplicable
            },
            ..ExtractionMetadata::default()
        },
    }
    .bounded(limits)
}

fn extract_pdf(bytes: &[u8]) -> Option<String> {
    let result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match result {
        Ok(Ok(text)) => nonempty(text),
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

/// CSV to a TSV-style representation. Appending stops soon after the limit;
/// `ExtractionOutput::bounded` performs the final UTF-8-safe cut and records
/// truncation. This avoids materializing an unbounded expansion in memory.
fn extract_csv(bytes: &[u8], limit: usize) -> Option<String> {
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
            ',' if !in_quotes => row.push(std::mem::take(&mut field)),
            '\n' if !in_quotes => {
                row.push(std::mem::take(&mut field));
                append_line(&mut out, &row.join("\t"), limit);
                row.clear();
                if output_over_limit(&out, limit) {
                    break;
                }
            }
            '\r' if !in_quotes => {}
            _ => field.push(c),
        }
    }
    if !output_over_limit(&out, limit) && (!field.is_empty() || !row.is_empty()) {
        row.push(field);
        append_line(&mut out, &row.join("\t"), limit);
    }
    nonempty(out)
}

fn extract_spreadsheet(bytes: &[u8], ext: &str, limit: usize) -> Option<String> {
    use calamine::{open_workbook_auto_from_rs, Data, Reader};
    let cursor = std::io::Cursor::new(bytes);
    let mut wb = match open_workbook_auto_from_rs(cursor) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, ext, "calamine open_workbook failed");
            return None;
        }
    };
    let mut out = String::new();
    for name in wb.sheet_names().to_vec() {
        let Ok(range) = wb.worksheet_range(&name) else {
            continue;
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        append_line(&mut out, &format!("## Sheet: {name}"), limit);
        if output_over_limit(&out, limit) {
            break;
        }
        for row in range.rows() {
            let line: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Data::Empty => String::new(),
                    Data::String(s) => s.clone(),
                    Data::Float(f) => format!("{f}"),
                    Data::Int(i) => format!("{i}"),
                    Data::Bool(b) => b.to_string(),
                    Data::DateTime(d) => format!("{d:?}"),
                    Data::DurationIso(s) | Data::DateTimeIso(s) => s.clone(),
                    Data::Error(e) => format!("#{e:?}"),
                })
                .collect();
            append_line(&mut out, &line.join("\t"), limit);
            if output_over_limit(&out, limit) {
                break;
            }
        }
        if output_over_limit(&out, limit) {
            break;
        }
    }
    nonempty(out)
}

fn append_line(out: &mut String, line: &str, limit: usize) {
    if output_over_limit(out, limit) {
        return;
    }
    let remaining = limit.saturating_add(1).saturating_sub(out.len());
    if remaining == 0 {
        return;
    }
    push_prefix(out, line, remaining);
    if out.len() <= limit {
        out.push('\n');
    }
}

fn push_prefix(out: &mut String, value: &str, max_bytes: usize) {
    let end = utf8_boundary_at_or_before(value, max_bytes);
    out.push_str(&value[..end]);
}

fn output_over_limit(out: &str, limit: usize) -> bool {
    out.len() > limit
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn bound_optional_text(
    value: Option<String>,
    max_bytes: usize,
    previous: Option<TextExtractionMetadata>,
) -> (Option<String>, Option<TextExtractionMetadata>) {
    let value = value.and_then(nonempty);
    let Some(value) = value else {
        return (None, None);
    };
    let source_bytes = value.len();
    let truncated = source_bytes > max_bytes;
    let stored = if truncated {
        const MARKER: &str = "\n\n…[truncated]";
        if max_bytes >= MARKER.len() {
            let prefix_limit = max_bytes - MARKER.len();
            let end = utf8_boundary_at_or_before(&value, prefix_limit);
            format!("{}{}", &value[..end], MARKER)
        } else {
            let end = utf8_boundary_at_or_before(&value, max_bytes);
            value[..end].to_string()
        }
    } else {
        value
    };
    let metadata = TextExtractionMetadata {
        truncated: truncated || previous.as_ref().map(|m| m.truncated).unwrap_or(false),
        stored_bytes: stored.len() as u64,
        source_bytes: previous
            .and_then(|metadata| metadata.source_bytes)
            .or(Some(source_bytes as u64)),
    };
    (Some(stored), Some(metadata))
}

fn utf8_boundary_at_or_before(value: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn ocr_image(bytes: &[u8], ext: &str, timeout: Duration, output_limit: usize) -> Option<String> {
    ocr_image_with_command(
        bytes,
        ext,
        timeout,
        output_limit,
        Path::new("tesseract"),
        &std::env::temp_dir(),
    )
}

fn ocr_image_with_command(
    bytes: &[u8],
    ext: &str,
    timeout: Duration,
    output_limit: usize,
    command: &Path,
    temp_root: &Path,
) -> Option<String> {
    #[cfg(unix)]
    {
        ocr_image_with_command_unix(
            bytes,
            ext,
            timeout,
            output_limit,
            command,
            temp_root,
            |_| {},
        )
    }

    // Tesseract supports `stdin` as an input name. Non-Unix platforms do not
    // have a portable inherited-descriptor pathname, so use the pipe itself
    // as the descriptor-anchored handoff rather than reopening a temp path.
    #[cfg(not(unix))]
    {
        let _ = (ext, temp_root);
        let child = Command::new(command)
            .arg("stdin")
            .arg("stdout")
            .arg("-l")
            .arg("eng")
            .arg("quiet")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        finish_ocr_child(child, timeout, output_limit, Some(bytes.to_vec()))
    }
}

#[cfg(unix)]
fn ocr_image_with_command_unix<F>(
    bytes: &[u8],
    ext: &str,
    timeout: Duration,
    output_limit: usize,
    command: &Path,
    temp_root: &Path,
    before_unlink: F,
) -> Option<String>
where
    F: FnOnce(&Path),
{
    let safe_ext =
        if !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            ext
        } else {
            "img"
        };
    let tmp_path = temp_root.join(format!(
        "axo-ocr-{}-{}.{}",
        std::process::id(),
        Uuid::new_v4(),
        safe_ext
    ));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
    let mut file = match options.open(&tmp_path) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(error = %e, "ocr: tempfile create failed");
            return None;
        }
    };
    if let Err(e) = file.write_all(bytes) {
        tracing::warn!(error = %e, "ocr: tempfile write failed");
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        tracing::warn!(error = %e, "ocr: tempfile rewind failed");
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }

    // Tests use this seam to deterministically model another same-UID process
    // unlinking and replacing the name between creation and process launch.
    // Production passes a no-op and removes the pathname before spawning.
    before_unlink(&tmp_path);
    match std::fs::remove_file(&tmp_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(error = %e, "ocr: tempfile unlink failed");
            return None;
        }
    }

    // Map the retained open file to the child's standard-input descriptor.
    // `Command` performs the descriptor inheritance itself, so the parent's
    // CLOEXEC state is never relaxed and unrelated concurrent children cannot
    // inherit the upload. The argument names that exact child descriptor, not
    // the removed ambient pathname.
    #[cfg(target_os = "linux")]
    let input_path = "/proc/self/fd/0";
    #[cfg(not(target_os = "linux"))]
    let input_path = "/dev/fd/0";

    let mut command = Command::new(command);
    command
        .arg(input_path)
        .arg("stdout")
        .arg("-l")
        .arg("eng")
        .arg("quiet")
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    finish_ocr_child(command.spawn(), timeout, output_limit, None)
}

fn finish_ocr_child(
    child: std::io::Result<Child>,
    timeout: Duration,
    output_limit: usize,
    input: Option<Vec<u8>>,
) -> Option<String> {
    match child {
        Ok(mut child) => {
            let input_thread = input.and_then(|input| {
                child.stdin.take().map(|mut stdin| {
                    thread::spawn(move || {
                        stdin.write_all(&input)?;
                        drop(stdin);
                        Ok::<(), std::io::Error>(())
                    })
                })
            });
            let output = wait_with_timeout(child, timeout, output_limit);
            if let Some(input_thread) = input_thread {
                match input_thread.join() {
                    Ok(Ok(())) => output,
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "tesseract stdin write failed");
                        None
                    }
                    Err(_) => {
                        tracing::warn!("tesseract stdin writer panicked");
                        None
                    }
                }
            } else {
                output
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::debug!("tesseract not on PATH; OCR skipped");
            } else {
                tracing::warn!(error = %e, "tesseract invocation failed");
            }
            None
        }
    }
}

fn wait_with_timeout(mut child: Child, timeout: Duration, output_limit: usize) -> Option<String> {
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let stdout_thread = thread::spawn(move || read_bounded(stdout, output_limit));
    let stderr_thread = thread::spawn(move || read_bounded(stderr, 64 * 1024));
    let started = Instant::now();
    let status: ExitStatus = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                tracing::warn!(timeout_ms = timeout.as_millis(), "tesseract timed out");
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "tesseract wait failed");
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return None;
            }
        }
    };
    let stdout = stdout_thread.join().ok()?;
    let stderr = stderr_thread.join().ok()?;
    if status.success() {
        nonempty(String::from_utf8_lossy(&stdout).into_owned())
    } else {
        tracing::debug!(stderr = %String::from_utf8_lossy(&stderr), "tesseract returned non-zero");
        None
    }
}

fn read_bounded<R: Read>(mut reader: R, limit: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(limit.min(16 * 1024));
    let store_limit = limit.saturating_add(1);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let keep = store_limit.saturating_sub(output.len()).min(read);
        output.extend_from_slice(&buffer[..keep]);
        // Keep draining after the cap so the child never blocks on a full or
        // prematurely closed pipe. Excess bytes are deliberately discarded.
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn pdf_fixture(catalog_extra: &str) -> Vec<u8> {
        let content = "BT /F1 12 Tf 72 720 Td (Hello Axocoatl) Tj ET";
        let objects = [
            format!("<< /Type /Catalog /Pages 2 0 R {catalog_extra} >>"),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> /MediaBox [0 0 612 792] /Contents 5 0 R >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ];

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn xlsx_fixture() -> Vec<u8> {
        use std::io::Cursor;
        use zip::write::SimpleFileOptions;

        let files = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Launch" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Ready</t></is></c><c r="B1"><v>1</v></c></row></sheetData>
</worksheet>"#,
            ),
        ];

        let cursor = Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        for (name, contents) in files {
            archive
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            archive.write_all(contents.as_bytes()).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn plain_text_extracts_inline() {
        let output = extract_with_limits(
            b"hello world",
            "text/plain",
            "note.txt",
            ExtractionLimits::default(),
        );
        assert_eq!(output.extracted_text.as_deref(), Some("hello world"));
        assert!(output.ocr_text.is_none());
        assert_eq!(output.metadata.version, EXTRACTION_VERSION);
        assert_eq!(output.metadata.status, ExtractionStatus::Complete);
    }

    #[test]
    fn markdown_is_treated_as_text() {
        let (text, _) = extract(b"# Title\n\nBody", "application/octet-stream", "readme.md");
        assert!(text.unwrap().contains("Title"));
    }

    #[test]
    fn multibyte_text_truncates_on_utf8_boundary_with_metadata() {
        let limits = ExtractionLimits {
            extracted_text_bytes: 5,
            ..ExtractionLimits::default()
        };
        let output = extract_with_limits("éééé".as_bytes(), "text/plain", "big.txt", limits);
        assert_eq!(output.extracted_text.as_deref(), Some("éé"));
        let metadata = output.metadata.extracted_text.unwrap();
        assert!(metadata.truncated);
        assert_eq!(metadata.stored_bytes, 4);
        assert_eq!(metadata.source_bytes, Some(8));
    }

    #[test]
    fn legacy_output_is_bounded() {
        let output =
            ExtractionOutput::from_legacy((Some("a".repeat(DEFAULT_TEXT_LIMIT_BYTES + 100)), None));
        assert_eq!(
            output.extracted_text.unwrap().len(),
            DEFAULT_TEXT_LIMIT_BYTES
        );
        let metadata = output.metadata.extracted_text.unwrap();
        assert!(metadata.truncated);
        assert_eq!(
            metadata.source_bytes,
            Some((DEFAULT_TEXT_LIMIT_BYTES + 100) as u64)
        );
    }

    #[test]
    fn csv_extracts_to_tsv() {
        let (text, _) = extract(b"a,b,c\n1,2,3\n4,5,6", "text/csv", "data.csv");
        let text = text.unwrap();
        assert!(text.contains("Sheet:"));
        assert!(text.contains("1\t2\t3") || text.contains("a\tb\tc"));
    }

    #[test]
    fn pdf_extracts_text_with_the_patched_parser_line() {
        let output = extract_with_limits(
            &pdf_fixture(""),
            "application/pdf",
            "launch.pdf",
            ExtractionLimits::default(),
        );
        assert!(output.extracted_text.unwrap().contains("Hello Axocoatl"));
    }

    #[test]
    fn deeply_nested_pdf_is_rejected_without_aborting() {
        let nested = format!("/X {}0{}", "[".repeat(12_000), "]".repeat(12_000));
        let started = Instant::now();
        let _ = extract_with_limits(
            &pdf_fixture(&nested),
            "application/pdf",
            "nested.pdf",
            ExtractionLimits::default(),
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn xlsx_extracts_cells_with_the_patched_parser_line() {
        let output = extract_with_limits(
            &xlsx_fixture(),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "launch.xlsx",
            ExtractionLimits::default(),
        );
        let text = output.extracted_text.unwrap();
        assert!(text.contains("## Sheet: Launch"));
        assert!(text.contains("Ready\t1"));
    }

    #[test]
    fn bounded_csv_does_not_grow_past_limit_by_a_row() {
        let limits = ExtractionLimits {
            extracted_text_bytes: 32,
            ..ExtractionLimits::default()
        };
        let output = extract_with_limits(
            b"header\nthis is a very long row that must be cut\nnext",
            "text/csv",
            "data.csv",
            limits,
        );
        assert!(output.extracted_text.unwrap().len() <= 32);
        assert!(output.metadata.extracted_text.unwrap().truncated);
    }

    #[test]
    fn unknown_binary_yields_nothing() {
        let output = extract_with_limits(
            &[0xFFu8, 0xFE, 0xFD],
            "application/octet-stream",
            "x.bin",
            ExtractionLimits::default(),
        );
        assert!(output.extracted_text.is_none() && output.ocr_text.is_none());
        assert_eq!(output.metadata.status, ExtractionStatus::NotApplicable);
    }

    #[test]
    fn applicable_invalid_text_reports_unavailable() {
        let output = extract_with_limits(
            &[0xFFu8, 0xFE, 0xFD],
            "text/plain",
            "x.txt",
            ExtractionLimits::default(),
        );
        assert!(output.extracted_text.is_none());
        assert_eq!(output.metadata.status, ExtractionStatus::Unavailable);
    }

    #[cfg(unix)]
    #[test]
    fn ocr_timeout_kills_process_and_removes_unique_input() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("slow-tesseract");
        std::fs::write(&command, "#!/bin/sh\nexec sleep 5\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        let output = ocr_image_with_command(
            b"not-really-an-image",
            "png",
            Duration::from_millis(20),
            1024,
            &command,
            dir.path(),
        );
        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![command.file_name().unwrap().to_os_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_ocr_calls_use_distinct_inputs_and_clean_them() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("fake-tesseract");
        std::fs::write(&command, "#!/bin/sh\nsleep 0.05\nprintf extracted\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut workers = Vec::new();
        for _ in 0..2 {
            let command = command.clone();
            let temp_root = dir.path().to_path_buf();
            workers.push(thread::spawn(move || {
                ocr_image_with_command(
                    b"not-really-an-image",
                    "png",
                    Duration::from_secs(2),
                    1024,
                    &command,
                    &temp_root,
                )
            }));
        }
        for worker in workers {
            assert_eq!(worker.join().unwrap().as_deref(), Some("extracted"));
        }
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![command.file_name().unwrap().to_os_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn ocr_input_is_owner_only_while_external_process_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("mode-tesseract");
        std::fs::write(
            &command,
            "#!/bin/sh\nmode=$(stat -L -c %a \"$1\" 2>/dev/null || stat -L -f %Lp \"$1\")\n[ \"$mode\" = 600 ] || exit 42\nprintf extracted\n",
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();

        let output = ocr_image_with_command(
            b"sensitive-upload",
            "png",
            Duration::from_secs(2),
            1024,
            &command,
            dir.path(),
        );

        assert_eq!(output.as_deref(), Some("extracted"));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![command.file_name().unwrap().to_os_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn ocr_does_not_consume_path_replacement_after_create() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("read-tesseract-input");
        std::fs::write(&command, "#!/bin/sh\ncat \"$1\"\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();

        let output = ocr_image_with_command_unix(
            b"original-sensitive-upload",
            "png",
            Duration::from_secs(2),
            1024,
            &command,
            dir.path(),
            |path| {
                std::fs::remove_file(path).unwrap();
                std::fs::write(path, b"attacker-replacement").unwrap();
            },
        );

        assert_eq!(output.as_deref(), Some("original-sensitive-upload"));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![command.file_name().unwrap().to_os_string()]);
    }
}

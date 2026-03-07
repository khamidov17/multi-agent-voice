//! Document text extraction for PDF, DOCX, and XLSX files.
//!
//! - PDF: via `pdftotext` subprocess (requires poppler-utils on the server)
//! - DOCX: pure Rust via zip + XML stripping
//! - XLSX: pure Rust via calamine crate

use std::io::Cursor;
use std::process::Command;

use tracing::{info, warn};

/// Max characters to extract from a document (to avoid flooding Claude's context).
const MAX_DOC_CHARS: usize = 8000;

/// Extract plain text from a document file.
///
/// Returns `None` if the file type is unsupported.
/// Returns `Some(text)` with extracted content, possibly truncated.
pub fn extract_text(filename: &str, data: &[u8]) -> Option<String> {
    let ext = filename.rsplit('.').next()?.to_lowercase();
    let result = match ext.as_str() {
        "pdf" => extract_pdf(data),
        "docx" => extract_docx(data),
        "xlsx" | "xls" => extract_xlsx(data, &ext),
        _ => return None,
    };

    match result {
        Ok(text) => {
            let truncated = if text.chars().count() > MAX_DOC_CHARS {
                let cut: String = text.chars().take(MAX_DOC_CHARS).collect();
                format!("{}\n\n[truncated — document is too long]", cut)
            } else {
                text
            };
            Some(truncated)
        }
        Err(e) => {
            warn!("Document extraction failed for {}: {}", filename, e);
            Some(format!("[Failed to extract text from {}: {}]", filename, e))
        }
    }
}

/// Extract text from a PDF using the `pdftotext` command-line tool (poppler-utils).
pub fn extract_pdf(data: &[u8]) -> Result<String, String> {
    let temp_dir = std::env::temp_dir();
    let input_path = temp_dir.join(format!("doc_input_{}.pdf", std::process::id()));

    std::fs::write(&input_path, data)
        .map_err(|e| format!("Failed to write temp PDF: {e}"))?;

    let output = Command::new("pdftotext")
        .args([input_path.to_str().unwrap(), "-"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("pdftotext not found (install poppler-utils): {e}"))?;

    let _ = std::fs::remove_file(&input_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pdftotext failed: {}", stderr));
    }

    let text = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string();

    info!("PDF: extracted {} chars", text.len());
    Ok(text)
}

/// Extract text from a DOCX file (which is a ZIP containing word/document.xml).
fn extract_docx(data: &[u8]) -> Result<String, String> {
    let cursor = Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Failed to open DOCX as ZIP: {e}"))?;

    let xml = {
        let mut file = archive
            .by_name("word/document.xml")
            .map_err(|_| "word/document.xml not found in DOCX".to_string())?;

        let mut buf = String::new();
        std::io::Read::read_to_string(&mut file, &mut buf)
            .map_err(|e| format!("Failed to read word/document.xml: {e}"))?;
        buf
    };

    // Strip XML tags, collapse whitespace, preserve paragraph breaks
    let text = strip_docx_xml(&xml);
    info!("DOCX: extracted {} chars", text.len());
    Ok(text)
}

/// Strip XML tags from DOCX word/document.xml and return readable text.
///
/// Inserts newlines at paragraph boundaries (`<w:p>` elements).
fn strip_docx_xml(xml: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    let mut tag_buf = String::new();

    for ch in xml.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                in_tag = false;
                // Paragraph end → newline
                let tag = tag_buf.trim();
                if tag == "w:p" || tag == "/w:p" || tag.starts_with("w:p ") {
                    result.push('\n');
                }
            }
            _ if in_tag => {
                tag_buf.push(ch);
            }
            _ => {
                result.push(ch);
            }
        }
    }

    // Collapse multiple blank lines, trim
    let text: String = result
        .lines()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("\n");

    // Collapse 3+ consecutive newlines to 2
    let mut prev_blank = 0usize;
    let mut out = String::new();
    for line in text.lines() {
        if line.is_empty() {
            prev_blank += 1;
            if prev_blank <= 1 {
                out.push('\n');
            }
        } else {
            prev_blank = 0;
            out.push_str(line);
            out.push('\n');
        }
    }

    out.trim().to_string()
}

/// Extract text from an XLSX/XLS file using the calamine crate.
fn extract_xlsx(data: &[u8], ext: &str) -> Result<String, String> {
    use calamine::{open_workbook_from_rs, Xlsx, Xls};

    let cursor = Cursor::new(data);

    let mut result = String::new();

    match ext {
        "xlsx" => {
            let mut workbook: Xlsx<_> = open_workbook_from_rs(cursor)
                .map_err(|e| format!("Failed to open XLSX: {e}"))?;
            append_sheets(&mut workbook, &mut result)?;
        }
        "xls" => {
            let mut workbook: Xls<_> = open_workbook_from_rs(cursor)
                .map_err(|e| format!("Failed to open XLS: {e}"))?;
            append_sheets(&mut workbook, &mut result)?;
        }
        _ => return Err("Unsupported format".to_string()),
    }

    info!("XLSX: extracted {} chars", result.len());
    Ok(result)
}

fn append_sheets<R: calamine::Reader<RS>, RS: std::io::Read + std::io::Seek>(
    workbook: &mut R,
    out: &mut String,
) -> Result<(), String> {
    use calamine::Data;

    let sheet_names: Vec<String> = workbook.sheet_names().to_vec();

    for sheet_name in &sheet_names {
        let range = workbook
            .worksheet_range(sheet_name)
            .map_err(|e| format!("Failed to read sheet '{}': {:?}", sheet_name, e))?;

        out.push_str(&format!("=== {} ===\n", sheet_name));

        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Data::Empty => String::new(),
                    Data::String(s) => s.clone(),
                    Data::Float(f) => format!("{}", f),
                    Data::Int(i) => format!("{}", i),
                    Data::Bool(b) => format!("{}", b),
                    Data::Error(e) => format!("#ERR({:?})", e),
                    Data::DateTime(dt) => format!("{}", dt),
                    Data::DateTimeIso(s) => s.clone(),
                    Data::DurationIso(s) => s.clone(),
                })
                .collect();

            // Skip completely empty rows
            if cells.iter().all(|c| c.is_empty()) {
                continue;
            }

            out.push_str(&cells.join("\t"));
            out.push('\n');
        }

        out.push('\n');
    }

    Ok(())
}

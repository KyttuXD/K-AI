//! Plain text from documentation files: PDF (pdf-extract), Word DOCX (the paragraphs
//! of word/document.xml, Heading/Title styles become "# " lines) and the text formats
//! handled by `docs::read_document`.
use super::docs;
use anyhow::{Context, Result};
use quick_xml::{Reader, events::Event};
use std::{io::Read, path::Path};

const MAX_BINARY: u64 = 50_000_000;
const MAX_XML: u64 = 40_000_000;

pub fn extract_text(path: &Path) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if !matches!(ext.as_str(), "pdf" | "docx") {
        return docs::read_document(path);
    }
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= MAX_BINARY,
        "File exceeds 50 MB limit"
    );
    let text = if ext == "pdf" {
        // pdf-extract panics on some malformed files; treat that as a skipped file.
        std::panic::catch_unwind(|| pdf_extract::extract_text(path))
            .map_err(|_| anyhow::anyhow!("Cannot read this PDF"))?
            .map_err(|e| anyhow::anyhow!("Cannot read PDF: {e}"))?
    } else {
        docx_text(&std::fs::read(path)?)?
    };
    anyhow::ensure!(
        !text.trim().is_empty(),
        "No text found (scanned or image-only document)"
    );
    Ok(text)
}

/// Paragraph text of a DOCX archive held in memory.
pub fn docx_text(bytes: &[u8]) -> Result<String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("Not a DOCX file")?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")
        .context("Not a Word document")?
        .take(MAX_XML)
        .read_to_string(&mut xml)?;
    document_text(&xml)
}

fn heading_level(style: &str) -> usize {
    let style = style.to_lowercase().replace(' ', "");
    if style == "title" {
        return 1;
    }
    style
        .strip_prefix("heading")
        .and_then(|n| n.parse().ok())
        .filter(|n| (1..=6).contains(n))
        .unwrap_or(0)
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

// One line per paragraph; runs are joined, tabs and breaks kept.
fn document_text(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    let (mut out, mut para, mut level, mut in_text) = (String::new(), String::new(), 0, false);
    loop {
        let event = reader.read_event()?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                match e.local_name().as_ref() {
                    b"t" => in_text = !empty,
                    b"tab" if empty => para.push('\t'),
                    b"br" | b"cr" => para.push('\n'),
                    b"pStyle" => {
                        if let Ok(Some(a)) = e.try_get_attribute("w:val") {
                            level = heading_level(&String::from_utf8_lossy(&a.value));
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => {
                    let text = para.trim();
                    if !text.is_empty() {
                        if level > 0 {
                            out.push_str(&"#".repeat(level));
                            out.push(' ');
                        }
                        out.push_str(text);
                        out.push('\n');
                    }
                    para.clear();
                    level = 0;
                }
                _ => {}
            },
            Event::Text(t) if in_text => para.push_str(&String::from_utf8_lossy(t)),
            Event::CData(t) if in_text => para.push_str(&String::from_utf8_lossy(t)),
            Event::GeneralRef(r) if in_text => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    para.push(c);
                } else if let Some(c) = entity(&String::from_utf8_lossy(r)) {
                    para.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn docx_paragraphs_and_headings() -> Result<()> {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Install</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">Run </w:t></w:r><w:r><w:t>setup &amp; go</w:t></w:r><w:r><w:tab/><w:t>&#233;</w:t></w:r></w:p>
<w:p></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading 2"/></w:pPr><w:r><w:t>Next</w:t></w:r></w:p>
</w:body></w:document>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("word/document.xml", options)?;
        zip.write_all(xml.as_bytes())?;
        let bytes = zip.finish()?.into_inner();
        assert_eq!(
            docx_text(&bytes)?,
            "# Install\nRun setup & go\té\n## Next\n"
        );
        assert!(docx_text(b"not a zip").is_err());
        Ok(())
    }

    #[test]
    fn heading_styles() {
        assert_eq!(heading_level("Heading3"), 3);
        assert_eq!(heading_level("Title"), 1);
        assert_eq!(heading_level("Normal"), 0);
        assert_eq!(heading_level("Heading9"), 0);
    }
}

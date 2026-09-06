//! Bounded local PDF rasterization for model and human visual inspection.

use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Rasterize one page of caller-authorized PDF bytes using Poppler on PATH.
///
/// No URLs or model-controlled filesystem paths are accepted. Temporary source
/// and image files are removed when the operation finishes. Callers must obtain
/// authorization before transmitting the returned private image to a provider.
///
/// # Errors
/// Returns an error for invalid limits, missing Poppler, cancellation, timeout,
/// invalid PDFs, missing pages, or oversized output.
pub fn rasterize_pdf_page(
    pdf: &[u8],
    page: u16,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>, String> {
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    if page == 0 || page > 20 || pdf.len() > MAX_BYTES || !pdf.starts_with(b"%PDF-") {
        return Err("PDF rendering requires a PDF of at most 16 MiB and page 1–20".into());
    }
    if cancelled() {
        return Err("PDF rendering cancelled".into());
    }
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let source = directory.path().join("source.pdf");
    let output = directory.path().join("page");
    std::fs::File::create(&source)
        .and_then(|mut file| file.write_all(pdf))
        .map_err(|error| error.to_string())?;
    let mut child = Command::new("pdftoppm")
        .args([
            "-f",
            &page.to_string(),
            "-l",
            &page.to_string(),
            "-singlefile",
            "-scale-to",
            "1600",
            "-png",
        ])
        .arg(&source)
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            "PDF rendering requires pdftoppm on PATH (for example: nix-shell -p poppler-utils)"
                .to_owned()
        })?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if cancelled() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("PDF rendering cancelled or exceeded 30 seconds".into());
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err("PDF rasterizer rejected document or page".into()),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
    }
    let mut bytes = Vec::new();
    std::fs::File::open(output.with_extension("png"))
        .map_err(|_| "PDF page was not produced".to_owned())?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_BYTES || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("PDF rasterizer produced invalid or oversized PNG".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    #[ignore = "requires pdftoppm; run under nix-shell -p poppler-utils"]
    fn rasterizes_actual_pdf_page() {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>",
            "<< /Length 0 >>\nstream\n\nendstream",
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
        }
        let xref = pdf.len();
        pdf.push_str("xref\n0 5\n0000000000 65535 f \n");
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(
            pdf,
            "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
        )
        .unwrap();
        let png = rasterize_pdf_page(pdf.as_bytes(), 1, || false).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(rasterize_pdf_page(pdf.as_bytes(), 2, || false).is_err());
    }

    #[test]
    fn rejects_invalid_input_before_spawning() {
        assert!(rasterize_pdf_page(b"not a PDF", 1, || false).is_err());
        assert!(rasterize_pdf_page(b"%PDF-1.4", 0, || false).is_err());
        assert!(rasterize_pdf_page(b"%PDF-1.4", 21, || false).is_err());
        assert!(rasterize_pdf_page(b"%PDF-1.4", 1, || true).is_err());
    }
}

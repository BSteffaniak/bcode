//! Explicit endpoint-owner opt-in to lossless request gzip. Never inferred from model names.
use std::io::Write as _;

pub struct EncodedBody {
    pub bytes: Vec<u8>,
    pub gzip: bool,
}

pub fn encode(bytes: Vec<u8>, mode: Option<&str>) -> Result<EncodedBody, &'static str> {
    match mode {
        None | Some("off") => Ok(EncodedBody { bytes, gzip: false }),
        Some("gzip") => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder
                .write_all(&bytes)
                .map_err(|_| "request compression failed")?;
            let compressed = encoder.finish().map_err(|_| "request compression failed")?;
            if compressed.len() < bytes.len() {
                Ok(EncodedBody {
                    bytes: compressed,
                    gzip: true,
                })
            } else {
                Ok(EncodedBody { bytes, gzip: false })
            }
        }
        Some(_) => Err("request_compression must be off or gzip"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn gzip_preserves_exact_image_json_and_off_preserves_bytes() {
        let original = format!(
            "{{\"image\":\"data:image/png;base64,{}\"}}",
            "AQID".repeat(4096)
        )
        .into_bytes();
        let encoded = encode(original.clone(), Some("gzip")).expect("gzip");
        assert!(encoded.gzip);
        assert!(encoded.bytes.len() < original.len());
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(encoded.bytes.as_slice())
            .read_to_end(&mut decoded)
            .expect("decode");
        assert_eq!(decoded, original);
        assert_eq!(encode(original.clone(), None).expect("off").bytes, original);
        assert!(!encode(b"{}".to_vec(), Some("gzip")).expect("tiny").gzip);
        assert!(encode(Vec::new(), Some("unknown")).is_err());
    }
}

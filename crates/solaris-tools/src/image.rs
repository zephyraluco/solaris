//! Small image attachments are detected by content, not a filename extension.
use crate::InlineImage;
use base64::Engine;
use std::io::{self, Read};
use std::path::Path;
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}
/// Return None for text/other binary data. Image bytes are limited before encoding.
pub fn read_image(path: &Path) -> io::Result<Option<InlineImage>> {
    let mut file = std::fs::File::open(path)?;
    let mut prefix = [0u8; 16];
    let count = file.read(&mut prefix)?;
    let Some(media_type) = media_type(&prefix[..count]) else {
        return Ok(None);
    };
    let mut bytes = prefix[..count].to_vec();
    file.take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(io::Error::other("image exceeds the 8 MiB input limit"));
    }
    Ok(Some(InlineImage {
        media_type: media_type.into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_signatures_determine_images_and_text_stays_text() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("no-extension");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\npayload").expect("image");
        let image = read_image(&path).expect("read").expect("detected image");
        assert_eq!(image.media_type, "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(image.data)
                .expect("decode"),
            b"\x89PNG\r\n\x1a\npayload"
        );
        std::fs::write(&path, "plain text").expect("text");
        assert_eq!(read_image(&path).expect("read"), None);
    }
}

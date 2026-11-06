//! Small image attachments are detected by content, not a filename extension.
use crate::InlineImage;
use base64::Engine;
use std::io;
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
    let prefix = crate::environment::ExecutionEnvironment.checked(
        "[[ -f $1 ]] || { printf 'file not found or not a regular file' >&2; exit 44; }; head -c 16 -- \"$1\"", &[path], &[], None)?;
    let Some(_) = media_type(&prefix) else {
        return Ok(None);
    };
    let bytes = crate::filesystem::read_limited(path, MAX_IMAGE_BYTES).map_err(|error| {
        io::Error::other(format!(
            "image exceeds the 8 MiB input limit or could not be read: {error}"
        ))
    })?;
    from_bytes(&bytes)
}

pub(crate) fn from_bytes(bytes: &[u8]) -> io::Result<Option<InlineImage>> {
    let Some(media_type) = media_type(bytes) else {
        return Ok(None);
    };
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

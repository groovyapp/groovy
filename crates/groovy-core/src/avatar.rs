//! Avatars: a picture chosen by the user, reduced to a small PNG thumbnail.
//!
//! Inputs are limited before they are decoded, so a huge file cannot exhaust
//! memory or time.

use std::io::Cursor;
use std::path::Path;
use std::{fs, io};

use image::{ImageFormat, ImageReader, Limits};
use thiserror::Error;

/// The longest side of the stored thumbnail, in pixels.
pub const THUMBNAIL_SIZE: u32 = 128;
/// Larger files are refused before they are read.
pub const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;
/// Larger images are refused before they are decoded.
pub const MAX_INPUT_DIMENSION: u32 = 8192;

#[derive(Debug, Error)]
pub enum AvatarError {
    #[error("the picture is larger than {} MB", MAX_INPUT_BYTES / 1024 / 1024)]
    TooLarge,
    #[error("failed to read the picture")]
    Read(#[source] io::Error),
    #[error("that file is not a picture this app can read")]
    Decode(#[source] image::ImageError),
}

/// Reads the picture at `path` and returns its PNG thumbnail.
pub fn thumbnail_from_file(path: &Path) -> Result<Vec<u8>, AvatarError> {
    let size = fs::metadata(path).map_err(AvatarError::Read)?.len();
    if size > MAX_INPUT_BYTES {
        return Err(AvatarError::TooLarge);
    }
    let bytes = fs::read(path).map_err(AvatarError::Read)?;
    thumbnail(&bytes)
}

/// Decodes a picture and returns it as a PNG thumbnail that fits in
/// [`THUMBNAIL_SIZE`] pixels, keeping its aspect ratio.
pub fn thumbnail(bytes: &[u8]) -> Result<Vec<u8>, AvatarError> {
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(AvatarError::TooLarge);
    }

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(AvatarError::Read)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_INPUT_DIMENSION);
    limits.max_image_height = Some(MAX_INPUT_DIMENSION);
    reader.limits(limits);

    let picture = reader.decode().map_err(AvatarError::Decode)?;
    // Only shrink: enlarging a small picture would blur it.
    let small = if picture.width() <= THUMBNAIL_SIZE && picture.height() <= THUMBNAIL_SIZE {
        picture
    } else {
        picture.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE)
    };

    let mut png = Vec::new();
    small
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(AvatarError::Decode)?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use image::{ImageBuffer, Rgb};

    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let picture: ImageBuffer<Rgb<u8>, _> =
            ImageBuffer::from_pixel(width, height, Rgb([200, 80, 40]));
        let mut bytes = Vec::new();
        picture
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn large_pictures_shrink_to_the_thumbnail_size_keeping_aspect() {
        let thumb = thumbnail(&png(600, 300)).unwrap();

        let decoded = image::load_from_memory(&thumb).unwrap();
        assert_eq!(decoded.width(), THUMBNAIL_SIZE);
        assert_eq!(decoded.height(), THUMBNAIL_SIZE / 2);
    }

    #[test]
    fn small_pictures_are_not_enlarged() {
        let thumb = thumbnail(&png(40, 30)).unwrap();

        let decoded = image::load_from_memory(&thumb).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (40, 30));
    }

    #[test]
    fn files_that_are_not_pictures_are_refused() {
        let err = thumbnail(b"definitely not an image").unwrap_err();
        assert!(matches!(err, AvatarError::Decode(_)), "{err}");
    }

    #[test]
    fn oversized_input_is_refused_before_decoding() {
        let bytes = vec![0u8; MAX_INPUT_BYTES as usize + 1];

        assert!(matches!(thumbnail(&bytes), Err(AvatarError::TooLarge)));
    }
}

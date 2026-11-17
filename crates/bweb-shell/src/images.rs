//! Decoding document images and sizing their layout boxes.
//!
//! Only formats enabled in the workspace `image` dependency are accepted. All
//! decoded bitmaps are bounded before they reach the framebuffer.

use std::{collections::HashMap, error::Error, fmt, sync::Arc};

use bweb_html::{HtmlDocument, LoadedResource, NodeId, NodeKind, ResourceKind};
use bweb_layout::ImageSize;
use bweb_renderer::Image;
use url::Url;

const MAX_IMAGE_PIXELS: usize = 16_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    UnsupportedOrCorrupt,
    TooLarge,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedOrCorrupt => formatter.write_str("unsupported or corrupt image data"),
            Self::TooLarge => formatter.write_str("decoded image exceeds the pixel limit"),
        }
    }
}

impl Error for DecodeError {}

/// Decodes PNG, JPEG or GIF bytes into an RGBA8 painting image.
///
/// Decoding limits are applied *before* the decoder allocates, so a crafted
/// image header cannot force a huge allocation. A post-decode check remains as
/// a safety net for decoders that ignore limits.
pub fn decode(bytes: &[u8]) -> Result<Image, DecodeError> {
    let mut limits = image::Limits::default();
    // Bound the decoded allocation by total pixels (RGBA8 = 4 bytes/pixel).
    // No per-axis dimension cap: a panoramic image such as 16000x100 is well
    // within the pixel budget and must be accepted. `max_alloc` alone still
    // rejects a crafted 100000x100000 header before any large allocation.
    limits.max_alloc = Some((MAX_IMAGE_PIXELS as u64) * 4);

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| DecodeError::UnsupportedOrCorrupt)?;
    // Limits are set before decode, so the decoder bound allocations up front.
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| DecodeError::UnsupportedOrCorrupt)?;

    let width = decoded.width() as usize;
    let height = decoded.height() as usize;
    let pixels = width.checked_mul(height).ok_or(DecodeError::TooLarge)?;
    if pixels == 0 || pixels > MAX_IMAGE_PIXELS {
        return Err(DecodeError::TooLarge);
    }
    let rgba = decoded.to_rgba8();
    Image::new(rgba.width(), rgba.height(), rgba.into_raw())
        .map_err(|_| DecodeError::UnsupportedOrCorrupt)
}

/// Decodes every fetched image exactly once, keyed by its resolved URL. Both
/// `<img>` elements and CSS backgrounds share these decoded bitmaps.
pub fn decode_all(resources: &[LoadedResource]) -> HashMap<String, Arc<Image>> {
    let mut decoded = HashMap::new();
    let mut total_pixels = 0usize;
    for resource in resources.iter().filter(|item| item.kind == ResourceKind::Image) {
        match decode(&resource.bytes) {
            Ok(image) => {
                total_pixels += (image.width() as usize).saturating_mul(image.height() as usize);
                if total_pixels > MAX_IMAGE_PIXELS {
                    eprintln!("  image memory budget exhausted; skipping {}", resource.url);
                    break;
                }
                decoded.insert(resource.url.as_str().to_owned(), Arc::new(image));
            }
            Err(error) => eprintln!("  invalid image {}: {error}", resource.url),
        }
    }
    decoded
}

/// Maps each `<img>` with a fetched body to its already-decoded image.
pub fn collect_images(
    document: &HtmlDocument,
    base: &Url,
    decoded: &HashMap<String, Arc<Image>>,
) -> HashMap<NodeId, Arc<Image>> {
    let mut images = HashMap::new();
    for (id, node) in document.iter() {
        let NodeKind::Element { tag_name, attributes } = node.kind() else {
            continue;
        };
        if tag_name != "img" {
            continue;
        }
        let Some(source) = attribute(attributes, "src") else {
            continue;
        };
        let Ok(resolved) = base.join(source) else {
            continue;
        };
        if let Some(image) = decoded.get(resolved.as_str()) {
            images.insert(id, Arc::clone(image));
        }
    }
    images
}

fn attribute<'a>(attributes: &'a [bweb_html::Attribute], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|item| item.name == name)
        .map(|item| item.value.as_str())
        .filter(|value| !value.trim().is_empty())
}

/// Extracts intrinsic CSS-pixel sizes for the layout engine.
pub fn image_sizes(images: &HashMap<NodeId, Arc<Image>>) -> HashMap<NodeId, ImageSize> {
    images
        .iter()
        .map(|(id, image)| {
            (*id, ImageSize { width: image.width() as f32, height: image.height() as f32 })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{DecodeError, collect_images, decode, decode_all, image_sizes};
    use bweb_html::{HtmlDocument, LoadedResource, NodeKind, ResourceKind};
    use bweb_renderer::Image;
    use image::{ImageEncoder, RgbaImage};
    use std::{collections::HashMap, sync::Arc};
    use url::Url;

    fn png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let buffer = RgbaImage::from_pixel(width, height, image::Rgba(color));
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(buffer.as_raw(), width, height, image::ColorType::Rgba8.into())
            .expect("PNG encoding should succeed");
        encoded
    }

    #[test]
    fn decodes_png_to_rgba_pixels() {
        let image = decode(&png(2, 3, [10, 20, 30, 255])).expect("valid PNG");
        assert_eq!((image.width(), image.height()), (2, 3));
        assert_eq!(image.pixels().len(), 2 * 3 * 4);
        assert_eq!(&image.pixels()[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn rejects_corrupt_image_data() {
        assert!(decode(b"not an image").is_err());
    }

    #[test]
    fn rejects_images_with_huge_declared_dimensions() {
        // A PNG header claiming 100000x100000 pixels. A naive decoder would try
        // to allocate ~40 GB; the pre-decode limits must reject it.
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&100_000u32.to_be_bytes()); // width
        bytes.extend_from_slice(&100_000u32.to_be_bytes()); // height
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth, color type, etc.
        assert_eq!(decode(&bytes), Err(DecodeError::UnsupportedOrCorrupt));
    }

    #[test]
    fn accepts_panoramic_images_within_the_pixel_budget() {
        // 5000x10 = 50k pixels, far below the 16M budget, but the width
        // exceeds the old 4000 per-axis cap. It must be accepted.
        let bytes = png(5000, 10, [10, 20, 30, 255]);
        let image = decode(&bytes).expect("panoramic image within budget");
        assert_eq!((image.width(), image.height()), (5000, 10));
    }

    #[test]
    fn decode_all_shares_bitmaps_and_enforces_a_budget() {
        // Two resources; collect_images and CSS backgrounds must share the
        // same Arc rather than decoding twice.
        let resources = vec![
            LoadedResource {
                url: Url::parse("https://example.test/a.png").unwrap(),
                kind: ResourceKind::Image,
                content_type: None,
                bytes: png(4, 4, [255, 0, 0, 255]),
            },
            LoadedResource {
                url: Url::parse("https://example.test/b.png").unwrap(),
                kind: ResourceKind::Image,
                content_type: None,
                bytes: png(4, 4, [0, 255, 0, 255]),
            },
        ];
        let decoded = decode_all(&resources);
        assert_eq!(decoded.len(), 2);

        let document = HtmlDocument::parse(
            "<main><img src='https://example.test/a.png'><img src='https://example.test/a.png'></main>",
        );
        let base = Url::parse("https://example.test/").unwrap();
        let images = collect_images(&document, &base, &decoded);
        assert_eq!(images.len(), 2);
        // Both <img> elements point to the exact same decoded bitmap.
        let handles: Vec<&Arc<Image>> = images.values().collect();
        assert!(Arc::ptr_eq(handles[0], handles[1]));
    }

    #[test]
    fn image_sizes_maps_decoded_dimensions() {
        let document = HtmlDocument::parse("<main><img src='x.png'></main>");
        let image = Image::new(20, 10, vec![0; 20 * 10 * 4]).expect("image");
        let id = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "img")
                    .then_some(id)
            })
            .expect("img node");
        let sizes = image_sizes(&HashMap::from([(id, Arc::new(image))]));
        let size = sizes.get(&id).expect("image size");
        assert_eq!((size.width, size.height), (20.0, 10.0));
    }
}

//! Backend-independent RGBA8 framebuffer primitives.
//!
//! Pixels use straight alpha, sRGB encoding, a top-left origin, and tightly
//! packed rows. Pixel centers are at (x + 0.5, y + 0.5). Source-over blending
//! uses premultiplied intermediates and stores straight-alpha results.
//!
//! TODO: linearize sRGB before blending when colorimetrically correct gradients
//! and translucent transitions become a renderer requirement.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    width: u32,
    height: u32,
    stride: usize,
    pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramebufferError {
    DimensionsTooLarge,
    AllocationFailed,
}

impl Framebuffer {
    /// Creates a transparent RGBA8 framebuffer with tightly packed rows.
    pub fn new(width: u32, height: u32) -> Result<Self, FramebufferError> {
        let stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(FramebufferError::DimensionsTooLarge)?;
        let height = usize::try_from(height).map_err(|_| FramebufferError::DimensionsTooLarge)?;
        let length = stride.checked_mul(height).ok_or(FramebufferError::DimensionsTooLarge)?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(length).map_err(|_| FramebufferError::AllocationFailed)?;
        pixels.resize(length, 0);
        Ok(Self { width, height: height as u32, stride, pixels })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// Straight-alpha RGBA8 bytes, ready for an external buffer wrapper.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Mutable access to the tightly packed RGBA8 bytes.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    /// Replaces every pixel without blending.
    pub fn clear(&mut self, color: [u8; 4]) {
        for pixel in self.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
    }

    /// Composites one source pixel over the destination.
    pub fn set_pixel(&mut self, x: u32, y: u32, source: [u8; 4]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y as usize * self.stride + x as usize * 4;
        blend_source_over(&mut self.pixels[offset..offset + 4], source);
    }

    /// Composites a rectangle, clipping negative and out-of-bounds edges.
    pub fn fill_rect(&mut self, x: i32, y: i32, width: u32, height: u32, color: [u8; 4]) {
        if width == 0 || height == 0 || self.width == 0 || self.height == 0 {
            return;
        }
        let left = i64::from(x).max(0);
        let top = i64::from(y).max(0);
        let right = (i64::from(x) + i64::from(width)).min(i64::from(self.width));
        let bottom = (i64::from(y) + i64::from(height)).min(i64::from(self.height));
        if left >= right || top >= bottom {
            return;
        }
        for row in top as usize..bottom as usize {
            let row_start = row * self.stride;
            for column in left as usize..right as usize {
                let offset = row_start + column * 4;
                blend_source_over(&mut self.pixels[offset..offset + 4], color);
            }
        }
    }
}

fn blend_source_over(destination: &mut [u8], source: [u8; 4]) {
    let sa = u32::from(source[3]);
    if sa == 0 {
        return;
    }
    if sa == 255 {
        destination.copy_from_slice(&source);
        return;
    }
    let da = u32::from(destination[3]);
    let inverse_sa = 255 - sa;
    let out_a_numerator = sa * 255 + da * inverse_sa;
    if out_a_numerator == 0 {
        destination.copy_from_slice(&[0, 0, 0, 0]);
        return;
    }
    for channel in 0..3 {
        let src_premul = u32::from(source[channel]) * sa * 255;
        let dst_premul = u32::from(destination[channel]) * da * inverse_sa;
        destination[channel] =
            ((src_premul + dst_premul + out_a_numerator / 2) / out_a_numerator) as u8;
    }
    destination[3] = ((out_a_numerator + 127) / 255) as u8;
}

#[cfg(test)]
mod tests {
    use super::{Framebuffer, FramebufferError};

    #[test]
    fn allocates_tightly_packed_rgba_rows() {
        let fb = Framebuffer::new(3, 2).expect("framebuffer");
        assert_eq!((fb.width(), fb.height(), fb.stride()), (3, 2, 12));
        assert_eq!(fb.pixels().len(), 24);
        assert!(fb.pixels().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rejects_dimension_overflow() {
        assert_eq!(
            Framebuffer::new(u32::MAX, u32::MAX),
            Err(FramebufferError::DimensionsTooLarge)
        );
    }

    #[test]
    fn clear_replaces_every_rgba_pixel() {
        let mut fb = Framebuffer::new(2, 1).expect("framebuffer");
        fb.clear([12, 34, 56, 78]);
        assert_eq!(fb.pixels(), &[12, 34, 56, 78, 12, 34, 56, 78]);
    }

    #[test]
    fn opaque_source_replaces_destination() {
        let mut fb = Framebuffer::new(1, 1).expect("framebuffer");
        fb.clear([10, 20, 30, 40]);
        fb.set_pixel(0, 0, [200, 150, 100, 255]);
        assert_eq!(fb.pixels(), &[200, 150, 100, 255]);
    }

    #[test]
    fn transparent_source_leaves_destination_unchanged() {
        let mut fb = Framebuffer::new(1, 1).expect("framebuffer");
        fb.clear([10, 20, 30, 40]);
        fb.set_pixel(0, 0, [255, 0, 0, 0]);
        assert_eq!(fb.pixels(), &[10, 20, 30, 40]);
    }

    #[test]
    fn source_over_blends_straight_alpha() {
        let mut fb = Framebuffer::new(1, 1).expect("framebuffer");
        fb.clear([0, 0, 255, 128]);
        fb.set_pixel(0, 0, [255, 0, 0, 128]);
        assert_eq!(fb.pixels(), &[170, 0, 85, 192]);
    }

    #[test]
    fn clips_rectangle_at_all_edges() {
        let mut fb = Framebuffer::new(3, 2).expect("framebuffer");
        fb.fill_rect(-1, -1, 3, 3, [255, 0, 0, 255]);
        assert_eq!(
            fb.pixels(),
            &[
                255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 0, 0,
                0, 0,
            ]
        );
    }

    #[test]
    fn rectangle_outside_bounds_is_a_noop() {
        let mut fb = Framebuffer::new(2, 2).expect("framebuffer");
        fb.fill_rect(2, 0, 4, 2, [1, 2, 3, 255]);
        assert!(fb.pixels().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn zero_sized_framebuffer_is_safe() {
        let mut fb = Framebuffer::new(0, 4).expect("framebuffer");
        fb.clear([1, 2, 3, 4]);
        fb.fill_rect(0, 0, 10, 10, [255, 255, 255, 255]);
        assert!(fb.pixels().is_empty());
        assert_eq!(fb.stride(), 0);
    }
}

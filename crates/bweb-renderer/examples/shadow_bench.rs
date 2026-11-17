//! Micro-benchmark for the blurred text-shadow rasterization path.
//!
//! Run with: `cargo run --release -p bweb-renderer --example shadow_bench [blur] [iterations]`

use bweb_renderer::{
    Color, DisplayList, Framebuffer, PaintItem, PositionedGlyph, Rasterizer, Rect, TextDecoration,
    TextShadow,
};
use bweb_text::Coverage;
use std::time::Instant;

fn main() {
    let blur: f32 = std::env::args().nth(1).and_then(|value| value.parse().ok()).unwrap_or(8.0);
    let iterations: u32 =
        std::env::args().nth(2).and_then(|value| value.parse().ok()).unwrap_or(20);
    let (width, height) = (800u32, 600u32);

    let mut list = DisplayList::default();
    list.push(PaintItem::PushClip {
        rect: Rect { x: 0.0, y: 0.0, width: width as f32, height: height as f32 },
    });
    // 40 lines of 12 glyphs, each line with its own blurred shadow.
    for row in 0..40 {
        let glyphs = (0..12)
            .map(|column| PositionedGlyph {
                coverage: Coverage { width: 8, height: 8, left: 0, top: 0, data: vec![255; 64] },
                x: 20.0 + column as f32 * 12.0,
                y: 10.0 + row as f32 * 14.0,
            })
            .collect();
        list.push(PaintItem::Text {
            glyphs,
            color: Color::rgba(0, 0, 0, 255),
            decoration: TextDecoration::None,
            shadows: vec![TextShadow {
                offset_x: 1.0,
                offset_y: 1.0,
                blur,
                color: Color::rgba(0, 0, 0, 120),
            }],
        });
    }
    list.push(PaintItem::PopClip);

    let mut framebuffer = Framebuffer::new(width, height).expect("framebuffer");
    for _ in 0..2 {
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(&list, &mut framebuffer);
    }

    let start = Instant::now();
    for _ in 0..iterations {
        framebuffer.clear([255, 255, 255, 255]);
        Rasterizer.rasterize(&list, &mut framebuffer);
    }
    let elapsed = start.elapsed();
    println!("blur={blur}, 40 text runs x 12 glyphs, {width}x{height}");
    println!("{iterations} frames in {elapsed:?} -> {:?}/frame", elapsed / iterations);
}

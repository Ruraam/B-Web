use bweb_css::{StyleEngine, Stylesheet};
use bweb_filterlists::FilterListDownloader;
use bweb_html::DocumentLoader;
use bweb_layout::{LayoutTree, PaintAdapter, Rect as LayoutRect};
use bweb_net::{HttpTransport, NetworkPolicy};
use bweb_renderer::{Framebuffer, Painter, Rasterizer};
use bweb_shield::{FilterListCache, FilterListError, Shield};
use bweb_text::Font;
use softbuffer::{Context, Surface};
use std::{
    env,
    error::Error,
    fs, io,
    io::{BufWriter, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process,
    sync::Arc,
};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("B-Web: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    match arguments.next().as_deref() {
        None => show_status()?,
        Some(command) if command == "--help" || command == "-h" => print_usage(),
        Some(command) if command == "update-filters" => update_filters()?,
        Some(command) if command == "open" => {
            let url = arguments.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "usage: b-web open <http(s)-url>")
            })?;
            let mut dump_layout = false;
            let mut render_path = None;
            let mut show_window = false;
            while let Some(flag) = arguments.next() {
                if flag == "--dump-layout" && !dump_layout {
                    dump_layout = true;
                } else if flag == "--render-ppm" && render_path.is_none() {
                    render_path = Some(PathBuf::from(arguments.next().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "missing output path after --render-ppm",
                        )
                    })?));
                } else if flag == "--window" && !show_window {
                    show_window = true;
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unexpected or duplicate option '{}'", flag.to_string_lossy()),
                    )
                    .into());
                }
            }
            open_document(
                url.to_string_lossy().as_ref(),
                dump_layout,
                render_path.as_deref(),
                show_window,
            )?;
        }
        Some(command) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown command '{}'; run 'b-web --help'", command.to_string_lossy()),
            )
            .into());
        }
    }
    Ok(())
}

fn show_status() -> Result<(), Box<dyn Error>> {
    println!("B-Web {}", bweb_core::VERSION);
    println!("Stage: {:?}", bweb_core::EngineStage::Bootstrap);
    println!("Privacy engine: {}", bweb_shield::FILTER_ENGINE);
    println!("JavaScript runtime: {:?}", bweb_js::RuntimeStatus::NotImplemented);
    let (_transport, _shield_active) = build_transport()?;
    println!("HTTP/HTTPS transport: ready (rustls, Shield-gated, Sec-GPC/DNT enabled)");
    Ok(())
}

fn open_document(
    url: &str,
    dump_layout: bool,
    render_path: Option<&Path>,
    show_window: bool,
) -> Result<(), Box<dyn Error>> {
    let (transport, shield_active) = build_transport()?;
    println!("Loading: {url}");
    if !shield_active {
        println!("Warning: Shield is inactive because no cached filter lists are available.");
    }
    let loaded = DocumentLoader::new(transport).load(url)?;
    println!("Document: {}", loaded.url);
    println!("Title: {}", loaded.document.title().as_deref().unwrap_or("(untitled)"));
    println!("DOM nodes: {}", loaded.document.node_count());
    println!("Loaded subresources: {}", loaded.resources.len());
    let mut stylesheets = Vec::new();
    let user_agent = Stylesheet::parse(
        "html, body, main, header, footer, nav, section, article, aside, div, p, h1, h2, h3, ul, ol, li, form, table, blockquote { display: block; }",
    )
    .expect("built-in user-agent stylesheet");
    stylesheets.push(user_agent);
    let mut stylesheet_count = 0usize;
    for resource in loaded
        .resources
        .iter()
        .filter(|resource| resource.kind == bweb_html::ResourceKind::Stylesheet)
    {
        match std::str::from_utf8(&resource.bytes) {
            Ok(source) => match Stylesheet::parse(source) {
                Ok(sheet) => {
                    stylesheets.push(sheet);
                    stylesheet_count += 1;
                }
                Err(error) => eprintln!("  invalid stylesheet {}: {error}", resource.url),
            },
            Err(_) => eprintln!("  invalid UTF-8 stylesheet: {}", resource.url),
        }
    }
    let computed = StyleEngine::new().compute(&loaded.document, &stylesheets);
    let layout = LayoutTree::build(&loaded.document, &computed, 1280.0);
    println!("Parsed stylesheets: {stylesheet_count}");
    println!("Computed styles: {} NodeId entries", computed.len());
    println!("Layout boxes: {}", layout.box_count);
    if render_path.is_some() || show_window {
        let (framebuffer, glyph_count) = render_frame(&layout, &computed, 1280, 720)?;
        if let Some(path) = render_path {
            write_ppm(path, &framebuffer)?;
            println!("Rendered frame: 1280x720 px, {glyph_count} glyphs -> {}", path.display());
        }
        if show_window {
            println!("Opening native B-Web window...");
            show_framebuffer(framebuffer)?;
        }
    }
    if dump_layout {
        println!();
        print!("{}", layout.dump(&loaded.document));
    }
    println!("Viewport: 1280 CSS px");
    println!("Failed/blocked subresources: {}", loaded.failed_resources.len());
    for resource in &loaded.resources {
        println!("  {:?}: {} ({} bytes)", resource.kind, resource.url, resource.bytes.len());
    }
    for failure in &loaded.failed_resources {
        println!("  blocked/unavailable: {} ({})", failure.reference.url, failure.error);
    }
    println!(
        "Note: CSS and first-pass layout are active; JavaScript execution and GPU painting are not implemented."
    );
    Ok(())
}

/// Runs HTML/CSS layout through text preparation, painting, and rasterization.
fn render_frame(
    layout: &LayoutTree,
    styles: &bweb_css::ComputedStyleMap,
    width: u32,
    height: u32,
) -> Result<(Framebuffer, usize), Box<dyn Error>> {
    let mut font = load_system_font()?;
    let scale_factor = 1.0;
    let text_runs = bweb_layout::prepare_text_runs(layout, styles, &mut font, scale_factor);
    let glyph_count = text_runs.values().map(|run| run.glyphs.len()).sum();
    let viewport = LayoutRect { x: 0.0, y: 0.0, width: width as f32, height: height as f32 };
    let adapter = PaintAdapter::new(layout, styles, viewport).with_text_runs(&text_runs);
    let display_list = Painter::new(&adapter, scale_factor)?.paint();
    let mut framebuffer = Framebuffer::new(width, height)
        .map_err(|error| io::Error::other(format!("could not allocate render frame: {error:?}")))?;
    framebuffer.clear([255, 255, 255, 255]);
    Rasterizer.rasterize(&display_list, &mut framebuffer);
    Ok((framebuffer, glyph_count))
}

/// Displays the software framebuffer in a native window using winit + softbuffer.
fn show_framebuffer(framebuffer: Framebuffer) -> Result<(), Box<dyn Error>> {
    let event_loop = EventLoop::new()?;
    let context = Context::new(event_loop.owned_display_handle())?;
    let mut app = WindowApp { context, framebuffer, window: None, surface: None, failure: None };
    event_loop.run_app(&mut app)?;
    if let Some(error) = app.failure {
        return Err(io::Error::other(error).into());
    }
    Ok(())
}

struct WindowApp<D> {
    context: Context<D>,
    framebuffer: Framebuffer,
    window: Option<Arc<Window>>,
    surface: Option<Surface<D, Arc<Window>>>,
    failure: Option<String>,
}

impl<D: raw_window_handle::HasDisplayHandle + 'static> ApplicationHandler for WindowApp<D> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match event_loop.create_window(
            Window::default_attributes().with_title("B-Web").with_inner_size(
                winit::dpi::PhysicalSize::new(self.framebuffer.width(), self.framebuffer.height()),
            ),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.failure = Some(format!("could not create B-Web window: {error}"));
                event_loop.exit();
                return;
            }
        };
        match Surface::new(&self.context, window.clone()) {
            Ok(surface) => {
                window.request_redraw();
                self.surface = Some(surface);
                self.window = Some(window);
            }
            Err(error) => {
                self.failure = Some(format!("could not create window surface: {error}"));
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => window.request_redraw(),
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.present_frame() {
                    self.failure = Some(format!("could not present B-Web frame: {error}"));
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }
}

impl<D: raw_window_handle::HasDisplayHandle> WindowApp<D> {
    fn present_frame(&mut self) -> Result<(), Box<dyn Error>> {
        let window =
            self.window.as_ref().ok_or_else(|| io::Error::other("window is not initialized"))?;
        let size = window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return Ok(());
        };
        let surface = self
            .surface
            .as_mut()
            .ok_or_else(|| io::Error::other("window surface is not initialized"))?;
        surface.resize(width, height)?;
        let mut buffer = surface.buffer_mut()?;
        copy_scaled_pixels(
            self.framebuffer.pixels(),
            self.framebuffer.width(),
            self.framebuffer.height(),
            &mut buffer,
            width.get(),
            height.get(),
        )?;
        window.pre_present_notify();
        buffer.present()?;
        Ok(())
    }
}

/// Copies an RGBA software frame into a packed RGBX surface using nearest-neighbor scaling.
fn copy_scaled_pixels(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target: &mut [u32],
    target_width: u32,
    target_height: u32,
) -> Result<(), io::Error> {
    let source_len = (source_width as usize)
        .checked_mul(source_height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::other("source framebuffer dimensions overflow"))?;
    let target_len = (target_width as usize)
        .checked_mul(target_height as usize)
        .ok_or_else(|| io::Error::other("target surface dimensions overflow"))?;
    if source_width == 0
        || source_height == 0
        || target_width == 0
        || target_height == 0
        || source.len() < source_len
        || target.len() < target_len
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "framebuffer or surface dimensions do not match their pixel buffers",
        ));
    }

    for y in 0..target_height {
        let source_y =
            (u64::from(y) * u64::from(source_height) / u64::from(target_height)) as usize;
        for x in 0..target_width {
            let source_x =
                (u64::from(x) * u64::from(source_width) / u64::from(target_width)) as usize;
            let source_index = (source_y * source_width as usize + source_x) * 4;
            let red = u32::from(source[source_index]);
            let green = u32::from(source[source_index + 1]);
            let blue = u32::from(source[source_index + 2]);
            let target_index = y as usize * target_width as usize + x as usize;
            target[target_index] = (red << 16) | (green << 8) | blue;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::copy_scaled_pixels;

    #[test]
    fn scales_rgba_frame_to_surface_with_nearest_neighbor_sampling() {
        let source = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255];
        let mut target = [0; 16];

        copy_scaled_pixels(&source, 2, 2, &mut target, 4, 4).unwrap();

        assert_eq!(
            target,
            [
                0xFF0000, 0xFF0000, 0x00FF00, 0x00FF00, 0xFF0000, 0xFF0000, 0x00FF00, 0x00FF00,
                0x0000FF, 0x0000FF, 0xFFFFFF, 0xFFFFFF, 0x0000FF, 0x0000FF, 0xFFFFFF, 0xFFFFFF,
            ]
        );
    }

    #[test]
    fn rejects_pixel_buffers_smaller_than_their_dimensions() {
        let mut target = [0; 4];
        let error = copy_scaled_pixels(&[0; 3], 1, 1, &mut target, 2, 2).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}

/// Loads a user-selected font or the first available common Linux sans font.
fn load_system_font() -> Result<Font, Box<dyn Error>> {
    if let Some(path) = env::var_os("BWEB_FONT") {
        let path = PathBuf::from(path);
        let bytes = fs::read(&path)?;
        return Ok(Font::from_bytes(bytes)?);
    }

    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSans-Regular.ttf",
    ];
    for candidate in CANDIDATES {
        let path = Path::new(candidate);
        if let Ok(bytes) = fs::read(path) {
            if let Ok(font) = Font::from_bytes(bytes) {
                return Ok(font);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no supported system font found; install DejaVu Sans or set BWEB_FONT to a TTF/OTF file",
    )
    .into())
}

/// Writes the framebuffer as binary PPM (P6), dropping alpha over its white base.
fn write_ppm(path: &Path, framebuffer: &Framebuffer) -> Result<(), io::Error> {
    let file = fs::File::create(path)?;
    let mut writer = BufWriter::new(file);
    writeln!(writer, "P6")?;
    writeln!(writer, "{} {}", framebuffer.width(), framebuffer.height())?;
    writeln!(writer, "255")?;
    for pixel in framebuffer.pixels().chunks_exact(4) {
        writer.write_all(&pixel[..3])?;
    }
    writer.flush()
}

fn build_transport() -> Result<(HttpTransport, bool), Box<dyn Error>> {
    let cache_directory = filter_list_cache_directory()?;
    let cache = FilterListCache::new(&cache_directory);
    let shield = match cache.load_shield() {
        Ok(shield) => {
            println!("Shield: ready (cached EasyList/EasyPrivacy rules loaded)");
            (shield, true)
        }
        Err(FilterListError::NoCachedLists) => {
            println!("Shield: inactive (empty ruleset; no cached filter lists)");
            println!("Run 'b-web update-filters' to download the built-in filter lists.");
            (Shield::empty(), false)
        }
        Err(error) => return Err(error.into()),
    };
    let transport = HttpTransport::new(NetworkPolicy::new(shield.0))?;
    println!("Cache: {}", cache.directory().display());
    Ok((transport, shield.1))
}

fn print_usage() {
    println!("B-Web {}", bweb_core::VERSION);
    println!();
    println!("Usage:");
    println!("  b-web                 Show status and load cached Shield rules");
    println!(
        "  b-web open <URL> [--dump-layout] [--render-ppm FILE] [--window]  Fetch, layout, and display a frame"
    );
    println!("  b-web update-filters  Download and cache EasyList and EasyPrivacy");
    println!(
        "  b-web --help          Show this help\n\nRendering: set BWEB_FONT to select a TTF/OTF font; otherwise common Linux sans fonts are tried.\nUse --window to display the software-rendered frame in a native window."
    );
}

fn update_filters() -> Result<(), Box<dyn Error>> {
    let cache_directory = filter_list_cache_directory()?;
    let cache = FilterListCache::new(&cache_directory);
    let downloader = FilterListDownloader::new()?;
    println!("Updating built-in filter lists...");
    let update = downloader.update_cache(&cache)?;
    println!("Filter lists updated successfully.");
    println!("  EasyList:    {} bytes", update.easylist_bytes);
    println!("  EasyPrivacy: {} bytes", update.easyprivacy_bytes);
    println!("  Cache:       {}", cache.directory().display());
    Ok(())
}

fn filter_list_cache_directory() -> Result<PathBuf, io::Error> {
    if let Some(path) = env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        if path.is_absolute() {
            return Ok(path.join("b-web").join("filter-lists"));
        }
    }
    let home = env::var_os("HOME").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not set; cannot locate the filter-list cache",
        )
    })?;
    Ok(PathBuf::from(home).join(".cache").join("b-web").join("filter-lists"))
}

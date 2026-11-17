mod address_bar;
mod chrome;
mod generated;
mod history;
mod images;
mod render_view;
mod scripting;
mod shell;

use address_bar::AddressBar;
use bweb_css::{ComputedStyle, ComputedStyleMap, PseudoElement, StyleEngine, Stylesheet};
use bweb_filterlists::FilterListDownloader;
use bweb_html::{DocumentLoader, LoadedDocument, NodeId, ResourceKind};
use bweb_js::{Runtime, Value};
use bweb_layout::{ImageSize, LayoutTree, PaintAdapter, Rect as LayoutRect, View, Viewport};
use bweb_net::{HttpTransport, NetworkPolicy};
use bweb_renderer::{Framebuffer, Image, Painter, Rasterizer};
use bweb_shield::{FilterListCache, FilterListError, Shield};
use bweb_text::Font;
use chrome::{address_bar_height, draw_address_bar};
use generated::pseudo_content;
use history::{History, address_to_url, resolve_link_with_base};
use images::{collect_images, image_sizes};
use render_view::BrowserViewBackend;
use scripting::{DomHost, EventRegistry};
use shell::{ResizeController, ResizeDispatch};
use softbuffer::{Context, Surface};
use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    error::Error,
    fs, io,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process,
    rc::Rc,
    sync::Arc,
};
use url::Url;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
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
    println!("Stage: {:?}", bweb_core::STAGE);
    println!("Privacy engine: {}", bweb_shield::FILTER_ENGINE);
    println!("JavaScript runtime: {:?} (in-process Rust runtime)", bweb_js::RuntimeStatus::Ready);
    let (_transport, _shield_active) = build_transport(true)?;
    println!("HTTP/HTTPS transport: ready (rustls, Shield-gated, Sec-GPC/DNT enabled)");
    Ok(())
}

/// Why a navigation was started; used to roll history back on failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationKind {
    New,
    Back,
    Forward,
}

struct PendingNavigation {
    id: u64,
    kind: NavigationKind,
}

/// Result of a background page load, delivered back to the event loop.
enum UserEvent {
    NavigationFinished { id: u64, url: Url, result: Result<Page, String> },
}

/// A loaded document together with the styles used to lay it out.
struct Page {
    loaded: LoadedDocument,
    stylesheets: Vec<Stylesheet>,
    images: HashMap<NodeId, Arc<Image>>,
    background_images: HashMap<String, Arc<Image>>,
    scripts: Vec<String>,
}

/// Loads a page through the guarded transport and computes its styles.
///
/// Navigation from the native window reuses this exact path, so every visited
/// URL still passes through Shield and the privacy policy.
fn load_page(url: &str, verbose: bool) -> Result<Page, Box<dyn Error>> {
    let (transport, shield_active) = build_transport(verbose)?;
    if verbose {
        println!("Loading: {url}");
        if !shield_active {
            println!("Warning: Shield is inactive because no cached filter lists are available.");
        }
    }
    let loaded = DocumentLoader::new(transport).load(url)?;

    let mut stylesheets = Vec::new();
    let user_agent = Stylesheet::parse(
        "html, body, main, header, footer, nav, section, article, aside, div, p, h1, h2, h3, ul, ol, li, form, table, blockquote { display: block; }",
    )
    .expect("built-in user-agent stylesheet");
    stylesheets.push(user_agent);
    for source in loaded.document.inline_stylesheets() {
        match Stylesheet::parse(&source) {
            Ok(sheet) => {
                stylesheets.push(sheet);
            }
            Err(error) if verbose => eprintln!("  invalid inline stylesheet: {error}"),
            Err(_) => {}
        }
    }
    for resource in
        loaded.resources.iter().filter(|resource| resource.kind == ResourceKind::Stylesheet)
    {
        match std::str::from_utf8(&resource.bytes) {
            Ok(source) => match Stylesheet::parse(source) {
                Ok(sheet) => {
                    stylesheets.push(sheet);
                }
                Err(error) if verbose => {
                    eprintln!("  invalid stylesheet {}: {error}", resource.url)
                }
                Err(_) => {}
            },
            Err(_) if verbose => eprintln!("  invalid UTF-8 stylesheet: {}", resource.url),
            Err(_) => {}
        }
    }

    let mut scripts = loaded.document.script_sources();
    for resource in loaded.resources.iter().filter(|resource| resource.kind == ResourceKind::Script)
    {
        match std::str::from_utf8(&resource.bytes) {
            Ok(source) => scripts.push(source.to_owned()),
            Err(_) if verbose => eprintln!("  invalid UTF-8 script: {}", resource.url),
            Err(_) => {}
        }
    }
    let images = collect_images(&loaded.document, &loaded.url, &loaded.resources);
    let mut background_images = HashMap::new();
    for resource in loaded.resources.iter().filter(|item| item.kind == ResourceKind::Image) {
        if let Ok(image) = images::decode(&resource.bytes) {
            background_images.insert(resource.url.as_str().to_owned(), Arc::new(image));
        }
    }
    Ok(Page { loaded, stylesheets, images, background_images, scripts })
}

struct PreparedPage {
    styles: ComputedStyleMap,
    pseudo_styles: HashMap<(NodeId, PseudoElement), ComputedStyle>,
    runtime: Runtime,
    events: Rc<RefCell<EventRegistry>>,
}

/// Runs scripts on the UI thread, then computes styles from the mutated DOM.
fn prepare_page(page: &mut Page) -> PreparedPage {
    let document = Rc::new(RefCell::new(std::mem::take(&mut page.loaded.document)));
    let events = Rc::new(RefCell::new(EventRegistry::default()));
    let mut runtime = Runtime::new();
    runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
    runtime.define_global("document", Value::Host("document".into()));
    for source in &page.scripts {
        if let Err(error) = runtime.eval(source) {
            eprintln!("  script error: {error}");
        }
    }
    for line in runtime.output() {
        println!("  script: {line}");
    }
    page.loaded.document = Rc::try_unwrap(document).expect("script host released").into_inner();
    let (styles, pseudo_styles) = StyleEngine::new().compute_generated(
        &page.loaded.document,
        &page.stylesheets,
        1280.0,
        720.0,
    );
    PreparedPage { styles, pseudo_styles, runtime, events }
}

fn open_document(
    url: &str,
    dump_layout: bool,
    render_path: Option<&Path>,
    show_window: bool,
) -> Result<(), Box<dyn Error>> {
    let mut page = load_page(url, true)?;
    let mut prepared = prepare_page(&mut page);
    prepared.styles.resolve_urls(page.loaded.url.as_str());
    let mut font = if render_path.is_some() || show_window || !page.images.is_empty() {
        Some(load_system_font()?)
    } else {
        None
    };
    let styles = &prepared.styles;
    let sizes: HashMap<NodeId, ImageSize> = image_sizes(&page.images);
    let pseudo = pseudo_content(&prepared.pseudo_styles);
    let layout = LayoutTree::build_with_all(
        &page.loaded.document,
        styles,
        1280.0,
        font.as_ref(),
        &sizes,
        &pseudo,
    );
    println!("Document: {}", page.loaded.url);
    println!("Title: {}", page.loaded.document.title().as_deref().unwrap_or("(untitled)"));
    println!("DOM nodes: {}", page.loaded.document.node_count());
    println!("Loaded subresources: {}", page.loaded.resources.len());
    println!("Parsed stylesheets: {}", page.stylesheets.len());
    println!("Computed styles: {} NodeId entries", styles.len());
    println!("Layout boxes: {}", layout.box_count);
    if let Some(path) = render_path {
        let (framebuffer, glyph_count) = render_frame(
            &layout,
            styles,
            1280,
            720,
            font.as_mut().expect("a font is loaded when rendering"),
            &page.images,
            &page.background_images,
        )?;
        write_ppm(path, &framebuffer)?;
        println!("Rendered frame: 1280x720 px, {glyph_count} glyphs -> {}", path.display());
    }
    if show_window {
        println!("Opening native B-Web window...");
        show_browser_window(&page, prepared.runtime, prepared.events, &prepared.pseudo_styles)?;
    }
    if dump_layout {
        println!();
        print!("{}", layout.dump(&page.loaded.document));
    }
    println!("Viewport: 1280 CSS px");
    println!("Failed/blocked subresources: {}", page.loaded.failed_resources.len());
    for resource in &page.loaded.resources {
        println!("  {:?}: {} ({} bytes)", resource.kind, resource.url, resource.bytes.len());
    }
    for failure in &page.loaded.failed_resources {
        println!("  blocked/unavailable: {} ({})", failure.reference.url, failure.error);
    }
    println!(
        "Note: CSS, JavaScript, first-pass layout, images and link navigation are active; GPU painting and full web-platform compatibility are not implemented."
    );
    Ok(())
}

/// Runs HTML/CSS layout through text preparation, painting, and rasterization.
fn render_frame(
    layout: &LayoutTree,
    styles: &bweb_css::ComputedStyleMap,
    width: u32,
    height: u32,
    font: &mut Font,
    images: &HashMap<NodeId, Arc<Image>>,
    background_images: &HashMap<String, Arc<Image>>,
) -> Result<(Framebuffer, usize), Box<dyn Error>> {
    let scale_factor = 1.0;
    let text_runs = bweb_layout::prepare_text_runs(layout, styles, font, scale_factor);
    let glyph_count = text_runs.values().map(|run| run.glyphs.len()).sum();
    let viewport = LayoutRect { x: 0.0, y: 0.0, width: width as f32, height: height as f32 };
    let adapter = PaintAdapter::new(layout, styles, viewport)
        .with_text_runs(&text_runs)
        .with_images(images)
        .with_background_images(background_images);
    let display_list = Painter::new(&adapter, scale_factor)?.paint();
    let mut framebuffer = Framebuffer::new(width, height)
        .map_err(|error| io::Error::other(format!("could not allocate render frame: {error:?}")))?;
    framebuffer.clear([255, 255, 255, 255]);
    Rasterizer.rasterize(&display_list, &mut framebuffer);
    Ok((framebuffer, glyph_count))
}

/// Viewport excluding the address bar, in physical pixels.
fn content_viewport(width: u32, height: u32, scale_factor: f64) -> Option<Viewport> {
    let bar = address_bar_height(scale_factor);
    Viewport::new(width, height.checked_sub(bar)?, scale_factor)
}

/// Displays the retained browser view in a native winit + softbuffer window.
fn show_browser_window(
    page: &Page,
    runtime: Runtime,
    events: Rc<RefCell<EventRegistry>>,
    pseudo_styles: &HashMap<(NodeId, PseudoElement), ComputedStyle>,
) -> Result<(), Box<dyn Error>> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let context = Context::new(event_loop.owned_display_handle())?;
    let font = load_system_font()?;
    let chrome_font = load_system_font()?;
    let backend = BrowserViewBackend::new(
        page.loaded.document.clone(),
        page.stylesheets.clone(),
        font,
        page.images.clone(),
        page.background_images.clone(),
        pseudo_styles.clone(),
        page.loaded.url.as_str(),
    );
    let history = History::new(page.loaded.url.clone());
    let address_bar = AddressBar::new(page.loaded.url.as_str());
    let mut app = WindowApp {
        context,
        controller: ResizeController::new(View::new(backend)),
        history,
        address_bar,
        chrome_font,
        runtime,
        events,
        proxy,
        pending: None,
        next_navigation_id: 0,
        window: None,
        surface: None,
        surface_size: None,
        content_viewport: None,
        cursor: None,
        modifiers: ModifiersState::empty(),
        failure: None,
    };
    event_loop.run_app(&mut app)?;
    if let Some(error) = app.failure {
        return Err(io::Error::other(error).into());
    }
    Ok(())
}

struct WindowApp<D> {
    context: Context<D>,
    controller: ResizeController<BrowserViewBackend>,
    history: History,
    address_bar: AddressBar,
    chrome_font: Font,
    runtime: Runtime,
    events: Rc<RefCell<EventRegistry>>,
    proxy: EventLoopProxy<UserEvent>,
    pending: Option<PendingNavigation>,
    next_navigation_id: u64,
    window: Option<Arc<Window>>,
    surface: Option<Surface<D, Arc<Window>>>,
    surface_size: Option<(u32, u32)>,
    content_viewport: Option<Viewport>,
    cursor: Option<(f64, f64)>,
    modifiers: ModifiersState,
    failure: Option<String>,
}

impl<D: raw_window_handle::HasDisplayHandle + 'static> ApplicationHandler<UserEvent>
    for WindowApp<D>
{
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::NavigationFinished { id, url, result } = event;
        if self.pending.as_ref().is_none_or(|pending| pending.id != id) {
            return;
        }
        let pending = self.pending.take().expect("pending navigation was verified above");
        let window = self.window.clone();

        match result {
            Ok(mut page) => {
                if pending.kind == NavigationKind::New {
                    self.history.push(url.clone());
                }
                self.address_bar.set_url(url.as_str());
                let Ok(font) = load_system_font() else {
                    eprintln!("B-Web: could not load a system font for the new page");
                    self.address_bar.set_url(self.history.current().as_str());
                    return;
                };
                let document = Rc::new(RefCell::new(std::mem::take(&mut page.loaded.document)));
                self.events.borrow_mut().clear();
                self.runtime.set_host(Box::new(DomHost::new(&document, self.events.clone())));
                self.runtime.define_global("document", Value::Host("document".into()));
                for source in &page.scripts {
                    if let Err(error) = self.runtime.eval(source) {
                        eprintln!("  script error: {error}");
                    }
                }
                for line in self.runtime.take_output() {
                    println!("  script: {line}");
                }
                page.loaded.document =
                    Rc::try_unwrap(document).expect("script host released").into_inner();
                let (_, pseudo_styles) = StyleEngine::new().compute_generated(
                    &page.loaded.document,
                    &page.stylesheets,
                    1280.0,
                    720.0,
                );
                let backend = BrowserViewBackend::new(
                    page.loaded.document,
                    page.stylesheets,
                    font,
                    page.images,
                    page.background_images,
                    pseudo_styles,
                    page.loaded.url.as_str(),
                );
                self.controller = ResizeController::new(View::new(backend));
                if let Some(viewport) = self.content_viewport {
                    self.controller.queue(viewport);
                }
            }
            Err(error) => {
                eprintln!("B-Web: navigation to {url} failed: {error}");
                match pending.kind {
                    NavigationKind::New => {}
                    NavigationKind::Back => {
                        self.history.forward();
                    }
                    NavigationKind::Forward => {
                        self.history.back();
                    }
                }
                self.address_bar.set_url(self.history.current().as_str());
            }
        }
        if let Some(window) = window {
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("B-Web")
                .with_inner_size(winit::dpi::PhysicalSize::new(1280, 720))
                .with_min_inner_size(winit::dpi::PhysicalSize::new(640, 360)),
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
                let size = window.inner_size();
                if let Some(viewport) =
                    content_viewport(size.width, size.height, window.scale_factor())
                {
                    self.content_viewport = Some(viewport);
                    self.controller.queue(viewport);
                }
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
            WindowEvent::Resized(size) => {
                if let Some(viewport) =
                    content_viewport(size.width, size.height, window.scale_factor())
                {
                    self.content_viewport = Some(viewport);
                    self.controller.queue(viewport);
                }
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = window.inner_size();
                if let Some(viewport) = content_viewport(size.width, size.height, scale_factor) {
                    self.content_viewport = Some(viewport);
                    self.controller.queue(viewport);
                }
                window.request_redraw();
            }
            WindowEvent::Occluded(false) => window.request_redraw(),
            WindowEvent::MouseWheel { delta, .. } => {
                let scale = window.scale_factor();
                let pixels = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => f64::from(lines) * 16.0 * scale,
                    MouseScrollDelta::PixelDelta(position) => position.y * scale,
                };
                if let Some(viewport) = self.content_viewport {
                    let backend = self.controller.view_mut().backend_mut();
                    let next = f64::from(backend.scroll_y()) + pixels;
                    backend.set_scroll_y(next as f32, viewport.height(), viewport.scale_factor());
                    if let Err(error) = self.controller.repaint() {
                        self.failure = Some(format!("could not repaint after scroll: {error}"));
                        event_loop.exit();
                    }
                }
                window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x, position.y));
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.handle_click(&window),
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    self.handle_key(&event, &window);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.dispatch_pending_and_present() {
                    self.failure = Some(format!("could not present B-Web frame: {error}"));
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }
}

impl<D: raw_window_handle::HasDisplayHandle> WindowApp<D> {
    fn handle_click(&mut self, window: &Window) {
        let Some((x, y)) = self.cursor else { return };
        let Some(scale) = self.content_viewport.map(|viewport| viewport.scale_factor()) else {
            return;
        };
        let bar = address_bar_height(scale);
        if y < f64::from(bar) {
            return;
        }
        let (target, href, base_href) = {
            let scroll = self.controller.view().backend().scroll_y();
            let backend = self.controller.view().backend();
            let target = backend.hit_test(x as f32, (y - f64::from(bar)) as f32 + scroll);
            let href =
                target.and_then(|node| backend.document().hyperlink_href(node).map(str::to_owned));
            let base_href = backend.document().base_href().map(str::to_owned);
            (target, href, base_href)
        };
        if let Some(target) = target {
            let mut chain = vec![target];
            let ancestors = {
                let backend = self.controller.view().backend();
                backend.document().ancestor_elements(target)
            };
            chain.extend(ancestors);
            self.dispatch_click(&chain);
        }
        if self.events.borrow().prevented() {
            return;
        }
        let Some(href) = href else { return };
        let Some(url) =
            resolve_link_with_base(self.history.current().as_str(), base_href.as_deref(), &href)
        else {
            return;
        };
        self.start_navigation(url, NavigationKind::New, window);
    }

    fn dispatch_click(&mut self, targets: &[NodeId]) {
        self.events.borrow_mut().reset();
        let event = Value::Host("event".into());
        for target in targets {
            let object = format!("element:{}", target.index());
            let callbacks = self.events.borrow().callbacks(&object, "click");
            for callback in callbacks {
                if let Err(error) = self.runtime.call(callback, vec![event.clone()]) {
                    eprintln!("B-Web: click handler error: {error}");
                }
            }
        }
    }

    fn handle_key(&mut self, event: &KeyEvent, window: &Window) {
        if self.modifiers.control_key()
            && !self.modifiers.alt_key()
            && matches!(&event.logical_key, Key::Character(text) if text.as_str().eq_ignore_ascii_case("l"))
        {
            self.address_bar.clear();
            window.request_redraw();
            return;
        }
        if self.modifiers.alt_key() && !self.modifiers.control_key() {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowLeft) => {
                    self.go_back(window);
                    return;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.go_forward(window);
                    return;
                }
                _ => {}
            }
        }
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => {
                if let Some(url) = address_to_url(self.address_bar.text()) {
                    self.start_navigation(url, NavigationKind::New, window);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                self.address_bar.backspace();
                window.request_redraw();
            }
            Key::Character(text) if !self.modifiers.control_key() && !self.modifiers.alt_key() => {
                for character in text.chars() {
                    self.address_bar.insert(character);
                }
                window.request_redraw();
            }
            _ => {}
        }
    }

    fn go_back(&mut self, window: &Window) {
        if !self.history.can_back() {
            return;
        }
        self.history.back();
        let url = self.history.current().clone();
        self.start_navigation(url, NavigationKind::Back, window);
    }

    fn go_forward(&mut self, window: &Window) {
        if !self.history.can_forward() {
            return;
        }
        self.history.forward();
        let url = self.history.current().clone();
        self.start_navigation(url, NavigationKind::Forward, window);
    }

    /// Starts a page load on a background thread; the event loop installs the
    /// result when it arrives, keeping the window responsive during network I/O.
    fn start_navigation(&mut self, url: Url, kind: NavigationKind, window: &Window) {
        self.next_navigation_id += 1;
        let id = self.next_navigation_id;
        self.pending = Some(PendingNavigation { id, kind });
        self.address_bar.set_url(url.as_str());
        window.request_redraw();

        let proxy = self.proxy.clone();
        let requested = url.clone();
        std::thread::spawn(move || {
            let result = load_page(requested.as_str(), false).map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::NavigationFinished { id, url: requested, result });
        });
    }

    fn dispatch_pending_and_present(&mut self) -> Result<(), Box<dyn Error>> {
        match self.controller.dispatch().map_err(io::Error::other)? {
            ResizeDispatch::Skip | ResizeDispatch::Repaint | ResizeDispatch::Reflow => {}
        }
        self.present_frame()
    }

    fn present_frame(&mut self) -> Result<(), Box<dyn Error>> {
        let window =
            self.window.as_ref().ok_or_else(|| io::Error::other("window is not initialized"))?;
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }

        let surface = self
            .surface
            .as_mut()
            .ok_or_else(|| io::Error::other("window surface is not initialized"))?;
        let dimensions = (size.width, size.height);
        if self.surface_size != Some(dimensions) {
            let width = std::num::NonZeroU32::new(size.width)
                .ok_or_else(|| io::Error::other("surface width is zero"))?;
            let height = std::num::NonZeroU32::new(size.height)
                .ok_or_else(|| io::Error::other("surface height is zero"))?;
            surface.resize(width, height)?;
            self.surface_size = Some(dimensions);
        }

        let scale = window.scale_factor();
        let mut composed = {
            let content = self
                .controller
                .view()
                .backend()
                .framebuffer()
                .ok_or_else(|| io::Error::other("browser frame has not been rendered"))?;
            let mut composed = Framebuffer::new(size.width, size.height).map_err(|error| {
                io::Error::other(format!("could not allocate composed frame: {error:?}"))
            })?;
            composed.clear([255, 255, 255, 255]);
            let bar = address_bar_height(scale).min(size.height);
            let rows = content.height().min(size.height.saturating_sub(bar));
            let columns = content.width().min(size.width) as usize * 4;
            for row in 0..rows {
                let source = row as usize * content.stride();
                let target = (row as usize + bar as usize) * composed.stride();
                composed.pixels_mut()[target..target + columns]
                    .copy_from_slice(&content.pixels()[source..source + columns]);
            }
            composed
        };
        draw_address_bar(&mut self.chrome_font, &mut composed, self.address_bar.text(), scale);

        let mut buffer = surface.buffer_mut()?;
        copy_scaled_pixels(
            composed.pixels(),
            composed.width(),
            composed.height(),
            &mut buffer,
            size.width,
            size.height,
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

    // Fit the source frame inside the surface without distorting its aspect ratio.
    target[..target_len].fill(0x00FF_FFFF);
    let (content_width, content_height) = if u64::from(target_width) * u64::from(source_height)
        <= u64::from(target_height) * u64::from(source_width)
    {
        let height =
            (u64::from(source_height) * u64::from(target_width) / u64::from(source_width)) as u32;
        (target_width, height.max(1))
    } else {
        let width =
            (u64::from(source_width) * u64::from(target_height) / u64::from(source_height)) as u32;
        (width.max(1), target_height)
    };
    let offset_x = (target_width - content_width) / 2;
    let offset_y = (target_height - content_height) / 2;

    for y in 0..content_height {
        let source_y =
            (u64::from(y) * u64::from(source_height) / u64::from(content_height)) as usize;
        for x in 0..content_width {
            let source_x =
                (u64::from(x) * u64::from(source_width) / u64::from(content_width)) as usize;
            let source_index = (source_y * source_width as usize + source_x) * 4;
            let red = u32::from(source[source_index]);
            let green = u32::from(source[source_index + 1]);
            let blue = u32::from(source[source_index + 2]);
            let target_index =
                ((y + offset_y) as usize * target_width as usize) + (x + offset_x) as usize;
            target[target_index] = (red << 16) | (green << 8) | blue;
        }
    }
    Ok(())
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

fn build_transport(verbose: bool) -> Result<(HttpTransport, bool), Box<dyn Error>> {
    let cache_directory = filter_list_cache_directory()?;
    let cache = FilterListCache::new(&cache_directory);
    let shield = match cache.load_shield() {
        Ok(shield) => {
            if verbose {
                println!("Shield: ready (cached EasyList/EasyPrivacy rules loaded)");
            }
            (shield, true)
        }
        Err(FilterListError::NoCachedLists) => {
            if verbose {
                println!("Shield: inactive (empty ruleset; no cached filter lists)");
                println!("Run 'b-web update-filters' to download the built-in filter lists.");
            }
            (Shield::empty(), false)
        }
        Err(error) => return Err(error.into()),
    };
    let transport = HttpTransport::new(NetworkPolicy::new(shield.0))?;
    if verbose {
        println!("Cache: {}", cache.directory().display());
    }
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
        "  b-web --help          Show this help\n\nRendering: set BWEB_FONT to select a TTF/OTF font; otherwise common Linux sans fonts are tried.\nUse --window to display the software-rendered frame in a native window. Click links, type a URL and press Enter, use Ctrl+L to clear the address bar, Alt+Left/Alt+Right for history, and the mouse wheel to scroll."
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

#[cfg(test)]
mod tests {
    use super::{content_viewport, copy_scaled_pixels};
    use crate::scripting::{DomHost, EventRegistry};
    use bweb_html::{HtmlDocument, NodeKind};
    use bweb_js::{Runtime, Value};
    use std::{cell::RefCell, rc::Rc};

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
    fn preserves_frame_aspect_ratio_and_centers_letterboxing() {
        let source = [255, 0, 0, 255, 0, 255, 0, 255];
        let mut target = [0; 16];

        copy_scaled_pixels(&source, 2, 1, &mut target, 4, 4).unwrap();

        assert_eq!(
            target,
            [
                0xFFFFFF, 0xFFFFFF, 0xFFFFFF, 0xFFFFFF, 0xFF0000, 0xFF0000, 0x00FF00, 0x00FF00,
                0xFF0000, 0xFF0000, 0x00FF00, 0x00FF00, 0xFFFFFF, 0xFFFFFF, 0xFFFFFF, 0xFFFFFF,
            ]
        );
    }

    #[test]
    fn rejects_pixel_buffers_smaller_than_their_dimensions() {
        let mut target = [0; 4];
        let error = copy_scaled_pixels(&[0; 3], 1, 1, &mut target, 2, 2).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn click_handlers_run_and_can_prevent_navigation() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p>x</p>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("document.querySelector('p').addEventListener('click', function(e) { e.preventDefault(); });")
            .expect("script");

        let paragraph = document
            .borrow()
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        events.borrow_mut().reset();
        let callbacks =
            events.borrow().callbacks(&format!("element:{}", paragraph.index()), "click");
        assert_eq!(callbacks.len(), 1);
        for callback in callbacks {
            runtime.call(callback, vec![Value::Host("event".into())]).expect("handler");
        }
        assert!(events.borrow().prevented());
    }

    #[test]
    fn content_viewport_reserves_space_for_the_address_bar() {
        let viewport = content_viewport(800, 600, 1.0).expect("viewport");
        assert_eq!((viewport.width(), viewport.height(), viewport.scale_factor()), (800, 568, 1.0));
        assert!(content_viewport(800, 16, 1.0).is_none());
    }
}

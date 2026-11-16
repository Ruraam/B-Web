use bweb_css::{StyleEngine, Stylesheet};
use bweb_filterlists::FilterListDownloader;
use bweb_html::DocumentLoader;
use bweb_layout::LayoutTree;
use bweb_net::{HttpTransport, NetworkPolicy};
use bweb_shield::{FilterListCache, FilterListError, Shield};
use std::{env, error::Error, io, path::PathBuf, process};

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
            let dump_layout = match arguments.next() {
                None => false,
                Some(flag) if flag == "--dump-layout" => true,
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "expected optional --dump-layout flag",
                    )
                    .into());
                }
            };
            if arguments.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "too many arguments for 'open'",
                )
                .into());
            }
            open_document(url.to_string_lossy().as_ref(), dump_layout)?;
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

fn open_document(url: &str, dump_layout: bool) -> Result<(), Box<dyn Error>> {
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
    println!("  b-web open <URL> [--dump-layout]  Fetch a document; optionally dump geometry");
    println!("  b-web update-filters  Download and cache EasyList and EasyPrivacy");
    println!("  b-web --help          Show this help");
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

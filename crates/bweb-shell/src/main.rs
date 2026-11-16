use bweb_filterlists::FilterListDownloader;
use bweb_shield::FilterListCache;
use std::{
    env,
    error::Error,
    io,
    path::PathBuf,
    process,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("B-Web: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let command = env::args_os().nth(1);

    match command.as_deref() {
        None => {
            println!("B-Web {}", bweb_core::VERSION);
            println!("Stage: {:?}", bweb_core::EngineStage::Bootstrap);
            println!("Privacy engine: {}", bweb_shield::FILTER_ENGINE);
            println!("JavaScript runtime: {:?}", bweb_js::RuntimeStatus::NotImplemented);
            println!("Run 'b-web update-filters' to download the built-in filter lists.");
        }
        Some(command) if command == "--help" || command == "-h" => print_usage(),
        Some(command) if command == "update-filters" => update_filters()?,
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

fn print_usage() {
    println!("B-Web {}", bweb_core::VERSION);
    println!();
    println!("Usage:");
    println!("  b-web                 Show engine status");
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
        io::Error::new(io::ErrorKind::NotFound, "HOME is not set; cannot locate the filter-list cache")
    })?;

    Ok(PathBuf::from(home).join(".cache").join("b-web").join("filter-lists"))
}

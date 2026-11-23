//! Time working out where every Library file goes on a device: the catalog
//! read and the paths, for the Library's folder structure and a device's
//! own, and for one that puts thousands of files at the same path, to be
//! numbered.
//!
//! Usage: cargo run --release -p coversync-core --example sync_paths_bench -- STATE [--create]
//!
//! STATE is a scratch copy of a state directory: opening it may migrate its
//! databases. `--create` gives its Library folder an empty database first,
//! as an import would, for filling from elsewhere.
use std::path::Path;
use std::time::Instant;

use coversync_core::Library;
use coversync_core::sync::{DeviceSettings, device_paths};
use coversync_core::template::DEFAULT;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let state = args.next().ok_or("expected a scratch STATE directory")?;
    let library = Library::open_in(Path::new(&state))?;
    if args.next().as_deref() == Some("--create") {
        let folder = library.library_setup()?.folder.ok_or("no Library folder")?;
        library.prepare_library(Path::new(&folder))?;
        println!("created the Library database in {folder}");
        return Ok(());
    }
    for structure in [None, Some(DEFAULT), Some("{genre}")] {
        let settings = DeviceSettings {
            structure: structure.map(str::to_string),
            ..Default::default()
        };
        for _ in 0..5 {
            let start = Instant::now();
            let catalog = library
                .sync_catalog(structure.is_some())?
                .ok_or("no Library")?;
            let read = start.elapsed();
            // As `PathCache::paths`: only a device's own structure needs it.
            let separator = match structure.is_some() {
                true => library.library_setup()?.rules.separator,
                false => String::new(),
            };
            let paths = device_paths(catalog, 1, &settings, &separator)?;
            println!(
                "{}",
                serde_json::json!({
                    "structure": structure.unwrap_or("Library's"),
                    "files": paths.files.len(),
                    "incomplete": paths.incomplete,
                    "renamed": paths.renamed,
                    "catalog_ms": read.as_secs_f64() * 1000.,
                    "total_ms": start.elapsed().as_secs_f64() * 1000.,
                })
            );
        }
    }
    Ok(())
}

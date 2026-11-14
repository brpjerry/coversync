//! Compare the hash pass with plain parallel reads of the same scratch Library.
//! Usage: cargo run --release -p coversync-core --example hash_bench -- STATE MODE
//! MODE is prepare (create the configured Library DB), read, or hash.
//! Use a scratch state/database; hash publishes digests in its configured Library.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

use coversync_core::{Collection, Library, hashes};
use rayon::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let [_, state, mode] = args.as_slice() else {
        return Err("usage: hash_bench STATE prepare|read|hash".into());
    };
    let lib = Library::open_in(Path::new(state))?;
    let start = Instant::now();
    let bytes = match mode.as_str() {
        "prepare" => {
            let folder = lib.library_setup()?.folder.ok_or("no Library folder")?;
            lib.prepare_library(Path::new(&folder))?;
            0
        }
        "read" => {
            let mut paths: Vec<_> = lib
                .file_stats()?
                .into_iter()
                .filter(|(_, (_, _, c))| *c == Collection::Library)
                .map(|(p, _)| p)
                .collect();
            paths.sort();
            let mut bytes = 0;
            for batch in paths.chunks(128) {
                bytes += batch
                    .par_iter()
                    .map(|path| -> std::io::Result<u64> {
                        let mut file = File::open(path)?;
                        let mut buffer = vec![0; 1024 * 1024];
                        let mut bytes = 0;
                        loop {
                            let n = file.read(&mut buffer)?;
                            if n == 0 {
                                break;
                            }
                            bytes += n as u64;
                        }
                        Ok(bytes)
                    })
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .sum::<u64>();
            }
            bytes
        }
        "hash" => {
            let progress = Default::default();
            hashes::run(&lib, &progress)?;
            let progress = progress.read().unwrap();
            println!("{}", serde_json::to_string(&*progress)?);
            progress.bytes_hashed
        }
        _ => return Err("mode must be prepare, read, or hash".into()),
    };
    let seconds = start.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({"mode": mode, "bytes": bytes, "seconds": seconds,
        "gib_per_second": bytes as f64 / seconds / (1_u64 << 30) as f64})
    );
    Ok(())
}

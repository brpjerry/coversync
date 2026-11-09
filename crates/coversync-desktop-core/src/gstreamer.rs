//! Whether GStreamer can play audio for WebKitGTK.
//!
//! WebKitGTK plays `<audio>` through GStreamer's `autoaudiosink`, from the
//! "good" plugins (gst-plugins-good on Arch, gstreamer1.0-plugins-good on
//! Debian and Ubuntu). Without it, WebKitGTK 2.54's web process aborts as
//! playback starts.

use std::env;
use std::path::{Path, PathBuf};

/// The plugin that holds `autoaudiosink`.
const AUTODETECT: &str = "libgstautodetect.so";

/// The folders GStreamer looks for plugins in: its environment variables, the
/// usual system folders (multiarch ones too), and the user's own.
fn plugin_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "GST_PLUGIN_SYSTEM_PATH_1_0",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_PATH_1_0",
        "GST_PLUGIN_PATH",
    ]
    .iter()
    .filter_map(env::var_os)
    .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
    .collect();
    for lib in ["/usr/lib", "/usr/lib64", "/usr/local/lib"] {
        dirs.push(Path::new(lib).join("gstreamer-1.0"));
        // Debian's /usr/lib/x86_64-linux-gnu/gstreamer-1.0 and the like.
        if let Ok(entries) = Path::new(lib).read_dir() {
            dirs.extend(
                entries
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().contains("-linux-"))
                    .map(|entry| entry.path().join("gstreamer-1.0")),
            );
        }
    }
    if let Some(data) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))
    {
        dirs.push(data.join("gstreamer-1.0/plugins"));
    }
    dirs
}

/// True when GStreamer is installed but `autoaudiosink` isn't: some plugin
/// folder exists and none holds the plugin. False when the plugin is there,
/// or when no plugin folder turns up, since then it can't be told.
pub fn autoaudiosink_missing() -> bool {
    missing_from(&plugin_dirs())
}

fn missing_from(dirs: &[PathBuf]) -> bool {
    let existing: Vec<&PathBuf> = dirs.iter().filter(|dir| dir.is_dir()).collect();
    !existing.is_empty() && !existing.iter().any(|dir| dir.join(AUTODETECT).is_file())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn tells_only_when_it_can() {
        let root = env::temp_dir().join(format!("coversync-gst-{}", std::process::id()));
        let core = root.join("core");
        let good = root.join("good");
        fs::create_dir_all(&core).unwrap();
        fs::create_dir_all(&good).unwrap();
        fs::write(core.join("libgstcoreelements.so"), "").unwrap();
        fs::write(good.join(AUTODETECT), "").unwrap();

        assert!(missing_from(std::slice::from_ref(&core)));
        assert!(!missing_from(&[core.clone(), good]));
        assert!(
            !missing_from(&[root.join("nowhere")]),
            "no folder, no claim"
        );
        assert!(!missing_from(&[]));

        fs::remove_dir_all(&root).unwrap();
    }
}

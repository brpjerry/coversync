//! Lossless audio that isn't FLAC (WAV, AIFF, WavPack, Monkey's Audio,
//! ALAC) is converted to FLAC on import, with ffmpeg: FLAC is the Library's
//! only lossless format.

use std::path::{Path, PathBuf};
use std::process::Command;

use lofty::file::{AudioFile, FileType};
use lofty::probe::Probe;

use crate::{Error, Result, tags};

/// Whether ffmpeg is installed.
pub fn available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether the import converts the file at `path` to FLAC: its audio is
/// lossless, but not FLAC.
pub fn converts(path: &Path) -> Result<bool> {
    let probe = Probe::open(path)
        .map_err(tags::tag_err)?
        .guess_file_type()?;
    Ok(match probe.file_type() {
        Some(FileType::Wav | FileType::Aiff | FileType::WavPack | FileType::Ape) => true,
        // ALAC, not AAC.
        Some(FileType::Mp4) => {
            let options = lofty::config::ParseOptions::new().read_cover_art(false);
            let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
            lofty::mp4::Mp4File::read_from(&mut f, options)
                .map_err(tags::tag_err)?
                .properties()
                .codec()
                == Some(lofty::mp4::Mp4Codec::ALAC)
        }
        _ => false,
    })
}

/// Convert `src` to a FLAC beside `dest`, with `src`'s tags, and return its
/// path: the caller puts it in place and removes it. Kept only if its audio
/// is `src`'s sample for sample, which float or 32-bit audio isn't.
pub fn to_flac(src: &Path, dest: &Path) -> Result<PathBuf> {
    let name = dest.file_name().ok_or(Error::NotFound)?.to_string_lossy();
    let flac = dest.with_file_name(format!(".coversync-tmp-flac-{name}"));
    let made = (|| {
        std::fs::create_dir_all(dest.parent().ok_or(Error::NotFound)?)?;
        // No tags or pictures from ffmpeg: coversync carries them over.
        let args = [
            "-map_metadata",
            "-1",
            "-c:a",
            "flac",
            "-fflags",
            "+bitexact",
            "-flags:a",
            "+bitexact",
            "-f",
            "flac",
        ];
        ffmpeg(src, &args, flac.as_os_str())?;
        if audio_md5(src)? != audio_md5(&flac)? {
            return Err(Error::Tags(format!(
                "{}: its audio can't be converted to FLAC without loss (float or 32-bit samples, say), so it wasn't imported",
                src.display()
            )));
        }
        tags::carry_tags(src, &flac)
    })();
    match made {
        Ok(()) => Ok(flac),
        Err(e) => {
            let _ = std::fs::remove_file(&flac);
            Err(e)
        }
    }
}

/// The MD5 of the file's first audio stream, decoded to 64-bit float
/// samples, which hold any integer sample exactly.
fn audio_md5(path: &Path) -> Result<String> {
    ffmpeg(path, &["-c:a", "pcm_f64le", "-f", "md5"], "-".as_ref())
}

/// Run ffmpeg quietly on `input`'s first audio stream, with `args`,
/// writing `output`; gives what it printed.
fn ffmpeg(input: &Path, args: &[&str], output: &std::ffi::OsStr) -> Result<String> {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(input)
        .args(["-map", "0:a:0"])
        .args(args)
        .arg(output)
        .output()
        .map_err(|e| Error::Tags(format!("ffmpeg couldn't run: {e}")))?;
    if !out.status.success() {
        return Err(Error::Tags(format!(
            "ffmpeg: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

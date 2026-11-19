//! Lossless audio that isn't FLAC (WAV, AIFF, WavPack, Monkey's Audio,
//! ALAC) is converted to FLAC on import, with ffmpeg: FLAC is the Library's
//! only lossless format.

use std::path::{Path, PathBuf};
use std::process::Command;

use lofty::file::{AudioFile, FileType};
use lofty::probe::Probe;

use crate::{Error, Result, tags};

/// Whether ffmpeg (with its ffprobe) is installed.
pub fn available() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Whether the import converts the file at `path` to FLAC: its audio is
/// lossless, but not FLAC (or FLAC in an MP4).
pub fn converts(path: &Path) -> Result<bool> {
    let probe = Probe::open(path)
        .map_err(tags::tag_err)?
        .guess_file_type()?;
    let options = lofty::config::ParseOptions::new().read_tags(false);
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    Ok(match probe.file_type() {
        Some(FileType::Wav | FileType::Aiff | FileType::Ape) => true,
        // Not hybrid: ffmpeg can't read its correction file, so the FLAC
        // would hold the lossy part alone.
        Some(FileType::WavPack) => lofty::wavpack::WavPackFile::read_from(&mut f, options)
            .map_err(tags::tag_err)?
            .properties()
            .is_lossless(),
        // ALAC, not AAC.
        Some(FileType::Mp4) => {
            use lofty::mp4::Mp4Codec::{ALAC, FLAC};
            let mp4 = lofty::mp4::Mp4File::read_from(&mut f, options).map_err(tags::tag_err)?;
            matches!(mp4.properties().codec(), Some(ALAC | FLAC))
        }
        _ => false,
    })
}

/// Convert `src` to a FLAC beside `dest`, with `src`'s tags and
/// permissions, and return its path: the caller puts it in place and
/// removes it. Kept only if its audio is `src`'s sample for sample, which
/// float or 32-bit audio FLAC can't hold isn't, and refused for a file with
/// more than one audio stream (and cover pictures), which a FLAC would lose.
pub fn to_flac(src: &Path, dest: &Path) -> Result<PathBuf> {
    let streams = ffprobe(src)?;
    let extra = streams
        .lines()
        .filter(|s| !s.starts_with("audio"))
        .any(|s| !s.ends_with(",1"));
    if extra || streams.lines().filter(|s| s.starts_with("audio")).count() != 1 {
        return Err(Error::Tags(format!(
            "{}: it holds more than one audio stream and its cover, which a FLAC would lose, so it wasn't imported",
            src.display()
        )));
    }
    std::fs::create_dir_all(dest.parent().ok_or(Error::NotFound)?)?;
    let (flac, _) = tags::new_beside(dest)?;
    let made = (|| {
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
        tags::carry_tags(src, &flac)?;
        std::fs::set_permissions(&flac, std::fs::metadata(src)?.permissions())?;
        Ok(())
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
/// writing `output`; gives what it printed. A decoding error stops it,
/// rather than leave out what it couldn't decode.
fn ffmpeg(input: &Path, args: &[&str], output: &std::ffi::OsStr) -> Result<String> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-v",
            "error",
            "-xerror",
            "-err_detect",
            "explode",
            "-y",
            "-i",
        ])
        .arg(input)
        .args(["-map", "0:a:0"])
        .args(args)
        .arg(output);
    run(command)
}

/// Each of the file's streams, a line each: its type, and 1 for a cover
/// picture (`video,1`).
fn ffprobe(input: &Path) -> Result<String> {
    let mut command = Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:stream_disposition=attached_pic",
        ])
        .args(["-of", "csv=p=0", "-i"])
        .arg(input);
    run(command)
}

fn run(mut command: Command) -> Result<String> {
    let tool = command.get_program().to_string_lossy().into_owned();
    let out = command
        .output()
        .map_err(|e| Error::Tags(format!("{tool} couldn't run: {e}")))?;
    if !out.status.success() {
        return Err(Error::Tags(format!(
            "{tool}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second of a tone made by ffmpeg with these output arguments.
    fn make(path: &Path, args: &[&str]) {
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=d=1"])
            .args(args)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn lossless_files_but_flacs_convert_and_extra_streams_are_refused() {
        if !available() {
            eprintln!("ffmpeg isn't installed: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("coversync-transcode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        for (name, codec, converted) in [
            ("a.wav", "pcm_s24le", true),
            ("alac.m4a", "alac", true),
            ("aac.m4a", "aac", false),
            ("a.flac", "flac", false),
        ] {
            make(&path(name), &["-c:a", codec]);
            assert_eq!(converts(&path(name)).unwrap(), converted, "{name}");
        }
        // 24 bits stay 24.
        let flac = to_flac(&path("a.wav"), &path("out/a.flac")).unwrap();
        let read = lofty::read_from_path(&flac).unwrap();
        assert_eq!(
            lofty::file::AudioFile::properties(&read).bit_depth(),
            Some(24)
        );
        // A second audio stream (another mix, say) would be lost.
        let two = path("two.m4a");
        let status = Command::new("ffmpeg")
            .args([
                "-v", "error", "-y", "-f", "lavfi", "-i", "sine=d=1", "-f", "lavfi",
            ])
            .args([
                "-i",
                "sine=d=1:f=880",
                "-map",
                "0",
                "-map",
                "1",
                "-c:a",
                "alac",
            ])
            .arg(&two)
            .status()
            .unwrap();
        assert!(status.success());
        let refused = to_flac(&two, &path("out/two.flac"))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("more than one audio stream"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

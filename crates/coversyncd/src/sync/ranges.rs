//! The byte ranges a reconcile samples: 64 KiB at each end of a file, and
//! every tag area in it. Tags live in those areas, so a tag edit changes the
//! sample even when the file kept its size, as it does when FLAC's padding
//! takes the edit. The audio between isn't read. The server picks the
//! ranges from its own copy, so the device needn't understand any format.

use std::io::{self, Read, Seek, SeekFrom};

/// Read at each end of every file.
const EDGE: u64 = 64 * 1024;
/// A tag area claiming more than this is read only this far: a damaged
/// header shouldn't make a reconcile read a whole file.
const MAX_AREA: u64 = 32 << 20;
/// Ogg pages looked through for the header packets.
const MAX_OGG_PAGES: usize = 4096;

/// The ranges to sample in a file of `size` bytes, `[start, end)`, in
/// order and apart.
pub fn ranges(file: &mut (impl Read + Seek), size: u64) -> io::Result<Vec<[u64; 2]>> {
    let mut areas = vec![[0, EDGE.min(size)], [size.saturating_sub(EDGE), size]];
    let mut at = 0;
    if let Some(end) = id3v2(file, 0)? {
        areas.push([0, end]);
        at = end;
    }
    if let Some(end) = flac(file, at, size)? {
        areas.push([at, end]);
    } else if let Some(end) = ogg(file, at, size)? {
        areas.push([at, end]);
    } else if let Some(chunks) = chunks(file, size)? {
        areas.extend(chunks);
    } else {
        areas.extend(mp4(file, at, size)?);
    }
    areas.extend(ape(file, size)?);
    for a in &mut areas {
        a[1] = a[1].min(a[0].saturating_add(MAX_AREA));
    }
    Ok(merge(areas, size))
}

/// Sort, clip to the file and join overlapping or touching ranges: those of
/// one file, or of several files of one size, whose union then samples any
/// of them.
pub fn merge(mut areas: Vec<[u64; 2]>, size: u64) -> Vec<[u64; 2]> {
    for a in &mut areas {
        a[1] = a[1].min(size);
    }
    areas.retain(|a| a[0] < a[1]);
    areas.sort_unstable();
    let mut out: Vec<[u64; 2]> = Vec::with_capacity(areas.len());
    for a in areas {
        match out.last_mut() {
            Some(last) if a[0] <= last[1] => last[1] = last[1].max(a[1]),
            _ => out.push(a),
        }
    }
    out
}

/// Read up to `buf.len()` bytes at `at`; how many there were.
fn read_at(file: &mut (impl Read + Seek), at: u64, buf: &mut [u8]) -> io::Result<usize> {
    file.seek(SeekFrom::Start(at))?;
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// The end of an ID3v2 tag at `at`.
fn id3v2(file: &mut (impl Read + Seek), at: u64) -> io::Result<Option<u64>> {
    let mut h = [0; 10];
    if read_at(file, at, &mut h)? < 10 || &h[..3] != b"ID3" {
        return Ok(None);
    }
    let size = h[6..10]
        .iter()
        .fold(0u64, |s, b| (s << 7) | u64::from(b & 0x7f));
    let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
    Ok(Some(at + 10 + size + footer))
}

/// The end of FLAC's metadata blocks, for a stream at `at`.
fn flac(file: &mut (impl Read + Seek), at: u64, size: u64) -> io::Result<Option<u64>> {
    let mut magic = [0; 4];
    if read_at(file, at, &mut magic)? < 4 || &magic != b"fLaC" {
        return Ok(None);
    }
    let mut pos = at + 4;
    loop {
        let mut h = [0; 4];
        if read_at(file, pos, &mut h)? < 4 {
            return Ok(Some(pos));
        }
        let len = u64::from(u32::from_be_bytes([0, h[1], h[2], h[3]]));
        pos += 4 + len;
        if h[0] & 0x80 != 0 || pos >= size {
            return Ok(Some(pos));
        }
    }
}

/// The end of the page that completes an Ogg stream's second packet (its
/// comments, for Vorbis, Opus and Ogg FLAC alike).
fn ogg(file: &mut (impl Read + Seek), at: u64, size: u64) -> io::Result<Option<u64>> {
    let mut pos = at;
    let mut packets = 0;
    for _ in 0..MAX_OGG_PAGES {
        let mut h = [0; 27];
        if read_at(file, pos, &mut h)? < 27 || &h[..4] != b"OggS" {
            return Ok((pos > at).then_some(pos));
        }
        let mut lacing = vec![0; usize::from(h[26])];
        let n = read_at(file, pos + 27, &mut lacing)?;
        lacing.truncate(n);
        packets += lacing.iter().filter(|&&l| l < 255).count();
        pos += 27 + lacing.len() as u64 + lacing.iter().map(|&l| u64::from(l)).sum::<u64>();
        if packets >= 2 || pos >= size {
            return Ok(Some(pos));
        }
    }
    Ok(Some(pos))
}

/// An MP4 file's `moov` box, which holds its tags, wherever it is.
fn mp4(file: &mut (impl Read + Seek), at: u64, size: u64) -> io::Result<Option<[u64; 2]>> {
    let mut pos = at;
    let mut first = true;
    while pos + 8 <= size {
        let mut h = [0; 16];
        let n = read_at(file, pos, &mut h)?;
        if n < 8 {
            break;
        }
        let kind = &h[4..8];
        if first && kind != b"ftyp" {
            return Ok(None);
        }
        first = false;
        let len = match u32::from_be_bytes([h[0], h[1], h[2], h[3]]) {
            0 => size - pos,
            1 if n == 16 => u64::from_be_bytes(h[8..16].try_into().expect("8 bytes")),
            1 => break,
            len => u64::from(len),
        };
        if len < 8 {
            break;
        }
        if kind == b"moov" {
            return Ok(Some([pos, pos.saturating_add(len)]));
        }
        pos = pos.saturating_add(len);
    }
    Ok(None)
}

/// Every chunk of a WAV (RIFF) or AIFF file but its audio: tags (an `id3 `
/// chunk, `LIST`, …) can be anywhere among them.
fn chunks(file: &mut (impl Read + Seek), size: u64) -> io::Result<Option<Vec<[u64; 2]>>> {
    let mut h = [0; 12];
    if read_at(file, 0, &mut h)? < 12 {
        return Ok(None);
    }
    let (big_endian, audio): (bool, &[u8; 4]) = match (&h[..4], &h[8..12]) {
        (b"RIFF" | b"RF64", b"WAVE") => (false, b"data"),
        (b"RIFX", b"WAVE") => (true, b"data"),
        (b"FORM", b"AIFF" | b"AIFC") => (true, b"SSND"),
        _ => return Ok(None),
    };
    let mut areas = Vec::new();
    let mut pos = 12;
    // RF64 gives the audio chunk's real length in its `ds64` chunk.
    let mut long_data = None;
    while pos + 8 <= size {
        let mut c = [0; 8];
        if read_at(file, pos, &mut c)? < 8 {
            break;
        }
        let len = c[4..8].try_into().expect("4 bytes");
        let mut len = u64::from(if big_endian {
            u32::from_be_bytes(len)
        } else {
            u32::from_le_bytes(len)
        });
        if &c[..4] == b"ds64" {
            let mut d = [0; 16];
            if read_at(file, pos + 8, &mut d)? == 16 {
                long_data = Some(u64::from_le_bytes(d[8..16].try_into().expect("8 bytes")));
            }
        }
        if len == u64::from(u32::MAX) {
            match long_data.filter(|_| &c[..4] == audio) {
                Some(real) => len = real,
                // A length nobody gave: read the rest whole.
                None => {
                    areas.push([pos, size]);
                    break;
                }
            }
        }
        // A damaged length mustn't wrap the walk back over itself.
        let end = pos
            .saturating_add(8)
            .saturating_add(len)
            .saturating_add(len & 1)
            .min(size);
        if &c[..4] != audio {
            areas.push([pos, end]);
        }
        pos = end;
    }
    Ok(Some(areas))
}

/// An APEv2 tag at the end, before an ID3v1 tag if there is one.
fn ape(file: &mut (impl Read + Seek), size: u64) -> io::Result<Option<[u64; 2]>> {
    let mut v1 = [0; 3];
    let end = if size >= 128 && read_at(file, size - 128, &mut v1)? == 3 && &v1 == b"TAG" {
        size - 128
    } else {
        size
    };
    if end < 32 {
        return Ok(None);
    }
    let mut f = [0; 32];
    if read_at(file, end - 32, &mut f)? < 32 || &f[..8] != b"APETAGEX" {
        return Ok(None);
    }
    let len = u64::from(u32::from_le_bytes(f[12..16].try_into().expect("4 bytes")));
    let header = if f[23] & 0x80 != 0 { 32 } else { 0 };
    Ok(Some([end.saturating_sub(len + header), end]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn flac_file(comments: usize, picture: usize, audio: usize) -> Vec<u8> {
        let mut f = b"fLaC".to_vec();
        for (kind, len, last) in [
            (0u8, 34usize, false),
            (6, picture, false),
            (4, comments, true),
        ] {
            f.push(kind | if last { 0x80 } else { 0 });
            f.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
            f.extend(std::iter::repeat_n(kind, len));
        }
        f.extend(std::iter::repeat_n(0xff, audio));
        f
    }

    #[test]
    fn flac_metadata_is_sampled_past_a_big_picture() {
        let data = flac_file(500, 300_000, 2_000_000);
        let got = ranges(&mut Cursor::new(&data), data.len() as u64).unwrap();
        let tags_end = 4 + (4 + 34) + (4 + 300_000) + (4 + 500);
        assert_eq!(
            got,
            [
                [0, tags_end as u64],
                [data.len() as u64 - EDGE, data.len() as u64]
            ]
        );
    }

    #[test]
    fn wav_tag_chunks_are_sampled_wherever_they_are() {
        let chunk = |id: &[u8], len: usize| {
            let mut c = id.to_vec();
            c.extend((len as u32).to_le_bytes());
            c.extend(std::iter::repeat_n(1, len));
            c
        };
        let mut wav = b"RIFF\0\0\0\0WAVE".to_vec();
        wav.extend(chunk(b"fmt ", 16));
        wav.extend(chunk(b"data", 1_000_000));
        let tags_at = wav.len() as u64;
        wav.extend(chunk(b"id3 ", 200_000));
        let size = wav.len() as u64;
        // The tags are within the last 64 KiB's reach only in part: the
        // whole chunk is read, the audio isn't.
        assert_eq!(
            ranges(&mut Cursor::new(&wav), size).unwrap(),
            [[0, EDGE], [tags_at, size]]
        );
    }

    #[test]
    fn small_files_are_read_whole_and_mp4_tags_found_anywhere() {
        let data = vec![7u8; 1000];
        assert_eq!(ranges(&mut Cursor::new(&data), 1000).unwrap(), [[0, 1000]]);

        let mut mp4 = Vec::new();
        let boxed = |kind: &[u8], len: usize| {
            let mut b = ((len + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend(std::iter::repeat_n(0, len));
            b
        };
        mp4.extend(boxed(b"ftyp", 16));
        mp4.extend(boxed(b"mdat", 1_000_000));
        let moov_at = mp4.len() as u64;
        mp4.extend(boxed(b"moov", 200_000));
        mp4.extend(boxed(b"free", 100_000));
        let size = mp4.len() as u64;
        assert_eq!(
            ranges(&mut Cursor::new(&mp4), size).unwrap(),
            [[0, EDGE], [moov_at, moov_at + 200_008], [size - EDGE, size]]
        );
    }
}

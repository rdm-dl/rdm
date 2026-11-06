//! Native muxing of a separate video stream and audio stream into one file, without
//! ffmpeg: fragmented MP4 + fragmented MP4 (M4A) → MP4, and WebM + WebM → WebM.
//!
//! Both work by copying the encoded data untouched: the audio track gets a new track
//! number, its header is added next to the video's, and the media fragments / clusters
//! of both are interleaved by timestamp. Large files are streamed, never loaded whole.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Which native muxer (if any) handles this pair, judged from the files' first bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Container {
    Mp4,
    WebM,
}

pub fn sniff(path: &Path) -> io::Result<Option<Container>> {
    let mut head = [0u8; 12];
    let n = File::open(path)?.read(&mut head)?;
    if n >= 8 && (&head[4..8] == b"ftyp" || &head[4..8] == b"styp" || &head[4..8] == b"moov") {
        return Ok(Some(Container::Mp4));
    }
    if n >= 4 && head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Ok(Some(Container::WebM));
    }
    Ok(None)
}

/// Merge `video` and `audio` into `out` if both are the same supported container.
/// Returns Ok(false) when the pair can't be muxed natively.
pub fn merge(video: &Path, audio: &Path, out: &Path) -> io::Result<bool> {
    match (sniff(video)?, sniff(audio)?) {
        (Some(Container::Mp4), Some(Container::Mp4)) => mp4::merge(video, audio, out),
        (Some(Container::WebM), Some(Container::WebM)) => webm::merge(video, audio, out),
        _ => Ok(false),
    }
}

fn copy_range(src: &mut File, start: u64, len: u64, dst: &mut impl Write) -> io::Result<()> {
    src.seek(SeekFrom::Start(start))?;
    let copied = io::copy(&mut src.take(len), dst)?;
    if copied != len {
        return Err(bad("unexpected end of file"));
    }
    Ok(())
}

fn read_at(src: &mut File, start: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; len as usize];
    src.seek(SeekFrom::Start(start))?;
    src.read_exact(&mut v)?;
    Ok(v)
}

// ------------------------------------------------------------------------- MP4

pub mod mp4 {
    use super::*;

    /// A box: type, absolute offset, header length, total size.
    #[derive(Debug, Clone, Copy)]
    pub struct BoxRef {
        pub kind: [u8; 4],
        pub start: u64,
        pub header: u64,
        pub size: u64,
    }

    /// Top-level boxes of a file (headers only).
    pub fn scan(f: &mut File) -> io::Result<Vec<BoxRef>> {
        let len = f.metadata()?.len();
        let mut out = vec![];
        let mut pos = 0u64;
        while pos + 8 <= len {
            let h = read_at(f, pos, 8.min(len - pos))?;
            let mut size = u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64;
            let kind: [u8; 4] = h[4..8].try_into().unwrap();
            let mut header = 8;
            if size == 1 {
                let l = read_at(f, pos + 8, 8)?;
                size = u64::from_be_bytes(l.try_into().unwrap());
                header = 16;
            } else if size == 0 {
                size = len - pos;
            }
            if size < header || pos + size > len {
                break; // truncated tail
            }
            out.push(BoxRef { kind, start: pos, header, size });
            pos += size;
        }
        Ok(out)
    }

    /// Child boxes inside an in-memory box payload.
    pub fn children(buf: &[u8]) -> Vec<(usize, usize, [u8; 4])> {
        // (offset of box start, total size, type)
        let mut out = vec![];
        let mut pos = 0usize;
        while pos + 8 <= buf.len() {
            let mut size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = buf[pos + 4..pos + 8].try_into().unwrap();
            if size == 1 && pos + 16 <= buf.len() {
                size = u64::from_be_bytes(buf[pos + 8..pos + 16].try_into().unwrap()) as usize;
            } else if size == 0 {
                size = buf.len() - pos;
            }
            if size < 8 || pos + size > buf.len() {
                break;
            }
            out.push((pos, size, kind));
            pos += size;
        }
        out
    }

    fn header_len(buf: &[u8], at: usize) -> usize {
        if u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) == 1 { 16 } else { 8 }
    }

    fn find<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Option<(usize, usize)> {
        children(buf).into_iter().find(|c| &c.2 == kind).map(|(o, s, _)| (o, s))
    }

    /// Payload range of the first child `kind` (after its header).
    fn payload(buf: &[u8], kind: &[u8; 4]) -> Option<std::ops::Range<usize>> {
        let (o, s) = find(buf, kind)?;
        Some(o + header_len(buf, o)..o + s)
    }

    fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(payload.len() + 8);
        v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn set_u32(buf: &mut [u8], at: usize, v: u32) {
        buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
    }

    /// Track id of a `trak` payload (from tkhd).
    fn trak_id(trak: &[u8]) -> Option<u32> {
        let r = payload(trak, b"tkhd")?;
        let p = &trak[r];
        let at = if p[0] == 1 { 4 + 16 } else { 4 + 8 };
        Some(u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?))
    }

    fn set_trak_id(trak: &mut [u8], id: u32) -> Option<()> {
        let r = payload(trak, b"tkhd")?;
        let at = r.start + if trak[r.start] == 1 { 4 + 16 } else { 4 + 8 };
        set_u32(trak, at, id);
        Some(())
    }

    /// Media timescale of a `trak` payload (mdia/mdhd).
    fn trak_timescale(trak: &[u8]) -> Option<u32> {
        let mdia = &trak[payload(trak, b"mdia")?];
        let p = &mdia[payload(mdia, b"mdhd")?];
        let at = if p[0] == 1 { 4 + 16 } else { 4 + 8 };
        Some(u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?))
    }

    struct Movie {
        moov: Vec<u8>, // payload
        traks: Vec<Vec<u8>>,
        fragments: Vec<BoxRef>, // moof, mdat in order
        ftyp: Option<BoxRef>,
    }

    fn load(f: &mut File) -> io::Result<Movie> {
        let boxes = scan(f)?;
        let moov = boxes.iter().find(|b| &b.kind == b"moov").ok_or_else(|| bad("no moov box"))?;
        let moov = read_at(f, moov.start + moov.header, moov.size - moov.header)?;
        if find(&moov, b"mvex").is_none() {
            return Err(bad("not a fragmented MP4"));
        }
        let traks = children(&moov).into_iter().filter(|c| &c.2 == b"trak").map(|(o, s, _)| moov[o + header_len(&moov, o)..o + s].to_vec()).collect();
        let fragments = boxes.iter().filter(|b| &b.kind == b"moof" || &b.kind == b"mdat").copied().collect();
        let ftyp = boxes.iter().find(|b| &b.kind == b"ftyp").copied();
        Ok(Movie { moov, traks, fragments, ftyp })
    }

    /// Decode time (seconds) of a moof's first traf.
    fn moof_time(moof: &[u8], timescale: u32) -> f64 {
        let Some(traf) = payload(moof, b"traf") else { return 0.0 };
        let traf = &moof[traf];
        let Some(r) = payload(traf, b"tfdt") else { return 0.0 };
        let p = &traf[r];
        let t = if p[0] == 1 { u64::from_be_bytes(p[4..12].try_into().unwrap()) } else { u32::from_be_bytes(p[4..8].try_into().unwrap()) as u64 };
        t as f64 / timescale.max(1) as f64
    }

    /// Patch a moof in place: sequence number, track id, and an absolute base offset
    /// (moved by `shift` bytes) if the file used one.
    fn patch_moof(moof: &mut [u8], seq: u32, track: Option<u32>, shift: i64) {
        if let Some(r) = payload(moof, b"mfhd") {
            set_u32(moof, r.start + 4, seq);
        }
        let mut traf_pos = 0usize;
        let kids = children(moof);
        for (o, s, k) in kids {
            if &k != b"traf" {
                continue;
            }
            let h = header_len(moof, o);
            let traf_range = o + h..o + s;
            let tfhd = payload(&moof[traf_range.clone()], b"tfhd").map(|r| (traf_range.start + r.start, traf_range.start + r.end));
            if let Some((a, _)) = tfhd {
                let flags = u32::from_be_bytes(moof[a..a + 4].try_into().unwrap()) & 0x00FF_FFFF;
                if let Some(id) = track {
                    set_u32(moof, a + 4, id);
                }
                if flags & 1 != 0 {
                    let base = u64::from_be_bytes(moof[a + 8..a + 16].try_into().unwrap());
                    let nb = (base as i64 + shift).max(0) as u64;
                    moof[a + 8..a + 16].copy_from_slice(&nb.to_be_bytes());
                }
            }
            traf_pos += 1;
        }
        let _ = traf_pos;
    }

    pub fn merge(video: &Path, audio: &Path, out: &Path) -> io::Result<bool> {
        let mut vf = File::open(video)?;
        let mut af = File::open(audio)?;
        let (v, a) = match (load(&mut vf), load(&mut af)) {
            (Ok(v), Ok(a)) => (v, a),
            _ => return Ok(false),
        };
        // The video file may carry more than the picture (captions): those tracks stay.
        if v.traks.is_empty() || a.traks.len() != 1 {
            return Ok(false);
        }
        let v_ts = trak_timescale(&v.traks[0]).unwrap_or(1);
        let a_ts = trak_timescale(&a.traks[0]).unwrap_or(1);
        let a_id = v.traks.iter().filter_map(|t| trak_id(t)).max().unwrap_or(1) + 1;

        // New moov: video moov with the audio trak added and the audio trex in mvex.
        let mut a_trak = a.traks[0].clone();
        set_trak_id(&mut a_trak, a_id);
        let a_trex = payload(&a.moov, b"mvex")
            .and_then(|r| {
                let mvex = &a.moov[r];
                payload(mvex, b"trex").map(|t| mvex[t].to_vec())
            })
            .ok_or_else(|| bad("audio has no trex"))?;
        let mut a_trex = a_trex;
        set_u32(&mut a_trex, 4, a_id);
        let mut moov = vec![];
        let mut traks_left = v.traks.len();
        for (o, s, k) in children(&v.moov) {
            let h = header_len(&v.moov, o);
            let body = &v.moov[o + h..o + s];
            match &k {
                b"mvhd" => {
                    let mut b = body.to_vec();
                    let at = if b[0] == 1 { 108 } else { 96 };
                    if b.len() >= at + 4 {
                        set_u32(&mut b, at, a_id + 1);
                    }
                    moov.extend(make_box(b"mvhd", &b));
                }
                b"trak" => {
                    moov.extend(make_box(b"trak", body));
                    traks_left -= 1;
                    if traks_left == 0 {
                        moov.extend(make_box(b"trak", &a_trak));
                    }
                }
                b"mvex" => {
                    let mut mvex = body.to_vec();
                    mvex.extend(make_box(b"trex", &a_trex));
                    moov.extend(make_box(b"mvex", &mvex));
                }
                _ => moov.extend_from_slice(&v.moov[o..o + s]),
            }
        }
        let moov = make_box(b"moov", &moov);

        // Pair each moof with the mdat that follows it.
        let pairs = |frags: &[BoxRef]| -> Vec<(BoxRef, Option<BoxRef>)> {
            let mut out = vec![];
            let mut i = 0;
            while i < frags.len() {
                if &frags[i].kind == b"moof" {
                    let mdat = frags.get(i + 1).filter(|b| &b.kind == b"mdat").copied();
                    out.push((frags[i], mdat));
                    i += if mdat.is_some() { 2 } else { 1 };
                } else {
                    i += 1;
                }
            }
            out
        };
        let mut items: Vec<(f64, bool, BoxRef, Option<BoxRef>, Vec<u8>)> = vec![];
        for (is_audio, file, movie, ts) in [(false, &mut vf, &v, v_ts), (true, &mut af, &a, a_ts)] {
            for (moof, mdat) in pairs(&movie.fragments) {
                let body = read_at(file, moof.start, moof.size)?;
                let t = moof_time(&body[moof.header as usize..], ts);
                items.push((t, is_audio, moof, mdat, body));
            }
        }
        items.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal).then(x.1.cmp(&y.1)));

        let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
        let mut pos = 0u64;
        if let Some(ft) = v.ftyp {
            copy_range(&mut vf, ft.start, ft.size, &mut w)?;
            pos += ft.size;
        }
        w.write_all(&moov)?;
        pos += moov.len() as u64;
        for (seq, (_, is_audio, moof, mdat, mut body)) in items.into_iter().enumerate() {
            let shift = pos as i64 - moof.start as i64;
            let hdr = moof.header as usize;
            let mut inner = body.split_off(hdr);
            patch_moof(&mut inner, seq as u32 + 1, is_audio.then_some(a_id), shift);
            w.write_all(&body)?;
            w.write_all(&inner)?;
            pos += moof.size;
            if let Some(md) = mdat {
                let src = if is_audio { &mut af } else { &mut vf };
                copy_range(src, md.start, md.size, &mut w)?;
                pos += md.size;
            }
        }
        w.flush()?;
        Ok(true)
    }
    fn handler(trak: &[u8]) -> Option<[u8; 4]> {
        let mdia = &trak[payload(trak, b"mdia")?];
        let p = &mdia[payload(mdia, b"hdlr")?];
        p.get(8..12)?.try_into().ok()
    }

    /// Whether this is a fragmented MP4 holding one audio track and nothing else.
    pub fn is_audio_only(path: &Path) -> bool {
        let Ok(mut f) = File::open(path) else { return false };
        load(&mut f).is_ok_and(|m| m.traks.len() == 1 && handler(&m.traks[0]) == Some(*b"soun"))
    }

    fn be32(b: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
    }

    /// What a track's `trex` box says about samples that don't say for themselves.
    #[derive(Default, Clone, Copy)]
    struct Defaults {
        duration: u32,
        size: u32,
        flags: u32,
    }

    /// Samples of a fragmented track, gathered from its `moof` boxes.
    #[derive(Default)]
    struct Samples {
        /// Where each run of samples is in the file: offset, bytes, samples.
        runs: Vec<(u64, u64, u32)>,
        sizes: Vec<u32>,
        durations: Vec<u32>,
        /// Display time minus decode time, for pictures that aren't stored in display order.
        shifts: Vec<i32>,
        keyframes: Vec<bool>,
        /// Decode time of the first sample, and of the end of the last.
        first: Option<u64>,
        end: u64,
        /// Samples stand on their own, so leaving some out harms nothing: sound.
        can_trim: bool,
    }

    /// The tracks a `traf` box belongs to is named in its `tfhd`.
    fn traf_track(traf: &[u8]) -> Option<u32> {
        be32(&traf[payload(traf, b"tfhd")?], 4)
    }

    impl Samples {
        /// Read one track's part of a fragment. `start` is the fragment's offset, `previous` where
        /// the track before it in this fragment ended. Returns where this one ended.
        fn read_traf(&mut self, traf: &[u8], start: u64, previous: u64, defaults: Defaults) -> Option<u64> {
            let tfhd = &traf[payload(traf, b"tfhd")?];
            let flags = be32(tfhd, 0)? & 0xFF_FFFF;
            let mut p = 8;
            let mut base = if flags & 0x2_0000 != 0 { start } else { previous };
            if flags & 1 != 0 {
                base = u64::from_be_bytes(tfhd.get(p..p + 8)?.try_into().ok()?);
                p += 8;
            }
            if flags & 2 != 0 {
                // A second sample description can't be told apart once this is one table.
                if be32(tfhd, p)? != 1 {
                    return None;
                }
                p += 4;
            }
            let Defaults { mut duration, mut size, flags: mut sample_flags } = defaults;
            if flags & 8 != 0 {
                duration = be32(tfhd, p)?;
                p += 4;
            }
            if flags & 0x10 != 0 {
                size = be32(tfhd, p)?;
                p += 4;
            }
            if flags & 0x20 != 0 {
                sample_flags = be32(tfhd, p)?;
            }
            let mut overlap = 0i64;
            if let Some(r) = payload(traf, b"tfdt") {
                let tfdt = &traf[r];
                let time = if tfdt[0] == 1 { u64::from_be_bytes(tfdt.get(4..12)?.try_into().ok()?) } else { u64::from(be32(tfdt, 4)?) };
                match self.first {
                    None => {
                        self.first = Some(time);
                        self.end = time;
                    }
                    // A plain file has no decode times, only durations: a gap in the stream
                    // goes into the sample before it, or the sound would drift off the picture.
                    Some(_) => {
                        let moved = time as i64 - self.end as i64;
                        let last = self.durations.last().map_or(0, |d| i64::from(*d));
                        if self.can_trim && moved < -last {
                            // This part starts before the last one ended. Playing that twice
                            // would put the sound behind for the rest of the file.
                            overlap = -moved;
                        } else {
                            if let Some(last) = self.durations.last_mut() {
                                *last = i64::from(*last).saturating_add(moved).clamp(0, i64::from(u32::MAX)) as u32;
                            }
                            self.end = time;
                        }
                    }
                }
            }
            let mut next = None;
            for (o, s, k) in children(traf) {
                if &k != b"trun" {
                    continue;
                }
                let run = &traf[o + header_len(traf, o)..o + s];
                let flags = be32(run, 0)? & 0xFF_FFFF;
                let count = be32(run, 4)?;
                let mut p = 8;
                let mut offset = next.unwrap_or(base);
                if flags & 1 != 0 {
                    offset = (base as i64 + i64::from(be32(run, p)? as i32)) as u64;
                    p += 4;
                }
                let mut first_flags = None;
                if flags & 4 != 0 {
                    first_flags = Some(be32(run, p)?);
                    p += 4;
                }
                let mut bytes = 0u64;
                let mut skipped = 0u64;
                let mut kept = 0u32;
                for i in 0..count {
                    let (mut d, mut z, mut f) = (duration, size, sample_flags);
                    if i == 0 {
                        f = first_flags.unwrap_or(f);
                    }
                    if flags & 0x100 != 0 {
                        d = be32(run, p)?;
                        p += 4;
                    }
                    if flags & 0x200 != 0 {
                        z = be32(run, p)?;
                        p += 4;
                    }
                    if flags & 0x400 != 0 {
                        f = be32(run, p)?;
                        p += 4;
                    }
                    let mut shift = 0;
                    if flags & 0x800 != 0 {
                        shift = be32(run, p)? as i32;
                        p += 4;
                    }
                    if overlap >= i64::from(d) && overlap > 0 && kept == 0 {
                        overlap -= i64::from(d);
                        skipped += u64::from(z);
                        continue;
                    }
                    self.durations.push(d);
                    self.sizes.push(z);
                    self.shifts.push(shift);
                    self.keyframes.push(f & 0x1_0000 == 0);
                    self.end += u64::from(d);
                    bytes += u64::from(z);
                    kept += 1;
                }
                if kept > 0 {
                    self.runs.push((offset + skipped, bytes, kept));
                }
                next = Some(offset + skipped + bytes);
            }
            next.or(Some(previous))
        }

        /// One track's samples out of a fragment holding any number of tracks.
        fn read_moof(&mut self, moof: &[u8], start: u64, defaults: Defaults) -> Option<()> {
            let mut previous = start;
            for (o, s, k) in children(moof) {
                if &k == b"traf" {
                    previous = self.read_traf(&moof[o + header_len(moof, o)..o + s], start, previous, defaults)?;
                }
            }
            Some(())
        }

        fn stts(&self) -> Vec<u8> {
            let mut same: Vec<(u32, u32)> = vec![];
            for &d in &self.durations {
                match same.last_mut() {
                    Some((n, v)) if *v == d => *n += 1,
                    _ => same.push((1, d)),
                }
            }
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&(same.len() as u32).to_be_bytes());
            same.iter().for_each(|(n, d)| b.extend([n.to_be_bytes(), d.to_be_bytes()].concat()));
            b
        }

        fn stsc(&self) -> Vec<u8> {
            let mut changes: Vec<(u32, u32)> = vec![];
            for (i, (_, _, count)) in self.runs.iter().enumerate() {
                if changes.last().is_none_or(|l| l.1 != *count) {
                    changes.push((i as u32 + 1, *count));
                }
            }
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&(changes.len() as u32).to_be_bytes());
            changes.iter().for_each(|(chunk, n)| b.extend([chunk.to_be_bytes(), n.to_be_bytes(), 1u32.to_be_bytes()].concat()));
            b
        }

        fn stsz(&self) -> Vec<u8> {
            let mut b = vec![0u8; 8];
            b.extend_from_slice(&(self.sizes.len() as u32).to_be_bytes());
            self.sizes.iter().for_each(|s| b.extend_from_slice(&s.to_be_bytes()));
            b
        }

        fn co64(&self, first: u64) -> Vec<u8> {
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&(self.runs.len() as u32).to_be_bytes());
            let mut at = first;
            for (_, bytes, _) in &self.runs {
                b.extend_from_slice(&at.to_be_bytes());
                at += bytes;
            }
            b
        }

        /// How far display order runs ahead of decode order, as the `ctts` table wants it: never
        /// below zero. `None` when every picture is stored in display order.
        fn ctts(&self) -> Option<Vec<u8>> {
            let lowest = self.shifts.iter().copied().min()?;
            let mut same: Vec<(u32, u32)> = vec![];
            for s in &self.shifts {
                let shift = (i64::from(*s) - i64::from(lowest)) as u32;
                match same.last_mut() {
                    Some((n, v)) if *v == shift => *n += 1,
                    _ => same.push((1, shift)),
                }
            }
            if same.len() == 1 && same[0].1 == 0 {
                return None;
            }
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&(same.len() as u32).to_be_bytes());
            same.iter().for_each(|(n, d)| b.extend([n.to_be_bytes(), d.to_be_bytes()].concat()));
            Some(b)
        }

        /// The samples a player can jump to, or `None` when it can jump to any of them.
        fn stss(&self) -> Option<Vec<u8>> {
            if self.keyframes.iter().all(|k| *k) {
                return None;
            }
            let marks: Vec<u32> = self.keyframes.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i as u32 + 1).collect();
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&(marks.len() as u32).to_be_bytes());
            marks.iter().for_each(|m| b.extend_from_slice(&m.to_be_bytes()));
            Some(b)
        }

        /// Chunk offsets, given where each of this track's chunks ended up.
        fn chunk_table(places: &[u64], wide: bool) -> Vec<u8> {
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&(places.len() as u32).to_be_bytes());
            for at in places {
                if wide {
                    b.extend_from_slice(&at.to_be_bytes());
                } else {
                    b.extend_from_slice(&(*at as u32).to_be_bytes());
                }
            }
            b
        }

        /// What the track starts with on its own timeline, in the track's units: the media time
        /// of the first picture shown, when that is shown, and how much is left out before it.
        /// `edit` is where the file says playing begins; before it is the padding some encoders
        /// put at the start. Only that shortens the track: pictures held back for display order
        /// move the whole track along, and leaving that out of its length would cut off the end.
        fn opening(&self, edit: u64) -> (u64, u64, u64) {
            let lowest = i64::from(self.shifts.iter().copied().min().unwrap_or(0));
            // The picture shown first isn't the one with the smallest shift, and it's among the
            // first few stored: a decoder can't hold back more than that.
            let mut decoded = 0i64;
            let mut soonest = i64::MAX;
            for (shift, duration) in self.shifts.iter().zip(&self.durations).take(64) {
                soonest = soonest.min(decoded + i64::from(*shift));
                decoded += i64::from(*duration);
            }
            let soonest = if soonest == i64::MAX { 0 } else { soonest };
            let inside = (soonest - lowest).max(0) as u64;
            let shown = (self.first.unwrap_or(0) as i64 + soonest).max(0) as u64;
            let padding = edit.saturating_sub(shown);
            (inside + padding, shown.saturating_sub(edit), padding)
        }
    }

    /// Where a track's edit list says playing begins, and how long nothing is shown before that
    /// in the movie's units.
    fn edit_start(trak: &[u8]) -> (u64, u64) {
        let Some(edts) = payload(trak, b"edts").map(|r| &trak[r]) else { return (0, 0) };
        let Some(elst) = payload(edts, b"elst").map(|r| &edts[r]) else { return (0, 0) };
        let wide = elst.first() == Some(&1);
        let width = if wide { 20 } else { 12 };
        let mut empty = 0u64;
        for i in 0..be32(elst, 4).unwrap_or(0) as usize {
            let at = 8 + i * width;
            let (length, media) = if wide {
                let read = |o: usize| elst.get(o..o + 8).and_then(|v| v.try_into().ok()).map(u64::from_be_bytes);
                (read(at).unwrap_or(0), read(at + 8).unwrap_or(0) as i64)
            } else {
                (u64::from(be32(elst, at).unwrap_or(0)), i64::from(be32(elst, at + 4).unwrap_or(0) as i32))
            };
            if media >= 0 {
                return (media as u64, empty);
            }
            empty += length;
        }
        (0, empty)
    }

    /// An edit list: nothing for `wait`, then from `media` on for `length`.
    fn edit_list(wait: u64, media: u64, length: u64) -> Vec<u8> {
        let mut rows: Vec<(u64, i64)> = vec![];
        if wait > 0 {
            rows.push((wait, -1));
        }
        rows.push((length, media as i64));
        let wide = rows.iter().any(|(l, m)| *l > u64::from(u32::MAX) || *m > i64::from(i32::MAX));
        let mut b = vec![u8::from(wide), 0, 0, 0];
        b.extend_from_slice(&(rows.len() as u32).to_be_bytes());
        for (length, media) in rows {
            if wide {
                b.extend_from_slice(&length.to_be_bytes());
                b.extend_from_slice(&media.to_be_bytes());
            } else {
                b.extend_from_slice(&(length as u32).to_be_bytes());
                b.extend_from_slice(&(media as i32).to_be_bytes());
            }
            b.extend_from_slice(&[0, 1, 0, 0]);
        }
        make_box(b"edts", &make_box(b"elst", &b))
    }

    /// Copy of `buf` with the first child `kind` rebuilt from its payload.
    fn rewrite(buf: &[u8], kind: &[u8; 4], change: impl FnOnce(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let mut change = Some(change);
        let mut out = vec![];
        for (o, s, k) in children(buf) {
            match change.take_if(|_| &k == kind) {
                Some(f) => out.extend(make_box(kind, &f(&buf[o + header_len(buf, o)..o + s]))),
                None => out.extend_from_slice(&buf[o..o + s]),
            }
        }
        out
    }

    /// A full box's duration, at `v0` in version 0 boxes and `v1` in version 1.
    fn set_duration(p: &mut [u8], v0: usize, v1: usize, value: u64) {
        if p[0] == 1 {
            p[v1..v1 + 8].copy_from_slice(&value.to_be_bytes());
        } else {
            set_u32(p, v0, value.min(u64::from(u32::MAX)) as u32);
        }
    }

    fn text_tag(kind: [u8; 4], text: &str) -> Vec<u8> {
        let data = [&[0, 0, 0, 1, 0, 0, 0, 0][..], text.as_bytes()].concat();
        make_box(&kind, &make_box(b"data", &data))
    }

    /// The `udta` box with a title and artist, as music players read them.
    fn tags_box(title: &str, artist: Option<&str>) -> Vec<u8> {
        let mut list = text_tag([0xA9, b'n', b'a', b'm'], title);
        if let Some(a) = artist {
            list.extend(text_tag([0xA9, b'A', b'R', b'T'], a));
        }
        let handler = make_box(b"hdlr", &[&[0u8; 8][..], b"mdir", b"appl", &[0u8; 9]].concat());
        let meta = make_box(b"meta", &[&[0u8; 4][..], &handler, &make_box(b"ilst", &list)].concat());
        make_box(b"udta", &meta)
    }

    /// Rewrite a one-track fragmented MP4 as an ordinary one. A fragmented file keeps its length
    /// spread over thousands of small pieces, which many players don't add up.
    pub fn defragment(src: &Path, dest: &Path, title: &str, artist: Option<&str>) -> io::Result<bool> {
        let mut f = File::open(src)?;
        let m = load(&mut f)?;
        let [trak] = m.traks.as_slice() else { return Ok(false) };
        let mvex = payload(&m.moov, b"mvex").map(|r| m.moov[r].to_vec()).unwrap_or_default();
        let trex = payload(&mvex, b"trex").map(|r| &mvex[r]).unwrap_or_default();
        let defaults = trex_defaults(trex);
        let mut samples = Samples::default();
        for b in m.fragments.iter().filter(|b| &b.kind == b"moof") {
            let moof = read_at(&mut f, b.start + b.header, b.size - b.header)?;
            samples.read_moof(&moof, b.start, defaults).ok_or_else(|| bad("damaged fragment"))?;
        }
        let data: u64 = samples.runs.iter().map(|r| r.1).sum();
        if samples.sizes.is_empty() || data >= u64::from(u32::MAX) - 8 {
            return Ok(false);
        }
        let media_scale = u64::from(trak_timescale(trak).unwrap_or(1).max(1));
        let media_length: u64 = samples.durations.iter().map(|d| u64::from(*d)).sum();
        let mvhd = payload(&m.moov, b"mvhd").map(|r| m.moov[r].to_vec()).ok_or_else(|| bad("no mvhd box"))?;
        let movie_scale = u64::from(be32(&mvhd, if mvhd[0] == 1 { 20 } else { 12 }).unwrap_or(1).max(1));
        let movie_length = media_length * movie_scale / media_scale;

        let ftyp = make_box(b"ftyp", &[&b"M4A "[..], &[0, 0, 0, 0], b"M4A ", b"isom", b"mp42"].concat());
        let moov_with = |first: u64| {
            // An edit list in a fragmented file usually covers nothing, which here would mean silence.
            let trak: Vec<u8> = children(trak).into_iter().filter(|c| &c.2 != b"edts").flat_map(|(o, s, _)| trak[o..o + s].to_vec()).collect();
            let trak = rewrite(&trak, b"tkhd", |p| {
                let mut p = p.to_vec();
                set_duration(&mut p, 20, 28, movie_length);
                p
            });
            let trak = rewrite(&trak, b"mdia", |mdia| {
                let mdia = rewrite(mdia, b"mdhd", |p| {
                    let mut p = p.to_vec();
                    set_duration(&mut p, 16, 24, media_length);
                    p
                });
                rewrite(&mdia, b"minf", |minf| {
                    rewrite(minf, b"stbl", |stbl| {
                        let stsd = find(stbl, b"stsd").map_or(&[][..], |(o, s)| &stbl[o..o + s]);
                        [stsd, &make_box(b"stts", &samples.stts()), &make_box(b"stsc", &samples.stsc()), &make_box(b"stsz", &samples.stsz()), &make_box(b"co64", &samples.co64(first))].concat()
                    })
                })
            });
            let mut mvhd = mvhd.clone();
            set_duration(&mut mvhd, 16, 24, movie_length);
            make_box(b"moov", &[make_box(b"mvhd", &mvhd), make_box(b"trak", &trak), tags_box(title, artist)].concat())
        };
        let header = ftyp.len() as u64 + moov_with(0).len() as u64 + 8;
        let mut w = BufWriter::with_capacity(1 << 20, File::create(dest)?);
        w.write_all(&ftyp)?;
        w.write_all(&moov_with(header))?;
        w.write_all(&((data + 8) as u32).to_be_bytes())?;
        w.write_all(b"mdat")?;
        for (offset, bytes, _) in &samples.runs {
            copy_range(&mut f, *offset, *bytes, &mut w)?;
        }
        w.flush()?;
        Ok(true)
    }

    /// What decides whether two fragmented headers can share a file: the first track's description
    /// of its samples, and its clock. `None` if this isn't a fragmented header.
    pub fn stream_identity(init: &[u8]) -> Option<Vec<u8>> {
        let (o, s) = find(init, b"moov")?;
        let moov = &init[o + header_len(init, o)..o + s];
        find(moov, b"mvex")?;
        let trak = &moov[payload(moov, b"trak")?];
        let mdia = &trak[payload(trak, b"mdia")?];
        let minf = &mdia[payload(mdia, b"minf")?];
        let stbl = &minf[payload(minf, b"stbl")?];
        let (o, s) = find(stbl, b"stsd")?;
        Some([&stbl[o..o + s], &trak_timescale(trak)?.to_be_bytes()].concat())
    }

    /// The `tfdt` boxes of a fragment, as their place in its bytes.
    fn time_boxes(moof: &[u8]) -> Vec<usize> {
        let trafs = children(moof).into_iter().filter(|c| &c.2 == b"traf");
        trafs.filter_map(|(o, s, _)| {
            let start = o + header_len(moof, o);
            payload(&moof[start..o + s], b"tfdt").map(|r| start + r.start)
        }).collect()
    }

    /// Which part of a file a byte belongs to, given where each part starts.
    fn part_of(starts: &[u64], at: u64) -> usize {
        starts.partition_point(|s| *s <= at).saturating_sub(1)
    }

    /// The time, in seconds, of the first fragment of each part of a fragmented file.
    pub fn first_times(path: &Path, starts: &[u64]) -> io::Result<Vec<Option<f64>>> {
        let mut f = File::open(path)?;
        let m = load(&mut f)?;
        let scale = m.traks.first().and_then(|t| trak_timescale(t)).unwrap_or(1);
        let mut out = vec![None; starts.len()];
        for b in m.fragments.iter().filter(|b| &b.kind == b"moof") {
            let part = part_of(starts, b.start);
            if out[part].is_none() {
                let moof = read_at(&mut f, b.start + b.header, b.size - b.header)?;
                out[part] = Some(moof_time(&moof, scale));
            }
        }
        Ok(out)
    }

    /// Move each part of a fragmented file along its clock by so many seconds. The file stays the
    /// same size: only the times in it change.
    pub fn move_times(path: &Path, starts: &[u64], seconds: &[f64]) -> io::Result<()> {
        let mut f = File::options().read(true).write(true).open(path)?;
        let m = load(&mut f)?;
        let scale = f64::from(m.traks.first().and_then(|t| trak_timescale(t)).unwrap_or(1));
        for b in m.fragments.iter().filter(|b| &b.kind == b"moof") {
            let by = (seconds[part_of(starts, b.start)] * scale).round() as i64;
            let body = b.start + b.header;
            let mut moof = read_at(&mut f, body, b.size - b.header)?;
            for at in time_boxes(&moof) {
                if moof[at] == 1 {
                    let time = u64::from_be_bytes(moof[at + 4..at + 12].try_into().map_err(|_| bad("damaged fragment"))?);
                    moof[at + 4..at + 12].copy_from_slice(&((time as i64 + by).max(0) as u64).to_be_bytes());
                } else {
                    let time = be32(&moof, at + 4).ok_or_else(|| bad("damaged fragment"))?;
                    let moved = (i64::from(time) + by).clamp(0, i64::from(u32::MAX)) as u32;
                    moof[at + 4..at + 8].copy_from_slice(&moved.to_be_bytes());
                }
            }
            f.seek(SeekFrom::Start(body))?;
            f.write_all(&moof)?;
        }
        Ok(())
    }

    fn trex_defaults(trex: &[u8]) -> Defaults {
        Defaults { duration: be32(trex, 12).unwrap_or(0), size: be32(trex, 16).unwrap_or(0), flags: be32(trex, 20).unwrap_or(0) }
    }

    /// The `trex` box of each track.
    fn all_defaults(moov: &[u8]) -> Vec<(u32, Defaults)> {
        let Some(mvex) = payload(moov, b"mvex").map(|r| &moov[r]) else { return vec![] };
        children(mvex).into_iter().filter(|c| &c.2 == b"trex").filter_map(|(o, s, _)| {
            let trex = &mvex[o + header_len(mvex, o)..o + s];
            Some((be32(trex, 4)?, trex_defaults(trex)))
        }).collect()
    }

    /// A movie's clock for the header boxes: it only places tracks, so a millisecond will do, and
    /// some fragmented files have one tick a second there.
    const MOVIE_SCALE: u64 = 1000;

    /// Rewrite a fragmented MP4 as an ordinary one with its index at the front: the media isn't
    /// touched, it's moved together and listed in tables. Players that don't add up fragments then
    /// show the length, and all of them can jump around at once. `Ok(false)` leaves it alone.
    pub fn to_plain(src: &Path, dest: &Path, title: &str) -> io::Result<bool> {
        let mut f = File::open(src)?;
        let Ok(m) = load(&mut f) else { return Ok(false) };
        let defaults = all_defaults(&m.moov);
        let ids: Vec<u32> = m.traks.iter().filter_map(|t| trak_id(t)).collect();
        if ids.len() != m.traks.len() {
            return Ok(false);
        }
        let mut tracks: Vec<Samples> = m.traks.iter().map(|t| Samples { can_trim: handler(t) == Some(*b"soun"), ..Default::default() }).collect();
        // Runs in file order: which track, and the run's number within that track.
        let mut order: Vec<(usize, usize)> = vec![];
        for b in m.fragments.iter().filter(|b| &b.kind == b"moof") {
            let moof = read_at(&mut f, b.start + b.header, b.size - b.header)?;
            let mut previous = b.start;
            for (o, s, k) in children(&moof) {
                if &k != b"traf" {
                    continue;
                }
                let traf = &moof[o + header_len(&moof, o)..o + s];
                let Some(index) = traf_track(traf).and_then(|id| ids.iter().position(|i| *i == id)) else { return Ok(false) };
                let rule = defaults.iter().find(|(id, _)| *id == ids[index]).map(|d| d.1).unwrap_or_default();
                let before = tracks[index].runs.len();
                let Some(end) = tracks[index].read_traf(traf, b.start, previous, rule) else { return Ok(false) };
                previous = end;
                order.extend((before..tracks[index].runs.len()).map(|run| (index, run)));
            }
        }
        if tracks.iter().all(|t| t.sizes.is_empty()) {
            return Ok(false);
        }
        let data: u64 = order.iter().map(|(t, r)| tracks[*t].runs[*r].1).sum();
        let wide = data >= u64::from(u32::MAX) - (1 << 26);
        let mdat_header = if wide { 16 } else { 8 };

        // When each track opens. The tracks keep their distance from each other: that's the sync.
        let scale = |t: usize| u64::from(trak_timescale(&m.traks[t]).unwrap_or(1).max(1));
        let openings: Vec<Option<(u64, u64, u64)>> = (0..tracks.len())
            .map(|t| {
                if tracks[t].sizes.is_empty() {
                    return None;
                }
                let (edit, wait) = edit_start(&m.traks[t]);
                let (media, shown, padding) = tracks[t].opening(edit);
                Some((media, shown * MOVIE_SCALE / scale(t) + wait, padding))
            })
            .collect();
        let earliest = openings.iter().flatten().map(|o| o.1).min().unwrap_or(0);

        let moov_with = |first: u64| {
            let mut at = first;
            let mut places: Vec<Vec<u64>> = tracks.iter().map(|_| vec![]).collect();
            for (t, r) in &order {
                places[*t].push(at);
                at += tracks[*t].runs[*r].1;
            }
            let mut longest = 0u64;
            let mut traks = vec![];
            for (index, samples) in tracks.iter().enumerate() {
                let Some((media, shown, padding)) = openings[index] else { continue };
                let media_length: u64 = samples.durations.iter().map(|d| u64::from(*d)).sum();
                let wait = shown - earliest;
                let playing = media_length.saturating_sub(padding) * MOVIE_SCALE / scale(index);
                longest = longest.max(wait + playing);
                let source = &m.traks[index];
                let mut trak = vec![];
                for (o, s, k) in children(source) {
                    let body = &source[o + header_len(source, o)..o + s];
                    match &k {
                        b"tkhd" => {
                            let mut p = body.to_vec();
                            set_duration(&mut p, 20, 28, wait + playing);
                            p[3] |= 1;
                            trak.extend(make_box(b"tkhd", &p));
                            if wait > 0 || media > 0 {
                                trak.extend(edit_list(wait, media, playing));
                            }
                        }
                        b"edts" => {}
                        b"mdia" => {
                            let mdia = rewrite(body, b"mdhd", |p| {
                                let mut p = p.to_vec();
                                set_duration(&mut p, 16, 24, media_length);
                                p
                            });
                            let mdia = rewrite(&mdia, b"minf", |minf| {
                                rewrite(minf, b"stbl", |stbl| {
                                    let mut out = find(stbl, b"stsd").map_or(vec![], |(o, s)| stbl[o..o + s].to_vec());
                                    out.extend(make_box(b"stts", &samples.stts()));
                                    if let Some(table) = samples.ctts() {
                                        out.extend(make_box(b"ctts", &table));
                                    }
                                    if let Some(table) = samples.stss() {
                                        out.extend(make_box(b"stss", &table));
                                    }
                                    out.extend(make_box(b"stsc", &samples.stsc()));
                                    out.extend(make_box(b"stsz", &samples.stsz()));
                                    out.extend(make_box(if wide { b"co64" } else { b"stco" }, &Samples::chunk_table(&places[index], wide)));
                                    out
                                })
                            });
                            trak.extend(make_box(b"mdia", &mdia));
                        }
                        _ => trak.extend_from_slice(&source[o..o + s]),
                    }
                }
                traks.extend(make_box(b"trak", &trak));
            }
            let mut mvhd = payload(&m.moov, b"mvhd").map(|r| m.moov[r].to_vec()).unwrap_or_default();
            let clock_at = if mvhd[0] == 1 { 20 } else { 12 };
            set_u32(&mut mvhd, clock_at, MOVIE_SCALE as u32);
            set_duration(&mut mvhd, 16, 24, longest);
            make_box(b"moov", &[make_box(b"mvhd", &mvhd), traks, tags_box(title, None)].concat())
        };
        if payload(&m.moov, b"mvhd").is_none() {
            return Ok(false);
        }

        let ftyp = make_box(b"ftyp", &[&b"isom"[..], &[0, 0, 2, 0], b"isom", b"iso2", b"mp41"].concat());
        let header = ftyp.len() as u64 + moov_with(0).len() as u64 + mdat_header;
        let mut w = BufWriter::with_capacity(1 << 20, File::create(dest)?);
        w.write_all(&ftyp)?;
        w.write_all(&moov_with(header))?;
        if wide {
            w.write_all(&1u32.to_be_bytes())?;
            w.write_all(b"mdat")?;
            w.write_all(&(data + 16).to_be_bytes())?;
        } else {
            w.write_all(&((data + 8) as u32).to_be_bytes())?;
            w.write_all(b"mdat")?;
        }
        for (t, r) in &order {
            let (offset, bytes, _) = tracks[*t].runs[*r];
            copy_range(&mut f, offset, bytes, &mut w)?;
        }
        w.flush()?;
        Ok(true)
    }

    /// The last step for a video stream: an ordinary MP4 in place of a fragmented one. Any
    /// trouble, a full disk included, leaves the file that was already good.
    pub fn finish_video(path: std::path::PathBuf, title: &str) -> std::path::PathBuf {
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")) {
            return path;
        }
        let work = path.with_extension("mp4.part");
        if !matches!(to_plain(&path, &work, title), Ok(true)) {
            let _ = std::fs::remove_file(&work);
            return path;
        }
        if std::fs::rename(&work, &path).is_err() {
            let replaced = std::fs::copy(&work, &path).is_ok();
            let _ = std::fs::remove_file(&work);
            debug_assert!(replaced);
        }
        path
    }

    /// The last step for a stream: audio on its own becomes an ordinary `.m4a` with its length
    /// and title. Anything else, and any trouble, leaves the file as it is.
    pub fn finish_audio(path: std::path::PathBuf, stem: &str) -> std::path::PathBuf {
        if !is_audio_only(&path) {
            return path;
        }
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let (artist, title) = stem.split_once(" - ").map_or((None, stem), |(a, t)| (Some(a), t));
        let work = path.with_extension("m4a.part");
        let done = defragment(&path, &work, title, artist).unwrap_or(false);
        let from = if done { work.clone() } else { path.clone() };
        if done {
            let _ = std::fs::remove_file(&path);
        } else {
            let _ = std::fs::remove_file(&work);
        }
        let dest = crate::naming::unique_path(&dir, &format!("{stem}.m4a"));
        if std::fs::rename(&from, &dest).is_ok() { dest } else { from }
    }

    #[cfg(test)]
    mod overlap {
        use super::*;

        #[test]
        fn sound_that_overlaps_the_part_before_is_left_out() {
            // Two fragments of four samples of 100, the second starting 200 before the first ended.
            let moof = |time: u64, offset: u32, sizes: [u32; 4]| {
                let tfhd = make_box(b"tfhd", &[&[0, 2, 0, 0x08][..], &1u32.to_be_bytes(), &100u32.to_be_bytes()].concat());
                let tfdt = make_box(b"tfdt", &[&[1, 0, 0, 0][..], &time.to_be_bytes()].concat());
                let mut trun = vec![0, 0, 2, 1];
                trun.extend_from_slice(&4u32.to_be_bytes());
                trun.extend_from_slice(&offset.to_be_bytes());
                sizes.iter().for_each(|s| trun.extend_from_slice(&s.to_be_bytes()));
                [tfhd, tfdt, make_box(b"trun", &trun)].concat()
            };
            let mut samples = Samples { can_trim: true, ..Default::default() };
            samples.read_traf(&moof(0, 8, [1, 1, 1, 1]), 1000, 1000, Defaults::default()).unwrap();
            samples.read_traf(&moof(200, 8, [10, 20, 30, 40]), 2000, 2000, Defaults::default()).unwrap();
            assert_eq!(samples.sizes, [1, 1, 1, 1, 30, 40]);
            assert_eq!(samples.durations.iter().sum::<u32>(), 600);
            // The kept run starts after the two samples left out.
            assert_eq!(samples.runs[1], (2000 + 8 + 30, 70, 2));
        }
    }
}

// ------------------------------------------------------------------------ WebM

pub mod webm {
    use super::*;

    const SEGMENT: u32 = 0x1853_8067;
    const INFO: u32 = 0x1549_A966;
    const TRACKS: u32 = 0x1654_AE6B;
    const TRACK_ENTRY: u32 = 0xAE;
    const TRACK_NUMBER: u32 = 0xD7;
    const TRACK_UID: u32 = 0x73C5;
    const CLUSTER: u32 = 0x1F43_B675;
    const TIMECODE: u32 = 0xE7;
    const TIMECODE_SCALE: u32 = 0x2A_D7B1;
    const SIMPLE_BLOCK: u32 = 0xA3;
    const BLOCK_GROUP: u32 = 0xA0;
    const BLOCK: u32 = 0xA1;
    const UNKNOWN: u64 = u64::MAX;

    /// Read an element id (keeps the length marker bits, as ids are written that way).
    fn read_id(b: &[u8]) -> Option<(u32, usize)> {
        let first = *b.first()?;
        let len = first.leading_zeros() as usize + 1;
        if len > 4 || b.len() < len {
            return None;
        }
        let mut v = 0u32;
        for &x in &b[..len] {
            v = (v << 8) | x as u32;
        }
        Some((v, len))
    }

    /// Read a size vint (marker removed). All-ones = unknown size.
    fn read_size(b: &[u8]) -> Option<(u64, usize)> {
        let first = *b.first()?;
        let len = first.leading_zeros() as usize + 1;
        if len > 8 || b.len() < len {
            return None;
        }
        let mut v = (first as u64) & (0xFF >> len);
        let mut all_ones = v == (0xFF >> len) as u64;
        for &x in &b[1..len] {
            v = (v << 8) | x as u64;
            all_ones &= x == 0xFF;
        }
        Some((if all_ones { UNKNOWN } else { v }, len))
    }

    fn write_size(v: u64) -> Vec<u8> {
        // 8-byte size, always valid.
        let mut out = vec![0x01];
        out.extend_from_slice(&v.to_be_bytes()[1..]);
        out
    }

    fn write_id(id: u32) -> Vec<u8> {
        let bytes = id.to_be_bytes();
        let skip = bytes.iter().position(|&b| b != 0).unwrap_or(3);
        bytes[skip..].to_vec()
    }

    fn element(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = write_id(id);
        v.extend(write_size(payload.len() as u64));
        v.extend_from_slice(payload);
        v
    }

    /// Children of an in-memory element payload: (id, start of element, header len, payload len).
    fn children(b: &[u8]) -> Vec<(u32, usize, usize, usize)> {
        let mut out = vec![];
        let mut pos = 0;
        while pos < b.len() {
            let Some((id, il)) = read_id(&b[pos..]) else { break };
            let Some((size, sl)) = read_size(&b[pos + il..]) else { break };
            let size = if size == UNKNOWN { (b.len() - pos - il - sl) as u64 } else { size };
            let end = pos + il + sl + size as usize;
            if end > b.len() {
                break;
            }
            out.push((id, pos, il + sl, size as usize));
            pos = end;
        }
        out
    }

    fn uint(b: &[u8]) -> u64 {
        b.iter().fold(0u64, |a, &x| (a << 8) | x as u64)
    }

    struct Parsed {
        ebml_header: (u64, u64),
        info: Option<Vec<u8>>, // whole element
        track_entries: Vec<Vec<u8>>, // payloads
        timecode_scale: u64,
        clusters: Vec<(u64, u64, u64)>, // (start, total len, timecode)
    }

    /// Read the header (start,len) of the element at `pos`.
    fn head_at(f: &mut File, pos: u64) -> io::Result<(u32, usize, u64)> {
        let h = read_at(f, pos, 12.min(f.metadata()?.len().saturating_sub(pos)))?;
        let (id, il) = read_id(&h).ok_or_else(|| bad("bad element id"))?;
        let (size, sl) = read_size(&h[il..]).ok_or_else(|| bad("bad element size"))?;
        Ok((id, il + sl, size))
    }

    fn parse(f: &mut File) -> io::Result<Parsed> {
        let len = f.metadata()?.len();
        let (id, hl, size) = head_at(f, 0)?;
        if id != 0x1A45_DFA3 {
            return Err(bad("not EBML"));
        }
        let ebml_header = (0, hl as u64 + size);
        let seg_pos = ebml_header.1;
        let (id, shl, ssize) = head_at(f, seg_pos)?;
        if id != SEGMENT {
            return Err(bad("no segment"));
        }
        let seg_end = if ssize == UNKNOWN { len } else { (seg_pos + shl as u64 + ssize).min(len) };
        let mut p = Parsed { ebml_header, info: None, track_entries: vec![], timecode_scale: 1_000_000, clusters: vec![] };
        let mut pos = seg_pos + shl as u64;
        while pos < seg_end {
            let (id, hl, size) = head_at(f, pos)?;
            let size = if size == UNKNOWN {
                // Only clusters are written with unknown size: it ends at the next cluster.
                let mut end = pos + hl as u64;
                loop {
                    if end >= seg_end {
                        break seg_end - pos - hl as u64;
                    }
                    let (cid, chl, csize) = head_at(f, end)?;
                    let top_level = [CLUSTER, 0x1C53_BB6B, 0x1254_C367, 0x114D_9B74, INFO, TRACKS, 0x1043_A770, 0x1941_A469];
                    if top_level.contains(&cid) || csize == UNKNOWN {
                        break end - pos - hl as u64;
                    }
                    end += chl as u64 + csize;
                }
            } else {
                size
            };
            let total = hl as u64 + size;
            match id {
                INFO => {
                    let el = read_at(f, pos, total)?;
                    for (cid, s, h, l) in children(&el[hl..]) {
                        if cid == TIMECODE_SCALE {
                            p.timecode_scale = uint(&el[hl + s + h..hl + s + h + l]);
                        }
                    }
                    p.info = Some(el);
                }
                TRACKS => {
                    let el = read_at(f, pos + hl as u64, size)?;
                    for (cid, s, h, l) in children(&el) {
                        if cid == TRACK_ENTRY {
                            p.track_entries.push(el[s + h..s + h + l].to_vec());
                        }
                    }
                }
                CLUSTER => {
                    // Cluster timecode is normally its first child.
                    let peek = read_at(f, pos + hl as u64, size.min(32))?;
                    let tc = children(&peek).into_iter().find(|c| c.0 == TIMECODE).map(|(_, s, h, l)| uint(&peek[s + h..s + h + l])).unwrap_or(0);
                    p.clusters.push((pos, total, tc));
                }
                _ => {}
            }
            if total == 0 {
                break;
            }
            pos += total;
        }
        Ok(p)
    }

    /// Set the track number vint in Block / SimpleBlock payloads of a cluster.
    fn renumber_cluster(cluster: &mut [u8], header: usize, track: u8) {
        let body_start = header;
        let kids = children(&cluster[body_start..]);
        for (id, s, h, l) in kids {
            let at = body_start + s + h;
            match id {
                SIMPLE_BLOCK if l > 0 => cluster[at] = 0x80 | track,
                BLOCK_GROUP => {
                    for (bid, bs, bh, bl) in children(&cluster[at..at + l]) {
                        if bid == BLOCK && bl > 0 {
                            cluster[at + bs + bh] = 0x80 | track;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn set_track_entry(entry: &[u8], number: u8, uid: u64) -> Vec<u8> {
        let mut out = vec![];
        for (id, s, h, l) in children(entry) {
            match id {
                TRACK_NUMBER => out.extend(element(TRACK_NUMBER, &[number])),
                TRACK_UID => out.extend(element(TRACK_UID, &uid.to_be_bytes())),
                _ => out.extend_from_slice(&entry[s..s + h + l]),
            }
        }
        out
    }

    pub fn merge(video: &Path, audio: &Path, out: &Path) -> io::Result<bool> {
        let mut vf = File::open(video)?;
        let mut af = File::open(audio)?;
        let (v, a) = match (parse(&mut vf), parse(&mut af)) {
            (Ok(v), Ok(a)) => (v, a),
            _ => return Ok(false),
        };
        if v.track_entries.len() != 1 || a.track_entries.len() != 1 || v.timecode_scale != a.timecode_scale {
            return Ok(false);
        }
        // Each input has a single track; it must be track 1 for the in-place renumbering.
        let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
        copy_range(&mut vf, v.ebml_header.0, v.ebml_header.1, &mut w)?;
        w.write_all(&write_id(SEGMENT))?;
        w.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?; // unknown size
        if let Some(info) = &v.info {
            w.write_all(info)?;
        }
        let mut tracks = element(TRACK_ENTRY, &set_track_entry(&v.track_entries[0], 1, 0x5244_4d01));
        tracks.extend(element(TRACK_ENTRY, &set_track_entry(&a.track_entries[0], 2, 0x5244_4d02)));
        w.write_all(&element(TRACKS, &tracks))?;
        let mut all: Vec<(u64, bool, u64, u64)> = v.clusters.iter().map(|&(s, l, t)| (t, false, s, l)).chain(a.clusters.iter().map(|&(s, l, t)| (t, true, s, l))).collect();
        all.sort_by_key(|x| (x.0, x.1));
        for (_, is_audio, start, len) in all {
            let src = if is_audio { &mut af } else { &mut vf };
            let mut c = read_at(src, start, len)?;
            let (_, il) = read_id(&c).ok_or_else(|| bad("cluster id"))?;
            let (_, sl) = read_size(&c[il..]).ok_or_else(|| bad("cluster size"))?;
            // Rewrite the cluster with a known size (inputs may use unknown sizes).
            let body = c.split_off(il + sl);
            let mut fixed = write_id(CLUSTER);
            fixed.extend(write_size(body.len() as u64));
            let hdr = fixed.len();
            fixed.extend(body);
            renumber_cluster(&mut fixed, hdr, if is_audio { 2 } else { 1 });
            w.write_all(&fixed)?;
        }
        w.flush()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(body);
        bx(kind, &p)
    }

    /// A fragmented audio file: two fragments of two samples each, with sizes in the `trun`.
    fn audio_fmp4() -> Vec<u8> {
        let mut tkhd = vec![0u8; 8];
        tkhd.extend_from_slice(&1u32.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 68]);
        let mut mdhd = vec![0u8; 8];
        mdhd.extend_from_slice(&1000u32.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 8]);
        let hdlr = full(b"hdlr", &[&[0u8; 4][..], b"soun", &[0u8; 13]].concat());
        let stbl = bx(b"stbl", &[full(b"stsd", &[0, 0, 0, 0]), full(b"stts", &[0, 0, 0, 0])].concat());
        let mdia = bx(b"mdia", &[full(b"mdhd", &mdhd), hdlr, bx(b"minf", &stbl)].concat());
        let edts = bx(b"edts", &full(b"elst", &[0u8; 4]));
        let trak = bx(b"trak", &[full(b"tkhd", &tkhd), edts, mdia].concat());
        let mut trex = 1u32.to_be_bytes().to_vec();
        trex.extend_from_slice(&[0, 0, 0, 1]);
        trex.extend_from_slice(&100u32.to_be_bytes()); // default duration
        trex.extend_from_slice(&[0u8; 8]);
        let mut mvhd = vec![0u8; 8];
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 84]);
        let moov = bx(b"moov", &[full(b"mvhd", &mvhd), trak, bx(b"mvex", &full(b"trex", &trex))].concat());
        let mut out = [bx(b"ftyp", b"iso6\0\0\0\0"), moov].concat();
        for (i, bytes) in [[10u8, 20], [30, 40]].iter().enumerate() {
            let mdat = [vec![bytes[0]; 3], vec![bytes[1]; 5]].concat();
            let moof_of = |offset: u32| {
                let tfhd = bx(b"tfhd", &[&[0, 2, 0, 0][..], &1u32.to_be_bytes()].concat());
                let mut trun = vec![0, 0, 2, 1]; // data offset and sample sizes
                trun.extend_from_slice(&2u32.to_be_bytes());
                trun.extend_from_slice(&offset.to_be_bytes());
                trun.extend_from_slice(&3u32.to_be_bytes());
                trun.extend_from_slice(&5u32.to_be_bytes());
                let traf = bx(b"traf", &[tfhd, bx(b"trun", &trun)].concat());
                bx(b"moof", &[full(b"mfhd", &(i as u32 + 1).to_be_bytes()), traf].concat())
            };
            let offset = moof_of(0).len() as u32 + 8;
            out.extend(moof_of(offset));
            out.extend(bx(b"mdat", &mdat));
        }
        out
    }

    #[test]
    fn audio_fragments_become_an_ordinary_file_with_a_length_and_tags() {
        let dir = std::env::temp_dir().join(format!("rdm-defrag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, out) = (dir.join("in.mp4"), dir.join("out.m4a"));
        std::fs::write(&src, audio_fmp4()).unwrap();
        assert!(mp4::is_audio_only(&src));
        assert!(mp4::defragment(&src, &out, "Song", Some("Artist")).unwrap());
        let b = std::fs::read(&out).unwrap();
        let top = mp4::children(&b);
        assert_eq!(top.iter().map(|c| c.2).collect::<Vec<_>>(), [*b"ftyp", *b"moov", *b"mdat"]);
        // The samples, in order, with nothing else.
        let (o, s, _) = top[2];
        assert_eq!(&b[o + 8..o + s], [vec![10; 3], vec![20; 5], vec![30; 3], vec![40; 5]].concat());
        let moov = &b[top[1].0 + 8..top[1].0 + top[1].1];
        let box_of = |buf: &[u8], k: &[u8; 4]| mp4::children(buf).into_iter().find(|c| &c.2 == k).map(|(o, s, _)| buf[o + 8..o + s].to_vec()).unwrap();
        // Four samples of 100 at 1000 per second: 0.4 s, in the movie and the track.
        assert_eq!(u32::from_be_bytes(box_of(moov, b"mvhd")[16..20].try_into().unwrap()), 400);
        let trak = box_of(moov, b"trak");
        assert_eq!(u32::from_be_bytes(box_of(&trak, b"tkhd")[20..24].try_into().unwrap()), 400);
        let stbl = box_of(&box_of(&box_of(&trak, b"mdia"), b"minf"), b"stbl");
        assert_eq!(box_of(&stbl, b"stts")[8..], [0, 0, 0, 4, 0, 0, 0, 100]);
        let co64 = box_of(&stbl, b"co64");
        assert_eq!(u64::from_be_bytes(co64[8..16].try_into().unwrap()), o as u64 + 8);
        // No fragments left, and the tags are there.
        assert!(mp4::children(moov).iter().all(|c| &c.2 != b"mvex"));
        assert!(mp4::children(&trak).iter().all(|c| &c.2 != b"edts"));
        assert!(b.windows(4).any(|w| w == b"Song") && b.windows(6).any(|w| w == b"Artist"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fragments_become_an_ordinary_file_with_the_samples_in_place() {
        let dir = std::env::temp_dir().join(format!("rdm-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, out) = (dir.join("in.mp4"), dir.join("out.mp4"));
        std::fs::write(&src, audio_fmp4()).unwrap();
        assert!(mp4::to_plain(&src, &out, "Clip").unwrap());
        let b = std::fs::read(&out).unwrap();
        let top = mp4::children(&b);
        assert_eq!(top.iter().map(|c| c.2).collect::<Vec<_>>(), [*b"ftyp", *b"moov", *b"mdat"]);
        let (o, s, _) = top[2];
        assert_eq!(&b[o + 8..o + s], [vec![10; 3], vec![20; 5], vec![30; 3], vec![40; 5]].concat());
        let moov = &b[top[1].0 + 8..top[1].0 + top[1].1];
        let box_of = |buf: &[u8], k: &[u8; 4]| mp4::children(buf).into_iter().find(|c| &c.2 == k).map(|(o, s, _)| buf[o + 8..o + s].to_vec()).unwrap();
        assert!(mp4::children(moov).iter().all(|c| &c.2 != b"mvex"));
        assert_eq!(u32::from_be_bytes(box_of(moov, b"mvhd")[16..20].try_into().unwrap()), 400);
        let trak = box_of(moov, b"trak");
        let stbl = box_of(&box_of(&box_of(&trak, b"mdia"), b"minf"), b"stbl");
        assert_eq!(box_of(&stbl, b"stts")[8..], [0, 0, 0, 4, 0, 0, 0, 100]);
        // The table points at the first sample, and at the second fragment's after it.
        let stco = box_of(&stbl, b"stco");
        let first = u32::from_be_bytes(stco[8..12].try_into().unwrap()) as usize;
        assert_eq!(first, o + 8);
        assert_eq!(u32::from_be_bytes(stco[12..16].try_into().unwrap()) as usize, first + 8);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parts_of_a_file_move_along_their_clock() {
        let dir = std::env::temp_dir().join(format!("rdm-times-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("in.mp4");
        // Three fragments of one second each, the last of them a new part starting over at 0.
        let mut file = fmp4(1, 1000, 3, 1);
        let at = mp4::children(&file).iter().filter(|c| &c.2 == b"moof").map(|c| c.0 as u64).collect::<Vec<_>>();
        let last = at[2] as usize;
        let tfdt = last + file[last..].windows(4).position(|w| w == b"tfdt").unwrap() + 4;
        file[tfdt + 4..tfdt + 12].copy_from_slice(&0u64.to_be_bytes());
        let size = file.len();
        std::fs::write(&path, file).unwrap();
        let parts = [0, at[2]];
        let times = |p: &std::path::Path| mp4::first_times(p, &parts).unwrap();
        assert_eq!(times(&path), [Some(0.0), Some(0.0)]);
        mp4::move_times(&path, &parts, &[0.0, 2.0]).unwrap();
        assert_eq!(times(&path), [Some(0.0), Some(2.0)]);
        assert_eq!(std::fs::read(&path).unwrap().len(), size);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_that_is_not_fragmented_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("rdm-plain-skip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, out) = (dir.join("in.mp4"), dir.join("out.mp4"));
        std::fs::write(&src, bx(b"moov", &full(b"mvhd", &[0u8; 96]))).unwrap();
        assert!(!mp4::to_plain(&src, &out, "Clip").unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A tiny fragmented MP4 with one track and `n` fragments.
    fn fmp4(track: u32, timescale: u32, n: u32, fill: u8) -> Vec<u8> {
        let mut tkhd = vec![0u8; 8];
        tkhd.extend_from_slice(&track.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 68]);
        let mut mdhd = vec![0u8; 8];
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 8]);
        let trak = bx(b"trak", &[full(b"tkhd", &tkhd), bx(b"mdia", &full(b"mdhd", &mdhd))].concat());
        let mut trex = track.to_be_bytes().to_vec();
        trex.extend_from_slice(&[0u8; 16]);
        let mvhd = full(b"mvhd", &[0u8; 96]);
        let moov = bx(b"moov", &[mvhd, trak, bx(b"mvex", &full(b"trex", &trex))].concat());
        let mut out = [bx(b"ftyp", b"isom\0\0\0\0"), moov].concat();
        for i in 0..n {
            let mut mfhd = vec![];
            mfhd.extend_from_slice(&(i + 1).to_be_bytes());
            let mut tfhd = vec![0x02, 0x00, 0x00]; // default-base-is-moof
            tfhd.extend_from_slice(&track.to_be_bytes());
            let mut tfhd_full = vec![0u8];
            tfhd_full.extend(tfhd);
            let mut tfdt = vec![1, 0, 0, 0];
            tfdt.extend_from_slice(&(u64::from(i) * u64::from(timescale)).to_be_bytes());
            let traf = bx(b"traf", &[bx(b"tfhd", &tfhd_full), bx(b"tfdt", &tfdt)].concat());
            out.extend(bx(b"moof", &[full(b"mfhd", &mfhd), traf].concat()));
            out.extend(bx(b"mdat", &vec![fill.wrapping_add(i as u8); 100]));
        }
        out
    }

    #[test]
    fn mp4_pair_is_merged_and_interleaved() {
        let dir = std::env::temp_dir().join(format!("rdm_mux_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (v, a, o) = (dir.join("v.mp4"), dir.join("a.m4a"), dir.join("out.mp4"));
        std::fs::write(&v, fmp4(1, 90000, 3, 10)).unwrap();
        std::fs::write(&a, fmp4(1, 48000, 3, 200)).unwrap();
        assert!(merge(&v, &a, &o).unwrap());
        let mut f = File::open(&o).unwrap();
        let boxes = mp4::scan(&mut f).unwrap();
        let kinds: Vec<String> = boxes.iter().map(|b| String::from_utf8_lossy(&b.kind).into_owned()).collect();
        assert_eq!(kinds, ["ftyp", "moov", "moof", "mdat", "moof", "mdat", "moof", "mdat", "moof", "mdat", "moof", "mdat", "moof", "mdat"]);
        let data = std::fs::read(&o).unwrap();
        let moov = &boxes[1];
        let moov_body = &data[(moov.start + 8) as usize..(moov.start + moov.size) as usize];
        assert_eq!(mp4::children(moov_body).iter().filter(|c| &c.2 == b"trak").count(), 2);
        // Fragments alternate video/audio (same times) and audio fragments carry track 2.
        let mdats: Vec<u8> = boxes.iter().filter(|b| &b.kind == b"mdat").map(|b| data[(b.start + 8) as usize]).collect();
        assert_eq!(mdats, [10, 200, 11, 201, 12, 202]);
        let second_moof = &boxes[4];
        let body = &data[(second_moof.start + 8) as usize..(second_moof.start + second_moof.size) as usize];
        let traf = mp4::children(body).into_iter().find(|c| &c.2 == b"traf").unwrap();
        let tfhd = mp4::children(&body[traf.0 + 8..traf.0 + traf.1])[0];
        let id_at = traf.0 + 8 + tfhd.0 + 8 + 4;
        assert_eq!(u32::from_be_bytes(body[id_at..id_at + 4].try_into().unwrap()), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ebml(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.push(0x80 | payload.len() as u8); // payloads in tests are < 127 bytes
        v.extend_from_slice(payload);
        v
    }

    fn webm_file(codec: &[u8], tcs: &[u8], fill: u8) -> Vec<u8> {
        let header = ebml(&[0x1A, 0x45, 0xDF, 0xA3], &ebml(&[0x42, 0x82], b"webm"));
        let info = ebml(&[0x15, 0x49, 0xA9, 0x66], &ebml(&[0x2A, 0xD7, 0xB1], &[0x0F, 0x42, 0x40]));
        let entry = ebml(&[0xAE], &[ebml(&[0xD7], &[1]), ebml(&[0x73, 0xC5], &[9]), ebml(&[0x86], codec)].concat());
        let tracks = ebml(&[0x16, 0x54, 0xAE, 0x6B], &entry);
        let mut clusters = vec![];
        for (i, &tc) in tcs.iter().enumerate() {
            let block = [vec![0x81, 0, 0, 0x80], vec![fill + i as u8; 4]].concat();
            clusters.extend(ebml(&[0x1F, 0x43, 0xB6, 0x75], &[ebml(&[0xE7], &[tc]), ebml(&[0xA3], &block)].concat()));
        }
        let mut seg = vec![0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        seg.extend([info, tracks, clusters].concat());
        [header, seg].concat()
    }

    #[test]
    fn webm_pair_is_merged_with_two_tracks() {
        let dir = std::env::temp_dir().join(format!("rdm_muxw_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (v, a, o) = (dir.join("v.webm"), dir.join("a.webm"), dir.join("out.webm"));
        std::fs::write(&v, webm_file(b"V_VP9", &[0, 50, 100], 10)).unwrap();
        std::fs::write(&a, webm_file(b"A_OPUS", &[0, 60], 100)).unwrap();
        assert!(merge(&v, &a, &o).unwrap());
        let d = std::fs::read(&o).unwrap();
        let find = |needle: &[u8]| d.windows(needle.len()).filter(|w| *w == needle).count();
        assert_eq!(find(b"V_VP9"), 1);
        assert_eq!(find(b"A_OPUS"), 1);
        assert_eq!(find(&[0x1F, 0x43, 0xB6, 0x75]), 5);
        // Audio blocks now carry track 2.
        assert_eq!(find(&[0x82, 0, 0, 0x80, 100, 100]), 1);
        assert_eq!(find(&[0x81, 0, 0, 0x80, 10, 10]), 1);
        // Clusters are ordered by time: v0, a0, v50, a60, v100.
        let order: Vec<u8> = d.windows(6).filter(|w| w[0] & 0xF0 == 0x80 && w[1..4] == [0, 0, 0x80]).map(|w| w[4]).collect();
        assert_eq!(order, [10, 100, 11, 101, 12]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

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
        if v.traks.len() != 1 || a.traks.len() != 1 {
            return Ok(false);
        }
        let v_ts = trak_timescale(&v.traks[0]).unwrap_or(1);
        let a_ts = trak_timescale(&a.traks[0]).unwrap_or(1);
        let v_id = trak_id(&v.traks[0]).unwrap_or(1);
        let a_id = v_id + 1;

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
                    moov.extend(make_box(b"trak", &a_trak));
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

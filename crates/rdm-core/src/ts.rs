//! MPEG-TS, raw AAC and raw MP3 streams turned into fragmented MP4, without ffmpeg.
//!
//! Classic HLS delivers `.ts` segments (or `.aac` for a separate audio rendition). Each track
//! becomes its own fragmented MP4 here, which [`crate::mux`] can then join like any other
//! pair. The encoded data is copied untouched: only the packaging changes, so nothing is
//! re-encoded. Anything this code does not understand is an error, and the caller falls back
//! to ffmpeg or to saving the stream as it came, so nothing is lost by trying.
//!
//! Supported: H.264 and H.265 video; AAC and MP3 audio. Files are streamed, never read whole.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const PACKET: usize = 188;
const CLOCK: i64 = 90_000;
const WRAP: i64 = 1 << 33;
/// A timestamp further than this from the last one is a splice (an ad break, a new encoder
/// session), not a gap in the media: the track simply carries on from where it was.
const JUMP: i64 = 2 * CLOCK;
const FRAGMENT_SECONDS: u64 = 2;
const MAX_FRAGMENT_BYTES: usize = 32 << 20;

fn unsupported(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Ts,
    Adts,
    Mp3,
}

/// What a joined stream really is, judged from its content: segment names can't be trusted
/// (some sites serve TS as `.jpg` or put a few bytes of an image in front of it).
pub fn sniff(path: &Path) -> io::Result<Option<Kind>> {
    let mut head = vec![0u8; 1 << 20];
    let mut f = File::open(path)?;
    let mut n = 0;
    while n < head.len() {
        let got = f.read(&mut head[n..])?;
        if got == 0 {
            break;
        }
        n += got;
    }
    let head = &head[..n];
    if head.len() >= PACKET * 4 {
        for p in 0..head.len() - PACKET * 3 {
            if (0..4).all(|i| head[p + i * PACKET] == 0x47) {
                return Ok(Some(Kind::Ts));
            }
        }
    }
    let start = skip_id3(head);
    Ok(match (frame_at(head, start), start) {
        (Some(Frame::Adts { len, .. }), s) if matches!(frame_at(head, s + len), Some(Frame::Adts { .. })) => Some(Kind::Adts),
        (Some(Frame::Mp3 { len, .. }), s) if matches!(frame_at(head, s + len), Some(Frame::Mp3 { .. })) => Some(Kind::Mp3),
        _ => None,
    })
}

/// Past any ID3 tags at the start of `b`.
fn skip_id3(b: &[u8]) -> usize {
    let mut p = 0;
    while let Some(len) = id3_len(&b[p..]) {
        if p + len > b.len() {
            break;
        }
        p += len;
    }
    p
}

fn id3_len(b: &[u8]) -> Option<usize> {
    if b.len() < 10 || &b[..3] != b"ID3" {
        return None;
    }
    let size = b[6..10].iter().fold(0usize, |v, &x| (v << 7) | (x & 0x7F) as usize);
    Some(10 + size + if b[5] & 0x10 != 0 { 10 } else { 0 })
}

/// The stream time HLS packed audio carries in an ID3 tag (Apple's transport stream timestamp).
fn id3_time(b: &[u8]) -> Option<u64> {
    const OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp\0";
    let at = b.windows(OWNER.len()).position(|w| w == OWNER)? + OWNER.len();
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?) & (WRAP as u64 - 1))
}

// ---------------------------------------------------------------- audio frames

#[derive(Debug, Clone, Copy, PartialEq)]
enum Frame {
    /// One AAC frame with its ADTS header.
    Adts { len: usize, header: usize, profile: u8, rate_index: u8, channels: u8, blocks: u8 },
    /// One MP3 frame.
    Mp3 { len: usize, rate: u32, channels: u8, samples: u32 },
}

const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

fn frame_at(b: &[u8], p: usize) -> Option<Frame> {
    let h = b.get(p..p + 7)?;
    if h[0] != 0xFF || h[1] & 0xF0 != 0xF0 && h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let layer = (h[1] >> 1) & 3;
    if h[1] & 0xF0 == 0xF0 && layer == 0 {
        let header = if h[1] & 1 == 1 { 7 } else { 9 };
        let rate_index = (h[2] >> 2) & 0xF;
        let channels = ((h[2] & 1) << 2) | (h[3] >> 6);
        let len = ((h[3] as usize & 3) << 11) | ((h[4] as usize) << 3) | (h[5] as usize >> 5);
        if len <= header || rate_index as usize >= AAC_RATES.len() {
            return None;
        }
        return Some(Frame::Adts { len, header, profile: h[2] >> 6, rate_index, channels, blocks: (h[6] & 3) + 1 });
    }
    if layer != 1 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5
    if version == 1 {
        return None;
    }
    let mpeg1 = version == 3;
    const BR1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BR2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let bitrate = *(if mpeg1 { &BR1 } else { &BR2 }).get((h[2] >> 4) as usize)?;
    let rates: [u32; 3] = match version {
        3 => [44100, 48000, 32000],
        2 => [22050, 24000, 16000],
        _ => [11025, 12000, 8000],
    };
    let rate = *rates.get(((h[2] >> 2) & 3) as usize)?;
    if bitrate == 0 {
        return None;
    }
    let pad = ((h[2] >> 1) & 1) as u32;
    let len = (if mpeg1 { 144 } else { 72 } * bitrate * 1000 / rate + pad) as usize;
    Some(Frame::Mp3 { len, rate, channels: if h[3] >> 6 == 3 { 1 } else { 2 }, samples: if mpeg1 { 1152 } else { 576 } })
}

#[derive(PartialEq, Clone, Copy)]
enum AudioConfig {
    Aac { profile: u8, rate_index: u8, channels: u8 },
    Mp3 { rate: u32, channels: u8 },
}

struct Audio<'a> {
    out: &'a Path,
    writer: Option<Writer>,
    config: Option<AudioConfig>,
    buf: Vec<u8>,
    /// Stream time of the next frame, in samples.
    next: Option<u64>,
    rate: u64,
    frame: u64,
    /// Time of the first data, kept until the format is known.
    pending: Option<u64>,
    /// Samples still to leave out: sound that would start before the last one ended.
    overlap: u64,
}

impl<'a> Audio<'a> {
    fn new(out: &'a Path) -> Self {
        Self { out, writer: None, config: None, buf: vec![], next: None, rate: 0, frame: 1024, pending: None, overlap: 0 }
    }

    /// `time` is where this data starts, in 90 kHz ticks from the start of the file.
    fn feed(&mut self, time: Option<u64>, data: &[u8]) -> io::Result<()> {
        self.buf.extend_from_slice(data);
        if let (Some(t), true) = (time, self.rate > 0) {
            self.anchor(t);
        }
        if self.rate == 0 {
            self.pending = self.pending.or(time);
        }
        let mut p = 0;
        loop {
            if let Some(len) = id3_len(&self.buf[p..]) {
                if p + len > self.buf.len() {
                    break;
                }
                p += len;
                continue;
            }
            match frame_at(&self.buf, p) {
                Some(f) => {
                    let (len, header) = match f {
                        Frame::Adts { len, header, .. } => (len, header),
                        Frame::Mp3 { len, .. } => (len, 0),
                    };
                    if p + len > self.buf.len() {
                        break;
                    }
                    // A false sync inside damaged data: the next frame must follow on.
                    if p + len + 2 <= self.buf.len() && !(self.buf[p + len] == 0xFF || self.buf[p + len..].starts_with(b"ID")) {
                        p += 1;
                        continue;
                    }
                    self.configure(f)?;
                    if self.overlap >= self.frame / 2 {
                        self.overlap = self.overlap.saturating_sub(self.frame);
                        p += len;
                        continue;
                    }
                    let start = self.next.unwrap_or(0);
                    let w = self.writer.as_mut().ok_or_else(|| unsupported("no audio writer"))?;
                    w.push(start, 0, true, &self.buf[p + header..p + len])?;
                    self.next = Some(start + self.frame);
                    p += len;
                }
                None if self.buf.len() - p < 9 => break,
                None => p += 1,
            }
        }
        self.buf.drain(..p);
        Ok(())
    }

    /// Where the data so far ends, in 90 kHz ticks from the start of the file.
    fn end(&self) -> Option<u64> {
        self.next.filter(|_| self.rate > 0).map(|n| n * CLOCK as u64 / self.rate)
    }

    fn anchor(&mut self, t: u64) {
        let t = t * self.rate / CLOCK as u64;
        match self.next {
            Some(n) if n.abs_diff(t) <= self.frame * 3 / 2 => {}
            // Time can't run backwards: what comes next starts late instead, in the right place.
            Some(n) if t < n => self.overlap = n - t,
            _ => self.next = Some(t),
        }
    }

    fn configure(&mut self, f: Frame) -> io::Result<()> {
        let config = match f {
            Frame::Adts { profile, rate_index, channels, blocks, .. } => {
                if blocks != 1 || channels == 0 {
                    return Err(unsupported("unusual AAC framing"));
                }
                AudioConfig::Aac { profile, rate_index, channels }
            }
            Frame::Mp3 { rate, channels, .. } => AudioConfig::Mp3 { rate, channels },
        };
        if let Some(c) = self.config {
            return if c == config { Ok(()) } else { Err(unsupported("audio format changes mid-stream")) };
        }
        self.config = Some(config);
        let (rate, entry) = match config {
            AudioConfig::Aac { profile, rate_index, channels } => {
                let asc = (((profile as u16 + 1) << 11) | ((rate_index as u16) << 7) | ((channels as u16) << 3)).to_be_bytes();
                let rate = AAC_RATES[rate_index as usize];
                (rate, mp4a(rate, channels, 0x40, &asc))
            }
            AudioConfig::Mp3 { rate, channels } => (rate, mp4a(rate, channels, 0x6B, &[])),
        };
        self.rate = rate as u64;
        self.frame = match f {
            Frame::Mp3 { samples, .. } => samples as u64,
            _ => 1024,
        };
        self.writer = Some(Writer::create(self.out, Init { timescale: rate, audio: true, width: 0, height: 0, entry })?);
        if let Some(t) = self.pending.take() {
            self.anchor(t);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- video

/// Reads Exp-Golomb coded headers; past the end it reads zeros.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> u32 {
        let v = self.b.get(self.pos / 8).map_or(0, |x| (x >> (7 - self.pos % 8)) & 1);
        self.pos += 1;
        v as u32
    }
    fn bits(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |v, _| (v << 1) | self.bit())
    }
    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 {
            zeros += 1;
            if zeros > 31 {
                return 0;
            }
        }
        (1u32 << zeros) - 1 + self.bits(zeros)
    }
    fn se(&mut self) -> i32 {
        let k = self.ue() as i64;
        (if k % 2 == 0 { -(k / 2) } else { (k + 1) / 2 }) as i32
    }
}

/// A NAL unit with the emulation prevention bytes removed.
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

#[derive(Debug, Default, PartialEq)]
struct H264Sps {
    width: u32,
    height: u32,
    chroma: u32,
    depth_luma: u32,
    depth_chroma: u32,
}

fn parse_h264_sps(nal: &[u8]) -> Option<H264Sps> {
    let data = rbsp(nal.get(1..)?);
    let mut r = Bits { b: &data, pos: 0 };
    let profile = r.bits(8);
    r.skip(16);
    r.ue();
    let (mut chroma, mut depth_luma, mut depth_chroma) = (1, 0, 0);
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = r.ue();
        if chroma == 3 {
            r.skip(1);
        }
        depth_luma = r.ue();
        depth_chroma = r.ue();
        r.skip(1);
        if r.bit() == 1 {
            for i in 0..(if chroma == 3 { 12 } else { 8 }) {
                if r.bit() == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se() + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    r.ue();
    match r.ue() {
        0 => {
            r.ue();
        }
        1 => {
            r.skip(1);
            r.se();
            r.se();
            for _ in 0..r.ue().min(256) {
                r.se();
            }
        }
        _ => {}
    }
    r.ue();
    r.skip(1);
    let (w, h) = (r.ue(), r.ue());
    let frame_only = r.bit();
    if frame_only == 0 {
        r.skip(1);
    }
    r.skip(1);
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit() == 1 {
        (cl, cr, ct, cb) = (r.ue(), r.ue(), r.ue(), r.ue());
    }
    let (ux, uy) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let uy = uy * (2 - frame_only);
    let width = ((w + 1) * 16).checked_sub(ux * (cl + cr))?;
    let height = ((2 - frame_only) * (h + 1) * 16).checked_sub(uy * (ct + cb))?;
    (width > 0 && height > 0).then_some(H264Sps { width, height, chroma, depth_luma, depth_chroma })
}

#[derive(Debug, Default, PartialEq)]
struct HevcSps {
    width: u32,
    height: u32,
    chroma: u32,
    depth_luma: u32,
    depth_chroma: u32,
    /// The 12 bytes of general profile, tier and level.
    ptl: Vec<u8>,
    sub_layers: u8,
    nesting: u8,
}

fn parse_hevc_sps(nal: &[u8]) -> Option<HevcSps> {
    let data = rbsp(nal.get(2..)?);
    let mut r = Bits { b: &data, pos: 0 };
    r.skip(4);
    let max_sub = r.bits(3);
    let nesting = r.bit();
    let ptl = data.get(1..13)?.to_vec();
    r.skip(96);
    let flags: Vec<(u32, u32)> = (0..max_sub).map(|_| (r.bit(), r.bit())).collect();
    if max_sub > 0 {
        r.skip(2 * (8 - max_sub as usize));
    }
    for (profile, level) in flags {
        r.skip(if profile == 1 { 88 } else { 0 } + if level == 1 { 8 } else { 0 });
    }
    r.ue();
    let chroma = r.ue();
    if chroma == 3 {
        r.skip(1);
    }
    let (w, h) = (r.ue(), r.ue());
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit() == 1 {
        (cl, cr, ct, cb) = (r.ue(), r.ue(), r.ue(), r.ue());
    }
    let (ux, uy) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = w.checked_sub(ux * (cl + cr))?;
    let height = h.checked_sub(uy * (ct + cb))?;
    let (depth_luma, depth_chroma) = (r.ue(), r.ue());
    (width > 0 && height > 0).then_some(HevcSps { width, height, chroma, depth_luma, depth_chroma, ptl, sub_layers: max_sub as u8 + 1, nesting: nesting as u8 })
}

/// NAL units of an Annex B byte stream.
fn nal_units(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = vec![];
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut e = starts.get(n + 1).map_or(b.len(), |&next| next - 3);
        while e > s && b[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&b[s..e]);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum VideoCodec {
    Avc,
    Hevc,
}

struct Unit {
    dts: u64,
    cts: i32,
    data: Vec<u8>,
}

struct Video<'a> {
    out: &'a Path,
    codec: VideoCodec,
    writer: Option<Writer>,
    vps: Vec<Vec<u8>>,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    current: Option<Unit>,
    sample: Vec<u8>,
    /// The stream marks where pictures start, so only those marks count.
    delimited: bool,
}

/// Where a new picture starts in `data`, as an offset that goes below zero when its start code
/// began in `tail`, the last bytes before `data`. `None` if no picture starts here.
fn picture_start(codec: VideoCodec, delimited: bool, tail: &[u8], data: &[u8]) -> Option<isize> {
    let scan = [tail, data].concat();
    let mut first = None;
    for i in 0..scan.len().saturating_sub(3) {
        if scan[i] == 0 && scan[i + 1] == 0 && scan[i + 2] == 1 {
            let at = i as isize - tail.len() as isize;
            let marker = match codec {
                VideoCodec::Avc => scan[i + 3] & 0x1F == 9,
                VideoCodec::Hevc => (scan[i + 3] >> 1) & 0x3F == 35,
            };
            if marker {
                return Some(at);
            }
            first = first.or(Some(at));
        }
    }
    first.filter(|_| !delimited)
}

impl<'a> Video<'a> {
    fn new(out: &'a Path, codec: VideoCodec) -> Self {
        Self { out, codec, writer: None, vps: vec![], sps: vec![], pps: vec![], current: None, sample: vec![], delimited: false }
    }

    /// One PES packet. Without a timestamp it continues the current picture (encoders may
    /// split a large picture over several packets). A timestamp belongs to the first picture
    /// that starts in the packet, which need not be at its start: packets are often cut to a
    /// fixed size, so one can open with the end of the previous picture.
    fn feed(&mut self, time: Option<(u64, i32)>, data: &[u8]) -> io::Result<()> {
        let Some((dts, cts)) = time else {
            if let Some(u) = self.current.as_mut() {
                u.data.extend_from_slice(data);
            }
            return Ok(());
        };
        let Some(mut u) = self.current.take() else {
            self.current = Some(Unit { dts, cts, data: data.to_vec() });
            return Ok(());
        };
        let tail = u.data.len().saturating_sub(2);
        let start = (u.dts != dts).then(|| picture_start(self.codec, self.delimited, &u.data[tail..], data)).flatten();
        let Some(start) = start else {
            u.data.extend_from_slice(data);
            self.current = Some(u);
            return Ok(());
        };
        // Below zero: the start code itself was cut in two.
        let mut next = u.data.split_off(u.data.len() - (-start).max(0) as usize);
        let start = start.max(0) as usize;
        u.data.extend_from_slice(&data[..start]);
        next.extend_from_slice(&data[start..]);
        self.current = Some(u);
        self.finish_unit()?;
        self.current = Some(Unit { dts, cts, data: next });
        Ok(())
    }

    fn finish_unit(&mut self) -> io::Result<()> {
        let Some(u) = self.current.take() else { return Ok(()) };
        self.sample.clear();
        let (mut key, mut delimiters) = (false, 0);
        for nal in nal_units(&u.data) {
            let (kind, is_key, delimiter) = match self.codec {
                VideoCodec::Avc => {
                    let t = nal[0] & 0x1F;
                    (t, t == 5, t == 9)
                }
                VideoCodec::Hevc => {
                    let t = (nal[0] >> 1) & 0x3F;
                    (t, (16..=23).contains(&t), t == 35)
                }
            };
            let params = match (self.codec, kind) {
                (VideoCodec::Avc, 7) | (VideoCodec::Hevc, 33) => Some(&mut self.sps),
                (VideoCodec::Avc, 8) | (VideoCodec::Hevc, 34) => Some(&mut self.pps),
                (VideoCodec::Hevc, 32) => Some(&mut self.vps),
                _ => None,
            };
            if let Some(list) = params {
                if self.writer.is_none() && !list.iter().any(|p| p == nal) {
                    list.push(nal.to_vec());
                }
            }
            key |= is_key;
            delimiters += delimiter as u32;
            self.delimited |= delimiter;
            // The delimiter and filler mean nothing in MP4; parameter sets stay in the
            // stream so a splice with a different resolution still decodes.
            if delimiter || (self.codec == VideoCodec::Avc && kind == 12) {
                continue;
            }
            self.sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            self.sample.extend_from_slice(nal);
        }
        if delimiters > 1 {
            return Err(unsupported("several pictures in one PES packet"));
        }
        if self.sample.is_empty() {
            return Ok(());
        }
        if self.writer.is_none() {
            let Some((width, height, entry)) = self.sample_entry() else { return Ok(()) };
            self.writer = Some(Writer::create(self.out, Init { timescale: CLOCK as u32, audio: false, width, height, entry })?);
            key = true;
        }
        if let Some(w) = self.writer.as_mut() {
            w.push(u.dts, u.cts, key, &self.sample)?;
        }
        Ok(())
    }

    fn sample_entry(&self) -> Option<(u32, u32, Vec<u8>)> {
        match self.codec {
            VideoCodec::Avc => {
                let first = self.sps.first().filter(|_| !self.pps.is_empty())?;
                let s = parse_h264_sps(first)?;
                let mut c = vec![1, *first.get(1)?, *first.get(2)?, *first.get(3)?, 0xFF, 0xE0 | self.sps.len() as u8];
                for p in &self.sps {
                    c.extend_from_slice(&(p.len() as u16).to_be_bytes());
                    c.extend_from_slice(p);
                }
                c.push(self.pps.len() as u8);
                for p in &self.pps {
                    c.extend_from_slice(&(p.len() as u16).to_be_bytes());
                    c.extend_from_slice(p);
                }
                if ![66, 77, 88].contains(&first[1]) {
                    c.extend_from_slice(&[0xFC | s.chroma as u8, 0xF8 | s.depth_luma as u8, 0xF8 | s.depth_chroma as u8, 0]);
                }
                Some((s.width, s.height, visual(b"avc1", s.width, s.height, &bx(b"avcC", &[&c]))))
            }
            VideoCodec::Hevc => {
                let first = self.sps.first().filter(|_| !self.vps.is_empty() && !self.pps.is_empty())?;
                let s = parse_hevc_sps(first)?;
                let mut c = vec![1];
                c.extend_from_slice(&s.ptl);
                c.extend_from_slice(&[0xF0, 0, 0xFC, 0xFC | s.chroma as u8, 0xF8 | s.depth_luma as u8, 0xF8 | s.depth_chroma as u8, 0, 0]);
                c.push((s.sub_layers << 3) | (s.nesting << 2) | 3);
                c.push(3);
                for (kind, list) in [(32u8, &self.vps), (33, &self.sps), (34, &self.pps)] {
                    c.push(kind);
                    c.extend_from_slice(&(list.len() as u16).to_be_bytes());
                    for p in list {
                        c.extend_from_slice(&(p.len() as u16).to_be_bytes());
                        c.extend_from_slice(p);
                    }
                }
                Some((s.width, s.height, visual(b"hvc1", s.width, s.height, &bx(b"hvcC", &[&c]))))
            }
        }
    }
}

// ---------------------------------------------------------------- fragmented MP4 writer

fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let size = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut v = Vec::with_capacity(size);
    v.extend_from_slice(&(size as u32).to_be_bytes());
    v.extend_from_slice(kind);
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

fn visual(kind: &[u8; 4], width: u32, height: u32, config: &[u8]) -> Vec<u8> {
    let mut e = vec![0u8; 6];
    e.extend_from_slice(&[0, 1]);
    e.extend_from_slice(&[0; 16]);
    e.extend_from_slice(&(width as u16).to_be_bytes());
    e.extend_from_slice(&(height as u16).to_be_bytes());
    e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    e.extend_from_slice(&[0; 4]);
    e.extend_from_slice(&[0, 1]);
    e.extend_from_slice(&[0; 32]);
    e.extend_from_slice(&[0, 0x18, 0xFF, 0xFF]);
    bx(kind, &[&e, config])
}

/// An `mp4a` sample entry. `object`: 0x40 AAC, 0x6B MP3.
fn mp4a(rate: u32, channels: u8, object: u8, config: &[u8]) -> Vec<u8> {
    let mut dec = vec![object, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    if !config.is_empty() {
        dec.extend_from_slice(&[5, config.len() as u8]);
        dec.extend_from_slice(config);
    }
    let mut es = vec![0, 0, 0, 4, dec.len() as u8];
    es.extend_from_slice(&dec);
    es.extend_from_slice(&[6, 1, 2]);
    let mut desc = vec![0, 0, 0, 0, 3, es.len() as u8];
    desc.extend_from_slice(&es);
    let mut e = vec![0u8; 6];
    e.extend_from_slice(&[0, 1]);
    e.extend_from_slice(&[0; 8]);
    e.extend_from_slice(&(channels as u16).to_be_bytes());
    e.extend_from_slice(&[0, 16, 0, 0, 0, 0]);
    e.extend_from_slice(&((rate.min(65535) as u32) << 16).to_be_bytes());
    bx(b"mp4a", &[&e, &bx(b"esds", &[&desc])])
}

struct Init {
    timescale: u32,
    audio: bool,
    width: u32,
    height: u32,
    entry: Vec<u8>,
}

fn init_segment(i: &Init) -> (Vec<u8>, usize) {
    let ftyp = bx(b"ftyp", &[b"isom", &[0, 0, 2, 0], b"isom", b"iso6", b"mp41"]);
    let mut mvhd = vec![0u8; 12];
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&[0; 4]);
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd.extend_from_slice(&[1, 0]);
    mvhd.extend_from_slice(&[0; 10]);
    mvhd.extend_from_slice(&matrix());
    mvhd.extend_from_slice(&[0; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes());
    let mut tkhd = vec![0, 0, 0, 7];
    tkhd.extend_from_slice(&[0; 8]);
    tkhd.extend_from_slice(&1u32.to_be_bytes());
    tkhd.extend_from_slice(&[0; 16]);
    tkhd.extend_from_slice(&[0; 4]);
    tkhd.extend_from_slice(&[if i.audio { 1 } else { 0 }, 0, 0, 0]);
    tkhd.extend_from_slice(&matrix());
    tkhd.extend_from_slice(&(i.width << 16).to_be_bytes());
    tkhd.extend_from_slice(&(i.height << 16).to_be_bytes());
    let mut mdhd = vec![0u8; 12];
    mdhd.extend_from_slice(&i.timescale.to_be_bytes());
    mdhd.extend_from_slice(&[0; 4]);
    mdhd.extend_from_slice(&[0x55, 0xC4, 0, 0]);
    let handler: (&[u8; 4], &[u8]) = if i.audio { (b"soun", b"SoundHandler\0") } else { (b"vide", b"VideoHandler\0") };
    let hdlr = bx(b"hdlr", &[&[0; 8], handler.0, &[0; 12], handler.1]);
    let media_header = if i.audio { bx(b"smhd", &[&[0; 8]]) } else { bx(b"vmhd", &[&[0, 0, 0, 1], &[0; 8]]) };
    let url = bx(b"url ", &[&[0, 0, 0, 1]]);
    let dinf = bx(b"dinf", &[&bx(b"dref", &[&[0, 0, 0, 0, 0, 0, 0, 1], &url])]);
    let stbl = bx(
        b"stbl",
        &[
            &bx(b"stsd", &[&[0, 0, 0, 0, 0, 0, 0, 1], &i.entry]),
            &bx(b"stts", &[&[0; 8]]),
            &bx(b"stsc", &[&[0; 8]]),
            &bx(b"stsz", &[&[0; 12]]),
            &bx(b"stco", &[&[0; 8]]),
        ],
    );
    let minf = bx(b"minf", &[&media_header, &dinf, &stbl]);
    let mdia = bx(b"mdia", &[&bx(b"mdhd", &[&mdhd]), &hdlr, &minf]);
    let trak = bx(b"trak", &[&bx(b"tkhd", &[&tkhd]), &mdia]);
    let mehd = bx(b"mehd", &[&[1, 0, 0, 0], &[0; 8]]);
    let mut trex = vec![0, 0, 0, 0];
    trex.extend_from_slice(&1u32.to_be_bytes());
    trex.extend_from_slice(&1u32.to_be_bytes());
    trex.extend_from_slice(&[0; 12]);
    let mvex = bx(b"mvex", &[&mehd, &bx(b"trex", &[&trex])]);
    let mvhd = bx(b"mvhd", &[&mvhd]);
    let moov = bx(b"moov", &[&mvhd, &trak, &mvex]);
    // Where the movie duration goes once it is known.
    let mehd_value = ftyp.len() + 8 + mvhd.len() + trak.len() + 8 + 12;
    let mut out = ftyp;
    out.extend_from_slice(&moov);
    (out, mehd_value)
}

fn matrix() -> [u8; 36] {
    let mut m = [0u8; 36];
    m[1] = 1;
    m[17] = 1;
    m[32] = 0x40;
    m
}

struct Sample {
    time: u64,
    duration: u32,
    size: u32,
    cts: i32,
    key: bool,
}

/// One track of a fragmented MP4, written as samples arrive.
struct Writer {
    file: BufWriter<File>,
    timescale: u32,
    audio: bool,
    samples: Vec<Sample>,
    data: Vec<u8>,
    sequence: u32,
    last_duration: u32,
    mehd_value: u64,
    end: u64,
}

impl Writer {
    fn create(path: &Path, init: Init) -> io::Result<Self> {
        let (bytes, mehd_value) = init_segment(&init);
        let mut file = BufWriter::with_capacity(1 << 20, File::create(path)?);
        file.write_all(&bytes)?;
        Ok(Self { file, timescale: init.timescale, audio: init.audio, samples: vec![], data: vec![], sequence: 0, last_duration: 0, mehd_value: mehd_value as u64, end: 0 })
    }

    /// `time` in the track's timescale; `cts` is the display offset from it.
    fn push(&mut self, time: u64, cts: i32, key: bool, data: &[u8]) -> io::Result<()> {
        if let Some(prev) = self.samples.last_mut() {
            let d = time.saturating_sub(prev.time).clamp(1, u32::MAX as u64) as u32;
            prev.duration = d;
            self.last_duration = d;
        }
        let first = self.samples.first().map_or(time, |s| s.time);
        let long = time.saturating_sub(first) >= FRAGMENT_SECONDS * self.timescale as u64;
        if !self.samples.is_empty() && ((long && (key || self.audio)) || self.data.len() >= MAX_FRAGMENT_BYTES) {
            self.flush()?;
        }
        self.samples.push(Sample { time, duration: self.last_duration, size: data.len() as u32, cts, key });
        self.data.extend_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        let n = self.samples.len();
        if n == 0 {
            return Ok(());
        }
        self.sequence += 1;
        let mut trun = vec![1, 0, 0x0F, 0x01];
        trun.extend_from_slice(&(n as u32).to_be_bytes());
        let offset_at = trun.len();
        trun.extend_from_slice(&[0; 4]);
        for s in &self.samples {
            trun.extend_from_slice(&s.duration.max(1).to_be_bytes());
            trun.extend_from_slice(&s.size.to_be_bytes());
            trun.extend_from_slice(&(if s.key { 0x0200_0000u32 } else { 0x0101_0000u32 }).to_be_bytes());
            trun.extend_from_slice(&s.cts.to_be_bytes());
        }
        let mfhd = bx(b"mfhd", &[&[0; 4], &self.sequence.to_be_bytes()]);
        let tfhd = bx(b"tfhd", &[&[0, 2, 0, 0], &1u32.to_be_bytes()]);
        let tfdt = bx(b"tfdt", &[&[1, 0, 0, 0], &self.samples[0].time.to_be_bytes()]);
        let moof_size = 8 + mfhd.len() + 8 + tfhd.len() + tfdt.len() + 8 + trun.len();
        trun[offset_at..offset_at + 4].copy_from_slice(&(moof_size as u32 + 8).to_be_bytes());
        let traf = bx(b"traf", &[&tfhd, &tfdt, &bx(b"trun", &[&trun])]);
        self.file.write_all(&bx(b"moof", &[&mfhd, &traf]))?;
        self.file.write_all(&((self.data.len() + 8) as u32).to_be_bytes())?;
        self.file.write_all(b"mdat")?;
        self.file.write_all(&self.data)?;
        let last = &self.samples[n - 1];
        self.end = last.time + last.duration as u64;
        self.samples.clear();
        self.data.clear();
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        if let Some(last) = self.samples.last_mut() {
            last.duration = self.last_duration.max(1);
        }
        self.flush()?;
        let ms = self.end * 1000 / self.timescale.max(1) as u64;
        self.file.flush()?;
        let mut f = self.file.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(self.mehd_value))?;
        f.write_all(&ms.to_be_bytes())?;
        f.sync_all()
    }
}

// ---------------------------------------------------------------- transport stream

fn pes_time(b: &[u8]) -> u64 {
    (((b[0] as u64 >> 1) & 7) << 30) | ((b[1] as u64) << 22) | ((b[2] as u64 >> 1) << 15) | ((b[3] as u64) << 7) | (b[4] as u64 >> 1)
}

/// (pts, dts, payload) of a PES packet.
fn parse_pes(b: &[u8]) -> Option<(Option<u64>, Option<u64>, &[u8])> {
    if b.len() < 9 || b[..3] != [0, 0, 1] {
        return None;
    }
    let flags = b[7] >> 6;
    let body = 9 + b[8] as usize;
    let pts = (flags & 2 != 0).then(|| b.get(9..14).map(pes_time)).flatten();
    let dts = (flags == 3).then(|| b.get(14..19).map(pes_time)).flatten();
    Some((pts, dts, b.get(body..)?))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stream {
    Video(u16, u8),
    Audio(u16, u8),
}

/// One packetized elementary stream packet.
struct Pes<'a> {
    video: bool,
    codec: u8,
    pts: Option<u64>,
    dts: Option<u64>,
    data: &'a [u8],
}

fn is_audio_type(kind: u8) -> bool {
    matches!(kind, 0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x87 | 0x82 | 0x83 | 0x84 | 0x85)
}

/// The streams of a PMT section. Fails on what can't be carried over without losing it.
fn parse_pmt(b: &[u8]) -> io::Result<Vec<Stream>> {
    let mut out = vec![];
    let mut cues = false;
    if b.len() < 12 || b[0] != 2 {
        return Ok(out);
    }
    let end = (3 + (((b[1] as usize & 0xF) << 8) | b[2] as usize)).min(b.len()).saturating_sub(4);
    let mut p = 12 + (((b[10] as usize & 0xF) << 8) | b[11] as usize);
    while p + 5 <= end {
        let (kind, pid) = (b[p], ((b[p + 1] as u16 & 0x1F) << 8) | b[p + 2] as u16);
        let info = ((b[p + 3] as usize & 0xF) << 8) | b[p + 4] as usize;
        match kind {
            0x1B | 0x24 => out.push(Stream::Video(pid, kind)),
            0x0F | 0x03 | 0x04 => out.push(Stream::Audio(pid, kind)),
            0xDB | 0xCF | 0xC1 | 0xC2 => return Err(unsupported("encrypted stream")),
            k if is_audio_type(k) => return Err(unsupported(format!("audio type {k:#x}"))),
            // Ad cues in streams, a rare audio format elsewhere.
            0x86 => cues = true,
            // DVB marks AC-3 audio as private data with a descriptor.
            0x06 if b.get(p + 5..p + 5 + info).is_some_and(|d| {
                let mut i = 0;
                let mut found = false;
                while i + 2 <= d.len() {
                    found |= matches!(d[i], 0x6A | 0x7A);
                    i += 2 + d[i + 1] as usize;
                }
                found
            }) =>
            {
                return Err(unsupported("AC-3 audio"));
            }
            _ => {}
        }
        p += 5 + info;
    }
    if cues && !out.iter().any(|s| matches!(s, Stream::Audio(..))) {
        return Err(unsupported("audio type 0x86"));
    }
    Ok(out)
}

struct Demux {
    pmt_pid: Option<u16>,
    video: Option<(u16, u8)>,
    audio: Option<(u16, u8)>,
    open_video: Option<Vec<u8>>,
    open_audio: Option<Vec<u8>>,
    packets: u64,
}

/// Walk a transport stream's elementary packets until `sink` returns false.
fn demux(path: &Path, want_video: bool, want_audio: bool, read: &AtomicU64, mut sink: impl FnMut(Pes) -> io::Result<bool>) -> io::Result<()> {
    let mut f = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut st = Demux { pmt_pid: None, video: None, audio: None, open_video: None, open_audio: None, packets: 0 };
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 20];
    let mut eof = false;
    // Once in step, a packet counts even if junk follows it (the end of a disguised segment).
    let mut in_step = false;
    loop {
        while buf.len() < PACKET * 3 && !eof {
            let n = f.read(&mut chunk)?;
            read.fetch_add(n as u64, Ordering::Relaxed);
            eof = n == 0;
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut p = 0;
        while p + PACKET <= buf.len() {
            if !eof && p + PACKET * 2 > buf.len() {
                break;
            }
            let followed = p + PACKET < buf.len() && buf[p + PACKET] == 0x47;
            if buf[p] != 0x47 || !(in_step || followed || eof) {
                in_step = false;
                p += 1;
                continue;
            }
            in_step = followed;
            if !packet(&buf[p..p + PACKET], &mut st, want_video, want_audio, &mut sink)? {
                return Ok(());
            }
            p += PACKET;
        }
        buf.drain(..p);
        if eof && buf.len() < PACKET {
            break;
        }
        if eof && p == 0 {
            break;
        }
    }
    if st.packets == 0 {
        return Err(unsupported("no transport stream packets"));
    }
    flush_open(&mut st, true, want_video, want_audio, &mut sink)?;
    flush_open(&mut st, false, want_video, want_audio, &mut sink)?;
    Ok(())
}

fn flush_open(st: &mut Demux, video: bool, want_video: bool, want_audio: bool, sink: &mut impl FnMut(Pes) -> io::Result<bool>) -> io::Result<bool> {
    let (open, codec, wanted) = if video { (st.open_video.take(), st.video.map(|v| v.1), want_video) } else { (st.open_audio.take(), st.audio.map(|a| a.1), want_audio) };
    if let (Some(b), Some(codec), true) = (open, codec, wanted) {
        if let Some((pts, dts, data)) = parse_pes(&b) {
            return sink(Pes { video, codec, pts, dts, data });
        }
    }
    Ok(true)
}

fn packet(b: &[u8], st: &mut Demux, want_video: bool, want_audio: bool, sink: &mut impl FnMut(Pes) -> io::Result<bool>) -> io::Result<bool> {
    if b[1] & 0x80 != 0 {
        return Ok(true);
    }
    st.packets += 1;
    let start = b[1] & 0x40 != 0;
    let pid = ((b[1] as u16 & 0x1F) << 8) | b[2] as u16;
    let control = (b[3] >> 4) & 3;
    if control & 1 == 0 {
        return Ok(true);
    }
    let body = if control & 2 != 0 { 5 + b[4] as usize } else { 4 };
    if body >= PACKET {
        return Ok(true);
    }
    let payload = &b[body..];
    let section = || -> Option<&[u8]> { start.then(|| payload.get(1 + payload[0] as usize..)).flatten() };
    if pid == 0 {
        if let Some(s) = section() {
            let mut p = 8;
            while p + 4 <= s.len().min(3 + (((s[1] as usize & 0xF) << 8) | s[2] as usize)).saturating_sub(4) {
                let program = u16::from_be_bytes([s[p], s[p + 1]]);
                if program != 0 {
                    st.pmt_pid = Some(((s[p + 2] as u16 & 0x1F) << 8) | s[p + 3] as u16);
                    break;
                }
                p += 4;
            }
        }
        return Ok(true);
    }
    if Some(pid) == st.pmt_pid {
        if let Some(s) = section() {
            let streams = parse_pmt(s)?;
            let video = streams.iter().find_map(|x| if let Stream::Video(p, k) = x { Some((*p, *k)) } else { None });
            let audio = streams.iter().find_map(|x| if let Stream::Audio(p, k) = x { Some((*p, *k)) } else { None });
            // A splice may renumber the streams; finish what is open first.
            if video.is_some() && video != st.video {
                flush_open(st, true, want_video, want_audio, sink)?;
                if st.video.is_some_and(|v| Some(v.1) != video.map(|n| n.1)) {
                    return Err(unsupported("video format changes mid-stream"));
                }
                st.video = video;
            }
            if audio.is_some() && audio != st.audio {
                flush_open(st, false, want_video, want_audio, sink)?;
                st.audio = audio;
            }
        }
        return Ok(true);
    }
    let is_video = st.video.is_some_and(|v| v.0 == pid);
    let is_audio = st.audio.is_some_and(|a| a.0 == pid);
    if !(is_video || is_audio) {
        return Ok(true);
    }
    if b[3] >> 6 != 0 {
        return Err(unsupported("scrambled stream"));
    }
    if start && !flush_open(st, is_video, want_video, want_audio, sink)? {
        return Ok(false);
    }
    let open = if is_video { &mut st.open_video } else { &mut st.open_audio };
    if start {
        *open = Some(payload.to_vec());
    } else if let Some(o) = open.as_mut() {
        o.extend_from_slice(payload);
    }
    Ok(true)
}

/// The offset the tracks of one stream share. A join moves them by the same amount: moved one
/// by one, a break whose sound and picture differ in length would push them apart for good.
/// Each track still changes over at its own join, because packets from before a join can arrive
/// after the other track has crossed it.
#[derive(Default)]
struct Clock {
    shift: i64,
    /// Counts the joins so far.
    joins: u32,
}

/// Turns 33-bit stream timestamps into one straight timeline: wraps are unrolled and splices
/// closed up, so what comes after an ad break carries on from where the stream stopped.
#[derive(Default)]
struct Timeline {
    last_raw: Option<i64>,
    last: i64,
    step: i64,
    shift: i64,
    /// The last join this track has crossed.
    joins: u32,
}

impl Timeline {
    fn unwrap(&self, raw: u64) -> i64 {
        let raw = raw as i64;
        match self.last_raw {
            Some(l) => raw + ((l - raw + WRAP / 2).div_euclid(WRAP)) * WRAP,
            None => raw,
        }
    }

    /// Where this track's data ends so far, going by its stamps.
    fn end(&self) -> Option<i64> {
        self.last_raw.map(|_| self.last + self.step.max(1))
    }

    /// `end` is where the previous data really finished, for tracks that know: one audio
    /// packet can hold several seconds, so a stamp that far on is not a splice. `other` is
    /// where the stream's other track has got to: after a join both carry on from the later end.
    fn map(&mut self, clock: &mut Clock, raw: u64, end: Option<i64>, other: Option<i64>) -> i64 {
        let u = self.unwrap(raw);
        let Some(last_raw) = self.last_raw else {
            self.last_raw = Some(u);
            self.shift = clock.shift;
            self.joins = clock.joins;
            self.last = u + self.shift;
            return self.last;
        };
        let delta = u - last_raw;
        let span = end.map_or(0, |e| (e - self.last).max(0));
        if delta < 0 || delta - span > JUMP {
            let on = if span > 0 { span } else { self.step.max(1) };
            let own_end = self.last + on;
            // The other track may have crossed this same join already: follow it.
            let same_join = self.joins != clock.joins && (u + clock.shift - own_end).abs() <= 5 * JUMP;
            if !same_join {
                clock.shift = other.map_or(own_end, |o| o.max(own_end)) - u;
                clock.joins += 1;
            }
            self.shift = clock.shift;
            self.joins = clock.joins;
        } else if delta > 0 {
            self.step = delta;
        }
        self.last_raw = Some(u);
        self.last = u + self.shift;
        self.last
    }
}

/// When a stream's chosen tracks start, in 90 kHz ticks (for keeping separate files in sync).
pub fn start_time(path: &Path, video: bool, audio: bool) -> io::Result<Option<u64>> {
    match sniff(path)? {
        Some(Kind::Ts) => {
            let (mut first_v, mut first_a) = (None, None);
            let idle = AtomicU64::new(0);
            demux(path, video, audio, &idle, |p| {
                if p.video {
                    first_v = first_v.or(p.dts.or(p.pts));
                } else {
                    first_a = first_a.or(p.pts);
                }
                Ok(!((!video || first_v.is_some()) && (!audio || first_a.is_some())))
            })?;
            Ok([first_v, first_a].into_iter().flatten().min())
        }
        Some(_) if audio => {
            let mut head = vec![0u8; 4096];
            let n = File::open(path)?.read(&mut head)?;
            Ok(id3_time(&head[..n]))
        }
        _ => Ok(None),
    }
}

/// Convert `src` into separate fragmented MP4 tracks. A track is written only if its output
/// is given and the stream has it. `base` is the time that becomes zero, so two tracks that
/// come from different files stay in step. Returns which tracks were written.
pub fn to_fmp4(src: &Path, video_out: Option<&Path>, audio_out: Option<&Path>, base: u64, read: &AtomicU64) -> io::Result<(bool, bool)> {
    let kind = sniff(src)?.ok_or_else(|| unsupported("not a transport stream"))?;
    let mut audio = audio_out.map(Audio::new);
    if kind != Kind::Ts {
        let Some(a) = audio.as_mut() else { return Ok((false, false)) };
        let mut f = BufReader::with_capacity(1 << 20, File::open(src)?);
        let mut chunk = vec![0u8; 1 << 20];
        let mut start = None;
        let mut first = true;
        loop {
            let n = f.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            read.fetch_add(n as u64, Ordering::Relaxed);
            if first {
                start = id3_time(&chunk[..n.min(4096)]).map(|t| t.saturating_sub(base)).or(Some(0));
                first = false;
            }
            a.feed(start.take(), &chunk[..n])?;
        }
        return finish(None, audio);
    }

    let mut video: Option<Video> = None;
    let (mut vt, mut at) = (Timeline::default(), Timeline::default());
    let mut clock = Clock::default();
    let base = base as i64;
    demux(src, video_out.is_some(), audio_out.is_some(), read, |p| {
        if p.video {
            let Some(out) = video_out else { return Ok(true) };
            let v = video.get_or_insert_with(|| Video::new(out, if p.codec == 0x24 { VideoCodec::Hevc } else { VideoCodec::Avc }));
            let audio_end = audio.as_ref().and_then(Audio::end).map(|e| e as i64 + base);
            let time = match p.dts.or(p.pts) {
                Some(raw) => {
                    let dts = vt.map(&mut clock, raw, None, audio_end);
                    let cts = p.pts.map_or(0, |pts| (pts as i64 - raw as i64 + WRAP / 2).rem_euclid(WRAP) - WRAP / 2);
                    Some(((dts - base).max(0) as u64, cts as i32))
                }
                None => None,
            };
            v.feed(time, p.data)?;
        } else if let Some(a) = audio.as_mut() {
            if p.codec == 0x03 || p.codec == 0x04 || p.codec == 0x0F {
                let end = a.end().map(|e| e as i64 + base);
                let time = p.pts.map(|raw| (at.map(&mut clock, raw, end, vt.end()) - base).max(0) as u64);
                a.feed(time, p.data)?;
            }
        }
        Ok(true)
    })?;
    if let Some(v) = video.as_mut() {
        v.finish_unit()?;
    }
    finish(video, audio)
}

fn finish(video: Option<Video>, audio: Option<Audio>) -> io::Result<(bool, bool)> {
    let v = match video.and_then(|v| v.writer) {
        Some(w) => {
            w.finish()?;
            true
        }
        None => false,
    };
    let a = match audio.and_then(|a| a.writer) {
        Some(w) => {
            w.finish()?;
            true
        }
        None => false,
    };
    Ok((v, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small transport stream: PAT, PMT, then video and audio PES packets.
    fn stream(pictures: usize, frame_ticks: u64) -> Vec<u8> {
        fn psi(pid: u16, section: &[u8]) -> Vec<u8> {
            let mut p = vec![0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10, 0];
            p.extend_from_slice(section);
            p.resize(PACKET, 0xFF);
            p
        }
        fn pes(pid: u16, id: u8, pts: u64, data: &[u8]) -> Vec<u8> {
            let time = |t: u64| vec![0x21 | ((t >> 29) & 0xE) as u8, (t >> 22) as u8, 1 | ((t >> 14) & 0xFE) as u8, (t >> 7) as u8, 1 | ((t << 1) & 0xFE) as u8];
            let mut body = vec![0, 0, 1, id, 0, 0, 0x80, 0x80, 5];
            body.extend(time(pts));
            body.extend_from_slice(data);
            let mut out = vec![];
            for (n, c) in body.chunks(PACKET - 8).enumerate() {
                let mut p = vec![0x47, if n == 0 { 0x40 } else { 0 } | (pid >> 8) as u8, pid as u8, 0x30];
                let stuffing = PACKET - 5 - c.len();
                p.push(stuffing as u8);
                if stuffing > 0 {
                    p.push(0);
                    p.resize(5 + stuffing, 0xFF);
                }
                p.extend_from_slice(c);
                out.extend(p);
            }
            out
        }
        const SPS: [u8; 19] = [0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x2F, 0xF9, 0x7F, 0xF0, 0x0F, 0x00, 0x10, 0x60, 0x3C, 0x58, 0xB9, 0x20];
        let mut out = psi(0, &[0, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1, 0xE1, 0x00, 0, 0, 0, 0]);
        out.extend(psi(0x100, &[2, 0xB0, 23, 0, 1, 0xC1, 0, 0, 0xE1, 0x01, 0xF0, 0, 0x1B, 0xE1, 0x01, 0xF0, 0, 0x0F, 0xE1, 0x02, 0xF0, 0, 0, 0, 0, 0]));
        for i in 0..pictures as u64 {
            let mut au = vec![0, 0, 0, 1, 9, 0xF0];
            if i == 0 {
                au.extend_from_slice(&[0, 0, 0, 1]);
                au.extend_from_slice(&SPS);
                au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
            }
            au.extend_from_slice(&[0, 0, 0, 1, if i % 30 == 0 { 0x65 } else { 0x41 }, 0x88, i as u8]);
            out.extend(pes(0x101, 0xE0, 1000 + i * frame_ticks, &au));
            // 48 kHz stereo AAC frames, 16 bytes each.
            let adts = [0xFF, 0xF1, 0x4C, 0x80, 0x02, 0x1F, 0xFC, 1, 2, 3, 4, 5, 6, 7, 8, 9];
            out.extend(pes(0x102, 0xC0, 1000 + i * frame_ticks, &adts));
        }
        out
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rdm_ts_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn transport_stream_becomes_two_tracks_and_merges() {
        let d = temp("merge");
        let src = d.join("in.ts");
        // Three fragments' worth of video at 30 fps.
        std::fs::write(&src, stream(200, 3000)).unwrap();
        assert_eq!(sniff(&src).unwrap(), Some(Kind::Ts));
        assert_eq!(start_time(&src, true, true).unwrap(), Some(1000));
        let (v, a, out) = (d.join("v.mp4"), d.join("a.m4a"), d.join("out.mp4"));
        let read = AtomicU64::new(0);
        assert_eq!(to_fmp4(&src, Some(&v), Some(&a), 1000, &read).unwrap(), (true, true));
        assert_eq!(read.load(Ordering::Relaxed), std::fs::metadata(&src).unwrap().len());
        assert!(crate::mux::merge(&v, &a, &out).unwrap());
        let mut f = File::open(&out).unwrap();
        let boxes = crate::mux::mp4::scan(&mut f).unwrap();
        assert!(boxes.iter().filter(|b| &b.kind == b"moof").count() >= 4);
        let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
        let mut body = vec![0; moov.size as usize];
        f.seek(SeekFrom::Start(moov.start)).unwrap();
        f.read_exact(&mut body).unwrap();
        let has = |what: &[u8]| body.windows(what.len()).any(|w| w == what);
        assert!(has(b"avcC") && has(b"esds"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn junk_and_unknown_data_are_refused_and_packets_around_junk_survive() {
        let d = temp("junk");
        let good = stream(40, 3000);
        // Junk between packets, and right before the file ends: nothing is lost.
        let mut mixed = vec![0x89u8; 300];
        mixed.extend_from_slice(&good[..PACKET * 30]);
        mixed.extend_from_slice(&[0x11; 500]);
        mixed.extend_from_slice(&good[PACKET * 30..]);
        let (a, b) = (d.join("a.ts"), d.join("b.ts"));
        std::fs::write(&a, &good).unwrap();
        std::fs::write(&b, &mixed).unwrap();
        let (va, vb) = (d.join("a.mp4"), d.join("b.mp4"));
        let read = AtomicU64::new(0);
        to_fmp4(&a, Some(&va), None, 0, &read).unwrap();
        to_fmp4(&b, Some(&vb), None, 0, &read).unwrap();
        assert_eq!(std::fs::read(&va).unwrap(), std::fs::read(&vb).unwrap());

        let noise = d.join("noise.bin");
        std::fs::write(&noise, (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect::<Vec<_>>()).unwrap();
        assert_eq!(sniff(&noise).unwrap(), None);
        assert!(to_fmp4(&noise, Some(&va), None, 0, &read).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101
        let d = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = Bits { b: &d, pos: 0 };
        assert_eq!((r.ue(), r.ue(), r.ue(), r.ue(), r.ue()), (0, 1, 2, 3, 4));
        let d = [0b0100_1100, 0b1000_0000];
        let mut r = Bits { b: &d, pos: 0 };
        assert_eq!((r.se(), r.se()), (1, -1));
    }

    #[test]
    fn h264_sps_gives_the_picture_size() {
        // Baseline 640x360, then High 1920x1080 with cropping.
        let s = parse_h264_sps(&[0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x2F, 0xF9, 0x7F, 0xF0, 0x0F, 0x00, 0x10, 0x60, 0x3C, 0x58, 0xB9, 0x20]).unwrap();
        assert_eq!((s.width, s.height), (640, 360));
        let s = parse_h264_sps(&[0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0x84, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58]).unwrap();
        assert_eq!((s.width, s.height), (1920, 1080));
    }

    #[test]
    fn nal_units_split_on_both_start_codes() {
        let b = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        assert_eq!(nal_units(&b), vec![&[0x67u8, 1, 2][..], &[0x68, 3], &[0x65, 4]]);
    }

    #[test]
    fn a_picture_can_start_part_way_through_a_packet() {
        let avc = VideoCodec::Avc;
        // The end of the last picture, then a delimiter: only the delimiter starts one.
        assert_eq!(picture_start(avc, true, &[7, 8], &[0xE4, 0x1D, 0, 0, 1, 9, 0x10]), Some(2));
        // Without one, other units of the same picture can't be told apart from a new one.
        assert_eq!(picture_start(avc, false, &[7, 8], &[0xE4, 0, 0, 1, 0x65, 3]), Some(1));
        assert_eq!(picture_start(avc, true, &[7, 8], &[0xE4, 0, 0, 1, 0x65, 3]), None);
        assert_eq!(picture_start(avc, false, &[7, 8], &[0xE4, 0x1D]), None);
        // The start code itself was cut in two.
        assert_eq!(picture_start(avc, true, &[0, 0], &[1, 9, 0x10]), Some(-2));
        assert_eq!(picture_start(avc, true, &[7, 0], &[0, 1, 9, 0x10]), Some(-1));
        assert_eq!(picture_start(VideoCodec::Hevc, true, &[], &[0, 0, 1, 35 << 1, 1]), Some(0));
    }

    #[test]
    fn timeline_unrolls_wraps_and_closes_splices() {
        let (mut t, mut c) = (Timeline::default(), Clock::default());
        assert_eq!(t.map(&mut c, WRAP as u64 - 3000, None, None), WRAP - 3000);
        assert_eq!(t.map(&mut c, WRAP as u64 - 1500, None, None), WRAP - 1500);
        // The 33-bit counter wraps: time keeps going.
        assert_eq!(t.map(&mut c, 0, None, None), WRAP);
        assert_eq!(t.map(&mut c, 1500, None, None), WRAP + 1500);
        // A splice jumps far away: the track carries on one frame later.
        assert_eq!(t.map(&mut c, 50_000_000, None, None), WRAP + 3000);
        assert_eq!(t.map(&mut c, 50_001_500, None, None), WRAP + 4500);
    }

    #[test]
    fn both_tracks_cross_a_join_together() {
        let (mut video, mut audio, mut clock) = (Timeline::default(), Timeline::default(), Clock::default());
        assert_eq!(video.map(&mut clock, 0, None, None), 0);
        assert_eq!(video.map(&mut clock, 3000, None, None), 3000);
        assert_eq!(audio.map(&mut clock, 0, Some(0), Some(6000)), 0);
        // The break's sound ended 9000 ticks after its picture: the picture waits for it.
        assert_eq!(video.map(&mut clock, 50_000_000, None, Some(15_000)), 15_000);
        // The sound reaches the join second, with its usual offset, and stays beside the picture.
        assert_eq!(audio.map(&mut clock, 50_000_900, Some(15_000), Some(18_000)), 15_900);
        assert_eq!(video.map(&mut clock, 50_003_000, None, Some(16_000)), 18_000);
        // A packet from before a join can come after the other track crossed it.
        let (mut v, mut a, mut c) = (Timeline::default(), Timeline::default(), Clock::default());
        v.map(&mut c, 0, None, None);
        a.map(&mut c, 0, Some(0), Some(3000));
        v.map(&mut c, 90_000, None, Some(45_000));
        assert_eq!(v.map(&mut c, 50_000_000, None, Some(60_000)), 180_000);
        assert_eq!(a.map(&mut c, 45_000, Some(45_000), Some(183_000)), 45_000);
        assert_eq!(a.map(&mut c, 49_999_000, Some(90_000), Some(183_000)), 179_000);
        // The other way round: the sound arrives first.
        assert_eq!(audio.map(&mut clock, 90_000_000, Some(20_000), Some(21_000)), 21_000);
        assert_eq!(video.map(&mut clock, 90_000_100, None, Some(22_000)), 21_100);
    }

    #[test]
    fn a_long_audio_packet_is_not_a_splice() {
        let (mut t, mut c) = (Timeline::default(), Clock::default());
        assert_eq!(t.map(&mut c, 90_000, None, None), 90_000);
        // Each packet holds 6.5 s of audio, well past the splice limit.
        assert_eq!(t.map(&mut c, 675_000, Some(675_000), None), 675_000);
        assert_eq!(t.map(&mut c, 1_260_000, Some(1_260_000), None), 1_260_000);
        // A real splice still closes up, right where the audio stopped.
        assert_eq!(t.map(&mut c, 50_000_000, Some(1_845_000), None), 1_845_000);
        assert_eq!(t.map(&mut c, 50_001_920, Some(1_846_920), None), 1_846_920);
    }

    #[test]
    fn adts_and_mp3_frames() {
        let adts = [0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC];
        assert!(matches!(frame_at(&adts, 0), Some(Frame::Adts { len: 16, header: 7, profile: 1, rate_index: 4, channels: 2, blocks: 1 })));
        // MPEG-1 layer III, 128 kbit/s, 44.1 kHz, no padding.
        let mp3 = [0xFF, 0xFB, 0x90, 0x00, 0, 0, 0];
        assert!(matches!(frame_at(&mp3, 0), Some(Frame::Mp3 { len: 417, rate: 44100, channels: 2, samples: 1152 })));
        assert_eq!(frame_at(&[0x47, 0, 0, 0, 0, 0, 0], 0), None);
    }

    #[test]
    fn id3_tags_are_skipped_and_their_time_is_read() {
        let owner = b"com.apple.streaming.transportStreamTimestamp\0";
        let mut frame = b"PRIV".to_vec();
        frame.extend_from_slice(&[0, 0, 0, (owner.len() + 8) as u8, 0, 0]);
        frame.extend_from_slice(owner);
        frame.extend_from_slice(&90_000u64.to_be_bytes());
        let mut tag = b"ID3\x04\0\0\0\0\0".to_vec();
        tag.push(frame.len() as u8);
        tag.extend_from_slice(&frame);
        assert_eq!(skip_id3(&tag), tag.len());
        assert_eq!(id3_time(&tag), Some(90_000));
    }
}

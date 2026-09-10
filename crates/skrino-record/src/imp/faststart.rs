//! MP4 compatibility repack for the finished recording.
//!
//! The Media Foundation MP4 sink produces files that lenient local players
//! accept but strict streaming players (Telegram inline preview and friends)
//! reject. Three problems are fixed here in one streaming rewrite:
//!
//! 1. **`moov` after `mdat`.** The MF sink writes `ftyp, uuid, mdat, moov`;
//!    progressive players need the sample tables (`moov`) before the bulk
//!    (`mdat`) so playback can start before the whole file is downloaded.
//!    `windows-capture` does not expose the MF "fast start" attribute, so the
//!    box order is rewritten here once the encoder has fully flushed the file.
//! 2. **Empty audio track.** `windows-capture` 2.x registers a video AND an
//!    audio stream with its `MediaStreamSource` unconditionally; with audio
//!    disabled it just answers every audio sample request with `None`, so the
//!    finalized mp4 still carries a fully formed but zero-sample audio track
//!    (`stsz`/`stco` empty, `mdhd` duration 0). Strict clients (Telegram on
//!    mobile, AVFoundation-based players) refuse to play such files at all.
//!    Zero-sample tracks are removed during the rewrite; at least one track is
//!    always kept, and tracks with any samples are never touched.
//! 3. **Exotic `ftyp` brands.** MF writes major brand `mp42` with compatible
//!    brands `mp41, isom`. Canonical ffmpeg-style branding (`isom` major,
//!    compatible `isom, iso2, avc1, mp41`) is what every streaming player is
//!    tested against, so the `ftyp` is replaced with that block.
//!
//! Chunk offset tables (`stco`/`co64`) are re-pointed at the relocated `mdat`.
//! The delta can be positive (moov moved in front of mdat) or negative (moov
//! shrank because an empty track was dropped) or zero; it is applied as a
//! wrapping add, so both directions work.
//!
//! This module is intentionally pure byte/IO manipulation with no OS calls, so
//! it compiles and unit-tests on any platform. The transform is:
//!
//! 1. Parse TOP-LEVEL boxes only. A box header is a 4-byte big-endian size + a
//!    4-byte type. `size == 1` means the real size is the following 8-byte
//!    big-endian largesize; `size == 0` means "to end of file" (typically the
//!    `mdat`). All three forms are handled.
//! 2. Read the (small, ~KBs) `moov` fully into memory and drop empty tracks
//!    from it.
//! 3. Walk ONLY the container chain `moov -> trak -> mdia -> minf -> stbl`,
//!    adding the net `mdat` shift to every 32-bit `stco` / 64-bit `co64` chunk
//!    offset. A proper recursive box walk is used, never an ASCII scan for
//!    "stco"/"co64" (which could match payload bytes).
//! 4. Stream the result into a sibling temp file (`ftyp` patched or copied at
//!    its original position, the other leading boxes, the patched `moov` at its
//!    original position if it already preceded `mdat`, otherwise right before
//!    `mdat`, then `mdat` streamed in chunks, then any boxes that followed
//!    `mdat` except the original trailing `moov`) and atomically replace the
//!    original.
//!
//! When nothing needs fixing (already faststart, canonical `ftyp`, no empty
//! tracks) the file is left untouched (idempotent). Any error or unexpected
//! layout is a no-op: the original file is left intact and the caller still
//! returns a playable path. Failure here must never fail the recording or
//! corrupt the file.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Chunk size for streaming `mdat` from the original file into the rewritten one.
/// `mdat` can be hundreds of MB, so it is never loaded fully into memory.
const COPY_CHUNK: usize = 1024 * 1024;

/// The `ftyp` block every mainstream muxer writes and every streaming player is
/// tested against: major brand `isom`, minor version 0x200, compatible brands
/// `isom`, `iso2`, `avc1`, `mp41` (the Media Foundation sink instead writes a
/// 24-byte `mp42`-major block).
const CANONICAL_FTYP: [u8; 32] = *b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00isomiso2avc1mp41";

/// `ftyp` sizes that may be replaced by [`CANONICAL_FTYP`]: the 24-byte Media
/// Foundation block and a 32-byte block with non-canonical content. Anything
/// else is left alone (conservative).
const PATCHABLE_FTYP_SIZES: [u64; 2] = [24, 32];

/// Container boxes whose children we descend into while hunting for chunk offset
/// tables. Anything else (e.g. `stsd`, `dinf`, `edts`) is left untouched: it
/// never contains an `stco`/`co64` that points into `mdat`.
fn is_container(box_type: &[u8; 4]) -> bool {
    matches!(box_type, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl")
}

/// One box header, parsed.
#[derive(Clone, Copy)]
struct BoxEntry {
    box_type: [u8; 4],
    /// Byte offset of the box header (its size field) in the file/buffer.
    offset: u64,
    /// Header length: 8 for a 32-bit size, 16 for a 64-bit largesize.
    header_len: u64,
    /// Full box length including the header.
    total_len: u64,
}

/// Parse the box header at `pos`. Returns `None` for any malformed or
/// out-of-bounds header.
fn parse_box_header(buf: &[u8], pos: usize, end: usize) -> Option<BoxEntry> {
    if pos + 8 > end {
        return None;
    }
    let size32 = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as u64;
    let box_type = [buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]];
    let (header_len, total_len) = if size32 == 1 {
        if pos + 16 > end {
            return None;
        }
        let large = u64::from_be_bytes([
            buf[pos + 8],
            buf[pos + 9],
            buf[pos + 10],
            buf[pos + 11],
            buf[pos + 12],
            buf[pos + 13],
            buf[pos + 14],
            buf[pos + 15],
        ]);
        (16u64, large)
    } else if size32 == 0 {
        // "To end of buffer" (top-level mdat form); meaningless for children, so
        // only accepted by the top-level reader which bounds `end` by the file.
        (8u64, end.saturating_sub(pos) as u64)
    } else {
        (8u64, size32)
    };
    if total_len < header_len || pos as u64 + total_len > end as u64 {
        return None;
    }
    Some(BoxEntry {
        box_type,
        offset: pos as u64,
        header_len,
        total_len,
    })
}

/// One direct child of a container box, as a byte range covering the whole
/// child box (header included).
struct ChildBox {
    box_type: [u8; 4],
    start: usize,
    end: usize,
}

/// Parse the direct children of the container box occupying `buf[start..end]`.
/// `start` must point at the container's own header; children begin right after
/// it (`moov`/`trak`/`mdia`/`minf`/`stbl` are plain boxes with no version/flags
/// word). Returns `None` on any malformed child.
fn child_boxes(buf: &[u8], start: usize, end: usize) -> Option<Vec<ChildBox>> {
    let header = parse_box_header(buf, start, end)?;
    let mut pos = start + header.header_len as usize;
    let mut children = Vec::new();
    while pos + 8 <= end {
        let c = parse_box_header(buf, pos, end)?;
        let cend = pos + c.total_len as usize;
        children.push(ChildBox {
            box_type: c.box_type,
            start: pos,
            end: cend,
        });
        pos = cend;
    }
    if pos != end {
        return None;
    }
    Some(children)
}

/// Count the media samples and chunks declared by the track occupying
/// `buf[start..end]` (a `trak` box range): `stsz`/`stz2` sample count and
/// `stco`/`co64` entry count inside its `stbl`. Returns `None` when the sample
/// tables are missing or malformed, i.e. emptiness cannot be proven.
fn trak_media_counts(buf: &[u8], start: usize, end: usize) -> Option<(u64, u64)> {
    let mut samples: Option<u64> = None;
    let mut chunks: Option<u64> = None;

    fn walk(
        buf: &[u8],
        start: usize,
        end: usize,
        samples: &mut Option<u64>,
        chunks: &mut Option<u64>,
    ) {
        let Some(children) = child_boxes(buf, start, end) else {
            return;
        };
        for c in children {
            match &c.box_type {
                b"trak" | b"mdia" | b"minf" | b"stbl" => {
                    walk(buf, c.start, c.end, samples, chunks);
                }
                b"stsz" | b"stz2" => {
                    // stsz: version/flags(4), sample_size(4), sample_count(4).
                    // stz2: version/flags(4), reserved(3), field_size(1), sample_count(4).
                    // Body starts at box_start+8, so the count sits at +16.
                    if c.end - c.start >= 20 {
                        *samples = Some(u32::from_be_bytes(
                            buf[c.start + 16..c.start + 20].try_into().unwrap(),
                        ) as u64);
                    }
                }
                b"stco" | b"co64" => {
                    // version/flags(4), entry_count(4): the count sits at
                    // box_start+8+4 = +12.
                    if c.end - c.start >= 16 {
                        *chunks = Some(u32::from_be_bytes(
                            buf[c.start + 12..c.start + 16].try_into().unwrap(),
                        ) as u64);
                    }
                }
                _ => {}
            }
        }
    }

    walk(buf, start, end, &mut samples, &mut chunks);
    Some((samples?, chunks?))
}

/// Remove zero-sample tracks (empty audio written by the MF sink when recording
/// audio was disabled) from the `moov` bytes. Returns the number of bytes
/// removed, or 0 when nothing was dropped (no empty tracks, or dropping would
/// leave the movie without any track, or the box structure is not provably
/// intact). Kept children keep their order; the outer `moov` size is fixed up.
fn strip_empty_traks(moov: &mut Vec<u8>) -> u64 {
    let original_len = moov.len() as u64;
    let Some(children) = child_boxes(moov, 0, moov.len()) else {
        return 0;
    };

    let mut empty: Vec<(usize, usize)> = Vec::new();
    let mut trak_total = 0usize;
    for c in &children {
        if c.box_type != *b"trak" {
            continue;
        }
        trak_total += 1;
        if trak_media_counts(moov, c.start, c.end) == Some((0, 0)) {
            empty.push((c.start, c.end));
        }
    }
    // A movie must keep at least one track; if every track is empty the file is
    // broken in a way this pass should not try to fix.
    if trak_total == 0 || empty.is_empty() || empty.len() == trak_total {
        return 0;
    }

    // A moov box is a plain box: [header][children...]. Rebuild from the kept
    // children, preserving order.
    let Some(header) = parse_box_header(moov, 0, moov.len()) else {
        return 0;
    };
    let mut out = Vec::with_capacity(moov.len());
    out.extend_from_slice(&moov[..header.header_len as usize]);
    for c in &children {
        if empty.contains(&(c.start, c.end)) {
            continue;
        }
        out.extend_from_slice(&moov[c.start..c.end]);
    }
    let new_len = out.len() as u64;
    if header.header_len == 8 {
        out[0..4].copy_from_slice(&(new_len as u32).to_be_bytes());
    } else {
        out[0..4].copy_from_slice(&1u32.to_be_bytes());
        out[8..16].copy_from_slice(&new_len.to_be_bytes());
    }
    let removed = original_len - new_len;
    *moov = out;
    removed
}

/// Outcome of the on-disk transform, used only for logging.
enum Outcome {
    Rewritten,
    AlreadyFaststart,
    Skipped(&'static str),
}

/// Result of planning the transform against a source stream.
enum PlanResult {
    Rewrite(RewritePlan),
    AlreadyFaststart,
    Skipped(&'static str),
}

/// Everything needed to stream the rewritten file: the original top-level box
/// map, the replacement `ftyp` (if branding needed fixing), and the fully
/// patched `moov` bytes (empty tracks dropped, chunk offsets re-pointed).
struct RewritePlan {
    boxes: Vec<BoxEntry>,
    moov_idx: usize,
    mdat_idx: usize,
    patched_ftyp: Option<Vec<u8>>,
    patched_moov: Vec<u8>,
}

/// Public entry: rewrite `path` in place to the streaming-friendly layout,
/// logging (in Russian) on skip/failure and never returning an error. A skip or
/// failure leaves the original file byte-for-byte intact.
pub(crate) fn make_faststart(path: &Path) {
    match faststart_file(path) {
        Ok(Outcome::Rewritten) => {
            log::debug!(
                "skrino-record: mp4 переупакован для потокового воспроизведения (moov перед mdat, пустые дорожки убраны)"
            );
        }
        Ok(Outcome::AlreadyFaststart) => {}
        Ok(Outcome::Skipped(reason)) => {
            log::warn!(
                "skrino-record: потоковая переупаковка mp4 пропущена ({reason}), файл записи оставлен без изменений"
            );
        }
        Err(e) => {
            log::warn!(
                "skrino-record: не удалось переупаковать mp4 для потокового воспроизведения ({e}), файл записи оставлен без изменений"
            );
        }
    }
}

/// Rewrite `path` to the streaming layout. Streams `mdat` and replaces the
/// original atomically. Returns the outcome; on any IO error the original is
/// untouched (the temp file, if any, is removed).
fn faststart_file(path: &Path) -> io::Result<Outcome> {
    let mut src = File::open(path)?;
    let file_len = src.metadata()?.len();

    let plan = match plan_faststart(&mut src, file_len)? {
        PlanResult::Rewrite(plan) => plan,
        PlanResult::AlreadyFaststart => return Ok(Outcome::AlreadyFaststart),
        PlanResult::Skipped(reason) => return Ok(Outcome::Skipped(reason)),
    };

    let tmp = tmp_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut out = BufWriter::new(File::create(&tmp)?);
        write_plan(&mut src, &mut out, &plan)?;
        out.flush()
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // The finished bytes are self-consistent; only now swap them over the
    // original. Drop the source handle first so Windows can replace the file.
    drop(src);
    if let Err(e) = replace_file(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(Outcome::Rewritten)
}

/// Parse top-level boxes from `src`, decide whether a rewrite is needed, and if
/// so produce the patched `ftyp` and `moov` bytes. Reads only headers plus the
/// small `moov`; never touches `mdat`.
fn plan_faststart<R: Read + Seek>(src: &mut R, file_len: u64) -> io::Result<PlanResult> {
    let boxes = match read_top_boxes(src, file_len)? {
        Some(boxes) => boxes,
        None => return Ok(PlanResult::Skipped("структура mp4 не распознана")),
    };

    let moov_idx = boxes.iter().position(|b| &b.box_type == b"moov");
    let mdat_idx = boxes.iter().position(|b| &b.box_type == b"mdat");
    let (moov_idx, mdat_idx) = match (moov_idx, mdat_idx) {
        (Some(m), Some(d)) => (m, d),
        _ => return Ok(PlanResult::Skipped("в mp4 нет moov или mdat")),
    };

    // Brand fix: replace a non-canonical first `ftyp` of a known size with the
    // canonical ffmpeg-style block.
    let mut patched_ftyp: Option<Vec<u8>> = None;
    if let Some(first) = boxes.first()
        && first.box_type == *b"ftyp"
        && PATCHABLE_FTYP_SIZES.contains(&first.total_len)
    {
        src.seek(SeekFrom::Start(first.offset))?;
        let mut cur = vec![0u8; first.total_len as usize];
        src.read_exact(&mut cur)?;
        if cur != CANONICAL_FTYP {
            patched_ftyp = Some(CANONICAL_FTYP.to_vec());
        }
    }

    // Read and patch the moov: drop empty tracks first, then re-point chunk
    // offsets by the net mdat shift (new bytes before mdat minus old).
    let moov = boxes[moov_idx];
    src.seek(SeekFrom::Start(moov.offset))?;
    let mut patched_moov = vec![0u8; moov.total_len as usize];
    src.read_exact(&mut patched_moov)?;
    let removed = strip_empty_traks(&mut patched_moov);

    let ftyp_delta: i64 = match &patched_ftyp {
        Some(v) => v.len() as i64 - boxes[0].total_len as i64,
        None => 0,
    };
    let moov_moves_front = moov_idx > mdat_idx;
    // Net mdat shift. With moov moving in front of mdat, the inserted bytes are
    // the (possibly shrunk) moov. With moov already leading the file, only its
    // shrinkage matters. The dropped-track byte count is already reflected in
    // the shrunk moov length, so it is never subtracted separately.
    let delta: i64 = if moov_moves_front {
        patched_moov.len() as i64
    } else {
        patched_moov.len() as i64 - moov.total_len as i64
    } + ftyp_delta;

    let needs_rewrite = moov_moves_front || patched_ftyp.is_some() || removed > 0;
    if !needs_rewrite {
        return Ok(PlanResult::AlreadyFaststart);
    }

    let moov_len = patched_moov.len();
    if !patch_boxes(&mut patched_moov, moov.header_len as usize, moov_len, delta) {
        return Ok(PlanResult::Skipped("не удалось обновить таблицы смещений mdat"));
    }

    Ok(PlanResult::Rewrite(RewritePlan {
        boxes,
        moov_idx,
        mdat_idx,
        patched_ftyp,
        patched_moov,
    }))
}

/// Stream the new layout to `out`: the replacement `ftyp` first (when branded),
/// the remaining leading boxes in original order, the patched `moov` at its
/// original position when it already preceded `mdat` (otherwise right before
/// `mdat`), then `mdat`, then boxes after `mdat` except the original trailing
/// `moov`. `mdat` is streamed in [`COPY_CHUNK`] slices.
fn write_plan<R: Read + Seek, W: Write>(
    src: &mut R,
    out: &mut W,
    plan: &RewritePlan,
) -> io::Result<()> {
    for (i, b) in plan.boxes.iter().enumerate() {
        if i == 0 && b.box_type == *b"ftyp" {
            match &plan.patched_ftyp {
                Some(ftyp) => out.write_all(ftyp)?,
                None => copy_range(src, out, b.offset, b.total_len)?,
            }
        } else if i == plan.moov_idx {
            if plan.moov_idx < plan.mdat_idx {
                // moov already led the file; it stays in place, patched.
                out.write_all(&plan.patched_moov)?;
            }
            // Otherwise it is emitted right before mdat below; skip its
            // original tail position.
        } else if i == plan.mdat_idx {
            if plan.moov_idx > plan.mdat_idx {
                out.write_all(&plan.patched_moov)?;
            }
            copy_range(src, out, b.offset, b.total_len)?;
        } else {
            copy_range(src, out, b.offset, b.total_len)?;
        }
    }
    Ok(())
}

/// Parse every top-level box. Returns `None` (skip the transform) on any
/// malformed header or if the boxes do not tile the file exactly, so a
/// truncated or padded file is never rewritten.
fn read_top_boxes<R: Read + Seek>(src: &mut R, file_len: u64) -> io::Result<Option<Vec<BoxEntry>>> {
    let mut boxes = Vec::new();
    let mut offset = 0u64;
    while offset + 8 <= file_len {
        src.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 8];
        src.read_exact(&mut header)?;
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let box_type = [header[4], header[5], header[6], header[7]];

        let (header_len, total_len) = if size32 == 1 {
            if offset + 16 > file_len {
                return Ok(None);
            }
            let mut ext = [0u8; 8];
            src.read_exact(&mut ext)?;
            (16u64, u64::from_be_bytes(ext))
        } else if size32 == 0 {
            // Extends to end of file (typically mdat as the final box).
            (8u64, file_len - offset)
        } else {
            (8u64, size32)
        };

        if total_len < header_len || offset + total_len > file_len {
            return Ok(None);
        }
        boxes.push(BoxEntry {
            box_type,
            offset,
            header_len,
            total_len,
        });
        offset += total_len;
    }

    // Require exact coverage: leftover trailing bytes we do not model must not be
    // silently dropped by a rewrite.
    if offset != file_len {
        return Ok(None);
    }
    Ok(Some(boxes))
}

/// Recursively walk the boxes contained in `buf[start..end]`, descending into
/// the known container chain and adding `delta` (which may be negative) to
/// every `stco`/`co64` chunk offset found. Returns `false` on any malformed
/// child box (caller then skips the transform, leaving the file intact). Every
/// slice access is bounds-checked against `end`, so malformed input can never
/// panic.
fn patch_boxes(buf: &mut [u8], start: usize, end: usize, delta: i64) -> bool {
    let mut pos = start;
    while pos + 8 <= end {
        let Some(entry) = parse_box_header(buf, pos, end) else {
            return false;
        };
        let total_len = entry.total_len as usize;
        let box_type = entry.box_type;
        let payload_start = pos + entry.header_len as usize;
        let payload_end = pos + total_len;

        if is_container(&box_type) {
            if !patch_boxes(buf, payload_start, payload_end, delta) {
                return false;
            }
        } else if &box_type == b"stco" {
            if !patch_stco(buf, payload_start, payload_end, delta) {
                return false;
            }
        } else if &box_type == b"co64" && !patch_co64(buf, payload_start, payload_end, delta) {
            return false;
        }

        pos += total_len;
    }
    true
}

/// Patch an `stco` box body (`buf[start..end]`): 4 bytes version/flags, a u32
/// entry count, then that many 32-bit chunk offsets. Each offset points into
/// `mdat` and is shifted by `delta` (wrapping, so a negative delta works; only
/// a >4 GiB file would use `co64` where the full 64-bit wrap applies).
fn patch_stco(buf: &mut [u8], start: usize, end: usize, delta: i64) -> bool {
    if start + 8 > end {
        return false;
    }
    let count = u32::from_be_bytes([buf[start + 4], buf[start + 5], buf[start + 6], buf[start + 7]])
        as usize;
    let entries_start = start + 8;
    let needed = match count.checked_mul(4).and_then(|n| entries_start.checked_add(n)) {
        Some(n) => n,
        None => return false,
    };
    if needed > end {
        return false;
    }
    let delta32 = delta as u32;
    for i in 0..count {
        let p = entries_start + i * 4;
        let v = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
        buf[p..p + 4].copy_from_slice(&v.wrapping_add(delta32).to_be_bytes());
    }
    true
}

/// Patch a `co64` box body: like `stco` but with 64-bit offsets.
fn patch_co64(buf: &mut [u8], start: usize, end: usize, delta: i64) -> bool {
    if start + 8 > end {
        return false;
    }
    let count = u32::from_be_bytes([buf[start + 4], buf[start + 5], buf[start + 6], buf[start + 7]])
        as usize;
    let entries_start = start + 8;
    let needed = match count.checked_mul(8).and_then(|n| entries_start.checked_add(n)) {
        Some(n) => n,
        None => return false,
    };
    if needed > end {
        return false;
    }
    for i in 0..count {
        let p = entries_start + i * 8;
        let v = u64::from_be_bytes([
            buf[p],
            buf[p + 1],
            buf[p + 2],
            buf[p + 3],
            buf[p + 4],
            buf[p + 5],
            buf[p + 6],
            buf[p + 7],
        ]);
        buf[p..p + 8].copy_from_slice(&v.wrapping_add(delta as u64).to_be_bytes());
    }
    true
}

/// Copy `len` bytes starting at `offset` from `src` to `out` in [`COPY_CHUNK`]
/// slices, so a large `mdat` never lands in memory all at once.
fn copy_range<R: Read + Seek, W: Write>(
    src: &mut R,
    out: &mut W,
    offset: u64,
    len: u64,
) -> io::Result<()> {
    src.seek(SeekFrom::Start(offset))?;
    let mut remaining = len;
    let mut buf = vec![0u8; COPY_CHUNK];
    while remaining > 0 {
        let n = remaining.min(COPY_CHUNK as u64) as usize;
        src.read_exact(&mut buf[..n])?;
        out.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Sibling temp path `<file>.faststart.tmp`.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".faststart.tmp");
    PathBuf::from(name)
}

/// Replace `dst` with `tmp`. `fs::rename` replaces atomically on Unix; on
/// Windows it fails when `dst` exists, so fall back to removing `dst` first.
fn replace_file(tmp: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(tmp, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::remove_file(dst)?;
            fs::rename(tmp, dst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Pure in-memory transform used by the unit tests: parse `data`, and if a
    /// rewrite is warranted produce the new bytes. `None` means "leave the
    /// original as-is" (nothing to fix, or malformed).
    fn faststart_bytes(data: &[u8]) -> Option<Vec<u8>> {
        let mut src = Cursor::new(data);
        let plan = match plan_faststart(&mut src, data.len() as u64).ok()? {
            PlanResult::Rewrite(plan) => plan,
            _ => return None,
        };
        let mut out = Vec::with_capacity(data.len());
        write_plan(&mut src, &mut out, &plan).ok()?;
        Some(out)
    }

    fn make_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut v = Vec::with_capacity(size as usize);
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(box_type);
        v.extend_from_slice(payload);
        v
    }

    fn stco_payload(offsets: &[u32]) -> Vec<u8> {
        let mut p = vec![0u8; 4]; // version + flags
        p.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            p.extend_from_slice(&o.to_be_bytes());
        }
        p
    }

    fn stsz_payload(sample_count: u32) -> Vec<u8> {
        let mut p = vec![0u8; 4]; // version + flags
        p.extend_from_slice(&0u32.to_be_bytes()); // uniform sample size
        p.extend_from_slice(&sample_count.to_be_bytes());
        p
    }

    fn co64_payload(offsets: &[u64]) -> Vec<u8> {
        let mut p = vec![0u8; 4];
        p.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            p.extend_from_slice(&o.to_be_bytes());
        }
        p
    }

    /// Wrap a chunk-offset box (stco/co64 body) in the stbl/minf/mdia/trak chain.
    fn make_trak(chunk_box: &[u8]) -> Vec<u8> {
        let stbl = make_box(b"stbl", chunk_box);
        let minf = make_box(b"minf", &stbl);
        let mdia = make_box(b"mdia", &minf);
        make_box(b"trak", &mdia)
    }

    /// A track with sample tables proving it is empty (what the MF sink writes
    /// for disabled audio): stsz sample_count=0 plus an stco with no entries.
    fn make_empty_trak() -> Vec<u8> {
        let mut stbl = make_box(b"stsz", &stsz_payload(0));
        stbl.extend_from_slice(&make_box(b"stco", &stco_payload(&[])));
        let minf = make_box(b"minf", &stbl);
        let mdia = make_box(b"mdia", &minf);
        make_box(b"trak", &mdia)
    }

    /// Count `trak` children of the (first) moov box in `data` via a real box
    /// walk (headers only).
    fn trak_count(data: &[u8]) -> usize {
        let mut src = Cursor::new(data);
        let boxes = read_top_boxes(&mut src, data.len() as u64)
            .unwrap()
            .unwrap();
        let moov = boxes.iter().find(|b| b.box_type == *b"moov").unwrap();
        child_boxes(
            data,
            moov.offset as usize,
            (moov.offset + moov.total_len) as usize,
        )
        .unwrap()
        .iter()
        .filter(|c| c.box_type == *b"trak")
        .count()
    }

    /// Build `ftyp + mdat + moov` (non-faststart). Returns the file bytes and
    /// the moov box length.
    fn build_mp4(ftyp_payload: &[u8], traks: &[Vec<u8>], mdat_data: &[u8]) -> (Vec<u8>, usize) {
        let ftyp = make_box(b"ftyp", ftyp_payload);
        let mdat = make_box(b"mdat", mdat_data);
        let mut moov_payload = Vec::new();
        for trak in traks {
            moov_payload.extend_from_slice(trak);
        }
        let moov = make_box(b"moov", &moov_payload);

        let mut file = Vec::new();
        file.extend_from_slice(&ftyp);
        file.extend_from_slice(&mdat);
        file.extend_from_slice(&moov);
        (file, moov.len())
    }

    /// The Media Foundation-style ftyp: 24 bytes, major brand mp42.
    fn mf_ftyp_payload() -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(b"mp42");
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(b"mp41");
        p.extend_from_slice(b"isom");
        p
    }

    /// Collect the top-level box types in order (own parser, no ffmpeg).
    fn top_level_types(data: &[u8]) -> Vec<[u8; 4]> {
        let mut src = Cursor::new(data);
        read_top_boxes(&mut src, data.len() as u64)
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|b| b.box_type)
            .collect()
    }

    /// Read every stco entry across the whole buffer. Test payloads are chosen so
    /// the ASCII "stco" never collides with data bytes.
    fn all_stco_entries(data: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            if &data[i + 4..i + 8] == b"stco" {
                let body = i + 8;
                let count =
                    u32::from_be_bytes(data[body + 4..body + 8].try_into().unwrap()) as usize;
                let entries = body + 8;
                for k in 0..count {
                    let p = entries + k * 4;
                    out.push(u32::from_be_bytes(data[p..p + 4].try_into().unwrap()));
                }
            }
            i += 1;
        }
        out
    }

    fn all_co64_entries(data: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            if &data[i + 4..i + 8] == b"co64" {
                let body = i + 8;
                let count =
                    u32::from_be_bytes(data[body + 4..body + 8].try_into().unwrap()) as usize;
                let entries = body + 8;
                for k in 0..count {
                    let p = entries + k * 8;
                    out.push(u64::from_be_bytes(data[p..p + 8].try_into().unwrap()));
                }
            }
            i += 1;
        }
        out
    }

    #[test]
    fn stco_transform_moves_moov_and_patches_offsets() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let orig_offsets = [24u32, 34, 44];
        let trak = make_trak(&make_box(b"stco", &stco_payload(&orig_offsets)));
        // A 16-byte ftyp is not a patchable size, so branding stays untouched.
        let (file, moov_len) = build_mp4(b"isom\x00\x00\x00\x00isom", &[trak], &mdat_data);

        // Sanity: input is non-faststart.
        assert_eq!(top_level_types(&file), vec![*b"ftyp", *b"mdat", *b"moov"]);

        let out = faststart_bytes(&file).expect("should rewrite");

        // Order becomes ftyp, moov, mdat.
        assert_eq!(top_level_types(&out), vec![*b"ftyp", *b"moov", *b"mdat"]);
        // Total length unchanged (moov only moved).
        assert_eq!(out.len(), file.len());
        // Each stco offset grew by exactly moov_len.
        let patched = all_stco_entries(&out);
        assert_eq!(
            patched,
            orig_offsets
                .iter()
                .map(|o| o + moov_len as u32)
                .collect::<Vec<_>>()
        );
        // mdat bytes are byte-identical, only relocated.
        let mdat_pos = out.windows(4).position(|w| w == b"mdat").unwrap();
        let new_data = &out[mdat_pos + 4..mdat_pos + 4 + mdat_data.len()];
        assert_eq!(new_data, mdat_data.as_slice());
    }

    #[test]
    fn mf_ftyp_is_rebranded_to_canonical_isom() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let trak = make_trak(&make_box(b"stco", &stco_payload(&[24])));
        let (file, _moov_len) = build_mp4(&mf_ftyp_payload(), &[trak], &mdat_data);

        let out = faststart_bytes(&file).expect("should rewrite");
        // The 24-byte MF ftyp became the canonical 32-byte isom block.
        assert_eq!(&out[..32], &CANONICAL_FTYP[..]);
        assert_eq!(top_level_types(&out), vec![*b"ftyp", *b"moov", *b"mdat"]);
    }

    #[test]
    fn empty_audio_trak_is_dropped_and_offsets_follow_both_shifts() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let video_offsets = [24u32, 34, 44];
        let video_trak = make_trak(&make_box(b"stco", &stco_payload(&video_offsets)));
        let empty_trak = make_empty_trak();
        let empty_len = empty_trak.len();
        let (file, moov_len) = build_mp4(
            &mf_ftyp_payload(),
            &[video_trak.clone(), empty_trak],
            &mdat_data,
        );

        let out = faststart_bytes(&file).expect("should rewrite");

        assert_eq!(top_level_types(&out), vec![*b"ftyp", *b"moov", *b"mdat"]);
        assert_eq!(&out[..32], &CANONICAL_FTYP[..]);
        // The empty track is gone, the video track remains.
        assert_eq!(trak_count(&out), 1);
        // mdat shifted by the larger ftyp (32 - 24), the inserted moov
        // (shrunk by the dropped trak), and nothing else.
        let expected_delta: i64 = 8 + (moov_len as i64 - empty_len as i64);
        assert_eq!(
            all_stco_entries(&out),
            video_offsets
                .iter()
                .map(|o| (*o as i64 + expected_delta) as u32)
                .collect::<Vec<_>>()
        );
        // The file shrank by exactly the dropped trak minus the ftyp growth.
        assert_eq!(out.len() as i64, file.len() as i64 - empty_len as i64 + 8);
    }

    #[test]
    fn track_with_samples_is_never_dropped() {
        let mdat_data: Vec<u8> = (0u8..60).collect();
        let video = make_trak(&make_box(b"stco", &stco_payload(&[24, 30])));
        let mut audio_stbl = make_box(b"stsz", &stsz_payload(2));
        audio_stbl.extend_from_slice(&make_box(b"stco", &stco_payload(&[40, 50])));
        let audio = make_trak(&audio_stbl);
        let (file, moov_len) = build_mp4(b"isom\x00\x00\x00\x00isom", &[video, audio], &mdat_data);

        let out = faststart_bytes(&file).expect("should rewrite");
        assert_eq!(trak_count(&out), 2);
        // Both tracks' chunk offsets moved by exactly the inserted moov length.
        assert_eq!(
            all_stco_entries(&out),
            vec![24 + moov_len as u32, 30 + moov_len as u32, 40 + moov_len as u32, 50 + moov_len as u32]
        );
    }

    #[test]
    fn all_empty_traks_are_kept() {
        // Degenerate: every track empty. The repack must not strip the movie
        // down to zero tracks; it still performs the faststart move.
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let a = make_empty_trak();
        let b = make_empty_trak();
        let (file, _moov_len) = build_mp4(b"isom\x00\x00\x00\x00isom", &[a, b], &mdat_data);

        let out = faststart_bytes(&file).expect("should rewrite for faststart");
        assert_eq!(top_level_types(&out), vec![*b"ftyp", *b"moov", *b"mdat"]);
        assert_eq!(trak_count(&out), 2);
        assert_eq!(out.len(), file.len());
    }

    #[test]
    fn already_faststart_with_empty_trak_gets_repaired_with_negative_shift() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let video_offsets = [24u32, 34];
        let video_trak = make_trak(&make_box(b"stco", &stco_payload(&video_offsets)));
        let empty_trak = make_empty_trak();
        let empty_len = empty_trak.len();
        let moov = make_box(b"moov", &[video_trak, empty_trak].concat());
        let mdat = make_box(b"mdat", &mdat_data);

        // Already faststart (moov before mdat) with canonical branding: only the
        // empty track warrants a rewrite.
        let mut file = Vec::new();
        file.extend_from_slice(&CANONICAL_FTYP);
        file.extend_from_slice(&moov);
        file.extend_from_slice(&mdat);

        let out = faststart_bytes(&file).expect("empty track alone warrants a rewrite");
        assert_eq!(top_level_types(&out), vec![*b"ftyp", *b"moov", *b"mdat"]);
        assert_eq!(trak_count(&out), 1);
        // mdat moved BACK by the dropped trak's bytes.
        assert_eq!(
            all_stco_entries(&out),
            video_offsets
                .iter()
                .map(|o| (*o as i64 - empty_len as i64) as u32)
                .collect::<Vec<_>>()
        );
        // Idempotent afterwards.
        assert!(faststart_bytes(&out).is_none());
    }

    #[test]
    fn co64_transform_patches_64bit_offsets() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let orig_offsets = [24u64, 34, 44];
        let trak = make_trak(&make_box(b"co64", &co64_payload(&orig_offsets)));
        let (file, moov_len) = build_mp4(b"isom\x00\x00\x00\x00isom", &[trak], &mdat_data);

        let out = faststart_bytes(&file).expect("should rewrite");
        assert_eq!(top_level_types(&out), vec![*b"ftyp", *b"moov", *b"mdat"]);
        assert_eq!(out.len(), file.len());
        let patched = all_co64_entries(&out);
        assert_eq!(
            patched,
            orig_offsets
                .iter()
                .map(|o| o + moov_len as u64)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn multiple_traks_all_patched() {
        let mdat_data: Vec<u8> = (0u8..60).collect();
        let a = [24u32, 30];
        let b = [40u32, 50, 55];
        let trak_a = make_trak(&make_box(b"stco", &stco_payload(&a)));
        let trak_b = make_trak(&make_box(b"stco", &stco_payload(&b)));
        let (file, moov_len) = build_mp4(b"isom\x00\x00\x00\x00isom", &[trak_a, trak_b], &mdat_data);

        let out = faststart_bytes(&file).expect("should rewrite");
        let patched = all_stco_entries(&out);
        let mut expected: Vec<u32> = a.iter().map(|o| o + moov_len as u32).collect();
        expected.extend(b.iter().map(|o| o + moov_len as u32));
        assert_eq!(patched, expected);
    }

    #[test]
    fn already_faststart_is_left_unchanged() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let trak = make_trak(&make_box(b"stco", &stco_payload(&[24, 34])));
        let (file, _moov_len) = build_mp4(b"isom\x00\x00\x00\x00isom", &[trak], &mdat_data);
        // Transform once to get a faststart file, then run again: no-op.
        let faststart = faststart_bytes(&file).expect("first pass rewrites");
        assert_eq!(top_level_types(&faststart), vec![*b"ftyp", *b"moov", *b"mdat"]);
        assert!(
            faststart_bytes(&faststart).is_none(),
            "already-faststart input must not be rewritten"
        );
    }

    #[test]
    fn truncated_input_is_skipped_without_panic() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let trak = make_trak(&make_box(b"stco", &stco_payload(&[24, 34])));
        let (file, _) = build_mp4(b"isomiso2", &[trak], &mdat_data);
        // Chop the tail so the final box's declared size runs past EOF.
        let truncated = &file[..file.len() - 5];
        let before = truncated.to_vec();
        assert!(faststart_bytes(truncated).is_none());
        // The pure function never mutates its input.
        assert_eq!(truncated, before.as_slice());
    }

    #[test]
    fn missing_moov_is_skipped() {
        let ftyp = make_box(b"ftyp", b"isomiso2");
        let mdat = make_box(b"mdat", &[1u8, 2, 3, 4]);
        let mut file = ftyp.clone();
        file.extend_from_slice(&mdat);
        assert!(faststart_bytes(&file).is_none());
    }

    #[test]
    fn faststart_file_rewrites_on_disk() {
        // Exercises the streaming file wrapper (copy_range + atomic replace) on
        // any platform.
        let mdat_data: Vec<u8> = (0u8..50).collect();
        let orig_offsets = [24u32, 40, 50];
        let trak = make_trak(&make_box(b"stco", &stco_payload(&orig_offsets)));
        let (file, moov_len) = build_mp4(b"isom\x00\x00\x00\x00isom", &[trak], &mdat_data);

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "skrino-faststart-test-{}-{}.mp4",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &file).unwrap();

        let outcome = faststart_file(&path).expect("file rewrite should succeed");
        assert!(matches!(outcome, Outcome::Rewritten));

        let rewritten = std::fs::read(&path).unwrap();
        assert_eq!(top_level_types(&rewritten), vec![*b"ftyp", *b"moov", *b"mdat"]);
        assert_eq!(rewritten.len(), file.len());
        assert_eq!(
            all_stco_entries(&rewritten),
            orig_offsets
                .iter()
                .map(|o| o + moov_len as u32)
                .collect::<Vec<_>>()
        );
        // No stray temp file left behind.
        assert!(!tmp_path(&path).exists());

        // Running again is a no-op (already faststart).
        let outcome2 = faststart_file(&path).expect("second pass should succeed");
        assert!(matches!(outcome2, Outcome::AlreadyFaststart));

        let _ = std::fs::remove_file(&path);
    }
}

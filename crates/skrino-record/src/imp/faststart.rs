//! qt-faststart post-process for the finished .mp4.
//!
//! The Media Foundation MP4 sink writes the `moov` box AFTER `mdat`
//! (`ftyp, uuid, mdat, moov`). Progressive / streaming players (Telegram inline
//! preview and friends) need `moov` BEFORE `mdat` so the sample tables are
//! available without seeking to the tail of the file. `windows-capture` does not
//! expose the MF "fast start" attribute, so we rewrite the box order ourselves
//! once the encoder has fully flushed the file.
//!
//! This module is intentionally pure byte/IO manipulation with no OS calls, so
//! it compiles and unit-tests on any platform. The transform is:
//!
//! 1. Parse TOP-LEVEL boxes only. A box header is a 4-byte big-endian size + a
//!    4-byte type. `size == 1` means the real size is the following 8-byte
//!    big-endian largesize; `size == 0` means "to end of file" (typically the
//!    `mdat`). All three forms are handled.
//! 2. If `moov` already precedes `mdat`, the file is already faststart and is
//!    left untouched (idempotent).
//! 3. Otherwise read the (small, ~KBs) `moov` fully into memory and, walking
//!    ONLY the container chain `moov -> trak -> mdia -> minf -> stbl`, add the
//!    `moov` box length (`delta`) to every 32-bit `stco` / 64-bit `co64` chunk
//!    offset. Those offsets point into `mdat`, which shifts down by exactly the
//!    inserted `moov` length. A proper recursive box walk is used, never an
//!    ASCII scan for "stco"/"co64" (which could match payload bytes).
//! 4. Stream the result into a sibling temp file (`ftyp`/`uuid` at their original
//!    positions, then the patched `moov`, then `mdat` streamed in chunks, then
//!    any boxes that followed `mdat` except the original `moov`) and atomically
//!    replace the original.
//!
//! Any error or unexpected layout is a no-op: the original file is left intact
//! and the caller still returns a playable (non-faststart) path. Failure here
//! must never fail the recording or corrupt the file.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Chunk size for streaming `mdat` from the original file into the rewritten one.
/// `mdat` can be hundreds of MB, so it is never loaded fully into memory.
const COPY_CHUNK: usize = 1024 * 1024;

/// Container boxes whose children we descend into while hunting for chunk offset
/// tables. Anything else (e.g. `stsd`, `dinf`, `edts`) is left untouched: it
/// never contains an `stco`/`co64` that points into `mdat`.
fn is_container(box_type: &[u8; 4]) -> bool {
    matches!(box_type, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl")
}

/// One top-level box as located in the file.
#[derive(Clone, Copy)]
struct BoxEntry {
    box_type: [u8; 4],
    /// Byte offset of the box header (its size field) in the file.
    offset: u64,
    /// Header length: 8 for a 32-bit size, 16 for a 64-bit largesize.
    header_len: u64,
    /// Full box length including the header.
    total_len: u64,
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
/// map plus the already-patched `moov` bytes to splice in before `mdat`.
struct RewritePlan {
    boxes: Vec<BoxEntry>,
    moov_idx: usize,
    mdat_idx: usize,
    patched_moov: Vec<u8>,
}

/// Public entry: rewrite `path` in place to faststart layout, logging (in
/// Russian) on skip/failure and never returning an error. A skip or failure
/// leaves the original file byte-for-byte intact.
pub(crate) fn make_faststart(path: &Path) {
    match faststart_file(path) {
        Ok(Outcome::Rewritten) => {
            log::debug!(
                "skrino-record: mp4 переупакован для потокового воспроизведения (moov перед mdat)"
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

/// Rewrite `path` to faststart layout. Streams `mdat` and replaces the original
/// atomically. Returns the outcome; on any IO error the original is untouched
/// (the temp file, if any, is removed).
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
/// so read and patch the `moov` box. Reads only headers plus the small `moov`;
/// never touches `mdat`.
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
    if moov_idx < mdat_idx {
        return Ok(PlanResult::AlreadyFaststart);
    }

    let moov = boxes[moov_idx];
    src.seek(SeekFrom::Start(moov.offset))?;
    let mut patched_moov = vec![0u8; moov.total_len as usize];
    src.read_exact(&mut patched_moov)?;

    // moov is inserted immediately before mdat, so mdat (and only mdat, since
    // moov was the trailing box) shifts down by exactly the moov length.
    let delta = moov.total_len;
    let moov_len = patched_moov.len();
    if !patch_boxes(&mut patched_moov, moov.header_len as usize, moov_len, delta) {
        return Ok(PlanResult::Skipped("не удалось обновить таблицы смещений mdat"));
    }

    Ok(PlanResult::Rewrite(RewritePlan {
        boxes,
        moov_idx,
        mdat_idx,
        patched_moov,
    }))
}

/// Stream the faststart layout to `out`: boxes before `mdat` in original order,
/// then the patched `moov`, then `mdat`, then boxes after `mdat` except the
/// original `moov`. `mdat` is streamed in [`COPY_CHUNK`] slices.
fn write_plan<R: Read + Seek, W: Write>(
    src: &mut R,
    out: &mut W,
    plan: &RewritePlan,
) -> io::Result<()> {
    for (i, b) in plan.boxes.iter().enumerate() {
        if i == plan.mdat_idx {
            out.write_all(&plan.patched_moov)?;
            copy_range(src, out, b.offset, b.total_len)?;
        } else if i == plan.moov_idx {
            // Emitted above, right before mdat; skip its original tail position.
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
/// the known container chain and adding `delta` to every `stco`/`co64` chunk
/// offset found. Returns `false` on any malformed child box (caller then skips
/// the transform, leaving the file intact). Every slice access is bounds-checked
/// against `end`, so malformed input can never panic.
fn patch_boxes(buf: &mut [u8], start: usize, end: usize, delta: u64) -> bool {
    let mut pos = start;
    while pos + 8 <= end {
        let size32 = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as u64;
        let box_type = [buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]];

        let (header_len, total_len) = if size32 == 1 {
            if pos + 16 > end {
                return false;
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
            (16usize, large as usize)
        } else if size32 == 0 {
            (8usize, end - pos)
        } else {
            (8usize, size32 as usize)
        };

        if total_len < header_len || pos + total_len > end {
            return false;
        }
        let payload_start = pos + header_len;
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
/// `mdat` and only grows, so `delta` is added wrapping into u32 (a >4 GiB file
/// would use `co64`, not `stco`).
fn patch_stco(buf: &mut [u8], start: usize, end: usize, delta: u64) -> bool {
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
fn patch_co64(buf: &mut [u8], start: usize, end: usize, delta: u64) -> bool {
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
        buf[p..p + 8].copy_from_slice(&v.wrapping_add(delta).to_be_bytes());
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
    /// rewrite is warranted produce the faststart bytes. `None` means "leave the
    /// original as-is" (already faststart, no moov/mdat, or malformed).
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

    /// Build `ftyp + mdat + moov` (non-faststart). Returns the file bytes, the
    /// moov length, and where the mdat *data* begins in the file.
    fn build_mp4(traks: &[Vec<u8>], mdat_data: &[u8]) -> (Vec<u8>, usize, usize) {
        let ftyp = make_box(b"ftyp", b"isomiso2");
        let mdat = make_box(b"mdat", mdat_data);
        let mut moov_payload = Vec::new();
        for trak in traks {
            moov_payload.extend_from_slice(trak);
        }
        let moov = make_box(b"moov", &moov_payload);

        let mut file = Vec::new();
        file.extend_from_slice(&ftyp);
        let mdat_data_start = file.len() + 8; // after mdat's 8-byte header
        file.extend_from_slice(&mdat);
        file.extend_from_slice(&moov);
        (file, moov.len(), mdat_data_start)
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
        let (file, moov_len, _mdat_data_start) = build_mp4(&[trak], &mdat_data);

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
    fn co64_transform_patches_64bit_offsets() {
        let mdat_data: Vec<u8> = (0u8..40).collect();
        let orig_offsets = [24u64, 34, 44];
        let trak = make_trak(&make_box(b"co64", &co64_payload(&orig_offsets)));
        let (file, moov_len, _) = build_mp4(&[trak], &mdat_data);

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
        let (file, moov_len, _) = build_mp4(&[trak_a, trak_b], &mdat_data);

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
        let (file, _moov_len, _) = build_mp4(&[trak], &mdat_data);
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
        let (file, _, _) = build_mp4(&[trak], &mdat_data);
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
        let (file, moov_len, _) = build_mp4(&[trak], &mdat_data);

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

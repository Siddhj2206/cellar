//! Rust-native icon extraction (blueprint §5: `cellar-desktop` owns icon
//! extraction + cache — superseding the legacy `wrestool` + `ImageMagick`
//! pipeline with pure Rust over the exe's bytes).
//!
//! A Windows executable carries its icons as a resource tree
//! (MS-PE §6.4 + MS-RC): `RT_GROUP_ICON` (14) names the icon set, whose
//! entries list `RT_ICON` (3) frames with their sizes and bit depths.
//! This module parses the PE structure and the resource tree with
//! boundary-checked reads only (the workspace forbids `unsafe`), picks
//! the best frame — largest 32-bpp, else largest 24-bpp, else none — and
//! re-encodes it as a PNG (32-bpp BGRA rows flipped, or the frame's own
//! embedded PNG passed through). The PNG encoder is local too: stored
//! DEFLATE blocks plus CRC-32/Adler-32, no external tools, no new
//! dependencies. Anything malformed degrades to "no icon" (`None`) — an
//! entry without an icon is still functional; the icon key is simply
//! omitted.

/// The PNG signature a frame payload starts with when the icon was
/// embedded as a PNG (Vista+ "PNG-compressed" icons — stored as-is).
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// The classic ICO frame signature: a DIB in front (`BITMAPINFOHEADER`).
const BMP_HEADER_SIZE: usize = 40;

/// Peel the icon out of exe bytes: the best frame of the first group
/// icon, as PNG bytes. Every malformed shape — not a PE, no resource
/// directory, no icon group, undecodable frames — is `None`.
/// The reference whole-buffer path: production reads ranges through
/// `extract_icon_from_file`; this stays the test oracle and the
/// in-memory caller's entry point.
#[allow(dead_code)]
pub(crate) fn extract_icon_png(exe: &[u8]) -> Option<Vec<u8>> {
    let (offset, size) = pe_resource_directory(exe)?;
    let section = exe.get(offset..offset.checked_add(size)?)?;
    extract_icon_png_from_section(section)
}

/// The same pipeline over an isolated resource-section buffer (#64): the
/// file-based entry reads exactly this range, so peak memory is
/// O(resource section), never O(exe size). Section-relative addressing —
/// the buffer starts at the resource directory's file offset.
pub(crate) fn extract_icon_png_from_section(section: &[u8]) -> Option<Vec<u8>> {
    let resource = (0, section.len());
    let group = resource_data(section, resource, &[14, 1, 0])?;
    let frames = parse_group_icon(group)?;
    let mut best: Vec<usize> = (0..frames.len()).collect();
    best.sort_by_key(|&index| {
        let frame = &frames[index];
        let (frame_width, frame_height) = frame.dimensions();
        let area = frame_width * frame_height;
        let depth_order = match frame.bit_count {
            32 => 0,
            24 => 1,
            _ => 2,
        };
        (depth_order, std::cmp::Reverse(area), index)
    });
    for index in best {
        let frame = &frames[index];
        let payload = resource_data(section, resource, &[3, u32::from(frame.id), 0])?;
        if payload.len() != frame.bytes as usize {
            continue;
        }
        if let Some(png) = frame_to_png(payload) {
            return Some(png);
        }
    }
    None
}
/// A group-icon entry: one frame's size, bit depth, and payload size.
#[derive(Debug, Clone, Copy)]
struct GroupEntry {
    /// Display width; 0 means 256 (the ICO convention).
    width: u8,
    /// Display height; 0 means 256.
    height: u8,
    bit_count: u16,
    /// The frame payload's size in the file (`bytesInRes`).
    bytes: u32,
    /// The `RT_ICON` resource id the payload lives under.
    id: u16,
}

impl GroupEntry {
    /// The real dimensions as `(width, height)` — 0 means 256 (the ICO
    /// convention), the size the frame preference sorts by.
    fn dimensions(self) -> (u32, u32) {
        let width = if self.width == 0 {
            256
        } else {
            u32::from(self.width)
        };
        let height = if self.height == 0 {
            256
        } else {
            u32::from(self.height)
        };
        (width, height)
    }
}

/// Parse `GRPICONDIR`: the reserved/type/count header plus the 14-byte
/// entries. Boundary-checked; malformed input is `None`.
fn parse_group_icon(data: &[u8]) -> Option<Vec<GroupEntry>> {
    if data.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    let mut entries = Vec::with_capacity(count);
    let mut offset = 6usize;
    for _ in 0..count {
        let entry = data.get(offset..offset + 14)?;
        let width = entry[0];
        let height = entry[1];
        // ICONDIRENTRY layout: width, height, colors, reserved, planes
        // (4..6), bit count (6..8), payload size (8..12), id (12..14).
        let planes = u16::from_le_bytes([entry[4], entry[5]]);
        let bit_count = u16::from_le_bytes([entry[6], entry[7]]);
        let bytes = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
        let id = u16::from_le_bytes([entry[12], entry[13]]);
        // Color count (entry[2]) is a hint only; planes 0/1 both occur.
        if planes > 1 || bytes == 0 {
            return None;
        }
        entries.push(GroupEntry {
            width,
            height,
            bit_count,
            bytes,
            id,
        });
        offset += 14;
    }
    Some(entries)
}

/// Decode one `RT_ICON` payload to PNG bytes: an embedded PNG passes
/// through unchanged; a DIB is flipped, unmasked, and re-encoded. Bounds
/// violations, unsupported bit depths, and undecodable shapes are `None`.
pub(crate) fn frame_to_png(payload: &[u8]) -> Option<Vec<u8>> {
    if payload.starts_with(&PNG_SIGNATURE) {
        return Some(payload.to_vec());
    }
    let header = payload.get(..BMP_HEADER_SIZE)?;
    let width = u32::from_le_bytes(header[4..8].try_into().ok()?);
    let height = i32::from_le_bytes(header[8..12].try_into().ok()?);
    let bit_count = u16::from_le_bytes([header[14], header[15]]);
    let size = u32::from_le_bytes(header[0..4].try_into().ok()?);
    // The DIB may be a v3/v4/v5 header (40/108/124 bytes), padded — the
    // pixel data starts at the declared header size when sane.
    let header_size = usize::try_from(size).ok()?.min(payload.len());
    let pixels = payload.get(header_size..)?;
    let width = usize::try_from(width).ok()?;
    if width == 0 || width > 1024 {
        return None;
    }
    let height = usize::try_from(height).ok()?;
    let rows = image_rows(width, bit_count, height, pixels)?;
    let xor_stride = row_stride(width, bit_count)?;
    match bit_count {
        32 => {
            let xor = pixels.get(..rows * xor_stride)?;
            let mut rgba = Vec::with_capacity(rows * width * 4);
            for row in xor.chunks_exact(xor_stride).take(rows).rev() {
                for pixel in row[..width * 4].chunks_exact(4) {
                    // ICO bitmaps are BGRA; PNG wants RGBA.
                    rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                }
            }
            png_encode(width, rows, &rgba)
        }
        24 => {
            let xor = pixels.get(..rows * xor_stride)?;
            // The AND mask follows the XOR bitmap (1-bpp rows, 32-bit
            // stride) and supplies the transparency 24-bpp lacks.
            let mask_stride = row_stride(width, 1)?;
            let mask = pixels.get(rows * xor_stride..rows * xor_stride + rows * mask_stride)?;
            let mut rgba = Vec::with_capacity(rows * width * 4);
            for (row, mask_row) in xor
                .chunks_exact(xor_stride)
                .take(rows)
                .rev()
                .zip(mask.chunks_exact(mask_stride).take(rows).rev())
            {
                // The row's visible bytes are the width × 3 BGR pixels;
                // the stride's tail is padding.
                for (column, pixel) in row[..width * 3].chunks_exact(3).enumerate() {
                    // The mask is 1 bpp, MSB-first within each byte.
                    let mask_byte = mask_row[column / 8];
                    let hidden = mask_byte & (0x80 >> (column % 8)) != 0;
                    let alpha = if hidden { 0 } else { 255 };
                    rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
                }
            }
            png_encode(width, rows, &rgba)
        }
        _ => None,
    }
}

/// The XOR bitmap's row count: a bottom-up DIB whose height is doubled by
/// a trailing AND mask yields `height / 2` image rows; a 32-bpp DIB
/// without a mask (full alpha) holds the whole height. The detail fits
/// are checked against what the frame payload actually carries.
fn image_rows(width: usize, bit_count: u16, height: usize, pixels: &[u8]) -> Option<usize> {
    let xor_stride = row_stride(width, bit_count)?;
    let mask_stride = row_stride(width, 1)?;
    if height % 2 == 0 {
        let half = height / 2;
        if half
            .checked_mul(xor_stride)?
            .checked_add(half.checked_mul(mask_stride)?)?
            <= pixels.len()
        {
            return Some(half);
        }
    }
    if bit_count == 32 && height.checked_mul(xor_stride)? <= pixels.len() {
        return Some(height);
    }
    None
}

/// The byte stride of one bottom-up DIB row at `bits_per_pixel`,
/// 32-bit-aligned (the Windows `((w * bpp + 31) / 32) * 4` rule).
fn row_stride(width: usize, bits_per_pixel: u16) -> Option<usize> {
    let bits = usize::from(bits_per_pixel).checked_mul(width)?;
    bits.div_ceil(32).checked_mul(4)
}

// ---------------------------------------------------------------------------
// The PE reader: DOS header → PE header → optional header → data
// directories → section table → resource tree. Every read is
// boundary-checked; no unsafe anywhere (workspace rule).
// ---------------------------------------------------------------------------

/// The resource directory's `(rva, size)` from the thunk's data
/// directory, mapped to a file range through the section table.
pub(crate) fn pe_resource_directory(exe: &[u8]) -> Option<(usize, usize)> {
    if !exe.starts_with(b"MZ") {
        return None;
    }
    let lfanew = u32::from_le_bytes(exe.get(0x3C..0x40)?.try_into().ok()?) as usize;
    if !exe.get(lfanew..lfanew + 4)?.starts_with(b"PE\0\0") {
        return None;
    }
    // COFF header: 20 bytes; its last two fields are the optional-header
    // size and image characteristics.
    let coff = exe.get(lfanew + 4..lfanew + 24)?;
    let optional_size = u16::from_le_bytes([coff[16], coff[17]]) as usize;
    // An optional header of 0 (or tiny) bytes is malformed — degrade to
    // no icon, never index past the slice (a hostile exe must not panic
    // the CLI; the whole module's contract is malformed → `None`).
    let optional = exe.get(lfanew + 24..lfanew + 24 + optional_size)?;
    let magic = u16::from_le_bytes(optional.get(0..2)?.try_into().ok()?);
    // The data-directory array starts at a fixed offset in each optional
    // header shape (MS-PE §A.1.2: 96 for PE32, 112 for PE32+).
    let directories_offset = match magic {
        0x10B => 96,
        0x20B => 112,
        _ => return None,
    };
    // Directory index 2 = resources (MS-PE §A.1.4).
    let directory = optional.get(directories_offset + 2 * 8..directories_offset + 3 * 8)?;
    let rva = u32::from_le_bytes(directory[0..4].try_into().ok()?) as usize;
    let size = u32::from_le_bytes(directory[4..8].try_into().ok()?) as usize;
    if rva == 0 || size == 0 {
        return None;
    }
    // The section table follows the optional header.
    let sections = exe.get(lfanew + 24 + optional_size..)?;
    let section_count = u16::from_le_bytes([coff[2], coff[3]]) as usize;
    for index in 0..section_count {
        let section = sections.get(index * 40..index * 40 + 40)?;
        let virtual_size = u32::from_le_bytes(section[8..12].try_into().ok()?) as usize;
        let virtual_address = u32::from_le_bytes(section[12..16].try_into().ok()?) as usize;
        let raw_size = u32::from_le_bytes(section[16..20].try_into().ok()?) as usize;
        let raw_offset = u32::from_le_bytes(section[20..24].try_into().ok()?) as usize;
        let extent = virtual_size.max(raw_size);
        if rva >= virtual_address && rva - virtual_address < extent {
            let file_offset = raw_offset + (rva - virtual_address);
            return Some((file_offset.min(exe.len()), size));
        }
    }
    None
}

/// The bytes of one resource leaf, addressed by its id path through the
/// resource tree — e.g. `[14, 1, 0]` is group-icon → first name → first
/// language. The tree is a directory of directories whose leaves are
/// `(offset, size)` pairs into the resource section.
fn resource_data<'a>(exe: &'a [u8], resource: (usize, usize), path: &[u32]) -> Option<&'a [u8]> {
    let (section_start, section_size) = resource;
    let section = exe.get(section_start..section_start + section_size)?;
    let mut offset = 0usize;
    for &wanted in path {
        offset = directory_child(section, offset, wanted)?;
    }
    // The leaf: a `(data offset, size)` pair relative to the section.
    let leaf = section.get(offset..offset + 8)?;
    let data_offset = u32::from_le_bytes(leaf[0..4].try_into().ok()?) as usize & 0x7FFF_FFFF;
    let size = u32::from_le_bytes(leaf[4..8].try_into().ok()?) as usize;
    section.get(data_offset..data_offset + size)
}

/// Follow one resource-directory level to the entry with id `wanted` and
/// return the offset its entry pointer targets — whether it points at a
/// nested directory (high bit set) or a data entry, the address is
/// relative to the resource directory's own start, so section-relative in
/// practice (MS-PE §6.4.4; the root sits at the section's first byte).
/// Name entries — the name field's high bit is set — never match an id.
fn directory_child(section: &[u8], directory: usize, wanted: u32) -> Option<usize> {
    let header = section.get(directory..directory + 16)?;
    let id_entries = u16::from_le_bytes([header[12], header[13]]) as usize;
    let name_entries = u16::from_le_bytes([header[14], header[15]]) as usize;
    let count = id_entries + name_entries;
    for index in 0..count {
        let entry = section.get(directory + 16 + index * 8..directory + 16 + (index + 1) * 8)?;
        let id = u32::from_le_bytes(entry[0..4].try_into().ok()?);
        if id & 0x8000_0000 == 0 && id == wanted {
            let target = u32::from_le_bytes(entry[4..8].try_into().ok()?);
            return Some((target & 0x7FFF_FFFF) as usize);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The PNG encoder: signature, IHDR, one IDAT (a zlib stream of stored
// DEFLATE blocks — uncompressed, so no compressor is needed), IEND.
// ---------------------------------------------------------------------------

/// Encode `width × height` of RGBA pixels as PNG bytes (8-bit, color type
/// 6). Each scanline is prefixed with filter 0 (none) as PNG requires.
pub(crate) fn png_encode(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(4)?;
    if rgba.len() != row_bytes.checked_mul(height)? {
        return None;
    }
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    let mut out = Vec::with_capacity(rgba.len() + 64);
    out.extend_from_slice(&PNG_SIGNATURE);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, *b"IHDR", &ihdr);
    // The zlib stream: a header byte pair (deflate, no dictionary) then
    // the raw scanlines (0x00 filter byte included) in stored blocks, the
    // last one flagged final. Adler-32 covers the raw scanlines.
    let height = usize::try_from(height).ok()?;
    let mut raw = Vec::with_capacity(rgba.len() + height);
    for row in rgba.chunks_exact(row_bytes) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut idat = vec![0x78, 0x01];
    let mut rest = raw.as_slice();
    while !rest.is_empty() {
        let take = rest.len().min(65_535);
        let (block, remaining) = rest.split_at(take);
        idat.push(u8::from(remaining.is_empty())); // BTYPE 00 = stored
        let len = u16::try_from(block.len()).ok()?;
        idat.extend_from_slice(&len.to_le_bytes());
        idat.extend_from_slice(&(!len).to_le_bytes());
        idat.extend_from_slice(block);
        rest = remaining;
    }
    idat.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, *b"IDAT", &idat);
    chunk(&mut out, *b"IEND", &[]);
    Some(out)
}

/// One PNG chunk: length, type, data, CRC-32 over type + data.
fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let length = u32::try_from(data.len()).expect("a PNG chunk never exceeds 4 GiB");
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let mut crc = crc32(&kind);
    crc = crc32_update(crc, data);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (the PNG polynomial 0xEDB88320), table-driven.
fn crc32(bytes: &[u8]) -> u32 {
    crc32_update(0xFFFF_FFFF, bytes) ^ 0xFFFF_FFFF
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = build_crc_table();
    for &byte in bytes {
        crc = TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0u32;
    while n < 256 {
        let mut crc = n;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[n as usize] = crc;
        n += 1;
    }
    table
}

/// Adler-32 over the zlib payload, big-endian as PNG's IDAT wants it.
fn adler32(bytes: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The minimum PE32 image: DOS stub, PE header, one `.rsrc` section
    /// carrying `resources` — the fixture equivalent of `build_lnk` in
    /// storage (a hand-built blob, no external tooling).
    fn build_pe(resources: &[u8]) -> Vec<u8> {
        let mut exe = Vec::new();
        exe.extend_from_slice(b"MZ");
        exe.resize(0x40, 0);
        exe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        exe.resize(0x80, 0);
        exe.extend_from_slice(b"PE\0\0");
        // COFF header (20 bytes): machine, 1 section, timestamp, symbol
        // pointers (all zero), optional-header size 0xE0, characteristics.
        exe.extend_from_slice(&0x14Cu16.to_le_bytes()); // Machine
        exe.extend_from_slice(&1u16.to_le_bytes()); // NumberOfSections
        exe.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
        exe.extend_from_slice(&0u32.to_le_bytes()); // PointerToSymbolTable
        exe.extend_from_slice(&0u32.to_le_bytes()); // NumberOfSymbols
        exe.extend_from_slice(&0xE0u16.to_le_bytes()); // SizeOfOptionalHeader
        exe.extend_from_slice(&0x0102u16.to_le_bytes()); // Characteristics
        // Optional header, PE32: the resource directory entry at 2.
        let mut optional = [0u8; 0xE0];
        optional[0..2].copy_from_slice(&0x10Bu16.to_le_bytes());
        optional[0x5C..0x60].copy_from_slice(&16u32.to_le_bytes());
        let rva = 0x1000u32;
        let size: u32 = resources.len().try_into().expect("fixture size fits u32");
        optional[96 + 2 * 8..96 + 2 * 8 + 4].copy_from_slice(&rva.to_le_bytes());
        optional[96 + 2 * 8 + 4..96 + 2 * 8 + 8].copy_from_slice(&size.to_le_bytes());
        exe.extend_from_slice(&optional);
        // Section table: ".rsrc" at RVA 0x1000, raw at 0x200.
        let mut section = [0u8; 40];
        section[0..5].copy_from_slice(b".rsrc");
        section[8..12].copy_from_slice(
            &(u32::try_from(resources.len()).expect("fixture sizes fit u32")).to_le_bytes(),
        );
        section[12..16].copy_from_slice(&0x1000u32.to_le_bytes());
        section[16..20].copy_from_slice(
            &(u32::try_from(resources.len()).expect("fixture sizes fit u32")).to_le_bytes(),
        );
        section[20..24].copy_from_slice(&0x200u32.to_le_bytes());
        exe.extend_from_slice(&section);
        exe.resize(0x200, 0);
        exe.extend_from_slice(resources);
        exe
    }

    /// A 24-bpp BGR icon frame (bottom-up, with AND mask) sized
    /// `width × height`, solid color `rgb`, fully opaque mask.
    fn bgr_frame(width: u16, height: u16, rgb: (u8, u8, u8)) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&40u32.to_le_bytes());
        frame.extend_from_slice(&u32::from(width).to_le_bytes());
        frame.extend_from_slice(&u32::from(height * 2).to_le_bytes()); // mask doubles it
        frame.extend_from_slice(&1u16.to_le_bytes());
        frame.extend_from_slice(&24u16.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.resize(40, 0);
        let xor_stride = (usize::from(width) * 24).div_ceil(32) * 4;
        let mask_stride = usize::from(width).div_ceil(32) * 4;
        for _ in 0..usize::from(height) {
            for _ in 0..usize::from(width) {
                frame.extend_from_slice(&[rgb.2, rgb.1, rgb.0]);
            }
            frame.resize(frame.len() + xor_stride - usize::from(width) * 3, 0);
        }
        frame.resize(
            40 + xor_stride * usize::from(height) + mask_stride * usize::from(height),
            0,
        );
        frame
    }

    /// A 32-bpp BGRA icon frame (bottom-up, with AND mask) sized
    /// `width × height`.
    fn bgra_frame(width: u16, height: u16, channels: (u8, u8, u8)) -> Vec<u8> {
        let mut frame = Vec::new();
        // BITMAPINFOHEADER.
        frame.extend_from_slice(&40u32.to_le_bytes());
        frame.extend_from_slice(&u32::from(width).to_le_bytes());
        frame.extend_from_slice(&u32::from(height * 2).to_le_bytes()); // mask doubles it
        frame.extend_from_slice(&1u16.to_le_bytes());
        frame.extend_from_slice(&32u16.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.resize(40, 0);
        let xor_stride = usize::from(width) * 4;
        let mask_stride = usize::from(width).div_ceil(32) * 4;
        for _row in 0..usize::from(height) {
            for _pixel in 0..usize::from(width) {
                frame.extend_from_slice(&[channels.2, channels.1, channels.0, 255]);
            }
        }
        frame.resize(
            40 + xor_stride * usize::from(height) + mask_stride * usize::from(height),
            0,
        );
        frame
    }

    /// `GRPICONDIR` entries for one or more frames: (width, height, bit
    /// depth, payload size, `RT_ICON` id).
    fn group_icon_multi(entries: &[(u8, u8, u16, u32, u16)]) -> Vec<u8> {
        let mut group = Vec::new();
        group.extend_from_slice(&0u16.to_le_bytes());
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(
            &(u16::try_from(entries.len()).expect("fixture counts fit u16")).to_le_bytes(),
        );
        for &(width, height, bit_count, bytes, id) in entries {
            group.push(width);
            group.push(height);
            group.push(0); // color count: hint, ignored
            group.push(0); // reserved
            group.extend_from_slice(&1u16.to_le_bytes());
            group.extend_from_slice(&bit_count.to_le_bytes());
            group.extend_from_slice(&bytes.to_le_bytes());
            group.extend_from_slice(&id.to_le_bytes());
        }
        group
    }

    /// The `GRPICONDIR` for one frame.
    fn group_icon(width: u8, height: u8, bit_count: u16, bytes: u32, id: u16) -> Vec<u8> {
        group_icon_multi(&[(width, height, bit_count, bytes, id)])
    }

    /// A resource section carrying every `(type, id)` leaf under the
    /// standard three-level tree (root by type → type dir by id → name
    /// dir by language, whose entries are the data entries — MS-PE
    /// §6.4). Offsets are assigned by layout, deterministic in (type,
    /// id) order.
    fn resource_section(leaves: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        use std::collections::BTreeMap;
        let mut by_type: BTreeMap<u32, BTreeMap<u32, Vec<u8>>> = BTreeMap::new();
        for (ty, id, data) in leaves {
            by_type.entry(*ty).or_default().insert(*id, data.clone());
        }
        // The root directory lists one entry per type; each type
        // directory lists its ids; each name directory holds the single
        // language-0 entry pointing at the data leaf; the data blocks
        // trail everything.
        let mut type_at = BTreeMap::new();
        let mut name_at = BTreeMap::new();
        let mut leaf_at = BTreeMap::new();
        let mut data_at = BTreeMap::new();
        let mut offset = 16usize + 8 * by_type.len();
        for (&ty, ids) in &by_type {
            type_at.insert(ty, offset);
            offset += 16 + 8 * ids.len();
            for &id in ids.keys() {
                name_at.insert((ty, id), offset);
                offset += 24; // name directory: one language entry
                leaf_at.insert((ty, id), offset);
                offset += 8;
            }
        }
        for (&ty, ids) in &by_type {
            for (id, data) in ids {
                data_at.insert((ty, *id), offset);
                offset += data.len();
            }
        }
        let mut out = vec![0u8; offset];
        // Root directory: [type] → the type directory.
        out[12..14].copy_from_slice(
            &(u16::try_from(by_type.len()).expect("fixture counts fit u16")).to_le_bytes(),
        );
        for (index, (&ty, &start)) in type_at.iter().enumerate() {
            let entry = 16 + index * 8;
            out[entry..entry + 4].copy_from_slice(&ty.to_le_bytes());
            let target = u32::try_from(start).expect("fixture offsets fit u32") | 0x8000_0000;
            out[entry + 4..entry + 8].copy_from_slice(&target.to_le_bytes());
        }
        // Type directories: [id] → the name directory.
        for (&ty, &start) in &type_at {
            let ids = &by_type[&ty];
            out[start + 12..start + 14].copy_from_slice(
                &(u16::try_from(ids.len()).expect("fixture counts fit u16")).to_le_bytes(),
            );
            for (index, &id) in ids.keys().enumerate() {
                let entry = start + 16 + index * 8;
                out[entry..entry + 4].copy_from_slice(&id.to_le_bytes());
                let target = u32::try_from(name_at[&(ty, id)]).expect("fixture offsets fit u32")
                    | 0x8000_0000;
                out[entry + 4..entry + 8].copy_from_slice(&target.to_le_bytes());
            }
        }
        // Name directories: language 0 → the data leaf (a data entry,
        // so no directory flag).
        for (&(ty, id), &start) in &name_at {
            out[start + 12..start + 14].copy_from_slice(&1u16.to_le_bytes());
            out[start + 16..start + 20].copy_from_slice(&0u32.to_le_bytes());
            let target = u32::try_from(leaf_at[&(ty, id)]).expect("fixture offsets fit u32");
            out[start + 20..start + 24].copy_from_slice(&target.to_le_bytes());
        }
        // Leaves and data blocks.
        for (&(ty, id), &start) in &leaf_at {
            let data = &by_type[&ty][&id];
            let data_start = u32::try_from(data_at[&(ty, id)]).expect("fixture offsets fit u32");
            out[start..start + 4].copy_from_slice(&data_start.to_le_bytes());
            let size = u32::try_from(data.len()).expect("fixture sizes fit u32");
            out[start + 4..start + 8].copy_from_slice(&size.to_le_bytes());
        }
        for (&(ty, id), &start) in &data_at {
            let data = &by_type[&ty][&id];
            out[start..start + data.len()].copy_from_slice(data);
        }
        out
    }

    /// The tree of one group icon (id 1) and its frame (id 1).
    fn icon_resources(group: &[u8], frame: &[u8]) -> Vec<u8> {
        resource_section(&[(14, 1, group.to_vec()), (3, 1, frame.to_vec())])
    }

    fn png_dimensions(png: &[u8]) -> (u32, u32) {
        assert!(png.starts_with(&PNG_SIGNATURE), "a real PNG signature");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        (width, height)
    }

    /// A ready-made PE with a 16×16 32-bpp icon, for the crate-level
    /// integration tests (the service's install/re-derive round-trips).
    pub(crate) fn fixture_exe_with_icon() -> Vec<u8> {
        let frame = bgra_frame(16, 16, (10, 200, 30));
        let group = group_icon(
            16,
            16,
            32,
            frame.len().try_into().expect("fixture size fits u32"),
            1,
        );
        build_pe(&icon_resources(&group, &frame))
    }

    #[test]
    fn pe_with_a_32bpp_icon_extracts_a_matching_png() {
        let frame = bgra_frame(16, 16, (10, 200, 30));
        let group = group_icon(
            16,
            16,
            32,
            frame.len().try_into().expect("fixture size fits u32"),
            1,
        );
        let png = extract_icon_png(&build_pe(&icon_resources(&group, &frame)))
            .expect("a 32-bpp icon must extract");
        assert_eq!(png_dimensions(&png), (16, 16));
    }

    #[test]
    fn largest_32bpp_frame_wins_over_smaller_and_over_24bpp() {
        let small = bgra_frame(8, 8, (1, 2, 3));
        let large = bgra_frame(32, 32, (4, 5, 6));
        let bgr = bgr_frame(48, 48, (7, 8, 9));
        let group = group_icon_multi(&[
            (
                8,
                8,
                32,
                small.len().try_into().expect("fixture size fits u32"),
                1,
            ),
            (
                32,
                32,
                32,
                large.len().try_into().expect("fixture size fits u32"),
                2,
            ),
            (
                48,
                48,
                24,
                bgr.len().try_into().expect("fixture size fits u32"),
                3,
            ),
        ]);
        let resources =
            resource_section(&[(14, 1, group), (3, 1, small), (3, 2, large), (3, 3, bgr)]);
        let png = extract_icon_png(&build_pe(&resources)).expect("an icon must extract");
        assert_eq!(
            png_dimensions(&png),
            (32, 32),
            "the largest 32-bpp frame wins over smaller or 24-bpp ones"
        );
    }

    #[test]
    fn embedded_png_frame_passes_through_unchanged() {
        let rgba = vec![255; 8 * 8 * 4];
        let png = png_encode(8, 8, &rgba).expect("encodes");
        let group = group_icon(
            8,
            8,
            32,
            png.len().try_into().expect("fixture size fits u32"),
            1,
        );
        let extracted = extract_icon_png(&build_pe(&icon_resources(&group, &png)))
            .expect("an embedded PNG icon must extract");
        assert_eq!(extracted, png, "the embedded PNG is stored as-is");
    }

    #[test]
    fn no_icon_resource_or_not_a_pe_degrades_to_none() {
        assert_eq!(extract_icon_png(b"not a pe at all"), None);
        assert_eq!(extract_icon_png(&build_pe(&[])), None, "no resource tree");
        assert_eq!(
            extract_icon_png(&build_pe(&[0u8; 0x200])),
            None,
            "resource directory without icons"
        );
        // A hostile shape must degrade, never panic: "MZ" + PE signature
        // with a zero-size optional header (the magic field is absent).
        let mut hostile = Vec::new();
        hostile.extend_from_slice(b"MZ");
        hostile.resize(0x40, 0);
        hostile[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        hostile.resize(0x80, 0);
        hostile.extend_from_slice(b"PE\0\0");
        hostile.extend_from_slice(&0x14Cu16.to_le_bytes());
        hostile.extend_from_slice(&0u16.to_le_bytes()); // zero sections
        hostile.extend_from_slice(&0u32.to_le_bytes());
        hostile.extend_from_slice(&0u32.to_le_bytes());
        hostile.extend_from_slice(&0u32.to_le_bytes());
        hostile.extend_from_slice(&0u16.to_le_bytes()); // zero optional size
        hostile.extend_from_slice(&0x0102u16.to_le_bytes());
        assert_eq!(
            extract_icon_png(&hostile),
            None,
            "a truncated optional header is no icon, not a panic"
        );
    }

    #[test]
    fn frame_to_png_decodes_24bpp_with_the_and_mask() {
        // A 2×2 24-bpp frame, mask hiding the top-left pixel.
        let mut frame = Vec::new();
        frame.extend_from_slice(&40u32.to_le_bytes());
        frame.extend_from_slice(&2u32.to_le_bytes());
        frame.extend_from_slice(&4u32.to_le_bytes()); // 2 rows + mask
        frame.extend_from_slice(&1u16.to_le_bytes());
        frame.extend_from_slice(&24u16.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.resize(40, 0);
        let xor_stride = 8; // 2 px × 3 bytes, aligned
        let mask_stride = 4;
        // Bottom-up: row 1 then row 0 (top); each row is 2 pixels plus
        // its 32-bit-aligned padding (2 bytes).
        for _ in 0..2 {
            for _ in 0..2 {
                frame.extend_from_slice(&[0, 0, 255]); // BGR: pure red
            }
            frame.extend_from_slice(&[0, 0]);
        }
        frame.resize(40 + 2 * xor_stride + 2 * mask_stride, 0);
        frame[40 + 2 * xor_stride + mask_stride] = 0x80; // top-left hidden
        let png = frame_to_png(&frame).expect("decodes");
        assert_eq!(png_dimensions(&png), (2, 2));
        eprintln!("png bytes 40..60: {:02x?}", &png[40..60]);
        // The raw scanlines start at 8 (signature) + 25 (IHDR chunk) + 8
        // (IDAT length + type) + 2 (zlib header) + 1 (stored-block header)
        // + 2 (LEN) + 2 (NLEN) = 48; the filter byte follows.
        assert_eq!(png[49], 255, "the fixture painted red (BGR flipped)");
        assert_eq!(png[52], 0, "the masked top-left pixel is transparent");
        assert_eq!(png[56], 255, "the unmasked neighbour stays opaque");
    }

    #[test]
    fn crc_and_adler_known_vectors() {
        // The standard CRC-32 check value...
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        // ...and the standard Adler-32 check value over the same input.
        assert_eq!(adler32(b"123456789"), 0x091E_01DE);
    }

    #[test]
    fn png_encoder_produces_a_valid_shape() {
        let rgba = vec![255u8; 4 * 4 * 4];
        let png = png_encode(4, 4, &rgba).expect("encodes");
        assert_eq!(png_dimensions(&png), (4, 4));
        assert!(png.starts_with(&PNG_SIGNATURE));
        // The standard IEND chunk ends every PNG.
        assert!(
            png.ends_with(&[0, 0, 0, 0, 73, 69, 78, 68, 0xAE, 0x42, 0x60, 0x82]),
            "a well-formed IEND trailer"
        );
        // The IDAT data begins after the signature (8), the IHDR chunk
        // (25), and the IDAT chunk's own length + type (8): the zlib
        // stream starts with the deflate header bytes.
        let zlib = &png[8 + 25 + 8..png.len() - 12];
        assert_eq!(zlib[0], 0x78);
        assert_eq!(zlib[1], 0x01);
    }
}

//! Windows Shell Link (`.lnk`) target decoding — MS-SHLLINK, restricted to
//! the fields discovery needs (#31): the header flags, the link-target ID
//! list (skipped structurally), and `LinkInfo`'s `LocalBasePath` +
//! `CommonPathSuffix` — the two fields that together name the shortcut's
//! target as a Windows path (e.g. `C:\Program Files\My Game\game.exe`),
//! in ANSI or UTF-16 per the `IsUnicode` flag.
//!
//! The format's full spec is far larger (strings, environments, volumes,
//! networks); this decoder walks past everything else by offset and never
//! interprets it. Malformed input degrades to `None`: discovery is
//! best-effort — a bad shortcut is skipped, never a session failure.

/// The 16-byte `LinkCLSID` every shell link header carries
/// (`00021401-0000-0000-C000-000000000046`).
const SHELL_LINK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

/// Header flag: a `LinkTargetIDList` follows the header.
const FLAG_HAS_LINK_TARGET_ID_LIST: u32 = 0x0000_0001;
/// Header flag: a `LinkInfo` structure follows the ID list.
const FLAG_HAS_LINK_INFO: u32 = 0x0000_0002;
/// Header flag: the link's strings and paths are UTF-16 rather than ANSI.
const FLAG_IS_UNICODE: u32 = 0x0000_0080;
/// `LinkInfo` flag: a `VolumeID` and a `LocalBasePath` are present.
const INFO_FLAG_VOLUME_ID_AND_LOCAL_BASE_PATH: u32 = 0x0000_0001;

/// Decode the target path of a `.lnk` file, or `None` when the bytes are
/// not a well-formed shell link carrying a local base path. The returned
/// path is Windows-shaped (`C:\…`); resolving it against a prefix's
/// `drive_c` is the caller's step.
pub(crate) fn parse_lnk_target(bytes: &[u8]) -> Option<String> {
    // The 76-byte header, magic and CLSID first — everything else is
    // garbage and skipped, never a candidate.
    if bytes.len() < 0x4C {
        return None;
    }
    if u32::from_le_bytes(bytes[0..4].try_into().ok()?) != 0x0000_004C {
        return None;
    }
    if bytes[4..20] != SHELL_LINK_CLSID {
        return None;
    }
    let flags = u32::from_le_bytes(bytes[0x14..0x18].try_into().ok()?);
    let unicode = flags & FLAG_IS_UNICODE != 0;
    // Walk past the ID list when present: a 2-byte size then that many
    // bytes — its shell-item structure is irrelevant to the target path.
    let mut pos = 0x4C;
    if flags & FLAG_HAS_LINK_TARGET_ID_LIST != 0 {
        let size = u16::from_le_bytes(bytes.get(pos..pos + 2)?.try_into().ok()?) as usize;
        if size == 0 {
            return None;
        }
        pos += 2 + size;
    }
    // Only a link carrying LinkInfo can name a local target.
    if flags & FLAG_HAS_LINK_INFO == 0 {
        return None;
    }
    let info = bytes.get(pos..)?;
    let info_size = u32::from_le_bytes(info.get(..4)?.try_into().ok()?) as usize;
    let info = info.get(..info_size)?;
    let header_size = u32::from_le_bytes(info.get(4..8)?.try_into().ok()?) as usize;
    let info_flags = u32::from_le_bytes(info.get(8..12)?.try_into().ok()?);
    if info_flags & INFO_FLAG_VOLUME_ID_AND_LOCAL_BASE_PATH == 0 || header_size < 0x1C {
        return None;
    }
    // `LocalBasePath` (start of the target path) and `CommonPathSuffix`
    // (its continuation) — concatenated they are the full target.
    let base_offset = u32::from_le_bytes(info.get(0x10..0x14)?.try_into().ok()?) as usize;
    let suffix_offset = u32::from_le_bytes(info.get(0x18..0x1C)?.try_into().ok()?) as usize;
    let base = read_string(info, base_offset, unicode)?;
    let suffix = read_string(info, suffix_offset, unicode).unwrap_or_default();
    Some(format!("{base}{suffix}"))
}

/// A null-terminated string at `offset`: UTF-16LE when `unicode`, else the
/// system ANSI codepage decoded per Windows-1252 (the Western default —
/// the common case for installers writing non-ASCII shortcut paths).
/// Missing terminator reads to the end of the structure.
fn read_string(data: &[u8], offset: usize, unicode: bool) -> Option<String> {
    let rest = data.get(offset..)?;
    if !unicode {
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        return Some(decode_ansi(&rest[..end]));
    }
    let end = rest
        .chunks_exact(2)
        .position(|pair| pair == [0, 0])
        .map_or(rest.len(), |index| index * 2);
    let units: Vec<u16> = rest[..end]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// ANSI strings in shell links use the system ANSI codepage — Windows-1252
/// for Western installers. The 0x80–0x9F window is mapped per CP1252 (the
/// range where it differs from Latin-1; the five undefined positions
/// degrade to U+FFFD), everything else byte-for-byte identity — so a
/// non-ASCII target decodes to the same characters the on-disk name
/// carries and the case-insensitive walk can still match it. Other ANSI
/// codepages (Cyrillic, Japanese, …) carry no marker in the link and
/// decode equally wrong but loss-free, exactly as an unknown codepage
/// must.
fn decode_ansi(bytes: &[u8]) -> String {
    const CP1252: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9F => CP1252[(byte - 0x80) as usize],
            _ => char::from(byte),
        })
        .collect()
}

#[cfg(test)]
/// Build a valid `.lnk` byte blob for storage tests: header + optional ID
/// list + LinkInfo carrying a `VolumeID`, `LocalBasePath` (complete or
/// leading), and `CommonPathSuffix` — joined by the parser into the full
/// target. `unicode` toggles the `IsUnicode` flag and the string encoding.
pub(crate) fn build_lnk(unicode: bool, id_list: bool, base: &str, suffix: &str) -> Vec<u8> {
    if unicode {
        let mut base_raw = Vec::new();
        for unit in base.encode_utf16() {
            base_raw.extend_from_slice(&unit.to_le_bytes());
        }
        let mut suffix_raw = Vec::new();
        for unit in suffix.encode_utf16() {
            suffix_raw.extend_from_slice(&unit.to_le_bytes());
        }
        build_lnk_raw(true, id_list, &base_raw, &suffix_raw)
    } else {
        build_lnk_raw(false, id_list, base.as_bytes(), suffix.as_bytes())
    }
}

#[cfg(test)]
/// The raw-bytes variant of [`build_lnk`]: the strings are written as the
/// given bytes (ANSI links may carry any codepage's bytes — e.g. a
/// Windows-1252 `é` as a single 0xE9), each null-terminated in the
/// link's encoding.
pub(crate) fn build_lnk_raw(unicode: bool, id_list: bool, base: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    // Header: magic, CLSID, flags, then zeros for the unused fields.
    bytes.extend_from_slice(&0x0000_004Cu32.to_le_bytes());
    bytes.extend_from_slice(&SHELL_LINK_CLSID);
    let mut flags = FLAG_HAS_LINK_INFO;
    if unicode {
        flags |= FLAG_IS_UNICODE;
    }
    if id_list {
        flags |= FLAG_HAS_LINK_TARGET_ID_LIST;
    }
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.resize(0x4C, 0);
    // Optional ID list: two bytes of size then the opaque items.
    if id_list {
        bytes.extend_from_slice(&[0x0E, 0x00]);
        bytes.extend_from_slice(&[
            0x1F, 0x50, 0xE0, 0x4F, 0xD0, 0x20, 0xEA, 0x3A, 0x69, 0x10, 0xA2, 0xD8, 0x08, 0x00,
        ]);
    }
    // LinkInfo: size first (patched at the end), header size 0x1C with the
    // volume/base path pair (all four trailing pointers), 0x14 without.
    let mut info = Vec::new();
    info.extend_from_slice(&0u32.to_le_bytes()); // LinkInfoSize, patched
    info.extend_from_slice(&0x1Cu32.to_le_bytes());
    info.extend_from_slice(&INFO_FLAG_VOLUME_ID_AND_LOCAL_BASE_PATH.to_le_bytes());
    info.extend_from_slice(&0x1Cu32.to_le_bytes()); // VolumeIDOffset
    info.extend_from_slice(&0x2Cu32.to_le_bytes()); // LocalBasePathOffset
    info.extend_from_slice(&0u32.to_le_bytes()); // CommonNetworkRelativeLinkOffset
    let suffix_offset_pos = info.len(); // 0x18
    info.extend_from_slice(&0u32.to_le_bytes()); // CommonPathSuffixOffset, patched
    // VolumeID block at 0x1C (four dwords: type, serial, empty label).
    info.extend_from_slice(&0x14u32.to_le_bytes());
    info.extend_from_slice(&3u32.to_le_bytes());
    info.extend_from_slice(&0x1234_5678u32.to_le_bytes());
    info.extend_from_slice(&0x14u32.to_le_bytes());
    // LocalBasePath at 0x2C.
    debug_assert_eq!(info.len(), 0x2C);
    push_bytes(&mut info, base, unicode);
    let suffix_offset = info.len();
    push_bytes(&mut info, suffix, unicode);
    let size = info.len() as u32;
    info[0..4].copy_from_slice(&size.to_le_bytes());
    info[suffix_offset_pos..suffix_offset_pos + 4]
        .copy_from_slice(&(suffix_offset as u32).to_le_bytes());
    bytes.extend_from_slice(&info);
    bytes
}

/// A null-terminated byte string in the fixture's encoding.
#[cfg(test)]
fn push_bytes(out: &mut Vec<u8>, text: &[u8], unicode: bool) {
    out.extend_from_slice(text);
    if unicode {
        out.extend_from_slice(&[0, 0]);
    } else {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_an_ansi_local_base_path() {
        let link = build_lnk(false, false, r"C:\Program Files\My Game", r"\bin\game.exe");
        assert_eq!(
            parse_lnk_target(&link).as_deref(),
            Some(r"C:\Program Files\My Game\bin\game.exe"),
            "base + suffix concatenate to the full target"
        );
    }

    #[test]
    fn decodes_ansi_strings_as_windows_1252() {
        // ANSI paths use the system codepage — Windows-1252 for Western
        // installers: 0xE9 is `é`, the 0x80–0x9F window maps per CP1252,
        // and the undefined positions degrade to U+FFFD rather than
        // silently corrupting the path.
        assert_eq!(decode_ansi(b"C:\\Caf\xE9"), "C:\\Caf\u{E9}");
        assert_eq!(decode_ansi(b"\x80\x82\x9F"), "\u{20AC}\u{201A}\u{178}");
        assert_eq!(decode_ansi(b"\x81\x8D\x90"), "\u{FFFD}\u{FFFD}\u{FFFD}");
        assert_eq!(decode_ansi(b"plain ASCII"), "plain ASCII");
    }

    #[test]
    fn decodes_a_cp1252_target_from_raw_link_bytes() {
        // End to end: an ANSI link whose target names a non-ASCII path —
        // the byte 0xE9 (`é` in CP1252) must decode to `é`, not U+FFFD.
        let link = build_lnk_raw(false, false, b"C:\\Program Files\\Caf\xE9\\game.exe", b"");
        assert_eq!(
            parse_lnk_target(&link).as_deref(),
            Some(r"C:\Program Files\Café\game.exe")
        );
    }

    #[test]
    fn decodes_a_unicode_target_and_skips_the_id_list() {
        let link = build_lnk(true, true, r"C:\Games\Balatro\balatro.exe", "");
        assert_eq!(
            parse_lnk_target(&link).as_deref(),
            Some(r"C:\Games\Balatro\balatro.exe"),
            "the ID list is walked without being interpreted"
        );
    }

    #[test]
    fn skips_non_links_and_truncated_headers() {
        assert_eq!(parse_lnk_target(b"opaque installer output"), None);
        assert_eq!(parse_lnk_target(b""), None);
        let mut truncated = build_lnk(true, true, "x", "");
        truncated.truncate(0x40);
        assert_eq!(parse_lnk_target(&truncated), None);
    }

    #[test]
    fn skips_links_without_link_info_or_without_a_local_path() {
        // No LinkInfo at all (garbage after the header).
        let mut bytes = vec![0x4C, 0, 0, 0];
        bytes.extend_from_slice(&SHELL_LINK_CLSID);
        bytes.extend_from_slice(&FLAG_HAS_LINK_TARGET_ID_LIST.to_le_bytes());
        bytes.resize(0x4C, 0);
        assert_eq!(
            parse_lnk_target(&bytes),
            None,
            "no LinkInfo means no local target"
        );
        // A LinkInfo whose flags omit the volume/base pair (e.g. a
        // network-only link) — offsets for the path are absent.
        let mut info = Vec::new();
        info.extend_from_slice(&0x18u32.to_le_bytes()); // LinkInfoSize
        info.extend_from_slice(&0x14u32.to_le_bytes()); // LinkInfoHeaderSize
        info.extend_from_slice(&0u32.to_le_bytes()); // LinkInfoFlags
        info.extend_from_slice(&0u32.to_le_bytes()); // CommonNetworkRelativeLinkOffset
        info.extend_from_slice(&0x14u32.to_le_bytes()); // CommonPathSuffixOffset
        info.push(0); // empty suffix
        let mut link = vec![0x4C, 0, 0, 0];
        link.extend_from_slice(&SHELL_LINK_CLSID);
        link.extend_from_slice(&FLAG_HAS_LINK_INFO.to_le_bytes());
        link.resize(0x4C, 0);
        link.extend_from_slice(&info);
        assert_eq!(parse_lnk_target(&link), None);
    }

    #[test]
    fn decodes_an_empty_base_with_a_suffix_only_path() {
        let link = build_lnk(false, false, "", r"\foo.exe");
        assert_eq!(parse_lnk_target(&link).as_deref(), Some(r"\foo.exe"));
    }

    #[test]
    fn a_wrong_clsid_is_rejected() {
        let mut link = build_lnk(true, true, "x", "");
        link[4] = 0xFF;
        assert_eq!(parse_lnk_target(&link), None);
    }
}

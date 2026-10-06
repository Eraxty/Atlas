//! A sealed file's articles in one compressed value (files.blob), see the
//! storage spec. Format v1, zstd compressed:
//! u8 version, varint count, then per segment: varint part+1 (0 = none),
//! zigzag varint bytes minus the previous segment's, varint domain, varint
//! local length, local bytes.

use std::io::{Error, ErrorKind, Result};

const VERSION: u8 = 1;
const LEVEL: i32 = 6;

/// One article of a sealed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seg {
    pub part: Option<i64>,
    pub bytes: i64,
    pub domain: i64,
    pub local: Vec<u8>,
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn cut_short() -> Error {
    Error::new(ErrorKind::UnexpectedEof, "blob cut short")
}

fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos).ok_or_else(cut_short)?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Error::new(ErrorKind::InvalidData, "varint too long"))
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Packs segments into a blob. Order is not kept, a copy is sorted by part
/// (whole file articles first) so neighbours compress well.
pub fn encode(segs: &[Seg]) -> Vec<u8> {
    let mut sorted: Vec<&Seg> = segs.iter().collect();
    sorted.sort_by(|a, b| (a.part.is_some(), a.part, &a.local).cmp(&(b.part.is_some(), b.part, &b.local)));
    let mut raw = vec![VERSION];
    put_varint(&mut raw, sorted.len() as u64);
    let mut prev_bytes = 0i64;
    for s in sorted {
        put_varint(&mut raw, s.part.map_or(0, |p| p as u64 + 1));
        put_varint(&mut raw, zigzag(s.bytes.wrapping_sub(prev_bytes)));
        prev_bytes = s.bytes;
        put_varint(&mut raw, s.domain as u64);
        put_varint(&mut raw, s.local.len() as u64);
        raw.extend_from_slice(&s.local);
    }
    zstd::bulk::compress(&raw, LEVEL).expect("zstd compress of an in memory buffer")
}

/// Unpacks a blob made by `encode`. Truncated or corrupt input is an error.
pub fn decode(blob: &[u8]) -> Result<Vec<Seg>> {
    parse(&zstd::stream::decode_all(blob)?, usize::MAX)
}

/// the most local bytes a segment may have to be sealed: a file with a longer
/// one stays rows (a message-id is 250 bytes at most, a stored one a domain
/// suffix less)
pub const MAX_LOCAL: usize = 512;

/// the most a segment takes unpacked, for bounding what a blob may expand to:
/// its local bytes (`MAX_LOCAL`, and up to 253 more when a compaction stores
/// the message-id with its domain suffix again) and four varints of 10 bytes
/// at most, 32 with the length's
const MAX_SEG_RAW: usize = 1024;
const _: () = assert!(MAX_LOCAL + 253 + 32 <= MAX_SEG_RAW);

/// `decode` for a blob of at most `max` segments: one that says it has more,
/// or unpacks to more than `max` segments could take, is an error and is
/// never held in memory.
pub fn decode_capped(blob: &[u8], max: usize) -> Result<Vec<Seg>> {
    use std::io::Read;
    let limit = (max as u64).saturating_mul(MAX_SEG_RAW as u64).saturating_add(16);
    let mut raw = Vec::new();
    zstd::stream::read::Decoder::new(blob)?.take(limit).read_to_end(&mut raw)?;
    if raw.len() as u64 >= limit {
        return Err(Error::new(ErrorKind::InvalidData, "blob unpacks to more than its cap"));
    }
    parse(&raw, max)
}

fn parse(raw: &[u8], max: usize) -> Result<Vec<Seg>> {
    if raw.first() != Some(&VERSION) {
        return Err(Error::new(ErrorKind::InvalidData, "unknown blob version"));
    }
    let mut pos = 1;
    let count = get_varint(raw, &mut pos)? as usize;
    if count > max {
        return Err(Error::new(ErrorKind::InvalidData, "blob has more segments than its cap"));
    }
    // every segment takes at least 4 bytes, so a bigger count is corrupt
    let mut segs = Vec::with_capacity(count.min(raw.len() / 4));
    let mut prev_bytes = 0i64;
    for _ in 0..count {
        let part = match get_varint(raw, &mut pos)? {
            0 => None,
            p => Some((p - 1) as i64),
        };
        let bytes = prev_bytes.wrapping_add(unzigzag(get_varint(raw, &mut pos)?));
        prev_bytes = bytes;
        let domain = get_varint(raw, &mut pos)? as i64;
        let len = usize::try_from(get_varint(raw, &mut pos)?).map_err(|_| cut_short())?;
        let end = pos.checked_add(len).ok_or_else(cut_short)?;
        let local = raw.get(pos..end).ok_or_else(cut_short)?.to_vec();
        pos = end;
        segs.push(Seg { part, bytes, domain, local });
    }
    Ok(segs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(part: Option<i64>, bytes: i64, domain: i64, local: &[u8]) -> Seg {
        Seg { part, bytes, domain, local: local.to_vec() }
    }

    #[test]
    fn a_blob_past_its_cap_is_an_error() {
        let segs: Vec<Seg> = (0..10).map(|i| seg(Some(i), 5, 0, b"\x00<a@b>")).collect();
        let blob = encode(&segs);
        assert_eq!(decode_capped(&blob, 10).unwrap().len(), 10);
        assert!(decode_capped(&blob, 9).is_err(), "more segments than the cap");
        // a count that fits the cap, with more unpacked than that many take
        let mut raw = vec![VERSION];
        put_varint(&mut raw, 1);
        raw.resize(10_000, 0);
        assert!(decode_capped(&zstd::bulk::compress(&raw, 1).unwrap(), 1).is_err());
        assert!(decode_capped(b"garbage", 10).is_err());
    }

    #[test]
    fn the_longest_sealable_segment_decodes_under_the_cap() {
        let segs =
            vec![
                Seg { part: Some(i64::MAX - 1), bytes: i64::MIN, domain: i64::MAX, local: vec![7; MAX_LOCAL + 253] };
                3
            ];
        assert_eq!(decode_capped(&encode(&segs), 3).unwrap(), segs);
    }

    #[test]
    fn segments_come_back_exactly() {
        let segs = vec![
            seg(Some(2), 750_000, 3, b"\x06\x05abc"),
            seg(Some(1), 750_123, 3, b"\x01\xde\xad"),
            seg(None, 0, 0, b"\x00<whole@id>"),
            seg(Some(3), 12, 9, b""),
        ];
        let mut back = decode(&encode(&segs)).unwrap();
        let mut want = segs.clone();
        back.sort_by(|a, b| (a.part, &a.local).cmp(&(b.part, &b.local)));
        want.sort_by(|a, b| (a.part, &a.local).cmp(&(b.part, &b.local)));
        assert_eq!(back, want);
        assert!(decode(&encode(&[])).unwrap().is_empty());
    }

    #[test]
    fn a_big_file_is_small() {
        let segs: Vec<Seg> = (1..=1000)
            .map(|p| {
                seg(
                    Some(p),
                    768_000,
                    1,
                    &[1, (p % 251) as u8, (p * 7 % 251) as u8, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9],
                )
            })
            .collect();
        let size = encode(&segs).len();
        assert!(size < 1000 * 20, "{size} bytes for 1000 segments");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(b"not zstd").is_err());
    }

    #[test]
    fn truncated_or_corrupt_input_is_an_error() {
        let segs: Vec<Seg> = (1..=50).map(|p| seg(Some(p), 1000 + p, 2, b"\x01abcdef")).collect();
        let blob = encode(&segs);
        for cut in 0..blob.len() {
            assert!(decode(&blob[..cut]).is_err(), "cut at {cut}");
        }
        // valid zstd around a payload that lies about its contents
        for raw in [
            &[2u8, 0][..],                                                                    // unknown version
            &[1u8][..],                                                                       // no count
            &[1, 5][..],                                                                      // count with no segments
            &[1, 1, 1, 0, 0, 200][..], // local longer than the data
            &[1, 1, 1, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01][..], // length near u64::MAX
            &[1, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01][..], // varint too long
        ] {
            assert!(decode(&zstd::bulk::compress(raw, 1).unwrap()).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn extreme_values_survive() {
        let segs = vec![seg(Some(0), i64::MAX, i64::MAX, b"a"), seg(Some(1), 0, 0, b"b"), seg(Some(2), -5, 1, b"c")];
        assert_eq!(decode(&encode(&segs)).unwrap(), segs);
    }
}

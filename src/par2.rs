//! Pull real filenames out of a base .par2 file (deobfuscation).

use std::sync::LazyLock;

use regex::Regex;

static BASE_PAR2_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\.par2(?:["\s]|$)"#).unwrap());
static VOL_PAR2_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.vol\d+[+-]\d+\.par2").unwrap());

const MAGIC: &[u8; 8] = b"PAR2\0PKT";
const FILE_DESC: &[u8; 16] = b"PAR 2.0\0FileDesc";
const HEADER_LEN: usize = 64;

pub fn is_base_par2(subject: &str) -> bool {
    BASE_PAR2_RE.is_match(subject) && !VOL_PAR2_RE.is_match(subject)
}

/// Every (filename, size) described by FileDesc packets.
pub fn file_descriptions(data: &[u8]) -> Vec<(String, u64)> {
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut pos = 0;

    while pos + HEADER_LEN <= data.len() {
        if &data[pos..pos + 8] != MAGIC {
            // resync on the next packet header
            match data[pos + 1..].windows(8).position(|w| w == MAGIC) {
                Some(skip) => {
                    pos += skip + 1;
                    continue;
                }
                None => break,
            }
        }

        let len = u64::from_le_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize;

        if len < HEADER_LEN || !len.is_multiple_of(4) || pos.checked_add(len).is_none_or(|end| end > data.len()) {
            pos += 8;
            continue;
        }

        let packet = &data[pos..pos + len];

        if &packet[48..64] == FILE_DESC && len >= HEADER_LEN + 56 {
            let body = &packet[HEADER_LEN..];
            let size = u64::from_le_bytes(body[48..56].try_into().unwrap());
            let raw_name = &body[56..];
            let end = raw_name.iter().position(|b| *b == 0).unwrap_or(raw_name.len());
            let name = String::from_utf8_lossy(&raw_name[..end]).trim().to_string();

            if !name.is_empty() && !files.iter().any(|(n, _)| *n == name) {
                files.push((name, size));
            }
        }

        pos += len;
    }

    files
}

/// Biggest file in the par2 set, which is the best guess at the release name.
pub fn display_name(data: &[u8]) -> Option<String> {
    file_descriptions(data).into_iter().max_by_key(|(_, size)| *size).map(|(name, _)| name)
}

#[cfg(test)]
pub(crate) fn file_desc_packet(name: &str, size: u64) -> Vec<u8> {
    let mut name_bytes = name.as_bytes().to_vec();
    while !name_bytes.len().is_multiple_of(4) {
        name_bytes.push(0);
    }

    let len = (HEADER_LEN + 56 + name_bytes.len()) as u64;
    let mut p = Vec::new();
    p.extend_from_slice(MAGIC);
    p.extend_from_slice(&len.to_le_bytes());
    p.extend_from_slice(&[0u8; 16]); // packet md5
    p.extend_from_slice(&[1u8; 16]); // set id
    p.extend_from_slice(FILE_DESC);
    p.extend_from_slice(&[2u8; 16]); // file id
    p.extend_from_slice(&[3u8; 16]); // md5
    p.extend_from_slice(&[4u8; 16]); // md5 16k
    p.extend_from_slice(&size.to_le_bytes());
    p.extend_from_slice(&name_bytes);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_par2_detection() {
        assert!(is_base_par2(r#"[1/9] - "abc.par2" yEnc (1/1)"#));
        assert!(is_base_par2("abc.par2"));
        assert!(!is_base_par2(r#""abc.vol00+01.par2" yEnc (1/1)"#));
        assert!(!is_base_par2(r#""abc.rar" yEnc (1/1)"#));
    }

    #[test]
    fn picks_largest_file() {
        let mut data = b"junk".to_vec();
        data.extend(file_desc_packet("small.nfo", 10));
        data.extend(file_desc_packet("Real.Movie.2024.mkv", 5_000_000));
        data.extend(file_desc_packet("small.nfo", 10));
        assert_eq!(display_name(&data).as_deref(), Some("Real.Movie.2024.mkv"));
        assert_eq!(display_name(b"nothing here"), None);
    }
}

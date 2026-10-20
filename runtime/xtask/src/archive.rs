//! The two archive formats the bundles need: reading the tar `git archive`
//! writes, and writing (and reading back) deflated ZIPs laid out as Python's
//! zipfile lays them out.

use std::collections::BTreeMap;

/// The regular files in a tar `git archive` wrote, by path. Directories and
/// symlinks are skipped; any other entry type is refused.
pub fn untar(raw: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    let (mut at, mut long_path) = (0, None);
    while let Some(header) = raw.get(at..at + 512) {
        if header.iter().all(|&b| b == 0) {
            return Ok(files);
        }
        let stored = octal(&header[148..156])?;
        let sum: u64 = header.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { b as u64 }).sum();
        if sum != stored {
            return Err(format!("tar header at byte {at} fails its checksum"));
        }
        let size = usize::try_from(octal(&header[124..136])?).map_err(|e| e.to_string())?;
        let body = raw.get(at + 512..at + 512 + size).ok_or("the tar ends inside an entry")?;
        at += 512 + size.div_ceil(512) * 512;
        match header[156] {
            b'g' => {}
            b'x' => long_path = pax_path(body)?,
            b'0' | 0 => {
                let path = match long_path.take() {
                    Some(p) => p,
                    None => match (text(&header[345..500])?, text(&header[..100])?) {
                        ("", name) => name.to_string(),
                        (prefix, name) => format!("{prefix}/{name}"),
                    },
                };
                files.insert(path, body.to_vec());
            }
            b'5' | b'2' => long_path = None,
            t => return Err(format!("unexpected tar entry type {:?}", t as char)),
        }
    }
    Err("the tar has no end-of-archive marker".into())
}

fn text(field: &[u8]) -> Result<&str, String> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..end]).map_err(|e| format!("tar path: {e}"))
}

fn octal(field: &[u8]) -> Result<u64, String> {
    let digits = text(field)?.trim_matches(' ');
    u64::from_str_radix(digits, 8).map_err(|e| format!("tar number {digits:?}: {e}"))
}

/// The `path` record of a pax extended header (`<len> path=<value>\n`).
fn pax_path(mut body: &[u8]) -> Result<Option<String>, String> {
    let mut path = None;
    while !body.is_empty() {
        let space = body.iter().position(|&b| b == b' ').ok_or("bad pax record")?;
        let len: usize = text(&body[..space])?.parse().map_err(|_| "bad pax record length")?;
        let record = body.get(space + 1..len).and_then(|r| r.strip_suffix(b"\n")).ok_or("bad pax record")?;
        if let Some(value) = record.strip_prefix(b"path=") {
            path = Some(String::from_utf8(value.to_vec()).map_err(|e| format!("pax path: {e}"))?);
        }
        body = &body[len..];
    }
    Ok(path)
}

/// 2026-10-04 00:00:00 as a DOS date and time: every entry's timestamp, so
/// the same files always give the same archive.
const DOS_DATE: u16 = (2026 - 1980) << 9 | 10 << 5 | 4;
const DOS_TIME: u16 = 0;
/// Version 2.0 (deflate) made on Unix, needing 2.0 to extract.
const MADE_BY: u16 = 3 << 8 | 20;
const NEEDS: u16 = 20;
/// A regular file, mode 0644.
const EXTERNAL_ATTR: u32 = 0o100644 << 16;

/// A ZIP of `files` in name order, each deflated at level 9.
pub fn zip(files: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data) in files {
        let packed = miniz_oxide::deflate::compress_to_vec(data, 9);
        let crc = crc32fast::hash(data);
        let flags: u16 = if name.is_ascii() { 0 } else { 0x800 };
        let offset = out.len() as u32;
        let shared = |v: &mut Vec<u8>| {
            for half in [flags, 8, DOS_TIME, DOS_DATE] {
                v.extend(half.to_le_bytes());
            }
            for word in [crc, packed.len() as u32, data.len() as u32] {
                v.extend(word.to_le_bytes());
            }
            v.extend((name.len() as u16).to_le_bytes());
            v.extend(0u16.to_le_bytes());
        };
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(NEEDS.to_le_bytes());
        shared(&mut out);
        out.extend(name.as_bytes());
        out.extend(&packed);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(MADE_BY.to_le_bytes());
        central.extend(NEEDS.to_le_bytes());
        shared(&mut central);
        for half in [0u16, 0, 0] {
            central.extend(half.to_le_bytes());
        }
        central.extend(EXTERNAL_ATTR.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let (directory_at, count) = (out.len() as u32, files.len() as u16);
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    for half in [0u16, 0, count, count] {
        out.extend(half.to_le_bytes());
    }
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(directory_at.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// The entries of a ZIP `zip` wrote, in directory order, each inflated and
/// checked against its size and CRC.
pub fn unzip(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).ok_or("truncated ZIP");
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or("truncated ZIP");
    let end = bytes.len().checked_sub(22).ok_or("truncated ZIP")?;
    if u32_at(end)? != 0x0605_4b50 {
        return Err("no ZIP end record".into());
    }
    let (count, mut at) = (u16_at(end + 10)?, u32_at(end + 16)?);
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if u32_at(at)? != 0x0201_4b50 {
            return Err("bad ZIP directory entry".into());
        }
        let (method, crc, packed, size, offset) = (u16_at(at + 10)?, u32_at(at + 16)?, u32_at(at + 20)?, u32_at(at + 24)?, u32_at(at + 42)?);
        let name_len = u16_at(at + 28)?;
        let name = bytes.get(at + 46..at + 46 + name_len).ok_or("truncated ZIP")?;
        let name = String::from_utf8(name.to_vec()).map_err(|e| format!("ZIP entry name: {e}"))?;
        at += 46 + name_len + u16_at(at + 30)? + u16_at(at + 32)?;
        if u32_at(offset)? != 0x0403_4b50 || bytes.get(offset + 30..offset + 30 + name_len) != Some(name.as_bytes()) {
            return Err(format!("{name}: local header does not match the directory"));
        }
        let start = offset + 30 + name_len + u16_at(offset + 28)?;
        let stored = bytes.get(start..start + packed).ok_or("truncated ZIP")?;
        let data = match method {
            0 => stored.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec(stored).map_err(|e| format!("{name}: {e:?}"))?,
            m => return Err(format!("{name}: unsupported compression method {m}")),
        };
        if data.len() != size || crc32fast::hash(&data) as usize != crc {
            return Err(format!("{name}: corrupt ZIP entry"));
        }
        entries.push((name, data));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    struct Repo(PathBuf);

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    #[test]
    fn untar_reads_what_git_archive_writes() {
        let repo = Repo(std::env::temp_dir().join(format!("xtask-untar-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&repo.0);
        let split = format!("{}/{}.txt", "d".repeat(80), "f".repeat(60));
        let unsplittable = format!("{}.txt", "n".repeat(120));
        let files: [(&str, &[u8]); 5] = [
            ("top.txt", b"top\n"),
            ("a/b/nested.bin", &[0, 1, 2, 255]),
            (&split, b"split into prefix and name\n"),
            (&unsplittable, b"named by a pax header\n"),
            ("run.ps1", b"line one\nline two\n"),
        ];
        for (path, data) in files {
            let p = repo.0.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, data).unwrap();
        }
        std::fs::write(repo.0.join(".gitattributes"), "*.ps1 text eol=crlf\n").unwrap();
        std::os::unix::fs::symlink("top.txt", repo.0.join("link")).unwrap();
        git(&repo.0, &["init", "-q"]);
        git(&repo.0, &["add", "-A"]);
        git(&repo.0, &["commit", "-q", "-m", "fixture"]);
        let got = untar(&git(&repo.0, &["archive", "--format=tar", "HEAD"])).unwrap();
        let mut want: BTreeMap<String, Vec<u8>> = files.iter().map(|(p, d)| (p.to_string(), d.to_vec())).collect();
        want.insert(".gitattributes".into(), b"*.ps1 text eol=crlf\n".to_vec());
        // git archive applies the attributes, as the bundles rely on.
        want.insert("run.ps1".into(), b"line one\r\nline two\r\n".to_vec());
        assert_eq!(got, want);
    }

    #[test]
    fn untar_refuses_a_corrupt_or_truncated_tar() {
        let mut header = [0u8; 512];
        header[..4].copy_from_slice(b"file");
        header[124..135].copy_from_slice(b"00000000005");
        header[156] = b'0';
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut tar = header.to_vec();
        tar.extend(b"hello");
        tar.resize(1024 + 1024, 0);
        assert_eq!(untar(&tar).unwrap(), BTreeMap::from([("file".to_string(), b"hello".to_vec())]));
        assert!(untar(&tar[..600]).unwrap_err().contains("end-of-archive"));
        assert!(untar(&tar[..515]).unwrap_err().contains("inside an entry"));
        tar[0] = b'g';
        assert!(untar(&tar).unwrap_err().contains("checksum"));
        tar[0] = b'f';
        tar[156] = b'3';
        let sum = sum - b'0' as u32 + b'3' as u32;
        tar[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        assert!(untar(&tar).unwrap_err().contains("entry type '3'"));
    }

    #[test]
    fn zip_round_trips_with_fixed_metadata() {
        let files = BTreeMap::from([
            ("b/inner.txt".to_string(), b"inner ".repeat(400)),
            ("a.txt".to_string(), Vec::new()),
            ("caf\u{e9}.txt".to_string(), vec![7; 3]),
        ]);
        let bytes = zip(&files);
        assert_eq!(zip(&files), bytes, "the same files give the same bytes");
        let entries = unzip(&bytes).unwrap();
        assert_eq!(entries, files.clone().into_iter().collect::<Vec<_>>());
        // The first local header: needs 2.0, no flags, deflate, 00:00 2026-10-04.
        assert_eq!(bytes[4..14], [20, 0, 0, 0, 8, 0, 0, 0, 0x44, 0x5d]);
        let directory = bytes.windows(4).position(|w| w == [0x50, 0x4b, 1, 2]).unwrap();
        assert_eq!(bytes[directory + 4..directory + 6], [20, 3], "made by 2.0 on Unix");
        assert_eq!(bytes[directory + 38..directory + 42], (0o100644u32 << 16).to_le_bytes());
        let at = |name: &str| bytes.windows(name.len()).position(|w| w == name.as_bytes()).unwrap();
        let non_ascii = at("caf\u{e9}.txt");
        assert_eq!(bytes[non_ascii - 24..non_ascii - 22], [0, 8], "UTF-8 name flag");
        let mut corrupt = bytes.clone();
        corrupt[at("b/inner.txt") + "b/inner.txt".len() + 3] ^= 0xff;
        assert!(unzip(&corrupt).is_err());
        assert!(unzip(&bytes[..bytes.len() - 1]).is_err());
    }
}

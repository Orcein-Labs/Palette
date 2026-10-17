//! Expired session receipts are archived, not deleted. When a closed session's
//! receipts directory passes the retention, its files are appended to
//! `~/.palette/docs/receipts/<YYYY-MM>.zip`, named `<session dir>/<relative
//! path>`, and only then is the directory removed. The month is the UTC month
//! of the directory's last modification.
//!
//! The archive is a plain stored (uncompressed) ZIP without ZIP64, so it
//! stops at 4 GiB; an append that would pass that is refused and the receipts
//! stay where they are. Every append writes a new archive beside the old one,
//! syncs it and renames it over, under an exclusive lock on `.lock` in the
//! archive directory. The Python launcher writes the same archive under the
//! same lock.

use crate::sandbox;
use crate::util;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The largest archive written: the ZIP format without ZIP64.
const ARCHIVE_LIMIT: u64 = u32::MAX as u64;
const LOCAL_HEADER: u64 = 30;
const CENTRAL_HEADER: u64 = 46;
const END_RECORD: u64 = 22;

/// Where expired receipts under `receipts_root` (`~/.palette/receipts`) are
/// archived: `~/.palette/docs/receipts`.
pub fn archive_dir(receipts_root: &Path) -> PathBuf {
    receipts_root.parent().unwrap_or(Path::new("/")).join("docs").join("receipts")
}

/// Archives the expired receipts directory `dir` into `archive` and removes
/// it. On any error the directory and its owner record are kept.
pub fn retire(dir: &Path, archive: &Path) -> Result<(), String> {
    retire_with_limit(dir, archive, ARCHIVE_LIMIT)
}

fn retire_with_limit(dir: &Path, archive: &Path, limit: u64) -> Result<(), String> {
    open_archive_dir(archive)?;
    let _lock = lock(archive)?;
    // Another session may have archived and removed it while this one waited.
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(format!("{} is not a directory", dir.display())),
        Err(_) => return Ok(()),
    }
    if fs::symlink_metadata(sandbox::clone_owner_path(dir)).is_err() {
        return Ok(());
    }
    let modified = sandbox::last_modified(dir).ok_or(format!("reading {}", dir.display()))?;
    let target = archive.join(format!("{}.zip", month(unix_secs(modified))));
    append(&target, dir, limit)?;
    sandbox::remove_owned_dir(dir);
    Ok(())
}

fn open_archive_dir(archive: &Path) -> Result<(), String> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(archive)
        .map_err(|e| format!("creating {}: {e}", archive.display()))?;
    let meta = fs::symlink_metadata(archive).map_err(|e| format!("checking {}: {e}", archive.display()))?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
        return Err(format!("refusing {}: it must be a directory this user owns", archive.display()));
    }
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(archive, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("restricting {}: {e}", archive.display()))?;
    }
    Ok(())
}

fn lock(archive: &Path) -> Result<File, String> {
    let path = archive.join(".lock");
    let file = OpenOptions::new().read(true).write(true).create(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!("locking {}: {}", path.display(), io::Error::last_os_error()));
    }
    Ok(file)
}

/// One central directory entry, enough to recognize an entry already archived.
struct Entry {
    name: Vec<u8>,
    crc: u32,
    size: u32,
}

/// An existing archive: its entries' data end at `data_end`, where the
/// central directory `central` starts.
struct Existing {
    file: File,
    data_end: u64,
    central: Vec<u8>,
    entries: Vec<Entry>,
}

/// The regular files under `dir` as (archive name, path), sorted. Symlinks
/// and special files are refused.
fn receipt_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let top = dir.file_name().and_then(|n| n.to_str()).ok_or(format!("unnamed directory {}", dir.display()))?;
    let mut files = Vec::new();
    let mut pending = vec![(top.to_string(), dir.to_path_buf())];
    while let Some((prefix, path)) = pending.pop() {
        let mut entries: Vec<_> = fs::read_dir(&path).map_err(|e| format!("reading {}: {e}", path.display()))?
            .collect::<Result<_, _>>().map_err(|e| format!("reading {}: {e}", path.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let child = entry.path();
            let name = entry.file_name().into_string()
                .map_err(|_| format!("refusing {}: the name is not UTF-8", child.display()))?;
            let kind = entry.file_type().map_err(|e| format!("checking {}: {e}", child.display()))?;
            let archived = format!("{prefix}/{name}");
            if kind.is_dir() {
                pending.push((archived, child));
            } else if kind.is_file() {
                files.push((archived, child));
            } else {
                return Err(format!("refusing {}: not a regular file or directory", child.display()));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn open_existing(target: &Path) -> Result<Option<Existing>, String> {
    let mut file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(target) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("opening {}: {e}", target.display())),
    };
    let bad = |why: &str| format!("refusing {}: {why}", target.display());
    let meta = file.metadata().map_err(|e| bad(&e.to_string()))?;
    if !meta.is_file() {
        return Err(bad("not a regular file"));
    }
    let len = meta.len();
    let tail_len = len.min(END_RECORD + 0xFFFF);
    let mut tail = vec![0; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len)).and_then(|_| file.read_exact(&mut tail)).map_err(|e| bad(&e.to_string()))?;
    let end = (0..=tail.len().saturating_sub(END_RECORD as usize)).rev()
        .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] && i + 22 + u16_at(&tail, i + 20) as usize == tail.len())
        .ok_or_else(|| bad("no ZIP end record"))?;
    let (disk, cd_disk, here, count) = (u16_at(&tail, end + 4), u16_at(&tail, end + 6), u16_at(&tail, end + 8), u16_at(&tail, end + 10));
    let (cd_size, cd_offset) = (u32_at(&tail, end + 12) as u64, u32_at(&tail, end + 16) as u64);
    if disk != 0 || cd_disk != 0 || here != count || count == 0xFFFF || cd_offset == ARCHIVE_LIMIT {
        return Err(bad("a multi-disk or ZIP64 archive"));
    }
    if cd_offset + cd_size != len - tail_len + end as u64 {
        return Err(bad("the central directory is not where the end record says"));
    }
    let mut central = vec![0; cd_size as usize];
    file.seek(SeekFrom::Start(cd_offset)).and_then(|_| file.read_exact(&mut central)).map_err(|e| bad(&e.to_string()))?;
    let mut entries = Vec::new();
    let mut at = 0;
    for _ in 0..count {
        if at + CENTRAL_HEADER as usize > central.len() || u32_at(&central, at) != 0x0201_4b50 {
            return Err(bad("a malformed central directory"));
        }
        let (n, e, c) = (u16_at(&central, at + 28) as usize, u16_at(&central, at + 30) as usize, u16_at(&central, at + 32) as usize);
        let name_at = at + CENTRAL_HEADER as usize;
        if name_at + n + e + c > central.len() {
            return Err(bad("a malformed central directory"));
        }
        entries.push(Entry { name: central[name_at..name_at + n].to_vec(), crc: u32_at(&central, at + 16), size: u32_at(&central, at + 24) });
        at = name_at + n + e + c;
    }
    if at != central.len() {
        return Err(bad("a malformed central directory"));
    }
    Ok(Some(Existing { file, data_end: cd_offset, central, entries }))
}

/// Appends the files of `dir` to the archive `target`, through a synced
/// temporary file renamed over it. Files already archived with the same
/// contents are skipped, so a retry after a crash adds nothing twice.
fn append(target: &Path, dir: &Path, limit: u64) -> Result<(), String> {
    let files = receipt_files(dir)?;
    let existing = open_existing(target)?;
    let mut fresh = Vec::new();
    for (name, path) in files {
        let source = open_source(&path)?;
        let size = source.metadata().map_err(|e| format!("checking {}: {e}", path.display()))?.len();
        let crc = checksum(source, &path)?;
        match existing.as_ref().and_then(|x| x.entries.iter().find(|e| e.name == name.as_bytes())) {
            Some(e) if e.crc == crc.0 && e.size as u64 == crc.1 => continue,
            Some(_) => return Err(format!("{} already holds a different {name}", target.display())),
            None => fresh.push((name, path, size)),
        }
    }
    if fresh.is_empty() {
        return Ok(());
    }
    let old_len = existing.as_ref().map_or(0, |x| x.data_end + x.central.len() as u64 + END_RECORD);
    let old_count = existing.as_ref().map_or(0, |x| x.entries.len());
    let projected = old_len.max(END_RECORD)
        + fresh.iter().map(|(n, _, s)| LOCAL_HEADER + CENTRAL_HEADER + 2 * n.len() as u64 + s).sum::<u64>();
    if projected > limit || old_count + fresh.len() >= 0xFFFF || fresh.iter().any(|(n, _, _)| n.len() > 0xFFFF) {
        return Err(format!("{} would pass 4 GiB or 65,534 entries, the limit without ZIP64", target.display()));
    }

    let parent = target.parent().unwrap_or(Path::new("/"));
    let tmp = parent.join(format!(".{}.{}.tmp", target.file_name().unwrap_or_default().to_string_lossy(), util::hex(8)));
    let mut out = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&tmp)
        .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
    let written = (|| -> Result<(), String> {
        let io = |e: io::Error| format!("writing {}: {e}", tmp.display());
        let mut central = Vec::new();
        let mut offset = 0u64;
        if let Some(x) = &existing {
            let mut source = &x.file;
            source.seek(SeekFrom::Start(0)).map_err(io)?;
            offset = io::copy(&mut source.take(x.data_end), &mut out).map_err(io)?;
            if offset != x.data_end {
                return Err(format!("{} changed while it was copied", target.display()));
            }
            central.extend_from_slice(&x.central);
        }
        for (name, path, size) in &fresh {
            let mut source = open_source(path)?;
            let meta = source.metadata().map_err(|e| format!("checking {}: {e}", path.display()))?;
            let (time, date) = dos_time(meta.mtime());
            let flags: u16 = if name.is_ascii() { 0 } else { 1 << 11 };
            let header = |crc: u32| {
                let mut h = Vec::new();
                h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
                for v in [10u16, flags, 0, time, date] { h.extend_from_slice(&v.to_le_bytes()); }
                for v in [crc, *size as u32, *size as u32] { h.extend_from_slice(&v.to_le_bytes()); }
                for v in [name.len() as u16, 0] { h.extend_from_slice(&v.to_le_bytes()); }
                h.extend_from_slice(name.as_bytes());
                h
            };
            out.write_all(&header(0)).map_err(io)?;
            let (crc, copied) = copy_with_crc(&mut source, &mut out).map_err(io)?;
            if copied != *size {
                return Err(format!("{} changed while it was archived", path.display()));
            }
            out.seek(SeekFrom::Start(offset)).and_then(|_| out.write_all(&header(crc))).map_err(io)?;
            out.seek(SeekFrom::End(0)).map_err(io)?;

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            for v in [(3u16 << 8) | 20, 10, flags, 0, time, date] { central.extend_from_slice(&v.to_le_bytes()); }
            for v in [crc, *size as u32, *size as u32] { central.extend_from_slice(&v.to_le_bytes()); }
            for v in [name.len() as u16, 0, 0, 0, 0] { central.extend_from_slice(&v.to_le_bytes()); }
            central.extend_from_slice(&((meta.mode() & 0xFFFF) << 16).to_le_bytes());
            central.extend_from_slice(&(offset as u32).to_le_bytes());
            central.extend_from_slice(name.as_bytes());
            offset += LOCAL_HEADER + name.len() as u64 + size;
        }
        let count = (old_count + fresh.len()) as u16;
        if offset + central.len() as u64 + END_RECORD > limit {
            return Err(format!("{} would pass 4 GiB, the limit without ZIP64", target.display()));
        }
        let mut end = Vec::new();
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        for v in [0u16, 0, count, count] { end.extend_from_slice(&v.to_le_bytes()); }
        for v in [central.len() as u32, offset as u32] { end.extend_from_slice(&v.to_le_bytes()); }
        end.extend_from_slice(&0u16.to_le_bytes());
        out.write_all(&central).and_then(|_| out.write_all(&end)).and_then(|_| out.sync_all()).map_err(io)?;
        fs::rename(&tmp, target).map_err(|e| format!("replacing {}: {e}", target.display()))?;
        File::open(parent).and_then(|d| d.sync_all()).map_err(|e| format!("syncing {}: {e}", parent.display()))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn open_source(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    match file.metadata() {
        Ok(m) if m.is_file() => Ok(file),
        _ => Err(format!("refusing {}: not a regular file", path.display())),
    }
}

fn checksum(mut file: File, path: &Path) -> Result<(u32, u64), String> {
    copy_with_crc(&mut file, &mut io::sink()).map_err(|e| format!("reading {}: {e}", path.display()))
}

/// Copies `from` to `to` and returns the CRC-32 (IEEE) and length of what was copied.
fn copy_with_crc(from: &mut impl Read, to: &mut impl Write) -> io::Result<(u32, u64)> {
    let table = crc_table();
    let mut crc = !0u32;
    let mut total = 0u64;
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for &b in &buf[..n] {
            crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        to.write_all(&buf[..n])?;
        total += n as u64;
    }
    Ok((!crc, total))
}

fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    table
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// UTC (year, month, day, hour, minute, second) of a Unix time.
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400) as u32);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3600, rem / 60 % 60, rem % 60)
}

/// `YYYY-MM`, the UTC month of a Unix time.
fn month(secs: i64) -> String {
    let (y, m, ..) = civil(secs);
    format!("{y:04}-{m:02}")
}

/// The MS-DOS (time, date) fields of a Unix time, in UTC, clamped to 1980–2107.
fn dos_time(secs: i64) -> (u16, u16) {
    let (y, mo, d, h, mi, s) = civil(secs);
    if y < 1980 {
        return (0, (1 << 5) | 1);
    }
    let y = y.min(2107) as u32;
    (((h << 11) | (mi << 5) | (s / 2)) as u16, (((y - 1980) << 9) | (mo << 5) | d) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// 2020-02-29T12:00:00Z: a leap day, years past any retention.
    const LEAP_DAY: u64 = 1_582_977_600;

    fn dead_record() -> String {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let namespace = fs::read_link("/proc/self/ns/pid").unwrap();
        format!("{} {pid} 1\n", namespace.display())
    }

    fn set_time(path: &Path, secs: u64) {
        File::open(path).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    /// A closed session's receipts dir with `files`, owned by `record`, all
    /// last modified at `secs`.
    fn receipts(root: &Path, name: &str, files: &[(&str, &[u8])], record: &str, secs: u64) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        for (rel, bytes) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            set_time(&path, secs);
            set_time(path.parent().unwrap(), secs);
        }
        set_time(&dir, secs);
        fs::write(sandbox::clone_owner_path(&dir), record).unwrap();
        dir
    }

    fn unzip(args: &[&str], zip: &Path) -> std::process::Output {
        Command::new("unzip").args(args).arg(zip).output().unwrap()
    }

    fn member(zip: &Path, name: &str) -> Vec<u8> {
        let out = Command::new("unzip").arg("-p").arg(zip).arg(name).output().unwrap();
        assert!(out.status.success(), "unzip -p {name}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    fn names(zip: &Path) -> Vec<String> {
        let out = unzip(&["-Z1"], zip);
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().lines().map(str::to_string).collect()
    }

    #[test]
    fn expired_receipts_are_zipped_by_month_and_then_removed() {
        let base = util::mkdtemp("palette-receipt-archive-").unwrap();
        let root = base.join("receipts");
        let archive = archive_dir(&root);
        assert_eq!(archive, base.join("docs").join("receipts"));
        let dead = dead_record();
        let blob: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
        let first = receipts(&root, "palette-aaaa", &[("receipts.jsonl", b"{\"a\":1}\n"), ("sub/blob.bin", &blob)], &dead, LEAP_DAY);
        let live = receipts(&root, "palette-live", &[("receipts.jsonl", b"live\n")], &sandbox::clone_owner_record().unwrap(), LEAP_DAY);
        let recent_secs = unix_secs(SystemTime::now()) as u64 - 2 * 86_400;
        let recent = receipts(&root, "palette-recent", &[("receipts.jsonl", b"recent\n")], &dead, recent_secs);
        let retention = Duration::from_secs(30 * 86_400);

        let session = sandbox::create_receipts_dir(&root, "palette-new1", retention).unwrap();
        let zip = archive.join("2020-02.zip");
        assert!(fs::symlink_metadata(&first).is_err() && fs::symlink_metadata(sandbox::clone_owner_path(&first)).is_err());
        for kept in [&live, &recent, &session] {
            assert!(kept.is_dir() && sandbox::clone_owner_path(kept).exists(), "{} was touched", kept.display());
        }
        assert_eq!(fs::read(recent.join("receipts.jsonl")).unwrap(), b"recent\n");
        assert_eq!(fs::metadata(&archive).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&zip).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(unzip(&["-tq"], &zip).status.success(), "unzip -t rejected the archive");
        assert_eq!(names(&zip), ["palette-aaaa/receipts.jsonl", "palette-aaaa/sub/blob.bin"]);
        assert_eq!(member(&zip, "palette-aaaa/receipts.jsonl"), b"{\"a\":1}\n");
        assert_eq!(member(&zip, "palette-aaaa/sub/blob.bin"), blob);

        // A later session appends to the same month and starts a new one for another.
        let second = receipts(&root, "palette-bbbb", &[("receipts.jsonl", b"{\"b\":2}\n")], &dead, LEAP_DAY + 43_199); // 23:59:59Z
        let march = receipts(&root, "palette-cccc", &[("receipts.jsonl", b"{\"c\":3}\n")], &dead, LEAP_DAY + 43_200); // 2020-03-01T00:00Z
        sandbox::create_receipts_dir(&root, "palette-new2", retention).unwrap();
        assert!(fs::symlink_metadata(&second).is_err() && fs::symlink_metadata(&march).is_err());
        assert!(unzip(&["-tq"], &zip).status.success());
        assert_eq!(names(&zip), ["palette-aaaa/receipts.jsonl", "palette-aaaa/sub/blob.bin", "palette-bbbb/receipts.jsonl"]);
        assert_eq!(member(&zip, "palette-aaaa/sub/blob.bin"), blob);
        assert_eq!(member(&zip, "palette-bbbb/receipts.jsonl"), b"{\"b\":2}\n");
        assert_eq!(names(&archive.join("2020-03.zip")), ["palette-cccc/receipts.jsonl"]);
        let leftovers: Vec<_> = fs::read_dir(&archive).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 3, "temporary files left behind: {leftovers:?}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_archive_failure_keeps_the_receipts() {
        let base = util::mkdtemp("palette-receipt-archive-failure-").unwrap();
        let root = base.join("receipts");
        let archive = archive_dir(&root);
        let dead = dead_record();
        let kept = |dir: &Path| dir.is_dir() && sandbox::clone_owner_path(dir).exists();

        // A symlink among the receipts is refused, and nothing is archived.
        let linked = receipts(&root, "palette-link", &[("receipts.jsonl", b"x\n")], &dead, LEAP_DAY);
        std::os::unix::fs::symlink("/etc/hostname", linked.join("elsewhere")).unwrap();
        set_time(&linked, LEAP_DAY);
        assert!(retire(&linked, &archive).unwrap_err().contains("not a regular file or directory"));
        assert!(kept(&linked) && !archive.join("2020-02.zip").exists());
        fs::remove_file(linked.join("elsewhere")).unwrap();
        set_time(&linked, LEAP_DAY);

        // A damaged archive is not appended to.
        let zip = archive.join("2020-02.zip");
        fs::write(&zip, b"not a zip").unwrap();
        assert!(retire(&linked, &archive).unwrap_err().contains("no ZIP end record"));
        assert!(kept(&linked) && fs::read(&zip).unwrap() == b"not a zip");

        // An archive that is a symlink is refused.
        fs::remove_file(&zip).unwrap();
        std::os::unix::fs::symlink(base.join("decoy.zip"), &zip).unwrap();
        assert!(retire(&linked, &archive).is_err());
        assert!(kept(&linked) && !base.join("decoy.zip").exists());
        fs::remove_file(&zip).unwrap();

        // Past the size limit the receipts stay and the archive is unchanged.
        retire(&receipts(&root, "palette-small", &[("r", b"1\n")], &dead, LEAP_DAY), &archive).unwrap();
        let before = fs::read(&zip).unwrap();
        let big = receipts(&root, "palette-big", &[("r", &[7u8; 4096])], &dead, LEAP_DAY);
        assert!(retire_with_limit(&big, &archive, 2048).unwrap_err().contains("4 GiB"));
        assert!(kept(&big) && fs::read(&zip).unwrap() == before);

        // So does a sweep that cannot create the archive directory.
        let blocked = base.join("blocked");
        let receipts_root = blocked.join("receipts");
        let stuck = receipts(&receipts_root, "palette-stuck", &[("r", b"1\n")], &dead, LEAP_DAY);
        fs::write(blocked.join("docs"), b"a file where the archive dir goes").unwrap();
        sandbox::create_receipts_dir(&receipts_root, "palette-new", Duration::from_secs(86_400)).unwrap();
        assert!(kept(&stuck));
        assert_eq!(fs::read(stuck.join("r")).unwrap(), b"1\n");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_interrupted_retirement_is_not_archived_twice() {
        let base = util::mkdtemp("palette-receipt-archive-retry-").unwrap();
        let root = base.join("receipts");
        let archive = archive_dir(&root);
        let dir = receipts(&root, "palette-retry", &[("r", b"1\n")], &dead_record(), LEAP_DAY);
        open_archive_dir(&archive).unwrap();
        let zip = archive.join("2020-02.zip");
        append(&zip, &dir, ARCHIVE_LIMIT).unwrap(); // archived, then the host died before removing it
        retire(&dir, &archive).unwrap();
        assert!(!dir.exists());
        assert_eq!(names(&zip), ["palette-retry/r"]);

        let changed = receipts(&root, "palette-retry", &[("r", b"2\n")], &dead_record(), LEAP_DAY);
        assert!(retire(&changed, &archive).unwrap_err().contains("already holds a different"));
        assert!(changed.is_dir());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn months_and_dos_times_are_utc() {
        assert_eq!(month(LEAP_DAY as i64), "2020-02");
        assert_eq!(month(0), "1970-01");
        assert_eq!(month(951_868_799), "2000-02"); // 2000-02-29T23:59:59Z
        assert_eq!(month(951_868_800), "2000-03");
        assert_eq!(civil(1_792_195_199), (2026, 10, 16, 23, 59, 59));
        assert_eq!(dos_time(LEAP_DAY as i64), ((12 << 11) as u16, ((40 << 9) | (2 << 5) | 29) as u16));
        assert_eq!(dos_time(0), (0, (1 << 5) | 1));
        let (crc, n) = copy_with_crc(&mut &b"123456789"[..], &mut io::sink()).unwrap();
        assert_eq!((crc, n), (0xCBF4_3926, 9));
    }
}

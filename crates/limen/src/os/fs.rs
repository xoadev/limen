//! Files: what the gate reads and what `install` writes.

use limen_core::system::procfs::{self, Account};
use limen_core::trust::FileStat;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU32, Ordering};

const CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Link,
    Other,
}

impl FileType {
    pub fn wire(self) -> &'static str {
        match self {
            FileType::File => "file",
            FileType::Directory => "dir",
            FileType::Link => "link",
            FileType::Other => "other",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: String,
    pub kind: FileType,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub modified: i64,
}

impl FileInfo {
    fn of(path: &str, m: &fs::Metadata) -> Self {
        let t = m.file_type();
        let kind = if t.is_symlink() {
            FileType::Link
        } else if t.is_file() {
            FileType::File
        } else if t.is_dir() {
            FileType::Directory
        } else {
            FileType::Other
        };
        FileInfo {
            path: path.into(),
            kind,
            size: m.size(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            modified: m.mtime(),
        }
    }

    pub fn to_stat(&self) -> FileStat {
        FileStat {
            path: self.path.clone(),
            uid: self.uid,
            mode: self.mode & 0o7777,
            is_directory: self.kind == FileType::Directory,
            is_regular: self.kind == FileType::File,
        }
    }
}

/// A slice of a file's lines, and whether there was more of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSlice {
    pub lines: Vec<String>,
    pub eof: bool,
    pub binary: bool,
}

pub fn real_path(path: &str) -> Option<String> {
    fs::canonicalize(path).ok().map(|p| p.to_string_lossy().into_owned())
}

/// [path] itself, not what a link points to.
pub fn lstat(path: &str) -> Option<FileInfo> {
    fs::symlink_metadata(path).ok().map(|m| FileInfo::of(path, &m))
}

pub fn stat(path: &str) -> Option<FileInfo> {
    fs::metadata(path).ok().map(|m| FileInfo::of(path, &m))
}

pub fn exists(path: &str) -> bool {
    lstat(path).is_some()
}

pub fn is_executable(path: &str) -> bool {
    let Ok(c) = CString::new(path) else { return false };
    stat(path).is_some_and(|i| i.kind == FileType::File) && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
}

pub fn list(dir: &str) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|e| format!("cannot open {dir}: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    Ok(names)
}

/// Opens [path] without following a link at its end: the caller resolved the path and checked it, and a link
/// swapped in since then must not be followed. With [exact], a directory on the way swapped for a link is caught
/// too: the file opened must be the one named, as `/proc/self/fd` says.
fn open_read(path: &str, exact: bool) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    if exact {
        use std::os::fd::AsRawFd;
        if let Some(opened) = read_link(&format!("/proc/self/fd/{}", file.as_raw_fd())) {
            if opened != path {
                return Err(format!("{path} changed while it was opened"));
            }
        }
    }
    Ok(file)
}

/// At most [max] bytes of [path], or None when it can't be opened.
pub fn read(path: &str, max: usize) -> Option<Vec<u8>> {
    let file = open_read(path, false).ok()?;
    let mut out = Vec::new();
    file.take(max as u64).read_to_end(&mut out).ok()?;
    Some(out)
}

pub fn read_text(path: &str) -> Option<String> {
    read(path, usize::MAX).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// A configuration file, following links: an operator's `limen.toml` or `authorized_keys` that links to a file kept
/// elsewhere is that file. Only for files under root's or the hub user's directories; what the read role opens goes
/// through the policy and [read] instead.
pub fn read_following(path: &str) -> Option<String> {
    real_path(path).and_then(|p| read_text(&p))
}

/// Replaces the file [path] leads to, so a link the operator made stays a link.
pub fn write_following(path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    write_atomic(&real_path(path).unwrap_or_else(|| path.to_string()), bytes, mode)
}

/// Whether [path] contains [needle], reading it in chunks, up to [limit] bytes.
pub fn contains(path: &str, needle: &[u8], limit: u64) -> bool {
    let Ok(file) = open_read(path, false) else { return false };
    let mut reader = file.take(limit);
    let mut buffer = vec![0u8; CHUNK + needle.len()];
    let mut carried = 0;
    loop {
        let n = match reader.read(&mut buffer[carried..carried + CHUNK]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let end = carried + n;
        if buffer[..end].windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        // The tail that could start a match across the chunk boundary goes first in the next round.
        carried = (needle.len() - 1).min(end);
        buffer.copy_within(end - carried..end, 0);
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

/// Lines [from]..[from]+[count]-1 of [path] (1-based), reading in chunks so a big file costs what is read, and never
/// more than [max_bytes] of content.
pub fn read_lines(path: &str, from: usize, count: usize, max_bytes: usize, exact: bool) -> Result<LineSlice, String> {
    let mut file = open_read(path, exact)?;
    let mut lines = Vec::new();
    let mut line_no = 1;
    let mut bytes = 0;
    let mut partial: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut first = true;
    let cut = |lines: Vec<String>| Ok(LineSlice { lines, eof: false, binary: false });
    loop {
        let n = file.read(&mut buffer).map_err(|e| format!("cannot read {path}: {e}"))?;
        if n == 0 {
            break;
        }
        if first && is_binary(&buffer[..n]) {
            return Ok(LineSlice { lines: vec![], eof: true, binary: true });
        }
        first = false;
        let mut start = 0;
        for i in 0..n {
            if buffer[i] != b'\n' {
                continue;
            }
            if line_no >= from {
                partial.extend_from_slice(&buffer[start..i]);
                let line = String::from_utf8_lossy(&partial).into_owned();
                bytes += line.len() + 1;
                if bytes > max_bytes || lines.len() >= count {
                    return cut(lines);
                }
                lines.push(line);
            }
            partial.clear();
            line_no += 1;
            start = i + 1;
        }
        if line_no >= from && start < n {
            partial.extend_from_slice(&buffer[start..n]);
            if partial.len() > max_bytes {
                return cut(lines);
            }
        }
    }
    if !partial.is_empty() && line_no >= from {
        if lines.len() >= count {
            return cut(lines);
        }
        lines.push(String::from_utf8_lossy(&partial).into_owned());
    }
    Ok(LineSlice { lines, eof: true, binary: false })
}

/// The last [count] lines of [path], reading backwards from the end at most [max_bytes].
pub fn tail(path: &str, count: usize, max_bytes: u64, exact: bool) -> Result<LineSlice, String> {
    let mut file = open_read(path, exact)?;
    let size = file.metadata().map_err(|e| format!("cannot read {path}: {e}"))?.len();
    let start = size.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut all = Vec::new();
    file.take(max_bytes).read_to_end(&mut all).map_err(|e| format!("cannot read {path}: {e}"))?;
    if is_binary(&all) {
        return Ok(LineSlice { lines: vec![], eof: true, binary: true });
    }
    let text = String::from_utf8_lossy(&all);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if start > 0 {
        lines.remove(0);
    }
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let eof = start == 0 && lines.len() <= count;
    let from = lines.len().saturating_sub(count);
    Ok(LineSlice { lines: lines[from..].iter().map(|s| s.to_string()).collect(), eof, binary: false })
}

pub fn append(path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(mode)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    f.write_all(bytes).map_err(|e| format!("cannot write {path}: {e}"))
}

pub fn append_line(path: &str, line: &str) -> Result<(), String> {
    append(path, format!("{line}\n").as_bytes(), 0o600)
}

/// Replaces [path] with [bytes] through a temporary file and `rename`, so a reader never sees half of it.
pub fn write_atomic(path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    static TEMPORARIES: AtomicU32 = AtomicU32::new(0);
    // Its own name and O_EXCL: two writers at once each rename a whole file, and none writes into a link.
    let tmp = format!("{path}.limen-{}-{}", std::process::id(), TEMPORARIES.fetch_add(1, Ordering::Relaxed));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
            .map_err(|e| format!("cannot write {tmp}: {e}"))?;
        f.write_all(bytes).map_err(|e| format!("cannot write {tmp}: {e}"))?;
        f.sync_all().ok();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(|e| format!("cannot chmod {tmp}: {e}"))?;
        fs::rename(&tmp, path).map_err(|e| format!("cannot replace {path}: {e}"))
    })();
    if result.is_err() {
        fs::remove_file(&tmp).ok();
    }
    result
}

pub fn mkdirs(path: &str, mode: u32) -> Result<(), String> {
    let mut current = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        current.push('/');
        current.push_str(part);
        if let Err(e) = fs::DirBuilder::new().mode(mode).create(&current) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(format!("cannot create {current}: {e}"));
            }
        }
    }
    Ok(())
}

pub fn chmod(path: &str, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| format!("cannot chmod {path}: {e}"))
}

pub fn chown(path: &str, uid: u32, gid: u32) -> Result<(), String> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|e| format!("cannot chown {path}: {e}"))
}

pub fn remove(path: &str) -> bool {
    fs::remove_file(path).is_ok()
}

/// Users and groups from `/etc/passwd` and `/etc/group`, read each time: the same on every libc.
pub fn accounts() -> Vec<Account> {
    read_text("/etc/passwd").map(|t| procfs::accounts(&t)).unwrap_or_default()
}

pub fn user_name(uid: u32) -> Option<String> {
    accounts().into_iter().find(|a| a.uid == uid).map(|a| a.name)
}

pub fn group_name(gid: u32) -> Option<String> {
    read_text("/etc/group").and_then(|t| procfs::groups(&t).remove(&gid))
}

pub fn account(name: &str) -> Option<Account> {
    accounts().into_iter().find(|a| a.name == name)
}

/// Free and total bytes of the filesystem holding [path].
pub fn space(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let unit = st.f_frsize as u64;
    Some((st.f_bavail as u64 * unit, st.f_blocks as u64 * unit))
}

/// Where a symlink points, unresolved.
pub fn read_link(path: &str) -> Option<String> {
    fs::read_link(path).ok().map(|p| p.to_string_lossy().into_owned())
}

/// [path] and every directory above it: what [limen_core::trust] checks.
pub fn chain(path: &str) -> Vec<FileStat> {
    let mut out = Vec::new();
    let mut current = path.to_string();
    loop {
        match stat(&current) {
            Some(info) => out.push(info.to_stat()),
            None => return out,
        }
        if current == "/" {
            return out;
        }
        current = match current.rsplit_once('/') {
            Some(("", _)) | None => "/".into(),
            Some((parent, _)) => parent.into(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> String {
        let dir = std::env::temp_dir().join(format!("limen-fs-{}-{}", std::process::id(), rand_suffix()));
        fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn rand_suffix() -> u32 {
        static N: AtomicU32 = AtomicU32::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn lines_by_range_and_from_the_end() {
        let dir = temp();
        let path = format!("{dir}/f");
        write_atomic(&path, b"one\ntwo\nthree\nfour", 0o644).unwrap();
        let s = read_lines(&path, 2, 2, 1000, true).unwrap();
        assert_eq!(s.lines, ["two", "three"]);
        assert!(!s.eof);
        assert_eq!(
            read_lines(&path, 3, 10, 1000, false).unwrap(),
            LineSlice { lines: vec!["three".into(), "four".into()], eof: true, binary: false }
        );
        assert_eq!(read_lines(&path, 99, 10, 1000, false).unwrap().lines, Vec::<String>::new());
        assert_eq!(tail(&path, 2, 1000, true).unwrap().lines, ["three", "four"]);
        // Reading from the middle drops the partial first line.
        assert_eq!(tail(&path, 10, 11, false).unwrap().lines, ["three", "four"]);
        assert_eq!(tail(&path, 10, 9, false).unwrap().lines, ["four"]);
        write_atomic(&path, b"a\0b", 0o644).unwrap();
        assert!(read_lines(&path, 1, 10, 1000, false).unwrap().binary);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn contains_across_chunks_and_links_are_not_followed() {
        let dir = temp();
        let path = format!("{dir}/big");
        let mut text = vec![b'x'; CHUNK - 3];
        text.extend_from_slice(b"PRIVATE KEY-----");
        write_atomic(&path, &text, 0o644).unwrap();
        assert!(contains(&path, b"PRIVATE KEY-----", 1 << 20));
        assert!(!contains(&path, b"nothing", 1 << 20));
        std::os::unix::fs::symlink(&path, format!("{dir}/link")).unwrap();
        assert!(read(&format!("{dir}/link"), 10).is_none());
        assert!(read_following(&format!("{dir}/link")).is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chain_goes_up_to_the_root() {
        let paths: Vec<String> = chain("/usr/bin").into_iter().map(|s| s.path).collect();
        assert_eq!(paths, ["/usr/bin", "/usr", "/"]);
    }
}

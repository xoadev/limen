//! Files: what the gate reads and what `install` writes.

use limen_core::system::procfs::{self, Account};
use limen_core::trust::FileStat;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU32, Ordering};

const CHUNK: usize = 64 * 1024;
/// Not following a link at the end of the path; std adds `O_CLOEXEC` to every open itself.
const NOFOLLOW: i32 = OFlags::NOFOLLOW.bits() as i32;
/// Not waiting on a FIFO swapped in for a file: it is opened, found not to be a file, and refused.
const NONBLOCK: i32 = OFlags::NONBLOCK.bits() as i32;

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
    /// Hard links: a file with more than one can be reached by a path the policy never saw.
    pub links: u64,
}

impl FileInfo {
    fn of(path: &str, m: &fs::Metadata) -> Self {
        FileInfo {
            path: path.into(),
            kind: kind_of(m.mode()),
            size: m.size(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            modified: m.mtime(),
            links: m.nlink(),
        }
    }

    // The widths of `stat`'s fields differ between architectures.
    #[allow(clippy::unnecessary_cast)]
    fn of_stat(path: &str, st: &rustix::fs::Stat) -> Self {
        FileInfo {
            path: path.into(),
            kind: kind_of(st.st_mode as u32),
            size: st.st_size.max(0) as u64,
            mode: st.st_mode as u32,
            uid: st.st_uid,
            gid: st.st_gid,
            modified: st.st_mtime as i64,
            links: st.st_nlink as u64,
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

fn kind_of(mode: u32) -> FileType {
    match rustix::fs::FileType::from_raw_mode(mode) {
        rustix::fs::FileType::Symlink => FileType::Link,
        rustix::fs::FileType::RegularFile => FileType::File,
        rustix::fs::FileType::Directory => FileType::Directory,
        _ => FileType::Other,
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
    stat(path).is_some_and(|i| i.kind == FileType::File)
        && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
}

pub fn list(dir: &str) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|e| format!("cannot open {dir}: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    Ok(names)
}

/// Opens [path] without following a link at its end.
pub fn open_read(path: &str) -> Result<File, String> {
    OpenOptions::new().read(true).custom_flags(NOFOLLOW).open(path).map_err(|e| format!("cannot open {path}: {e}"))
}

/// A file the read role opened, and what it is: everything checked about it is checked on this descriptor, never
/// on the path again, so a file swapped in after the check is not the one read.
pub struct Opened {
    pub file: File,
    pub info: FileInfo,
}

/// Why [open_exact] failed.
pub enum OpenError {
    Missing,
    NotDirectory,
    Other(String),
}

/// Opens [path], resolved and checked by the caller, as exactly that: no link at its end, and no directory on the
/// way swapped for one since, as `/proc/self/fd` says of what was opened.
pub fn open_exact(path: &str) -> Result<Opened, OpenError> {
    let file = OpenOptions::new().read(true).custom_flags(NOFOLLOW | NONBLOCK).open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            OpenError::Missing
        } else {
            OpenError::Other(format!("cannot open {path}: {e}"))
        }
    })?;
    same_path(file.as_raw_fd(), path)?;
    let info = FileInfo::of(path, &file.metadata().map_err(|e| OpenError::Other(format!("cannot stat {path}: {e}")))?);
    Ok(Opened { file, info })
}

fn same_path(fd: i32, path: &str) -> Result<(), OpenError> {
    match read_link(&format!("/proc/self/fd/{fd}")) {
        Some(opened) if opened == path => Ok(()),
        Some(_) => Err(OpenError::Other(format!("{path} changed while it was opened"))),
        None => Err(OpenError::Other("cannot tell what was opened: no /proc".into())),
    }
}

/// A directory opened as exactly [path], like [open_exact]: its entries are read and looked at through it.
pub struct OpenDir {
    fd: OwnedFd,
    path: String,
}

pub fn open_dir_exact(path: &str) -> Result<OpenDir, OpenError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, Mode::empty()).map_err(|e| match e {
        rustix::io::Errno::NOENT => OpenError::Missing,
        rustix::io::Errno::NOTDIR => OpenError::NotDirectory,
        e => OpenError::Other(format!("cannot open {path}: {e}")),
    })?;
    same_path(fd.as_raw_fd(), path)?;
    Ok(OpenDir { fd, path: path.into() })
}

impl OpenDir {
    /// Every entry and what it is itself —a link as a link—, by name.
    pub fn entries(&self) -> Result<Vec<(String, FileInfo)>, String> {
        let dir = rustix::fs::Dir::read_from(&self.fd).map_err(|e| format!("cannot read {}: {e}", self.path))?;
        let mut out = Vec::new();
        for entry in dir {
            let entry = entry.map_err(|e| format!("cannot read {}: {e}", self.path))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            // Gone since it was listed: not an entry any more.
            let Ok(st) = rustix::fs::statat(&self.fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW) else { continue };
            let full = if self.path == "/" { format!("/{name}") } else { format!("{}/{name}", self.path) };
            out.push((name, FileInfo::of_stat(&full, &st)));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

/// At most [max] bytes of [path], or None when it can't be opened.
pub fn read(path: &str, max: usize) -> Option<Vec<u8>> {
    let file = open_read(path).ok()?;
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

/// Takes the lock of [path] —made if missing, never through a link— if nobody holds it: the lock lasts as long as the
/// file returned. None when it is held.
pub fn try_lock(path: &str) -> Result<Option<File>, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(path)
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(format!("cannot lock {path}: {e}")),
    }
}

/// Whether [file] contains [needle], reading it from the start in chunks, up to [limit] bytes.
pub fn contains(mut file: &File, needle: &[u8], limit: u64) -> bool {
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
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

/// Lines [from]..[from]+[count]-1 of [file] (1-based), reading in chunks so a big file costs what is read, and never
/// more than [max_bytes] of content. Reaching line [from] may not take more than [max_skip] bytes.
pub fn read_lines(
    mut file: &File,
    from: usize,
    count: usize,
    max_bytes: usize,
    max_skip: u64,
) -> Result<LineSlice, String> {
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut skipped: u64 = 0;
    let mut lines = Vec::new();
    let mut line_no = 1;
    let mut bytes = 0;
    let mut partial: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut first = true;
    let cut = |lines: Vec<String>| Ok(LineSlice { lines, eof: false, binary: false });
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if line_no < from {
            skipped += n as u64;
            if skipped > max_skip {
                return Err(format!("line {from} is past the first {} MiB", max_skip >> 20));
            }
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

/// The last [count] lines of [file], reading backwards from the end at most [max_bytes].
pub fn tail(mut file: &File, count: usize, max_bytes: u64) -> Result<LineSlice, String> {
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let start = size.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut all = Vec::new();
    file.take(max_bytes).read_to_end(&mut all).map_err(|e| e.to_string())?;
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
            .custom_flags(NOFOLLOW)
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

/// gid → name, from `/etc/group`.
pub fn groups() -> std::collections::BTreeMap<u32, String> {
    read_text("/etc/group").map(|t| procfs::groups(&t)).unwrap_or_default()
}

pub fn account(name: &str) -> Option<Account> {
    accounts().into_iter().find(|a| a.name == name)
}

/// Free and total bytes of the filesystem holding [path].
pub fn space(path: &str) -> Option<(u64, u64)> {
    let st = rustix::fs::statvfs(path).ok()?;
    Some((st.f_bavail * st.f_frsize, st.f_blocks * st.f_frsize))
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
        let file = &open_exact(&path).ok().unwrap().file;
        let s = read_lines(file, 2, 2, 1000, 1000).unwrap();
        assert_eq!(s.lines, ["two", "three"]);
        assert!(!s.eof);
        assert_eq!(
            read_lines(file, 3, 10, 1000, 1000).unwrap(),
            LineSlice { lines: vec!["three".into(), "four".into()], eof: true, binary: false }
        );
        assert_eq!(read_lines(file, 99, 10, 1000, 1000).unwrap().lines, Vec::<String>::new());
        // Reaching a line far into a file costs what is read on the way: that is bounded.
        assert!(read_lines(file, 3, 10, 1000, 5).is_err());
        assert_eq!(tail(file, 2, 1000).unwrap().lines, ["three", "four"]);
        // Reading from the middle drops the partial first line.
        assert_eq!(tail(file, 10, 11).unwrap().lines, ["three", "four"]);
        assert_eq!(tail(file, 10, 9).unwrap().lines, ["four"]);
        write_atomic(&path, b"a\0b", 0o644).unwrap();
        assert!(read_lines(&open_read(&path).unwrap(), 1, 10, 1000, 1000).unwrap().binary);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn contains_across_chunks_and_links_are_not_followed() {
        let dir = temp();
        let path = format!("{dir}/big");
        let mut text = vec![b'x'; CHUNK - 3];
        text.extend_from_slice(b"PRIVATE KEY-----");
        write_atomic(&path, &text, 0o644).unwrap();
        let file = open_read(&path).unwrap();
        assert!(contains(&file, b"PRIVATE KEY-----", 1 << 20));
        assert!(!contains(&file, b"nothing", 1 << 20));
        std::os::unix::fs::symlink(&path, format!("{dir}/link")).unwrap();
        assert!(read(&format!("{dir}/link"), 10).is_none());
        assert!(matches!(open_exact(&format!("{dir}/link")), Err(OpenError::Other(_))));
        assert!(read_following(&format!("{dir}/link")).is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fifo_is_opened_without_waiting_and_is_not_a_file() {
        let dir = temp();
        let path = format!("{dir}/fifo");
        rustix::fs::mknodat(rustix::fs::CWD, path.as_str(), rustix::fs::FileType::Fifo, Mode::from_raw_mode(0o600), 0)
            .unwrap();
        let Ok(opened) = open_exact(&path) else { panic!("a FIFO opens without a writer") };
        assert_eq!(opened.info.kind, FileType::Other);
        let listed = open_dir_exact(&dir).ok().unwrap().entries().unwrap();
        assert_eq!(listed.iter().map(|(n, i)| (n.as_str(), i.kind)).collect::<Vec<_>>(), [("fifo", FileType::Other)]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chain_goes_up_to_the_root() {
        let paths: Vec<String> = chain("/usr/bin").into_iter().map(|s| s.path).collect();
        assert_eq!(paths, ["/usr/bin", "/usr", "/"]);
    }
}

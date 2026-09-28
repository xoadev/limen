//! Files: what the gate reads and what `install` writes.

use limen_core::system::procfs::{self, Account};
use limen_core::trust::FileStat;
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
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
    fn of(path: &str, metadata: &fs::Metadata) -> Self {
        FileInfo {
            path: path.into(),
            kind: kind_of(metadata.mode()),
            size: metadata.size(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            modified: metadata.mtime(),
            links: metadata.nlink(),
        }
    }

    #[allow(clippy::unnecessary_cast, reason = "the widths of `stat`'s fields differ between architectures")]
    fn of_stat(path: &str, stat: &rustix::fs::Stat) -> Self {
        FileInfo {
            path: path.into(),
            kind: kind_of(stat.st_mode as u32),
            size: stat.st_size.max(0) as u64,
            mode: stat.st_mode as u32,
            uid: stat.st_uid,
            gid: stat.st_gid,
            modified: stat.st_mtime as i64,
            links: stat.st_nlink as u64,
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

impl LineSlice {
    /// Not text: a NUL byte near the start of what was read.
    fn binary() -> Self {
        LineSlice { lines: vec![], eof: true, binary: true }
    }
}

/// The error of [action] on [path], as a person reads it: `cannot open /etc/x: Permission denied (os error 13)`.
fn cannot<E: Display>(action: &str, path: &str) -> impl FnOnce(E) -> String {
    move |error| format!("cannot {action} {path}: {error}")
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn real_path(path: &str) -> Option<String> {
    fs::canonicalize(path).ok().map(|resolved| lossy(&resolved))
}

/// [path] itself, not what a link points to.
pub fn lstat(path: &str) -> Option<FileInfo> {
    fs::symlink_metadata(path).ok().map(|metadata| FileInfo::of(path, &metadata))
}

pub fn stat(path: &str) -> Option<FileInfo> {
    fs::metadata(path).ok().map(|metadata| FileInfo::of(path, &metadata))
}

pub fn exists(path: &str) -> bool {
    lstat(path).is_some()
}

pub fn is_executable(path: &str) -> bool {
    stat(path).is_some_and(|info| info.kind == FileType::File)
        && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
}

pub fn list(dir: &str) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(cannot("open", dir))?
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    Ok(names)
}

/// Opens [path] without following a link at its end.
pub fn open_read(path: &str) -> Result<File, String> {
    OpenOptions::new().read(true).custom_flags(NOFOLLOW).open(path).map_err(cannot("open", path))
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
    let file = OpenOptions::new().read(true).custom_flags(NOFOLLOW | NONBLOCK).open(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            OpenError::Missing
        } else {
            OpenError::Other(cannot("open", path)(error))
        }
    })?;
    ensure_opened_as(file.as_raw_fd(), path)?;
    let metadata = file.metadata().map_err(|error| OpenError::Other(cannot("stat", path)(error)))?;
    Ok(Opened { file, info: FileInfo::of(path, &metadata) })
}

/// Fails unless [fd] is [path] itself, as `/proc/self/fd` says: not what a directory swapped in on the way leads to.
fn ensure_opened_as(fd: i32, path: &str) -> Result<(), OpenError> {
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
    let fd = rustix::fs::open(path, flags, Mode::empty()).map_err(|errno| match errno {
        Errno::NOENT => OpenError::Missing,
        Errno::NOTDIR => OpenError::NotDirectory,
        other => OpenError::Other(cannot("open", path)(other)),
    })?;
    ensure_opened_as(fd.as_raw_fd(), path)?;
    Ok(OpenDir { fd, path: path.into() })
}

impl OpenDir {
    /// Every entry and what it is itself —a link as a link—, by name.
    pub fn entries(&self) -> Result<Vec<(String, FileInfo)>, String> {
        let dir = rustix::fs::Dir::read_from(&self.fd).map_err(cannot("read", &self.path))?;
        let mut entries = Vec::new();
        for entry in dir {
            let entry = entry.map_err(cannot("read", &self.path))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            // Gone since it was listed: not an entry any more.
            let Ok(stat) = rustix::fs::statat(&self.fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            let info = FileInfo::of_stat(&self.path_of(&name), &stat);
            entries.push((name, info));
        }
        entries.sort_by(|(one, _), (other, _)| one.cmp(other));
        Ok(entries)
    }

    fn path_of(&self, name: &str) -> String {
        if self.path == "/" { format!("/{name}") } else { format!("{}/{name}", self.path) }
    }
}

/// At most [max] bytes of [path], or None when it can't be opened.
pub fn read(path: &str, max: usize) -> Option<Vec<u8>> {
    let file = open_read(path).ok()?;
    let mut bytes = Vec::new();
    file.take(max as u64).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

pub fn read_text(path: &str) -> Option<String> {
    read(path, usize::MAX).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// A configuration file, following links: an operator's `limen.toml` or `authorized_keys` that links to a file kept
/// elsewhere is that file. Only for files under root's or the hub user's directories; what the read role opens goes
/// through the policy and [read] instead.
pub fn read_following(path: &str) -> Option<String> {
    real_path(path).and_then(|resolved| read_text(&resolved))
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
        .map_err(cannot("open", path))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(Errno::WOULDBLOCK) => Ok(None),
        Err(other) => Err(cannot("lock", path)(other)),
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
        if buffer[..end].windows(needle.len()).any(|window| window == needle) {
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
    file.seek(SeekFrom::Start(0)).map_err(|error| error.to_string())?;
    let mut window = LineWindow::new(from, count, max_bytes);
    let mut buffer = vec![0u8; CHUNK];
    let mut skipped: u64 = 0;
    let mut first = true;
    loop {
        let n = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if n == 0 {
            return Ok(window.at_end());
        }
        let chunk = &buffer[..n];
        if window.before_start() {
            skipped += n as u64;
            if skipped > max_skip {
                return Err(format!("line {from} is past the first {} MiB", max_skip >> 20));
            }
        }
        if first && is_binary(chunk) {
            return Ok(LineSlice::binary());
        }
        first = false;
        if !window.take(chunk) {
            return Ok(window.cut());
        }
    }
}

/// The lines [read_lines] keeps as the file goes by: from line [from], at most [count] of them and [max_bytes].
struct LineWindow {
    from: usize,
    count: usize,
    max_bytes: usize,
    /// The line being read, 1-based.
    line_number: usize,
    /// What the lines kept take, newlines included.
    kept_bytes: usize,
    /// The start of a line in the window, not ended yet.
    partial: Vec<u8>,
    lines: Vec<String>,
}

impl LineWindow {
    fn new(from: usize, count: usize, max_bytes: usize) -> Self {
        LineWindow { from, count, max_bytes, line_number: 1, kept_bytes: 0, partial: Vec::new(), lines: Vec::new() }
    }

    fn before_start(&self) -> bool {
        self.line_number < self.from
    }

    /// Takes in the next [chunk] of the file; false when the window is full or a line in it is over [max_bytes].
    fn take(&mut self, chunk: &[u8]) -> bool {
        let mut start = 0;
        for (end, _) in chunk.iter().enumerate().filter(|&(_, &byte)| byte == b'\n') {
            if !self.before_start() {
                self.partial.extend_from_slice(&chunk[start..end]);
                let line = String::from_utf8_lossy(&self.partial).into_owned();
                self.kept_bytes += line.len() + 1;
                if self.kept_bytes > self.max_bytes || self.lines.len() >= self.count {
                    return false;
                }
                self.lines.push(line);
            }
            self.partial.clear();
            self.line_number += 1;
            start = end + 1;
        }
        if !self.before_start() && start < chunk.len() {
            self.partial.extend_from_slice(&chunk[start..]);
            if self.partial.len() > self.max_bytes {
                return false;
            }
        }
        true
    }

    /// The window at the end of the file, whose last line may have no newline.
    fn at_end(mut self) -> LineSlice {
        if !self.partial.is_empty() && !self.before_start() {
            if self.lines.len() >= self.count {
                return self.cut();
            }
            self.lines.push(String::from_utf8_lossy(&self.partial).into_owned());
        }
        LineSlice { lines: self.lines, eof: true, binary: false }
    }

    /// The window, full before the end of the file.
    fn cut(self) -> LineSlice {
        LineSlice { lines: self.lines, eof: false, binary: false }
    }
}

/// The last [count] lines of [file], reading backwards from the end at most [max_bytes].
pub fn tail(mut file: &File, count: usize, max_bytes: u64) -> Result<LineSlice, String> {
    let size = file.metadata().map_err(|error| error.to_string())?.len();
    let start = size.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).map_err(|error| error.to_string())?;
    let mut last_bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut last_bytes).map_err(|error| error.to_string())?;
    if is_binary(&last_bytes) {
        return Ok(LineSlice::binary());
    }
    let text = String::from_utf8_lossy(&last_bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    // Unless it starts the file, what comes before the first newline may be only the end of a line.
    if start > 0 {
        lines.remove(0);
    }
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let eof = start == 0 && lines.len() <= count;
    let last = lines.split_off(lines.len().saturating_sub(count));
    Ok(LineSlice { lines: last.into_iter().map(String::from).collect(), eof, binary: false })
}

pub fn append(path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new().append(true).create(true).mode(mode).open(path).map_err(cannot("open", path))?;
    file.write_all(bytes).map_err(cannot("write", path))
}

pub fn append_line(path: &str, line: &str) -> Result<(), String> {
    append(path, format!("{line}\n").as_bytes(), 0o600)
}

/// Replaces [path] with [bytes] through a temporary file and `rename`, so a reader never sees half of it.
pub fn write_atomic(path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    static TEMPORARIES: AtomicU32 = AtomicU32::new(0);
    // Its own name and O_EXCL: two writers at once each rename a whole file, and none writes into a link.
    let temporary = format!("{path}.limen-{}-{}", std::process::id(), TEMPORARIES.fetch_add(1, Ordering::Relaxed));
    let result =
        write_new(&temporary, bytes, mode).and_then(|()| fs::rename(&temporary, path).map_err(cannot("replace", path)));
    if result.is_err() {
        fs::remove_file(&temporary).ok();
    }
    result
}

/// Creates [path], which must not exist yet, with [bytes] and exactly [mode], whatever the umask.
fn write_new(path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(NOFOLLOW)
        .open(path)
        .map_err(cannot("write", path))?;
    file.write_all(bytes).map_err(cannot("write", path))?;
    file.sync_all().ok();
    chmod(path, mode)
}

pub fn mkdirs(path: &str, mode: u32) -> Result<(), String> {
    let mut current = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        match fs::DirBuilder::new().mode(mode).create(&current) {
            Err(error) if error.kind() != ErrorKind::AlreadyExists => return Err(cannot("create", &current)(error)),
            _ => {}
        }
    }
    Ok(())
}

pub fn chmod(path: &str, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(cannot("chmod", path))
}

pub fn chown(path: &str, uid: u32, gid: u32) -> Result<(), String> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(cannot("chown", path))
}

pub fn remove(path: &str) -> bool {
    fs::remove_file(path).is_ok()
}

/// Users and groups from `/etc/passwd` and `/etc/group`, read each time: the same on every libc.
pub fn accounts() -> Vec<Account> {
    read_text("/etc/passwd").map(|passwd| procfs::accounts(&passwd)).unwrap_or_default()
}

/// gid → name, from `/etc/group`.
pub fn groups() -> std::collections::BTreeMap<u32, String> {
    read_text("/etc/group").map(|group| procfs::groups(&group)).unwrap_or_default()
}

pub fn account(name: &str) -> Option<Account> {
    accounts().into_iter().find(|account| account.name == name)
}

/// Free and total bytes of the filesystem holding [path].
pub fn space(path: &str) -> Option<(u64, u64)> {
    let stats = rustix::fs::statvfs(path).ok()?;
    Some((stats.f_bavail * stats.f_frsize, stats.f_blocks * stats.f_frsize))
}

/// Where a symlink points, unresolved.
pub fn read_link(path: &str) -> Option<String> {
    fs::read_link(path).ok().map(|target| lossy(&target))
}

/// [path] and every directory above it: what [limen_core::trust] checks.
pub fn chain(path: &str) -> Vec<FileStat> {
    let mut stats = Vec::new();
    let mut current = path.to_string();
    loop {
        match stat(&current) {
            Some(info) => stats.push(info.to_stat()),
            None => return stats,
        }
        if current == "/" {
            return stats;
        }
        current = parent(&current).into();
    }
}

/// The directory [path] is in: `/` for what is directly under it.
pub fn parent(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((parent, _)) => parent,
    }
}

pub fn is_directory(path: &str) -> bool {
    stat(path).is_some_and(|info| info.kind == FileType::Directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("limen-fs-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        lossy(&dir)
    }

    #[test]
    fn lines_by_range_and_from_the_end() {
        let dir = temp();
        let path = format!("{dir}/f");
        write_atomic(&path, b"one\ntwo\nthree\nfour", 0o644).unwrap();
        let file = &open_exact(&path).ok().unwrap().file;
        let slice = read_lines(file, 2, 2, 1000, 1000).unwrap();
        assert_eq!(slice.lines, ["two", "three"]);
        assert!(!slice.eof);
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
        let kinds: Vec<_> = listed.iter().map(|(name, info)| (name.as_str(), info.kind)).collect();
        assert_eq!(kinds, [("fifo", FileType::Other)]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chain_goes_up_to_the_root() {
        let paths: Vec<String> = chain("/usr/bin").into_iter().map(|stat| stat.path).collect();
        assert_eq!(paths, ["/usr/bin", "/usr", "/"]);
    }
}

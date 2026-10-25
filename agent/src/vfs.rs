//! Guest-path filesystem layer — a drop-in for the slice of `std::fs` the
//! agent uses (`use crate::vfs as fs;`). Every path the agent handles stays
//! a *guest* path (`/memory/notes/x.md`, `/` = agent_home) exactly like it
//! was under the old WASI preopen; this module is the one place those get
//! mapped onto the real host path under agent_home.
//!
//! Containment: `..` is resolved lexically and may never climb above `/`,
//! and every existing component on the way down is checked — a symlink
//! whose target lands outside agent_home is refused (`PermissionDenied`)
//! rather than followed. Absolute paths are always re-rooted, so
//! `/etc/passwd` means `agent_home/etc/passwd`, never the host's.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// agent_home, canonicalized — the kernel passes it as `EBINA_AGENT_HOME`
/// and also starts the process with it as cwd, so cwd is the fallback.
fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let raw = std::env::var_os("EBINA_AGENT_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        raw.canonicalize().unwrap_or(raw)
    })
}

fn escape_error(guest: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("{} escapes the agent root", guest.display()))
}

/// Guest path → host path under agent_home, or `PermissionDenied` if it
/// would resolve outside it.
pub fn resolve(guest: impl AsRef<Path>) -> io::Result<PathBuf> {
    let guest = guest.as_ref();
    let root = root();
    let mut rel = PathBuf::new();
    for c in guest.components() {
        match c {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if !rel.pop() {
                    return Err(escape_error(guest));
                }
            }
            Component::Normal(n) => rel.push(n),
        }
    }
    let mut host = root.to_path_buf();
    for c in rel.components() {
        host.push(c);
        match std::fs::symlink_metadata(&host) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let real = std::fs::canonicalize(&host).map_err(|_| escape_error(guest))?;
                if !real.starts_with(root) {
                    return Err(escape_error(guest));
                }
            }
            Ok(_) => {}
            // first missing component — nothing below it can be a symlink yet
            Err(_) => break,
        }
    }
    Ok(root.join(rel))
}

pub fn read_to_string(p: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(resolve(p)?)
}

pub fn read(p: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    std::fs::read(resolve(p)?)
}

pub fn write(p: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    std::fs::write(resolve(p)?, contents)
}

pub fn create_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir_all(resolve(p)?)
}

pub fn metadata(p: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(resolve(p)?)
}

pub fn remove_file(p: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_file(resolve(p)?)
}

pub fn remove_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir_all(resolve(p)?)
}

/// Entries come back with *guest* paths (`dir` joined with the name), so
/// feeding `entry.path()` back into this module round-trips correctly.
pub fn read_dir(p: impl AsRef<Path>) -> io::Result<impl Iterator<Item = io::Result<DirEntry>>> {
    let dir = p.as_ref().to_path_buf();
    let inner = std::fs::read_dir(resolve(&dir)?)?;
    Ok(inner.map(move |e| e.map(|inner| DirEntry { guest: dir.join(inner.file_name()), inner })))
}

pub struct DirEntry {
    inner: std::fs::DirEntry,
    guest: PathBuf,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.guest.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.inner.file_name()
    }

    pub fn file_type(&self) -> io::Result<std::fs::FileType> {
        self.inner.file_type()
    }

    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.inner.metadata()
    }
}

pub struct File;

impl File {
    pub fn open(p: impl AsRef<Path>) -> io::Result<std::fs::File> {
        std::fs::File::open(resolve(p)?)
    }

    pub fn create(p: impl AsRef<Path>) -> io::Result<std::fs::File> {
        std::fs::File::create(resolve(p)?)
    }
}

pub struct OpenOptions(std::fs::OpenOptions);

impl OpenOptions {
    pub fn new() -> Self {
        OpenOptions(std::fs::OpenOptions::new())
    }

    pub fn write(&mut self, v: bool) -> &mut Self {
        self.0.write(v);
        self
    }

    pub fn append(&mut self, v: bool) -> &mut Self {
        self.0.append(v);
        self
    }

    pub fn create(&mut self, v: bool) -> &mut Self {
        self.0.create(v);
        self
    }

    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.0.create_new(v);
        self
    }

    pub fn open(&self, p: impl AsRef<Path>) -> io::Result<std::fs::File> {
        self.0.open(resolve(p)?)
    }
}

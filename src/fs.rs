/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Virtual filesystem, or "guest filesystem".
//!
//! This lets us put files and directories where the guest app expects them to
//! be, without constraining the layout of the host filesystem.
//!
//! Most of the filesystem is frozen at the point of creation and can't be
//! modified. The exception is the writeable parts of the app's sandboxed home
//! directory (`Documents` etc).
//!
//! All files in the guest filesystem must have a corresponding file in the host
//! filesystem, or a corresponding file inside a `.ipa` file (ZIP archive) in
//! the host filesystem. Accessing a file requires traversing the guest
//! filesystem's directory structure to find out the host path, or ZIP file
//! member. After that point, the underlying file is accessed directly; there is
//! no virtualization of file I/O.
//!
//! Directories only need a corresponding directory in the host filesystem if
//! they are writeable (i.e. if new files can be created in them).
//!
//! See also [crate::paths], which has paths for host files used by touchHLE.

mod bundle;

pub use bundle::BundleData;

use crate::fs::bundle::{IpaFile, IpaFileRef};
use crate::paths;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

/// The actual location of a file outside the virtual filesystem, e.g. a host
/// file path.
#[derive(Debug)]
enum FileLocation {
    /// Path for a normal file. Can be read or written.
    Path(PathBuf),
    /// Reference to a file inside a `.ipa` file (ZIP archive). Read only.
    IpaFileRef(IpaFileRef),
    /// Name of a resource file bundled with touchHLE. Read only.
    ResourceFilePath(String),
}

#[derive(Debug)]
pub enum FsError {
    AccessDenied,
    AlreadyExist,
    DirectoryNotEmpty,
    DoesNotExist,
    InvalidParentDir,
    NonexistentParentDir,
    ReadonlyParentDir,
}

#[derive(Debug)]
pub enum FsNodeType {
    File,
    Directory,
}

#[derive(Debug)]
enum FsNode {
    File {
        location: FileLocation,
        writeable: bool,
    },
    Directory {
        children: HashMap<String, FsNode>,
        writeable: Option<PathBuf>,
    },
}
impl FsNode {
    fn from_host_dir(host_path: &Path, writeable: bool) -> Self {
        let mut children = HashMap::new();
        for entry in std::fs::read_dir(host_path).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let host_path = entry.path();
            let name = entry.file_name().into_string().unwrap();
            // There is no support for symlinks within the virtual filesystem,
            // but symlinks aren't uncommon in app bundles, so we treat a
            // symlink as if it were a copy of the file it points to.
            let kind = if kind.is_symlink() {
                std::fs::metadata(&host_path).unwrap().file_type()
            } else {
                kind
            };
            if kind.is_file() {
                children.insert(
                    name,
                    FsNode::File {
                        location: FileLocation::Path(host_path),
                        writeable,
                    },
                );
            } else if kind.is_dir() {
                children.insert(name, FsNode::from_host_dir(&host_path, writeable));
            } else {
                panic!("{host_path:?} is not a symlink, file or directory");
            }
        }
        FsNode::Directory {
            children,
            writeable: match writeable {
                true => Some(host_path.to_owned()),
                false => None,
            },
        }
    }

    // Convenience methods for constructing the read-only parts of the initial
    // filesystem layout

    fn dir() -> Self {
        FsNode::Directory {
            children: HashMap::new(),
            writeable: None,
        }
    }
    fn with_child(mut self, name: &str, child: FsNode) -> Self {
        let FsNode::Directory {
            ref mut children,
            writeable: _,
        } = self
        else {
            panic!();
        };
        assert!(children.insert(String::from(name), child).is_none());
        self
    }
    fn bundle_zip_file(file_ref: IpaFileRef) -> Self {
        FsNode::File {
            location: FileLocation::IpaFileRef(file_ref),
            writeable: false,
        }
    }
    fn resource_file(name: String) -> Self {
        FsNode::File {
            location: FileLocation::ResourceFilePath(name),
            writeable: false,
        }
    }

    // ИСПРАВЛЕНИЕ: Рекурсивно обновляем пути хоста во всем дереве VFS 
    // при перемещении или переименовании директорий. Без этого дочерние 
    // файлы будут ссылаться на старые несуществующие пути.
    fn update_host_paths_recursively(&mut self, new_host_path: PathBuf) {
        match self {
            FsNode::File {
                location: FileLocation::Path(p),
                ..
            } => {
                *p = new_host_path;
            }
            FsNode::Directory {
                children,
                writeable: Some(p),
            } => {
                *p = new_host_path.clone();
                for (name, child) in children.iter_mut() {
                    child.update_host_paths_recursively(new_host_path.join(name));
                }
            }
            _ => {}
        }
    }
}

// Put well-known paths in the guest filesystem here.
/// Path of the applications directory in the guest filesystem.
pub const APPLICATIONS: &GuestPath = GuestPath::new_const("/var/mobile/Applications");

/// Like [Path] but for the virtual filesystem.
#[repr(transparent)]
#[derive(Debug)]
pub struct GuestPath(str);
impl GuestPath {
    const fn new_const(s: &str) -> &GuestPath {
        unsafe { &*(s as *const str as *const GuestPath) }
    }

    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> &GuestPath {
        unsafe { &*(s.as_ref() as *const str as *const GuestPath) }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Join a path component.
    ///
    /// This should use `AsRef<GuestPath>`, but we can't have a blanket
    /// implementation of `AsRef<GuestPath>` for all `AsRef<str>` types, so we
    /// would have to implement it for everything that can derference to `&str`.
    /// It's easier to just use `&str`.
    ///
    /// Warning!
    /// This function should only be used for internal touchHLE
    /// purposes.
    /// For Foundation case, use `[NSString stringByAppendingPathComponent:]`
    pub fn join<P: AsRef<str>>(&self, path: P) -> GuestPathBuf {
        GuestPathBuf::from(format!("{}/{}", self.as_str(), path.as_ref()))
    }

    /// Splits the path into a parent path and a file name.
    pub fn parent_and_file_name(&self) -> Option<(&GuestPath, &str)> {
        // Strip trailing slashes to handle paths like "/foo/bar/"
        let path_str = self.as_str().trim_end_matches('/');
        if path_str.is_empty() {
            return None;
        }
        let (parent_name, file_name) = path_str.rsplit_once('/')?;
        if file_name.is_empty() {
            return None;
        }
        Some((GuestPath::new(parent_name), file_name))
    }

    /// Get the final component of the path.
    pub fn file_name(&self) -> Option<&str> {
        let (_, file_name) = self.parent_and_file_name()?;
        Some(file_name)
    }

    /// Get the parent directory of the path.
    pub fn parent(&self) -> Option<&GuestPath> {
        let (parent_name, _) = self.parent_and_file_name()?;
        Some(parent_name)
    }
}
impl AsRef<GuestPath> for GuestPath {
    fn as_ref(&self) -> &Self {
        self
    }
}
impl AsRef<str> for GuestPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<GuestPath> for str {
    fn as_ref(&self) -> &GuestPath {
        unsafe { &*(self as *const str as *const GuestPath) }
    }
}
impl ToOwned for GuestPath {
    type Owned = GuestPathBuf;
    fn to_owned(&self) -> GuestPathBuf {
        GuestPathBuf::from(self)
    }
}

/// Like [PathBuf] but for the virtual filesystem.
#[derive(Debug, Clone, Default)]
pub struct GuestPathBuf(String);
impl From<String> for GuestPathBuf {
    fn from(string: String) -> GuestPathBuf {
        GuestPathBuf(string)
    }
}
impl From<&GuestPath> for GuestPathBuf {
    fn from(guest_path: &GuestPath) -> GuestPathBuf {
        guest_path.as_str().to_string().into()
    }
}
impl From<GuestPathBuf> for String {
    fn from(guest_path: GuestPathBuf) -> String {
        guest_path.0
    }
}
impl std::ops::Deref for GuestPathBuf {
    type Target = GuestPath;
    fn deref(&self) -> &GuestPath {
        let s: &str = &self.0;
        s.as_ref()
    }
}
impl AsRef<GuestPath> for GuestPathBuf {
    fn as_ref(&self) -> &GuestPath {
        self
    }
}
impl std::borrow::Borrow<GuestPath> for GuestPathBuf {
    fn borrow(&self) -> &GuestPath {
        self
    }
}

fn apply_path_component<'a>(components: &mut Vec<&'a str>, component: &'a str) {
    match component {
        "" => (),
        "." => (),
        ".." => {
            components.pop();
        }
        _ => components.push(component),
    }
}

/// Resolve a path so that it is absolute and has no `.`, `..` or empty
/// components.
/// The result is a series of zero or more path components forming
/// an absolute path (e.g. `["foo", "bar"]` means `/foo/bar`).
///
/// `relative_to` is the starting point for resolving a relative path, e.g. the
/// current directory.
/// It must be an absolute path. It is optional if `path`
/// is absolute.
pub fn resolve_path<'a>(path: &'a GuestPath, relative_to: Option<&'a GuestPath>) -> Vec<&'a str> {
    log_dbg!("Resolving {:?} relative to {:?}", path, relative_to);
    let mut components = Vec::new();

    if !path.as_str().starts_with('/') {
        let relative_to = relative_to.unwrap().as_str();
        assert!(relative_to.starts_with('/'));
        for component in relative_to.split('/') {
            apply_path_component(&mut components, component);
        }
    }

    for component in path.as_str().split('/') {
        apply_path_component(&mut components, component);
    }

    // --- Path deduplication heuristic ---
    // Some guest apps (Triniti engine, OMH!, etc.) construct invalid paths by
    // concatenating the Documents (or other sandbox) directory twice, e.g.:
    //   /var/mobile/.../Documents/ + /var/mobile/.../Documents/.AudioCache...
    // This results in a path like:
    //   /var/mobile/.../Documents/var/mobile/.../Documents/.AudioCache...
    // which has a repeated prefix. Detect and strip the duplication so the
    // filesystem lookup succeeds. We look for the *home directory prefix*
    // appearing a second time within the resolved components.
    //
    // Strategy: if we find the sequence ["var", "mobile", "Applications"] at
    // any position > 0, that second occurrence marks the start of the "real"
    // path — truncate everything before it.
    if components.len() > 6 {
        let marker = ["var", "mobile", "Applications"];
        // Skip the first occurrence (position 0) and look for a second one.
        if let Some(dup_start) = components.windows(marker.len()).position(|w| {
            w == marker
        }) {
            // Check if there's a second occurrence of the same marker.
            if let Some(second_pos) = components[dup_start + 1..].windows(marker.len()).position(|w| {
                w == marker
            }) {
                let real_start = dup_start + 1 + second_pos;
                log_dbg!(
                    "Path deduplication: stripping duplicate prefix at component {}",
                    real_start
                );
                components = components[real_start..].to_vec();
            }
        }
    }

    log_dbg!("=> {:?}", components);

    components
}

/// Like [std::fs::OpenOptions] but for the guest filesystem.
/// TODO: `create_new`.
#[derive(Debug)]
pub struct GuestOpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    truncate: bool,
}
impl GuestOpenOptions {
    pub fn new() -> GuestOpenOptions {
        GuestOpenOptions {
            read: false,
            write: false,
            append: false,
            create: false,
            truncate: false,
        }
    }
    pub fn read(&mut self) -> &mut Self {
        self.read = true;
        self
    }
    pub fn write(&mut self) -> &mut Self {
        self.write = true;
        self
    }
    pub fn append(&mut self) -> &mut Self {
        self.append = true;
        self
    }
    pub fn create(&mut self) -> &mut Self {
        self.create = true;
        self
    }
    pub fn truncate(&mut self) -> &mut Self {
        self.truncate = true;
        self
    }
}

/// Handles host I/O errors by panicking. This is intended specifically for
/// opening files.
/// The assumption is that the guest filesystem contains all the
/// information needed to tell if opening a file should succeed, so if opening
/// the file nonetheless fails, there's either a bug or the user has done
/// something wrong.
fn handle_open_err<T, E: std::fmt::Display, P: std::fmt::Debug>(
    open_result: Result<T, E>,
    host_path: P,
) -> T {
    match open_result {
        Ok(ok) => ok,
        Err(e) => panic!("Unexpected I/O failure when trying to access real path {host_path:?}: {e}. This might indicate that files needed by touchHLE are missing, or were moved while it was running."),
    }
}

/// Like [File] but for the guest filesystem.
#[derive(Debug)]
pub enum GuestFile {
    Directory,
    File(File),
    IpaBundleFile(IpaFile),
    ResourceFile(paths::ResourceFile),
    Socket,
}

impl GuestFile {
    fn from_host_file(file: File) -> GuestFile {
        GuestFile::File(file)
    }

    fn from_ipa_file(file: &IpaFileRef) -> GuestFile {
        GuestFile::IpaBundleFile(file.open())
    }

    fn from_resource_file(file: paths::ResourceFile) -> GuestFile {
        GuestFile::ResourceFile(file)
    }

    fn from_directory() -> GuestFile {
        GuestFile::Directory
    }

    pub fn sync_all(&self) -> std::io::Result<()> {
        match self {
            GuestFile::File(file) => file.sync_all(),
            GuestFile::IpaBundleFile(_) | GuestFile::ResourceFile(_) => Ok(()),
            GuestFile::Directory => {
                log!("Warning: syncing directory as a guest file.");
                Ok(())
            }
            GuestFile::Socket => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Sync operation not supported on socket",
            )),
        }
    }
    pub fn set_len(&self, len: u64) -> std::io::Result<()> {
        match self {
            GuestFile::File(file) => file.set_len(len),
            GuestFile::IpaBundleFile(_) | GuestFile::ResourceFile(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Attempt to resize a read-only file",
            )),
            GuestFile::Directory => Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "Attempt to resize a directory as a guest file",
            )),
            GuestFile::Socket => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "set_len not supported on socket",
            )),
        }
    }

    pub fn stream_len(&mut self) -> std::io::Result<u64> {
        // TODO: Remove if standard stream_len ever gets stabilized.
        let old_position = self.stream_position()?;
        let len = self.seek(std::io::SeekFrom::End(0))?;
        self.seek(std::io::SeekFrom::Start(old_position))?;
        Ok(len)
    }

    pub fn is_seekable(&self) -> bool {
        // Due to legacy directory iteration support, directories are seekable
        // https://stackoverflow.com/questions/65911066/what-does-lseek-mean-for-a-directory-file-descriptor
        !matches!(self, GuestFile::Socket)
    }

    /// Duplicate this file descriptor, creating an independent handle that
    /// shares the underlying kernel file description (for `GuestFile::File`)
    /// or creates a new cursor at the same position (for IPA/resource files).
    /// This is used to implement POSIX `dup(2)` / `fcntl(F_DUPFD)`.
    pub fn try_clone(&self) -> std::io::Result<GuestFile> {
        match self {
            GuestFile::File(file) => {
                let cloned = file.try_clone()?;
                Ok(GuestFile::File(cloned))
            }
            GuestFile::IpaBundleFile(ipa_file) => {
                // IpaFile uses Cursor<Rc<[u8]>> — clone shares the data and
                // copies the seek position.
                Ok(GuestFile::IpaBundleFile(ipa_file.clone()))
            }
            GuestFile::ResourceFile(_) => {
                // ResourceFile wraps a host File. We cannot easily clone it
                // without re-opening, so return an error. This is rare.
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "Cannot duplicate a resource file descriptor",
                ))
            }
            GuestFile::Directory => Ok(GuestFile::Directory),
            GuestFile::Socket => {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "Cannot duplicate a socket file descriptor",
                ))
            }
        }
    }
}

impl Read for GuestFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            GuestFile::File(file) => file.read(buf),
            GuestFile::IpaBundleFile(file) => file.read(buf),
            GuestFile::ResourceFile(file) => file.get().read(buf),
            GuestFile::Directory => Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "Attempt to read from a directory as a guest file",
            )),
            GuestFile::Socket => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "read not supported on socket via GuestFile",
            )),
        }
    }
}

impl Write for GuestFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            GuestFile::File(file) => file.write(buf),
            GuestFile::IpaBundleFile(_) | GuestFile::ResourceFile(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Attempt to write to a read-only file",
            )),
            GuestFile::Directory => Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "Attempt to write to a directory as a guest file",
            )),
            GuestFile::Socket => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "write not supported on socket via GuestFile",
            )),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            GuestFile::File(file) => file.flush(),
            GuestFile::IpaBundleFile(_) | GuestFile::ResourceFile(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Attempt to flush a read-only file",
            )),
            GuestFile::Directory => Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "Attempt to flush a directory as a guest file",
            )),
            GuestFile::Socket => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "flush not supported on socket via GuestFile",
            )),
        }
    }
}

impl Seek for GuestFile {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            GuestFile::File(file) => file.seek(pos),
            GuestFile::IpaBundleFile(file) => file.seek(pos),
            GuestFile::ResourceFile(file) => file.get().seek(pos),
            GuestFile::Directory => {
                // Note: directories as supposed to be seekable on iOS!
                // https://stackoverflow.com/questions/65911066/what-does-lseek-mean-for-a-directory-file-descriptor
                // As far as I can (f)tell, apps are really not using that
                // properly and returning -1 on fseek/ftell is fine.
                // TODO: implement seeking properly and return "cookie" values
                log!("Warning: Seeking a directory as a guest file!");
                Err(std::io::Error::new(
                    std::io::ErrorKind::IsADirectory,
                    "Attempt to seek a directory as a guest file",
                ))
            }
            GuestFile::Socket => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "seek not supported on socket via GuestFile",
            )),
        }
    }
}

/// The type that owns the guest filesystem and provides accessors for it.
#[derive(Debug)]
pub struct Fs {
    root: FsNode,
    working_directory: GuestPathBuf,
    home_directory: GuestPathBuf,
}
impl Fs {
    /// Construct a filesystem containing a home directory for the app, its
    /// bundle and documents, and the bundled shared libraries.
    /// Returns the new
    /// filesystem and the guest path of the bundle.
    ///
    /// The `bundle_dir_name` argument will be used as the name of the bundle
    /// directory in the guest filesystem, and must end in `.app`.
    /// This allows the host directory for the bundle to be renamed from its
    /// original name without confusing the app.
    /// Supposedly Apple does something
    /// similar when executing iOS apps on modern Macs.
    ///
    /// The `bundle_id` argument should be some value that uniquely identifies
    /// the app.
    /// This will be used to construct the host path for the app's
    /// sandbox directory, where documents can be stored.
    /// A directory will be
    /// created at that path if it does not already exist.
    ///
    /// `read_only_mode` can be used when the app won't actually be run, just
    /// just inspected (e.g. to retrieve display name and icon), so no user data
    /// directories are required and no sandbox directory will be created on the
    /// host.
    pub fn new(
        app_bundle: BundleData,
        bundle_dir_name: String,
        bundle_id: &str,
        read_only_mode: bool,
    ) -> (Fs, GuestPathBuf) {
        const FAKE_UUID: &str = "00000000-0000-0000-0000-000000000000";
        let home_directory = APPLICATIONS.join(FAKE_UUID);
        let working_directory = GuestPathBuf::from("/".to_string());

        let bundle_guest_path = home_directory.join(&bundle_dir_name);

        let directories = ["Documents", "Library", "tmp"];
        let host_path_directories = directories.map(|dir| {
            if !read_only_mode {
                let path = paths::user_data_base_path()
                    .join(paths::SANDBOX_DIR)
                    .join(bundle_id)
                    .join(dir);

                if dir == "tmp" {
                    // We clean temporary directory for current app at startup.
                    // This is no-op if directory doesn't exist.
                    match std::fs::remove_dir_all(&path) {
                        Ok(_) => {}
                        Err(e) => {
                            log_dbg!(
                                "Unable to clean tmp host folder {:?} at startup: {}",
                                path,
                                e
                            );
                        }
                    }
                }
                if let Err(e) = std::fs::create_dir_all(&path) {
                    panic!("Could not create documents directory for app at {path:?}: {e:?}");
                }
                Some(path)
            } else {
                None
            }
        });
        if !read_only_mode {
            // Special case: Some apps may create save files at
            // Library/Preferences at the start, thus presence of that
            // directory is expected
            let path = paths::user_data_base_path()
                .join(paths::SANDBOX_DIR)
                .join(bundle_id)
                .join("Library")
                .join("Preferences");
            if let Err(e) = std::fs::create_dir_all(&path) {
                panic!("Could not create documents sub-directory for app at {path:?}: {e:?}");
            }
        }

        // Some Free Software libraries are bundled with touchHLE.
        use paths::DYLIBS_DIR;
        let usr_lib = FsNode::dir()
            .with_child(
                "libgcc_s.1.dylib",
                FsNode::resource_file(format!("{}/libgcc_s.1.dylib", DYLIBS_DIR)),
            )
            .with_child(
                "libstdc++.6.dylib",
                FsNode::resource_file(format!("{}/libstdc++.6.0.9.dylib", DYLIBS_DIR)),
            )
            .with_child(
                "libstdc++.6.0.9.dylib",
                FsNode::resource_file(format!("{}/libstdc++.6.0.9.dylib", DYLIBS_DIR)),
            )
            .with_child(
                "libc++.1.dylib",
                FsNode::resource_file(format!("{}/libc++.1.dylib", DYLIBS_DIR)),
            )
            .with_child(
                "libz.1.2.3.dylib",
                FsNode::resource_file(format!("{DYLIBS_DIR}/libz.1.2.3.dylib")),
            )
            .with_child(
                // symlink
                "libz.1.dylib",
                FsNode::resource_file(format!("{DYLIBS_DIR}/libz.1.2.3.dylib")),
            )
            .with_child(
                // symlink
                "libz.dylib",
                FsNode::resource_file(format!("{DYLIBS_DIR}/libz.1.2.3.dylib")),
            )
            .with_child(
                // symlink
                "libz.1.1.3.dylib",
                FsNode::resource_file(format!("{DYLIBS_DIR}/libz.1.2.3.dylib")),
            )
            .with_child(
                "libiconv.2.dylib",
                FsNode::resource_file(format!("{}/libiconv.2.dylib", DYLIBS_DIR)),
            )
            .with_child(
                "libc++abi.dylib",
                FsNode::resource_file(format!("{DYLIBS_DIR}/libc++abi.dylib")),
            )
            .with_child(
                "libsqlite3.dylib",
                FsNode::resource_file(format!("{DYLIBS_DIR}/libsqlite3.dylib")),
            )
            .with_child(
                // symlink
                "libsqlite3.0.dylib",
                FsNode::resource_file(format!("{DYLIBS_DIR}/libsqlite3.dylib")),
            );

        let mut app_dir_children = HashMap::new();
        app_dir_children.insert(bundle_dir_name, app_bundle.into_fs_node());
        for (dir, host_path) in directories.iter().zip(host_path_directories.iter()) {
            if let Some(host_path) = host_path {
                app_dir_children.insert(
                    dir.to_string(),
                    FsNode::from_host_dir(host_path, /* writeable: */ true),
                );
            }
        }

        let library_node = match &host_path_directories[1] {
            Some(host_path) => FsNode::from_host_dir(host_path, true),
            None => FsNode::dir(),
        };

        // Создаем физическую папку для корня ФС (чтобы shm_open мог создавать
        // файлы вроде /mono.1)
        let root_host_path = paths::user_data_base_path()
            .join(paths::SANDBOX_DIR)
            .join(bundle_id)
            .join("root");

        if !read_only_mode {
            // Очищаем временные файлы корня при каждом запуске (аналогично tmp)
            let _ = std::fs::remove_dir_all(&root_host_path);
            if let Err(e) = std::fs::create_dir_all(&root_host_path) {
                panic!("Could not create root directory for app at {root_host_path:?}: {e:?}");
            }
        }

        // Если режим не read_only, монтируем физическую папку как корень с
        // правами на запись
        let root_node = if read_only_mode {
            FsNode::dir()
        } else {
            FsNode::from_host_dir(&root_host_path, true)
        };

        // Create a writable host directory for the app's sandbox root
        // (the UUID directory). Some apps (notably Unity/Mono games) need to
        // create hidden directories like .wapi in this location.
        let app_sandbox_host_path = if !read_only_mode {
            let path = paths::user_data_base_path()
                .join(paths::SANDBOX_DIR)
                .join(bundle_id)
                .join("AppSandbox");
            if let Err(e) = std::fs::create_dir_all(&path) {
                panic!("Could not create app sandbox directory at {path:?}: {e:?}");
            }
            Some(path)
        } else {
            None
        };

        let root = root_node
            .with_child(
                "var",
                FsNode::dir().with_child(
                    "mobile",
                    FsNode::dir()
                        .with_child(
                            "Applications",
                            FsNode::dir().with_child(
                                FAKE_UUID,
                                FsNode::Directory {
                                    children: app_dir_children,
                                    writeable: app_sandbox_host_path,
                                },
                            ),
                        )
                        .with_child("Library", library_node),
                ),
            )
            .with_child("usr", FsNode::dir().with_child("lib", usr_lib));
        log_dbg!("Initial filesystem layout: {:#?}", root);

        let fs = Fs {
            root,
            working_directory,
            home_directory,
        };
        assert!(fs.lookup_node(&bundle_guest_path).is_some());
        (fs, bundle_guest_path)
    }

    /// Create a fake filesystem (see [crate::Environment::new_without_app]).
    pub fn new_fake_fs() -> Fs {
        Fs {
            root: FsNode::dir(),
            working_directory: GuestPathBuf::from(String::new()),
            home_directory: GuestPathBuf::from(String::new()),
        }
    }

    /// Get the absolute path of the guest app's (sandboxed) home directory.
    pub fn home_directory(&self) -> &GuestPath {
        &self.home_directory
    }

    /// Get the absolute path of the current working directory.
    /// The resulting
    /// path may be invalid if the directory was moved or deleted.
    pub fn working_directory(&self) -> &GuestPath {
        &self.working_directory
    }

    /// Attempts to change the working directory.
    pub fn change_working_directory(&mut self, new_path: &GuestPath) -> Result<&GuestPath, ()> {
        let resolved = resolve_path(new_path, Some(&self.working_directory));
        if !matches!(
            self.lookup_node_inner(&resolved),
            Some(FsNode::Directory { .. })
        ) {
            return Err(());
        }
        let new_path = if resolved.is_empty() {
            String::from("/")
        } else {
            let mut new_path = String::with_capacity(resolved.iter().map(|c| c.len() + 1).sum());
            for component in resolved {
                new_path.push('/');
                new_path.push_str(component);
            }
            new_path
        };
        self.working_directory = GuestPathBuf::from(new_path);
        Ok(&self.working_directory)
    }

    /// [Self::lookup_node] with a pre-resolved path.
    fn lookup_node_inner(&self, resolved_path_components: &[&str]) -> Option<&FsNode> {
        let mut node = &self.root;
        for component in resolved_path_components {
            let FsNode::Directory {
                children,
                writeable: _,
            } = node
            else {
                return None;
            };
            node = children.get(*component)?
        }
        Some(node)
    }

    /// Get the node at a given path, if it exists.
    fn lookup_node(&self, path: &GuestPath) -> Option<&FsNode> {
        self.lookup_node_inner(&resolve_path(path, Some(&self.working_directory)))
    }

    /// Get the parent of the node at a given path, if it exists, and return it
    /// together with the final path component.
    /// This is an alternative to
    /// [Self::lookup_node] useful when writing to a file, where it might not
    /// exist yet (but its parent directory does).
    fn lookup_parent_node(&mut self, path: &GuestPath) -> Option<(&mut FsNode, String)> {
        let components = resolve_path(path, Some(&self.working_directory));
        let (&final_component, parent_components) = components.split_last()?;

        let mut parent = &mut self.root;
        for &component in parent_components {
            let FsNode::Directory {
                children,
                writeable: _,
            } = parent
            else {
                return None;
            };
            parent = children.get_mut(component)?
        }

        Some((parent, final_component.to_string()))
    }

    /// Like [Path::exists] but for the guest filesystem.
    pub fn exists(&self, path: &GuestPath) -> bool {
        self.lookup_node(path).is_some()
    }

    /// Returns access information about the file/directory at the path
    /// (exists, read, write, execute)
    pub fn access(&self, path: &GuestPath) -> (bool, bool, bool, bool) {
        match self.lookup_node(path) {
            None => (false, false, false, false),
            Some(node) => match node {
                FsNode::File {
                    location: _,
                    writeable,
                } => (true, true, *writeable, false),
                FsNode::Directory {
                    children: _,
                    writeable,
                } => (true, true, writeable.is_some(), true),
            },
        }
    }

    /// Like [Path::is_file] but for the guest filesystem.
    pub fn is_file(&self, path: &GuestPath) -> bool {
        matches!(self.lookup_node(path), Some(FsNode::File { .. }))
    }

    /// Like [Path::is_dir] but for the guest dirsystem.
    pub fn is_dir(&self, path: &GuestPath) -> bool {
        matches!(self.lookup_node(path), Some(FsNode::Directory { .. }))
    }

    pub fn modified(&self, path: &GuestPath) -> Result<i64, ()> {
        // TODO: error handling
        let node = self.lookup_node(path).ok_or(())?;
        match node {
            FsNode::File { location, .. } => match location {
                // Note: the returned time is consistent with 'Date' and 'Time'
                // of files inside IPA archive as reported by 7-zip.
                // But it can be few hours off in comparison with modification
                // time reported by NSFileModificationDate for app bundle files
                // and changes if system timezone changes and apps gets
                // re-installed!
                // This shouldn't be a big problem as we're always assuming
                // GMT in the codebase right now.
                // TODO: double check that when we support different timezones
                FileLocation::IpaFileRef(ipa_file_ref) => {
                    Ok(ipa_file_ref.get_last_modified().into())
                }
                FileLocation::Path(path) => {
                    // TODO: account for the current timezone, here it's in GMT
                    fs::metadata(path)
                        .and_then(|m| m.modified())
                        .map(|t| {
                            t.duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                                .try_into()
                                .unwrap()
                        })
                        .map_err(|_| ())
                }
                FileLocation::ResourceFilePath(_) => Ok(0),
            },
            FsNode::Directory { writeable, .. } => {
                if let Some(host_path) = writeable {
                    fs::metadata(host_path)
                        .and_then(|m| m.modified())
                        .map(|t| {
                            t.duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                                .try_into()
                                .unwrap()
                        })
                        .map_err(|_| ())
                } else {
                    Ok(0)
                }
            }
        }
    }

    pub fn size(&self, path: &GuestPath) -> Result<u64, ()> {
        // TODO: error handling
        let node = self.lookup_node(path).ok_or(())?;
        match node {
            FsNode::File { location, .. } => match location {
                FileLocation::IpaFileRef(ipa_file_ref) => Ok(ipa_file_ref.get_size()),
                FileLocation::Path(path) => {
                    fs::metadata(path).map(|meta| meta.len()).map_err(|_| ())
                }
                FileLocation::ResourceFilePath(_) => Ok(0),
            },
            FsNode::Directory { writeable, .. } => {
                if let Some(host_path) = writeable {
                    fs::metadata(host_path)
                        .map(|meta| meta.len())
                        .map_err(|_| ())
                } else {
                    Ok(4096)
                }
            }
        }
    }

    /// Get an iterator over the names of files/directories in a directory.
    pub fn enumerate<P: AsRef<GuestPath>>(
        &self,
        path: P,
    ) -> Result<impl Iterator<Item = &str>, ()> {
        let Some(FsNode::Directory { children, .. }) = self.lookup_node(path.as_ref()) else {
            return Err(());
        };
        Ok(children.keys().map(|name| name.as_str()))
    }

    /// Similar to [Fs::enumerate], but also returns fs node type.
    pub fn enumerate_with_types<P: AsRef<GuestPath>>(
        &self,
        path: P,
    ) -> Result<impl Iterator<Item = (&str, FsNodeType)>, ()> {
        let Some(FsNode::Directory { children, .. }) = self.lookup_node(path.as_ref()) else {
            return Err(());
        };
        Ok(children.iter().map(|(name, node)| {
            (
                name.as_str(),
                match node {
                    FsNode::File { .. } => FsNodeType::File,
                    FsNode::Directory { .. } => FsNodeType::Directory,
                },
            )
        }))
    }

    /// Recursively list the paths of files/directories in a directory.
    /// The base path (`path`) is not included in the returned paths.
    pub fn enumerate_recursive<P: AsRef<GuestPath>>(
        &self,
        path: P,
    ) -> Result<Vec<GuestPathBuf>, ()> {
        let Some(FsNode::Directory { children, .. }) = self.lookup_node(path.as_ref()) else {
            return Err(());
        };

        let mut paths = Vec::new();
        let mut component_stack: Vec<&str> = Vec::new();
        let mut iterator_stack = vec![children.iter()];
        loop {
            let current_iterator = iterator_stack.last_mut().unwrap();
            if let Some((next_component, next_node)) = current_iterator.next() {
                component_stack.push(next_component);
                paths.push(GuestPathBuf::from(component_stack.join("/")));
                if let FsNode::Directory { children, .. } = next_node {
                    iterator_stack.push(children.iter());
                } else {
                    component_stack.pop();
                }
            } else {
                iterator_stack.pop();
                if component_stack.pop().is_none() {
                    break;
                }
            }
        }
        assert!(component_stack.is_empty() && iterator_stack.is_empty());
        Ok(paths)
    }

    /// Like [std::fs::read] but for the guest filesystem.
    pub fn read<P: AsRef<GuestPath>>(&self, path: P) -> Result<Vec<u8>, ()> {
        let mut file = self.open(path.as_ref())?;
        let mut result = Vec::new();
        file.read_to_end(&mut result).map_err(|_| ())?;
        Ok(result)
    }

    /// Like [std::fs::write] but for the guest filesystem.
    pub fn write<P: AsRef<GuestPath>>(&mut self, path: P, data: &[u8]) -> Result<(), ()> {
        let mut options = GuestOpenOptions::new();
        options.write().create().truncate();
        self.open_with_options(path, options)?
            .write_all(data)
            .map_err(|_| ())
    }

    /// Like [File::open] but for the guest filesystem.
    #[allow(dead_code)]
    pub fn open<P: AsRef<GuestPath>>(&self, path: P) -> Result<GuestFile, ()> {
        // it would be nice to delegate to self.open_with_options, but
        // currently it wants a mutable reference to self
        let node = self.lookup_node(path.as_ref()).ok_or(())?;
        match node {
            FsNode::File { location, .. } => match location {
                FileLocation::Path(host_path) => {
                    let host_file = handle_open_err(File::open(host_path), host_path);
                    Ok(GuestFile::from_host_file(host_file))
                }
                FileLocation::IpaFileRef(file) => Ok(GuestFile::from_ipa_file(file)),
                FileLocation::ResourceFilePath(name) => {
                    let resource_file = handle_open_err(paths::ResourceFile::open(name), name);
                    Ok(GuestFile::from_resource_file(resource_file))
                }
            },
            FsNode::Directory { .. } => Err(()),
        }
    }

    // ИСПРАВЛЕНИЕ: ЧЕСТНАЯ РЕАЛИЗАЦИЯ ПЕРЕИМЕНОВАНИЯ 
    // Поддерживает и файлы, и директории, обновляет дерево VFS без паники
    pub fn rename<P: AsRef<GuestPath> + Copy>(&mut self, from: P, to: P) -> Result<(), ()> {
        let from_path = from.as_ref();
        let to_path = to.as_ref();

        if from_path.as_str() == to_path.as_str() {
            return Ok(());
        }

        let from_host_path = match self.lookup_node(from_path).ok_or(())? {
            FsNode::File {
                location: FileLocation::Path(p),
                writeable: true,
            } => p.clone(),
            FsNode::Directory {
                writeable: Some(p),
                ..
            } => p.clone(),
            _ => return Err(()), // Нельзя перемещать read-only или системные файлы архива
        };

        let (to_parent_node, to_name) = self.lookup_parent_node(to_path).ok_or(())?;
        let to_parent_host_path = match to_parent_node {
            FsNode::Directory {
                writeable: Some(p), ..
            } => p.clone(),
            _ => return Err(()), // Нельзя перемещать в read-only родителя
        };
        let to_host_path = to_parent_host_path.join(&to_name);

        // 1. Выполняем настоящее физическое перемещение на диске хоста
        if fs::rename(&from_host_path, &to_host_path).is_err() {
            return Err(());
        }

        // 2. Извлекаем старую ноду из VFS
        let (from_parent_node, from_name) = self.lookup_parent_node(from_path).unwrap();
        let FsNode::Directory {
            children: from_children,
            ..
        } = from_parent_node
        else {
            unreachable!()
        };
        let mut moving_node = from_children.remove(&from_name).unwrap();

        // 3. Рекурсивно обновляем внутри нее все Host-пути на новые
        moving_node.update_host_paths_recursively(to_host_path);

        // 4. Вставляем обновленную ноду по новому пути в VFS
        let (to_parent_final, to_name_final) = self.lookup_parent_node(to_path).unwrap();
        let FsNode::Directory {
            children: to_children,
            ..
        } = to_parent_final
        else {
            unreachable!()
        };
        to_children.insert(to_name_final, moving_node);

        Ok(())
    }

    /// Like [File::options] but for the guest filesystem.
    pub fn open_with_options<P: AsRef<GuestPath>>(
        &mut self,
        path: P,
        options: GuestOpenOptions,
    ) -> Result<GuestFile, ()> {
        let path = path.as_ref();
        let was_a_create = options.create;
        let res = self.open_with_options_inner(path, options);
        // A failed open of a file the app expected to read is worth reporting:
        // engines that can't find their assets typically carry on with null
        // objects and simply render nothing, so this miss is the only warning
        // we get, and it used to be `log_dbg!`-only, i.e. invisible in release
        // builds — which is every build users actually run.
        if res.is_err() && !was_a_create {
            self.log_failed_open(path);
        }
        res
    }

    /// Report a failed open, throttled. Apps probe for optional files (save
    /// games, per-language assets, config overrides) as a matter of course, so
    /// logging every miss forever would drown out everything else.
    fn log_failed_open(&self, path: &GuestPath) {
        // Deduplicate by path rather than counting occurrences: an app that
        // probes the same missing file every frame would otherwise use up the
        // budget and hide the *other* paths it failed to find, which are the
        // interesting ones. The distinct-path count is what needs bounding.
        const LIMIT: usize = 1024;
        static SEEN: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

        let Ok(mut seen) = SEEN.lock() else { return };
        let seen = seen.get_or_insert_with(std::collections::HashSet::new);
        if seen.len() >= LIMIT {
            return;
        }
        if !seen.insert(path.as_str().to_owned()) {
            return;
        }
        log!(
            "Warning: the app tried to open {:?}, which does not exist",
            path.as_str()
        );
        if seen.len() == LIMIT {
            log!("Warning: further \"file does not exist\" warnings are silenced");
        }
    }

    fn open_with_options_inner(
        &mut self,
        path: &GuestPath,
        options: GuestOpenOptions,
    ) -> Result<GuestFile, ()> {
        let GuestOpenOptions {
            read,
            mut write, // ИСПРАВЛЕНИЕ: Разрешаем менять переменную
            append,
            create,
            truncate,
        } = options;

        // ИСПРАВЛЕНИЕ: Мягкий перехват вместо вызова panic!.
        // Если запрашивается создание или очистка файла без права записи,
        // принудительно даем право на запись.
        if (truncate || create) && !write && !append {
            log!("Warning: App tried to create/truncate file without write permissions. Forcing write = true.");
            write = true;
        }

        let (parent_node, new_filename) = self.lookup_parent_node(path).ok_or(())?;
        let FsNode::Directory {
            children,
            writeable: dir_host_path,
        } = parent_node
        else {
            return Err(());
        };

        // Open an existing file if possible

        if let Some(existing_file) = children.get(&new_filename) {
            match existing_file {
                &FsNode::File {
                    ref location,
                    writeable,
                } => {
                    if !writeable && (append || write) {
                        log!("Warning: attempt to write to read-only file {:?}", path);
                        return Err(());
                    }
                    match location {
                        FileLocation::Path(host_path) => {
                            let file = handle_open_err(
                                File::options()
                                    .read(read)
                                    .write(write)
                                    .append(append)
                                    .create(false)
                                    .truncate(truncate)
                                    .open(host_path),
                                host_path,
                            );
                            return Ok(GuestFile::File(file));
                        }
                        FileLocation::IpaFileRef(file) => {
                            assert!(!(writeable || append || write));
                            return Ok(GuestFile::from_ipa_file(file));
                        }
                        FileLocation::ResourceFilePath(name) => {
                            assert!(!(writeable || append || write));
                            let resource_file =
                                handle_open_err(paths::ResourceFile::open(name), name);
                            return Ok(GuestFile::from_resource_file(resource_file));
                        }
                    }
                }
                FsNode::Directory { .. } => {
                    if write {
                        return Err(());
                    } else {
                        return Ok(GuestFile::from_directory());
                    }
                }
            }
        };
        // Create a new file otherwise

        if !create {
            return Err(());
        }

        let Some(dir_host_path) = dir_host_path else {
            log!(
                "Warning: attempt to create file at path {:?}, but directory is read-only",
                path
            );
            return Err(());
        };

        for c in new_filename.chars() {
            if std::path::is_separator(c) {
                panic!("Attempt to create file at path {path:?}, but filename contains path separator character {c:?}!");
            }
        }

        let host_path = dir_host_path.join(&new_filename);
        let file = handle_open_err(
            File::options()
                .read(read)
                .write(write)
                .append(append)
                .create(create)
                .truncate(truncate)
                .open(&host_path),
            &host_path,
        );
        log_dbg!(
            "Created file at path {:?} (host path: {:?})",
            path,
            host_path
        );
        children.insert(
            new_filename,
            FsNode::File {
                location: FileLocation::Path(host_path),
                writeable: true,
            },
        );
        Ok(GuestFile::File(file))
    }

    /// Removes a file or a directory.
    /// If the node is a directory, it must be
    /// empty.
    pub fn remove<P: AsRef<GuestPath>>(&mut self, path: P) -> Result<(), FsError> {
        let path = path.as_ref();
        let (parent_node, node_name) = self
            .lookup_parent_node(path)
            .ok_or(FsError::NonexistentParentDir)?;
        // Parent directory is not a directory
        let FsNode::Directory {
            children,
            writeable: dir_writeable,
        } = parent_node
        else {
            return Err(FsError::InvalidParentDir);
        };

        if !dir_writeable.is_some() {
            log!("Warning: attempt to delete file or directroy at path {:?}, but parent directory is read-only", path);
            return Err(FsError::ReadonlyParentDir);
        };

        let Some(node) = children.get(&node_name) else {
            // There is no file/directory with this name
            return Err(FsError::DoesNotExist);
        };

        match node {
            FsNode::File {
                location,
                writeable,
            } => {
                // Read-only files can't be removed.
                // (This is probably not
                // correct, but it is safer for now.)
                if !writeable {
                    return Err(FsError::AccessDenied);
                }

                let host_path = match location {
                    FileLocation::Path(host_path) => host_path,
                    FileLocation::IpaFileRef(_) | FileLocation::ResourceFilePath(_) => panic!(),
                };
                handle_open_err(std::fs::remove_file(host_path), host_path);
                log_dbg!(
                    "Deleted file at path {:?} (host path: {:?})",
                    path,
                    host_path
                );
            }
            FsNode::Directory {
                children,
                writeable,
            } => {
                // Directory is not empty
                if !children.is_empty() {
                    return Err(FsError::DirectoryNotEmpty);
                }
                // Read-only directories can't be removed.
                // (This is probably not
                // correct, but it is safer for now.)
                let Some(host_path) = writeable else {
                    return Err(FsError::AccessDenied);
                };

                handle_open_err(std::fs::remove_dir(host_path), host_path);
                log_dbg!(
                    "Deleted directory at path {:?} (host path: {:?})",
                    path,
                    host_path
                );
            }
        }

        children.remove(&node_name).unwrap();
        Ok(())
    }

    /// Like [std::fs::create_dir_all] but for the guest filesystem.
    pub fn create_dir_all<P: AsRef<GuestPath>>(&mut self, path: P) -> Result<(), FsError> {
        let path = path.as_ref();

        // 1. Получаем компоненты пути.
        // .into_iter().map(|s| s.to_string()).collect() — КРИТИЧЕСКИ ВАЖНО.
        // Это превращает Vec<&str> в Vec<String>, освобождая self от
        // заимствования.
        let components: Vec<String> = resolve_path(path, Some(&self.working_directory))
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        let mut current_path = String::new();

        // 2. Теперь мы можем спокойно итерироваться и вызывать мутабельные
        // методы self
        for component in components {
            // Собираем путь по кусочкам: /var -> /var/mobile ->
            // /var/mobile/Applications...
            current_path.push('/');
            current_path.push_str(&component);

            let res = self.create_dir(GuestPathBuf::from(current_path.clone()));
            match res {
                Ok(_) | Err(FsError::AlreadyExist) => {
                    // Если папка уже есть — это нормально, идем дальше к
                    // вложенным
                }
                _ => return res, // Если другая ошибка (нет прав и т.д.) — выходим
            }
        }
        Ok(())
    }

    /// Like [std::fs::create_dir] but for the guest filesystem.
    pub fn create_dir<P: AsRef<GuestPath>>(&mut self, path: P) -> Result<(), FsError> {
        let path = path.as_ref();
        let (parent_node, new_dir_name) = self
            .lookup_parent_node(path)
            .ok_or(FsError::NonexistentParentDir)?;
        // Parent directory is not a directory
        let FsNode::Directory {
            children,
            writeable: dir_host_path,
        } = parent_node
        else {
            return Err(FsError::InvalidParentDir);
        };

        // There's already a file/directory with this name
        if children.contains_key(&new_dir_name) {
            return Err(FsError::AlreadyExist);
        }

        let Some(dir_host_path) = dir_host_path else {
            log!("Warning: attempt to create directory at path {:?}, but parent directory is read-only", path);
            return Err(FsError::ReadonlyParentDir);
        };

        for c in new_dir_name.chars() {
            if std::path::is_separator(c) {
                panic!("Attempt to create directory at path {path:?}, but directory name contains path separator character {c:?}!");
            }
        }

        let host_path = dir_host_path.join(&new_dir_name);
        // Use create_dir but tolerate AlreadyExists — the directory may exist
        // on disk from a previous run even though it was not in the in-memory
        // filesystem tree (e.g. the tree was rebuilt on launch while the host
        // directory was preserved).
        match std::fs::create_dir(&host_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // The directory already exists on the host — this is fine.
                log_dbg!(
                    "create_dir: host directory already exists at {:?}, reusing",
                    host_path
                );
            }
            Err(e) => {
                handle_open_err(Err::<(), _>(e), &host_path);
            }
        }
        log_dbg!(
            "Created directory at path {:?} (host path: {:?})",
            path,
            host_path
        );
        children.insert(
            new_dir_name,
            FsNode::Directory {
                children: HashMap::new(),
                writeable: Some(host_path),
            },
        );
        Ok(())
    }
}

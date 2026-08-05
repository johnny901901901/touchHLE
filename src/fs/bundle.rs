/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! IPA file format support, allowing it to be used as part of the guest
//! filesystem.
use crate::fs::{FsNode, GuestPath};
use crate::libc::time::{calendar_date_to_timestamp, time_t, tm};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use zip::result::ZipError;
use zip::ZipArchive;

/// A helper struct to build an FsNode with files and directories coming in
/// arbitrary order. This is required, because ZIP files are allowed to store
/// entries in arbitrary order.
struct FsNodeBuilder {
    root: FsNode,
}

impl FsNodeBuilder {
    fn new() -> Self {
        Self {
            root: FsNode::dir(),
        }
    }

    fn find_or_make_directory(&mut self, path: &GuestPath) -> &mut FsNode {
        let mut current = &mut self.root;
        for part in path.as_str().split('/') {
            if part.is_empty() {
                continue;
            }
            assert_ne!(part, "..", "unexpected .. in path: {path:?}");
            let FsNode::Directory { children, .. } = current else {
                panic!("expected directory, got {current:?}");
            };

            let next = children.entry(part.to_string()).or_insert_with(FsNode::dir);
            current = next;
        }
        current
    }

    fn add_file(&mut self, path: &GuestPath, node: FsNode) {
        let (parent_name, file_name) = path.parent_and_file_name().unwrap();
        assert_ne!(file_name, "..", "unexpected .. in path: {path:?}");
        let dir = self.find_or_make_directory(parent_name);
        let FsNode::Directory { children, .. } = dir else {
            panic!("expected directory, got {dir:?}");
        };

        children.insert(file_name.to_string(), node);
    }

    fn add_directory(&mut self, path: &GuestPath) {
        self.find_or_make_directory(path);
    }

    fn build(self) -> FsNode {
        self.root
    }
}

/// Represents an open app bundle, either a directory or a zip file.
pub enum BundleData {
    HostDirectory(PathBuf),
    Zip {
        zip: ZipArchive<std::fs::File>,
        /// Path to the app bundle inside the zip file.
        /// It should be `"Payload/<app name>.app"` (no trailing slash!).
        bundle_path: String,
    },
}

impl BundleData {
    fn find_bundle_path_in_archive(zip: &mut ZipArchive<std::fs::File>) -> Result<String, String> {
        for i in 0..zip.len() {
            let file = zip
                .by_index(i)
                .map_err(|e| format!("Could not open IPA archive entry: {e}"))?;
            let path = file.name();
            if let Some(name) = path
                .strip_prefix("Payload/")
                .and_then(|path| path.split_once('/'))
                .and_then(|(name, _)| name.strip_suffix(".app"))
            {
                return Ok(format!("Payload/{name}.app"));
            }
        }
        Err("no app bundle found in the IPA archive".to_string())
    }

    pub fn bundle_name(&self) -> &str {
        match self {
            BundleData::HostDirectory(bundle_path) => {
                bundle_path.file_stem().unwrap().to_str().unwrap()
            }
            BundleData::Zip { bundle_path, .. } => bundle_path
                .rsplit_once('/')
                .unwrap()
                .1
                .strip_suffix(".app")
                .unwrap(),
        }
    }

    pub fn open_host_dir(path: &Path) -> Result<BundleData, String> {
        Ok(BundleData::HostDirectory(path.to_path_buf()))
    }

    pub fn open_ipa(path: &Path) -> Result<BundleData, String> {
        let file =
            std::fs::File::open(path).map_err(|e| format!("Could not open IPA file: {e}"))?;
        let mut zip =
            ZipArchive::new(file).map_err(|e| format!("Could not open IPA archive: {e}"))?;
        let bundle_path = Self::find_bundle_path_in_archive(&mut zip)?;
        Ok(BundleData::Zip { zip, bundle_path })
    }

    pub fn open_any(path: &Path) -> Result<BundleData, String> {
        if path.is_file()
            && path
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("ipa"))
                .unwrap_or(false)
        {
            Ok(Self::open_ipa(path)?)
        } else if path.is_dir() {
            Ok(Self::open_host_dir(path)?)
        } else {
            Err(format!(
                "{} is not a directory or an IPA file",
                path.display()
            ))
        }
    }

    pub(super) fn into_fs_node(self) -> FsNode {
        match self {
            BundleData::HostDirectory(path) => FsNode::from_host_dir(&path, false),
            BundleData::Zip { zip, bundle_path } => {
                let archive = Rc::new(RefCell::new(zip));
                let archive_cache = Rc::new(RefCell::new(ArchiveFilesCache::new()));
                let metadata_map = Rc::new(RefCell::new(HashMap::new()));

                let mut archive_guard = (*archive).borrow_mut();

                let mut builder = FsNodeBuilder::new();
                for i in 0..archive_guard.len() {
                    let file = archive_guard.by_index(i).unwrap(); // TODO: report IO error?
                    let name = file.name();
                    if let Some(path) = name.strip_prefix(&bundle_path) {
                        let path = GuestPath::new(path);
                        if file.is_dir() {
                            builder.add_directory(path);
                        } else {
                            builder.add_file(
                                path,
                                FsNode::bundle_zip_file(IpaFileRef {
                                    archive: archive.clone(),
                                    archive_files_cache: archive_cache.clone(),
                                    metadata_map: metadata_map.clone(),
                                    index: i,
                                }),
                            );
                        }
                    }
                }
                builder.build()
            }
        }
    }

    pub fn read_plist(&mut self) -> Result<Vec<u8>, String> {
        match self {
            BundleData::HostDirectory(path) => {
                std::fs::read(path.join("Info.plist")).map_err(|e| {
                    format!("Could not read Info.plist from the app bundle directory: {e}")
                })
            }
            BundleData::Zip { zip, bundle_path } => {
                let mut file = zip
                    .by_name(&format!("{bundle_path}/Info.plist"))
                    .map_err(|e| format!("Could not open Info.plist from the IPA archive: {e}"))?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .map_err(|e| format!("Could not read Info.plist from the IPA archive: {e}"))?;
                Ok(buf)
            }
        }
    }
}

#[derive(Debug)]
struct ArchivedFileMetadata {
    /// Unix timestamp of file modification
    last_modified: i64,
    /// Uncompressed file size
    size: u64,
}

/// Shared (refcounted) copy of the decompressed version of a file in an IPA.
///
/// Seeking in compressed files is hard, so the simple solution is to read the
/// whole file into memory. This is shared so having multiple copies of the same
/// file open won't waste memory.
type DecompressedFile = Rc<[u8]>;

/// Default limit on how much decompressed IPA content to keep cached, in MiB.
///
/// Entries used to be cached forever with no bound. That is fine for the small
/// bundles this cache was written for, but a Unity-era game keeps hundreds of
/// megabytes of assets in its IPA, and reading them all once was enough to push
/// the host process past iOS's per-process memory limit and get it killed by
/// jetsam (Colin McRae Rally reached ~1.85 GiB resident and died silently just
/// after its first frame).
///
/// Eviction only drops the *cache's* reference: an [IpaFile] that is still open
/// holds its own [Rc] and keeps reading happily from its buffer.
const DEFAULT_ARCHIVE_CACHE_BUDGET_MIB: usize = 64;

/// Environment variable overriding [DEFAULT_ARCHIVE_CACHE_BUDGET_MIB]. `0`
/// means unbounded, i.e. the original cache-forever behaviour.
///
/// This is read from the environment rather than plumbed through [crate::Options]
/// because the cache is built while the bundle is being opened, before an
/// `Environment` exists — and because the iOS host has to be able to set it
/// without changing the C ABI it shares with the other emulator core.
const ARCHIVE_CACHE_BUDGET_ENV: &str = "TOUCHHLE_IPA_CACHE_BUDGET_MIB";

fn archive_cache_budget() -> Option<usize> {
    // The launcher opens every game's bundle to read its metadata, so this runs
    // once per game in the library; only say it once.
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut announce = |message: std::fmt::Arguments| {
        ONCE.call_once(|| log!("{}", message));
    };
    let mib = match std::env::var(ARCHIVE_CACHE_BUDGET_ENV) {
        Ok(value) => match value.trim().parse::<usize>() {
            Ok(mib) => mib,
            Err(_) => {
                log!(
                    "Warning: ignoring {}={:?}: expected a number of MiB (0 for unlimited).",
                    ARCHIVE_CACHE_BUDGET_ENV,
                    value
                );
                DEFAULT_ARCHIVE_CACHE_BUDGET_MIB
            }
        },
        Err(_) => DEFAULT_ARCHIVE_CACHE_BUDGET_MIB,
    };
    if mib == 0 {
        announce(format_args!(
            "Decompressed-IPA cache is unlimited ({ARCHIVE_CACHE_BUDGET_ENV}=0). \
             Large bundles may exhaust memory."
        ));
        None
    } else {
        announce(format_args!("Decompressed-IPA cache limit: {mib} MiB."));
        Some(mib * 1024 * 1024)
    }
}

/// The decompressed-content cache for one IPA, with a byte budget.
#[derive(Debug)]
struct ArchiveFilesCache {
    /// Maximum total size of cached content, or [None] for unlimited.
    budget: Option<usize>,
    /// Cached content by zip entry index, with the tick at which it was last
    /// used.
    files: HashMap<usize, (DecompressedFile, u64)>,
    /// Total size of everything in `files`.
    bytes: usize,
    /// Monotonic counter standing in for a clock; only the order matters.
    tick: u64,
}

impl ArchiveFilesCache {
    fn new() -> Self {
        ArchiveFilesCache {
            budget: archive_cache_budget(),
            files: HashMap::new(),
            bytes: 0,
            tick: 0,
        }
    }

    fn get(&mut self, index: usize) -> Option<DecompressedFile> {
        self.tick += 1;
        let tick = self.tick;
        let (file, last_used) = self.files.get_mut(&index)?;
        *last_used = tick;
        Some(Rc::clone(file))
    }

    fn insert(&mut self, index: usize, file: DecompressedFile) {
        self.tick += 1;
        self.bytes += file.len();
        if let Some((evicted, _)) = self.files.insert(index, (file, self.tick)) {
            self.bytes -= evicted.len();
        }
        self.evict_to_budget(index);
    }

    /// Drop least-recently-used entries until we are within budget, never
    /// evicting `keep` (the entry the caller is about to hand out — evicting it
    /// would be pointless work, and a single entry larger than the whole budget
    /// must still be cacheable).
    fn evict_to_budget(&mut self, keep: usize) {
        let Some(budget) = self.budget else { return };
        while self.bytes > budget {
            let victim = self
                .files
                .iter()
                .filter(|&(&index, _)| index != keep)
                .min_by_key(|&(_, &(_, last_used))| last_used)
                .map(|(&index, _)| index);
            let Some(victim) = victim else {
                // Only `keep` is left; nothing more we can give back.
                return;
            };
            if let Some((evicted, _)) = self.files.remove(&victim) {
                self.bytes -= evicted.len();
                log_dbg!(
                    "Evicted IPA entry {} ({} bytes) from the decompressed-file cache; {} bytes cached",
                    victim,
                    evicted.len(),
                    self.bytes
                );
            }
        }
    }
}

/// Represents a file inside an IPA bundle that can be opened.
#[derive(Debug)]
pub struct IpaFileRef {
    archive: Rc<RefCell<ZipArchive<std::fs::File>>>,
    archive_files_cache: Rc<RefCell<ArchiveFilesCache>>,
    metadata_map: Rc<RefCell<HashMap<usize, ArchivedFileMetadata>>>,
    index: usize,
}

impl IpaFileRef {
    pub fn open(&self) -> IpaFile {
        // Some games, like THPS2, use a single resource bundle file which is
        // opened each time a new game resource is being read.
        // As IPA is basically an archive, this pattern requires unzipping to be
        // done each time, which is extremely slow.
        // The solution here is to cache unzipped data in memory, which should
        // be OK as early iOS IPA files are relatively small in size.
        let mut archive_cache = (*self.archive_files_cache).borrow_mut();
        if let Some(cached_file) = archive_cache.get(self.index) {
            return IpaFile {
                file: Cursor::new(cached_file),
            };
        }
        let decompressed = {
            // Read the zip entry into an owned buffer inside its own block so
            // the `archive` RefMut is released before we touch the caches.
            let mut archive = (*self.archive).borrow_mut();
            let decoded: Option<(Vec<u8>, ArchivedFileMetadata)> = match archive
                .by_index(self.index)
            {
                Ok(mut file) => {
                    let modified = file.last_modified();
                    // This is not the cleanest way!
                    // TODO: just use `time` or `chrono` crates for time conversions
                    // (this also entails a lot of refactoring in [crate::libc:time])
                    let tm = tm::from(
                        modified.year(),
                        modified.month(),
                        modified.day(),
                        modified.hour(),
                        modified.minute(),
                        modified.second(),
                    );
                    let timestamp = calendar_date_to_timestamp(tm);
                    let size = file.size();
                    let mut buf = Vec::new();
                    if let Err(e) = file.read_to_end(&mut buf) {
                        log!(
                            "Warning: IpaFileRef::open(): IO error decompressing IPA entry {}: {}; returning partial buffer ({} bytes) to guest.",
                            self.index,
                            e,
                            buf.len()
                        );
                    }
                    Some((
                        buf,
                        ArchivedFileMetadata {
                            last_modified: timestamp.into(),
                            size,
                        },
                    ))
                }
                Err(ZipError::Io(e)) => {
                    log!(
                        "Warning: IpaFileRef::open(): IO error reading IPA entry {}: {}; returning empty file to guest.",
                        self.index,
                        e
                    );
                    None
                }
                Err(e) => {
                    log!(
                        "Warning: IpaFileRef::open(): could not open IPA entry {}: {}; returning empty file to guest.",
                        self.index,
                        e
                    );
                    None
                }
            };
            drop(archive);

            match decoded {
                Some((buf, meta)) => {
                    (*self.metadata_map)
                        .borrow_mut()
                        .entry(self.index)
                        .or_insert(meta);
                    Rc::from(buf)
                }
                None => {
                    Rc::from(Vec::new())
                }
            }
        };
        archive_cache.insert(self.index, Rc::clone(&decompressed));
        IpaFile {
            file: Cursor::new(decompressed),
        }
    }

    pub fn get_last_modified(&self) -> time_t {
        if !self.metadata_map.borrow().contains_key(&self.index) {
            // This will force metadata loading
            // TODO: get metadata without reading the file
            _ = self.open();
        }
        self.metadata_map
            .borrow()
            .get(&self.index)
            .unwrap()
            .last_modified
            .try_into()
            .unwrap()
    }
    pub fn get_size(&self) -> u64 {
        if !self.metadata_map.borrow().contains_key(&self.index) {
            // This will force metadata loading
            // TODO: get metadata without reading the file
            _ = self.open();
        }
        self.metadata_map.borrow().get(&self.index).unwrap().size
    }
}

/// Represents an opened file in an IPA bundle.
#[derive(Clone)]
pub struct IpaFile {
    file: Cursor<DecompressedFile>,
}

impl Debug for IpaFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpaFile")
            .field("size", &self.file.get_ref().len())
            .finish()
    }
}

impl Read for IpaFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl std::io::Seek for IpaFile {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}

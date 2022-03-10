//! A DeltaLayer represents a collection of WAL records or page images in a range of
//! LSNs, and in a range of Keys. It is stored on a file on disk.
//!
//! Usually a delta layer only contains differences - in the form of WAL records against
//! a base LSN. However, if a segment is newly created, by creating a new relation or
//! extending an old one, there might be no base image. In that case, all the entries in
//! the delta layer must be page images or WAL records with the 'will_init' flag set, so
//! that they can be replayed without referring to an older page version. Also in some
//! circumstances, the predecessor layer might actually be another delta layer. That
//! can happen when you create a new branch in the middle of a delta layer, and the WAL
//! records on the new branch are put in a new delta layer.
//!
//! When a delta file needs to be accessed, we slurp the 'index' metadata
//! into memory, into the DeltaLayerInner struct. See load() and unload() functions.
//! To access a particular value, we search `index` for the given key.
//! The byte offset in the index can be used to find the value in
//! VALUES_CHAPTER.
//!
//! On disk, the delta files are stored in timelines/<timelineid> directory.
//! Currently, there are no subdirectories, and each delta file is named like this:
//!
//!    <key start>-<key end>__<start LSN>-<end LSN
//!
//! For example:
//!
//!    000000067F000032BE0000400000000020B6-000000067F000032BE0000400000000030B6__000000578C6B29-0000000057A50051
//!
//!
//! A delta file is constructed using the 'bookfile' crate. Each file consists of three
//! parts: the 'index', the values, and a short summary header. They are stored as
//! separate chapters.
//!
use crate::config::PageServerConf;
use crate::layered_repository::filename::{DeltaFileName, PathOrConf};
use crate::layered_repository::storage_layer::{
    Layer, ValueReconstructResult, ValueReconstructState,
};
use crate::layered_repository::utils;
use crate::repository::{Key, Value};
use crate::virtual_file::VirtualFile;
use crate::walrecord;
use crate::{ZTenantId, ZTimelineId};
use anyhow::{bail, Result};
use log::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zenith_utils::vec_map::VecMap;
// avoid binding to Write (conflicts with std::io::Write)
// while being able to use std::fmt::Write's methods
use std::fmt::Write as _;
use std::fs;
use std::io::BufWriter;
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard};

use bookfile::{Book, BookWriter, ChapterWriter};

use zenith_utils::bin_ser::BeSer;
use zenith_utils::lsn::Lsn;

// Magic constant to identify a Zenith delta file
pub const DELTA_FILE_MAGIC: u32 = 0x5A616E01;

/// Mapping from (key, lsn) -> page/WAL record
/// byte ranges in VALUES_CHAPTER
static INDEX_CHAPTER: u64 = 1;

/// Page/WAL bytes - cannot be interpreted
/// without the page versions from the INDEX_CHAPTER
static VALUES_CHAPTER: u64 = 2;

/// Contains the [`Summary`] struct
static SUMMARY_CHAPTER: u64 = 3;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Summary {
    tenantid: ZTenantId,
    timelineid: ZTimelineId,
    key_range: Range<Key>,
    lsn_range: Range<Lsn>,
}

impl From<&DeltaLayer> for Summary {
    fn from(layer: &DeltaLayer) -> Self {
        Self {
            tenantid: layer.tenantid,
            timelineid: layer.timelineid,
            key_range: layer.key_range.clone(),
            lsn_range: layer.lsn_range.clone(),
        }
    }
}

///
/// DeltaLayer is the in-memory data structure associated with an
/// on-disk delta file.  We keep a DeltaLayer in memory for each
/// file, in the LayerMap. If a layer is in "loaded" state, we have a
/// copy of the index in memory, in 'inner'. Otherwise the struct is
/// just a placeholder for a file that exists on disk, and it needs to
/// be loaded before using it in queries.
///
pub struct DeltaLayer {
    path_or_conf: PathOrConf,

    pub tenantid: ZTenantId,
    pub timelineid: ZTimelineId,
    pub key_range: Range<Key>,
    pub lsn_range: Range<Lsn>,

    inner: RwLock<DeltaLayerInner>,
}

pub struct DeltaLayerInner {
    /// If false, the 'index' has not been loaded into memory yet.
    loaded: bool,

    ///
    /// All versions of all pages in the layer are kept here.
    /// Indexed by block number and LSN. The value is an offset into the
    /// chapter where the page version is stored.
    ///
    index: HashMap<Key, VecMap<Lsn, u64>>,

    book: Option<Book<VirtualFile>>,
}

impl Layer for DeltaLayer {
    fn get_tenant_id(&self) -> ZTenantId {
        self.tenantid
    }

    fn get_timeline_id(&self) -> ZTimelineId {
        self.timelineid
    }

    fn get_key_range(&self) -> Range<Key> {
        self.key_range.clone()
    }

    fn get_lsn_range(&self) -> Range<Lsn> {
        self.lsn_range.clone()
    }

    fn filename(&self) -> PathBuf {
        PathBuf::from(self.layer_name().to_string())
    }

    fn get_value_reconstruct_data(
        &self,
        key: Key,
        lsn_range: Range<Lsn>,
        reconstruct_state: &mut ValueReconstructState,
    ) -> Result<ValueReconstructResult> {
        let mut need_image = true;

        assert!(self.key_range.contains(&key));

        {
            // Open the file and lock the metadata in memory
            let inner = self.load()?;
            let values_reader = inner
                .book
                .as_ref()
                .expect("should be loaded in load call above")
                .chapter_reader(VALUES_CHAPTER)?;

            // Scan the page versions backwards, starting from `lsn`.
            if let Some(vec_map) = inner.index.get(&key) {
                let slice = vec_map.slice_range(lsn_range);
                for (entry_lsn, pos) in slice.iter().rev() {
                    let val = Value::des(&utils::read_blob_from_chapter(&values_reader, *pos)?)?;
                    match val {
                        Value::Image(img) => {
                            reconstruct_state.img = Some((*entry_lsn, img));
                            need_image = false;
                            break;
                        }
                        Value::WalRecord(rec) => {
                            let will_init = rec.will_init();
                            reconstruct_state.records.push((*entry_lsn, rec));
                            if will_init {
                                // This WAL record initializes the page, so no need to go further back
                                need_image = false;
                                break;
                            }
                        }
                    }
                }
            }
            // release metadata lock and close the file
        }

        // If an older page image is needed to reconstruct the page, let the
        // caller know.
        if need_image {
            Ok(ValueReconstructResult::Continue)
        } else {
            Ok(ValueReconstructResult::Complete)
        }
    }

    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Key, Lsn, Value)>> + '_> {
        let inner = self.load().unwrap();

        let mut pairs: Vec<(&Key, &VecMap<Lsn, u64>)> = inner.index.iter().collect();
        pairs.sort_by_key(|x| x.0);

        match DeltaValueIter::new(inner) {
            Ok(iter) => Box::new(iter),
            Err(err) => Box::new(std::iter::once(Err(err))),
        }
    }

    ///
    /// Release most of the memory used by this layer. If it's accessed again later,
    /// it will need to be loaded back.
    ///
    fn unload(&self) -> Result<()> {
        if let Ok(mut inner) = self.inner.try_write() {
            inner.index = HashMap::default();
            inner.loaded = false;

            // Note: we keep the Book open. Is that a good idea? The virtual file
            // machinery has its own rules for closing the file descriptor if it's not
            // needed, but the Book struct uses up some memory, too.
        }

        Ok(())
    }

    fn delete(&self) -> Result<()> {
        // delete underlying file
        fs::remove_file(self.path())?;
        Ok(())
    }

    fn is_incremental(&self) -> bool {
        true
    }

    fn is_in_memory(&self) -> bool {
        false
    }

    /// debugging function to print out the contents of the layer
    fn dump(&self) -> Result<()> {
        println!(
            "----- delta layer for ten {} tli {} keys {}-{} lsn {}-{} ----",
            self.tenantid,
            self.timelineid,
            self.key_range.start,
            self.key_range.end,
            self.lsn_range.start,
            self.lsn_range.end
        );

        let inner = self.load()?;

        let path = self.path();
        let file = std::fs::File::open(&path)?;
        let book = Book::new(file)?;
        let chapter = book.chapter_reader(VALUES_CHAPTER)?;

        let mut values: Vec<(&Key, &VecMap<Lsn, u64>)> = inner.index.iter().collect();
        values.sort_by_key(|k| k.0);

        for (key, versions) in values {
            for (lsn, off) in versions.as_slice() {
                let mut desc = String::new();

                let buf = utils::read_blob_from_chapter(&chapter, *off)?;
                let val = Value::des(&buf);

                match val {
                    Ok(Value::Image(img)) => {
                        write!(&mut desc, " img {} bytes", img.len())?;
                    }
                    Ok(Value::WalRecord(rec)) => {
                        let wal_desc = walrecord::describe_wal_record(&rec);
                        write!(
                            &mut desc,
                            " rec {} bytes will_init: {} {}",
                            buf.len(),
                            rec.will_init(),
                            wal_desc
                        )?;
                    }
                    Err(err) => {
                        write!(&mut desc, " DESERIALIZATION ERROR: {}", err)?;
                    }
                }
                println!("  key {} at {}: {}", key, lsn, desc);
            }
        }

        Ok(())
    }
}

impl DeltaLayer {
    fn path_for(
        path_or_conf: &PathOrConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        fname: &DeltaFileName,
    ) -> PathBuf {
        match path_or_conf {
            PathOrConf::Path(path) => path.clone(),
            PathOrConf::Conf(conf) => conf
                .timeline_path(&timelineid, &tenantid)
                .join(fname.to_string()),
        }
    }

    ///
    /// Load the contents of the file into memory
    ///
    fn load(&self) -> Result<RwLockReadGuard<DeltaLayerInner>> {
        loop {
            // quick exit if already loaded
            {
                let inner = self.inner.read().unwrap();

                if inner.loaded {
                    return Ok(inner);
                }
            }
            // need to upgrade to write lock
            let mut inner = self.inner.write().unwrap();

            let path = self.path();

            // Open the file if it's not open already.
            if inner.book.is_none() {
                let file = VirtualFile::open(&path)?;
                inner.book = Some(Book::new(file)?);
            }
            let book = inner.book.as_ref().unwrap();

            match &self.path_or_conf {
                PathOrConf::Conf(_) => {
                    let chapter = book.read_chapter(SUMMARY_CHAPTER)?;
                    let actual_summary = Summary::des(&chapter)?;

                    let expected_summary = Summary::from(self);

                    if actual_summary != expected_summary {
                        bail!("in-file summary does not match expected summary. actual = {:?} expected = {:?}", actual_summary, expected_summary);
                    }
                }
                PathOrConf::Path(path) => {
                    let actual_filename = Path::new(path.file_name().unwrap());
                    let expected_filename = self.filename();

                    if actual_filename != expected_filename {
                        println!(
                            "warning: filename does not match what is expected from in-file summary"
                        );
                        println!("actual: {:?}", actual_filename);
                        println!("expected: {:?}", expected_filename);
                    }
                }
            }

            let chapter = book.read_chapter(INDEX_CHAPTER)?;
            let index = HashMap::des(&chapter)?;

            debug!("loaded from {}", &path.display());

            inner.index = index;
            inner.loaded = true;
        }
    }

    /// Create a DeltaLayer struct representing an existing file on disk.
    pub fn new(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        filename: &DeltaFileName,
    ) -> DeltaLayer {
        DeltaLayer {
            path_or_conf: PathOrConf::Conf(conf),
            timelineid,
            tenantid,
            key_range: filename.key_range.clone(),
            lsn_range: filename.lsn_range.clone(),
            inner: RwLock::new(DeltaLayerInner {
                loaded: false,
                book: None,
                index: HashMap::default(),
            }),
        }
    }

    /// Create a DeltaLayer struct representing an existing file on disk.
    ///
    /// This variant is only used for debugging purposes, by the 'dump_layerfile' binary.
    pub fn new_for_path<F>(path: &Path, book: &Book<F>) -> Result<Self>
    where
        F: FileExt,
    {
        let chapter = book.read_chapter(SUMMARY_CHAPTER)?;
        let summary = Summary::des(&chapter)?;

        Ok(DeltaLayer {
            path_or_conf: PathOrConf::Path(path.to_path_buf()),
            timelineid: summary.timelineid,
            tenantid: summary.tenantid,
            key_range: summary.key_range,
            lsn_range: summary.lsn_range,
            inner: RwLock::new(DeltaLayerInner {
                loaded: false,
                book: None,
                index: HashMap::default(),
            }),
        })
    }

    fn layer_name(&self) -> DeltaFileName {
        DeltaFileName {
            key_range: self.key_range.clone(),
            lsn_range: self.lsn_range.clone(),
        }
    }

    /// Path to the layer file in pageserver workdir.
    pub fn path(&self) -> PathBuf {
        Self::path_for(
            &self.path_or_conf,
            self.timelineid,
            self.tenantid,
            &self.layer_name(),
        )
    }
}

/// A builder object for constructing a new delta layer.
///
/// Usage:
///
/// 1. Create the DeltaLayerWriter by calling DeltaLayerWriter::new(...)
///
/// 2. Write the contents by calling `put_value` for every page
///    version to store in the layer.
///
/// 3. Call `finish`.
///
pub struct DeltaLayerWriter {
    conf: &'static PageServerConf,
    path: PathBuf,
    timelineid: ZTimelineId,
    tenantid: ZTenantId,

    key_start: Key,
    lsn_range: Range<Lsn>,

    index: HashMap<Key, VecMap<Lsn, u64>>,

    values_writer: ChapterWriter<BufWriter<VirtualFile>>,
    end_offset: u64,
}

impl DeltaLayerWriter {
    ///
    /// Start building a new delta layer.
    ///
    pub fn new(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        key_start: Key,
        lsn_range: Range<Lsn>,
    ) -> Result<DeltaLayerWriter> {
        // Create the file
        //
        // Note: This overwrites any existing file. There shouldn't be any.
        // FIXME: throw an error instead?

        let path = conf.timeline_path(&timelineid, &tenantid).join(format!(
            "{}-XXX__{:016X}-{:016X}.temp",
            key_start,
            u64::from(lsn_range.start),
            u64::from(lsn_range.end)
        ));
        info!("temp deltalayer path {}", path.display());
        let file = VirtualFile::create(&path)?;
        let buf_writer = BufWriter::new(file);
        let book = BookWriter::new(buf_writer, DELTA_FILE_MAGIC)?;

        // Open the page-versions chapter for writing. The calls to
        // `put_value` will use this to write the contents.
        let values_writer = book.new_chapter(VALUES_CHAPTER);

        Ok(DeltaLayerWriter {
            conf,
            path,
            timelineid,
            tenantid,
            key_start,
            lsn_range,
            index: HashMap::new(),
            values_writer,
            end_offset: 0,
        })
    }

    ///
    /// Append a key-value pair to the file.
    ///
    /// The values must be appended in key, lsn order.
    ///
    pub fn put_value(&mut self, key: Key, lsn: Lsn, val: Value) -> Result<()> {
        //info!("DELTA: key {} at {} on {}", key, lsn, self.path.display());
        assert!(self.lsn_range.start <= lsn);
        // Remember the offset and size metadata. The metadata is written
        // to a separate chapter, in `finish`.
        let off = self.end_offset;
        let len = utils::write_blob(&mut self.values_writer, &Value::ser(&val)?)?;
        self.end_offset += len;
        let vec_map = self.index.entry(key).or_default();
        let old = vec_map.append_or_update_last(lsn, off).unwrap().0;
        if old.is_some() {
            // We already had an entry for this LSN. That's odd..
            bail!(
                "Value for {} at {} already exists in delta layer being built",
                key,
                lsn
            );
        }

        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.end_offset
    }

    ///
    /// Finish writing the delta layer.
    ///
    /// 'seg_sizes' is a list of size changes to store with the actual data.
    ///
    pub fn finish(self, key_end: Key) -> Result<DeltaLayer> {
        // Close the values chapter
        let book = self.values_writer.close()?;

        // Write out the index
        let mut chapter = book.new_chapter(INDEX_CHAPTER);
        let buf = HashMap::ser(&self.index)?;
        chapter.write_all(&buf)?;
        let book = chapter.close()?;

        let mut chapter = book.new_chapter(SUMMARY_CHAPTER);
        let summary = Summary {
            tenantid: self.tenantid,
            timelineid: self.timelineid,
            key_range: self.key_start..key_end,
            lsn_range: self.lsn_range.clone(),
        };
        Summary::ser_into(&summary, &mut chapter)?;
        let book = chapter.close()?;

        // This flushes the underlying 'buf_writer'.
        book.close()?;

        // Note: Because we opened the file in write-only mode, we cannot
        // reuse the same VirtualFile for reading later. That's why we don't
        // set inner.book here. The first read will have to re-open it.
        let layer = DeltaLayer {
            path_or_conf: PathOrConf::Conf(self.conf),
            tenantid: self.tenantid,
            timelineid: self.timelineid,
            key_range: self.key_start..key_end,
            lsn_range: self.lsn_range.clone(),
            inner: RwLock::new(DeltaLayerInner {
                loaded: false,
                index: HashMap::new(),
                book: None,
            }),
        };

        // Rename the file to its final name
        //
        // Note: This overwrites any existing file. There shouldn't be any.
        // FIXME: throw an error instead?
        let final_path = DeltaLayer::path_for(
            &PathOrConf::Conf(self.conf),
            self.timelineid,
            self.tenantid,
            &DeltaFileName {
                key_range: self.key_start..key_end,
                lsn_range: self.lsn_range,
            },
        );
        std::fs::rename(self.path, &final_path)?;

        trace!("created delta layer {}", final_path.display());

        Ok(layer)
    }

    pub fn abort(self) {
        match self.values_writer.close() {
            Ok(book) => {
                if let Err(err) = book.close() {
                    error!("error while closing delta layer file: {}", err);
                }
            }
            Err(err) => {
                error!("error while closing chapter writer: {}", err);
            }
        }
        if let Err(err) = std::fs::remove_file(self.path) {
            error!("error removing unfinished delta layer file: {}", err);
        }
    }
}

///
/// Iterator over all key-value pairse stored in a delta layer
///
/// FIXME: This creates a Vector to hold the offsets of all key value pairs.
/// That takes up quite a lot of memory. Should do this in a more streaming
/// fashion.
///
struct DeltaValueIter<'a> {
    all_offsets: Vec<(Key, Lsn, u64)>,
    next_idx: usize,

    inner: RwLockReadGuard<'a, DeltaLayerInner>,
}

impl<'a> Iterator for DeltaValueIter<'a> {
    type Item = Result<(Key, Lsn, Value)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_res().transpose()
    }
}

impl<'a> DeltaValueIter<'a> {
    fn new(inner: RwLockReadGuard<'a, DeltaLayerInner>) -> Result<Self> {
        let mut index: Vec<(&Key, &VecMap<Lsn, u64>)> = inner.index.iter().collect();
        index.sort_by_key(|x| x.0);

        let mut all_offsets: Vec<(Key, Lsn, u64)> = Vec::new();
        for (key, vec_map) in index.iter() {
            for (lsn, off) in vec_map.as_slice().iter() {
                all_offsets.push((**key, *lsn, *off));
            }
        }

        Ok(DeltaValueIter {
            all_offsets,
            inner,
            next_idx: 0,
        })
    }

    fn next_res(&mut self) -> Result<Option<(Key, Lsn, Value)>> {
        if self.next_idx < self.all_offsets.len() {
            let (key, lsn, off) = self.all_offsets[self.next_idx];

            let values_reader = self
                .inner
                .book
                .as_ref()
                .expect("should be loaded in load call above")
                .chapter_reader(VALUES_CHAPTER)?;

            let val = Value::des(&utils::read_blob_from_chapter(&values_reader, off)?)?;

            self.next_idx += 1;
            Ok(Some((key, lsn, val)))
        } else {
            Ok(None)
        }
    }
}

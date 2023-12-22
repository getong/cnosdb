use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use models::predicate::domain::TimeRange;
use models::{FieldId, Timestamp};
use snafu::ResultExt;
use trace::{error, info, trace};
use utils::BloomFilter;

use super::iterator::BufferedIterator;
use crate::compaction::CompactReq;
use crate::context::GlobalContext;
use crate::error::{self, Result};
use crate::summary::{CompactMeta, VersionEdit};
use crate::tseries_family::TseriesFamily;
use crate::tsm::{
    self, BlockMeta, BlockMetaIterator, DataBlock, EncodedDataBlock, IndexIterator, IndexMeta,
    TsmReader, TsmWriter,
};
use crate::{ColumnFileId, Error, LevelId, TseriesFamilyId};

/// Temporary compacting data block meta, holding the priority of reader,
/// the reader and the meta of data block.
#[derive(Clone)]
pub(crate) struct CompactingBlockMeta {
    pub reader_idx: usize,
    pub reader: Arc<TsmReader>,
    pub meta: BlockMeta,
}

impl PartialEq for CompactingBlockMeta {
    fn eq(&self, other: &Self) -> bool {
        self.reader.file_id() == other.reader.file_id() && self.meta == other.meta
    }
}

impl Eq for CompactingBlockMeta {}

impl PartialOrd for CompactingBlockMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CompactingBlockMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.meta.cmp(&other.meta)
    }
}

impl Display for CompactingBlockMeta {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {{ len: {}, min_ts: {}, max_ts: {} }}",
            self.meta.field_type(),
            self.meta.count(),
            self.meta.min_ts(),
            self.meta.max_ts(),
        )
    }
}

impl CompactingBlockMeta {
    pub fn new(tsm_reader_idx: usize, tsm_reader: Arc<TsmReader>, block_meta: BlockMeta) -> Self {
        Self {
            reader_idx: tsm_reader_idx,
            reader: tsm_reader,
            meta: block_meta,
        }
    }

    pub fn time_range(&self) -> TimeRange {
        self.meta.time_range()
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.meta.min_ts() <= other.meta.max_ts() && self.meta.max_ts() >= other.meta.min_ts()
    }

    pub fn overlaps_time_range(&self, time_range: &TimeRange) -> bool {
        self.meta.min_ts() <= time_range.max_ts && self.meta.max_ts() >= time_range.min_ts
    }

    pub fn included_in_time_range(&self, time_range: &TimeRange) -> bool {
        self.meta.min_ts() >= time_range.min_ts && self.meta.max_ts() <= time_range.max_ts
    }

    /// Read data block of block meta from reader.
    pub async fn get_data_block(&self) -> Result<DataBlock> {
        self.reader
            .get_data_block(&self.meta)
            .await
            .context(error::ReadTsmSnafu)
    }

    /// Read data block of block meta from reader.
    pub async fn get_data_block_opt(&self, time_range: &TimeRange) -> Result<Option<DataBlock>> {
        // It's impossible that the reader got None by meta,
        // or blk.intersection(time_range) returned None.
        self.reader
            .get_data_block(&self.meta)
            .await
            .map(|blk| blk.intersection(time_range))
            .context(error::ReadTsmSnafu)
    }

    /// Read raw data of block meta from reader.
    pub async fn get_raw_data(&self, dst: &mut Vec<u8>) -> Result<usize> {
        self.reader
            .get_raw_data(&self.meta, dst)
            .await
            .context(error::ReadTsmSnafu)
    }

    pub fn has_tombstone(&self) -> bool {
        self.reader.has_tombstone()
    }
}

#[derive(Clone)]
pub(crate) struct CompactingBlockMetaGroup {
    field_id: FieldId,
    blk_metas: Vec<CompactingBlockMeta>,
    time_range: TimeRange,
}

impl CompactingBlockMetaGroup {
    pub fn new(field_id: FieldId, blk_meta: CompactingBlockMeta) -> Self {
        let time_range = blk_meta.time_range();
        Self {
            field_id,
            blk_metas: vec![blk_meta],
            time_range,
        }
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.time_range.overlaps(&other.time_range)
    }

    pub fn append(&mut self, other: &mut CompactingBlockMetaGroup) {
        self.blk_metas.append(&mut other.blk_metas);
        self.time_range.merge(&other.time_range);
    }

    pub async fn merge(
        mut self,
        previous_block: Option<CompactingBlock>,
        max_block_size: usize,
    ) -> Result<Vec<CompactingBlock>> {
        if self.blk_metas.is_empty() {
            return Ok(vec![]);
        }
        self.blk_metas
            .sort_by(|a, b| a.reader_idx.cmp(&b.reader_idx).reverse());

        let merged_block;
        if self.blk_metas.len() == 1 && !self.blk_metas[0].has_tombstone() {
            // Only one compacting block and has no tombstone, write as raw block.
            trace!("only one compacting block without tombstone, handled as raw block");
            let meta_0 = &self.blk_metas[0].meta;
            let mut buf_0 = Vec::with_capacity(meta_0.size() as usize);
            let data_len_0 = self.blk_metas[0].get_raw_data(&mut buf_0).await?;
            buf_0.truncate(data_len_0);

            if meta_0.size() >= max_block_size as u64 {
                // Raw data block is full, so do not merge with the previous, directly return.
                let mut merged_blks = Vec::new();
                if let Some(blk) = previous_block {
                    merged_blks.push(blk);
                }
                merged_blks.push(CompactingBlock::raw(
                    self.blk_metas[0].reader_idx,
                    meta_0.clone(),
                    buf_0,
                ));

                return Ok(merged_blks);
            } else if let Some(compacting_block) = previous_block {
                // Raw block is not full, so decode and merge with compacting_block.
                let decoded_raw_block = tsm::decode_data_block(
                    &buf_0,
                    meta_0.field_type(),
                    meta_0.val_off() - meta_0.offset(),
                )
                .context(error::ReadTsmSnafu)?;
                let mut data_block = compacting_block.decode()?;
                data_block.extend(decoded_raw_block);

                merged_block = data_block;
            } else {
                // Raw block is not full, but nothing to merge with, directly return.
                return Ok(vec![CompactingBlock::raw(
                    self.blk_metas[0].reader_idx,
                    meta_0.clone(),
                    buf_0,
                )]);
            }
        } else {
            // One block with tombstone or multi compacting blocks, decode and merge these data block.
            trace!(
                "there are {} compacting blocks, need to decode and merge",
                self.blk_metas.len()
            );
            let head = &mut self.blk_metas[0];
            let mut head_block = head.get_data_block().await?;

            if let Some(compacting_block) = previous_block {
                let mut data_block = compacting_block.decode()?;
                data_block.extend(head_block);
                head_block = data_block;
            }

            for blk_meta in self.blk_metas[1..].iter_mut() {
                // Merge decoded data block.
                let blk_block = blk_meta.get_data_block().await?;
                head_block = head_block.merge(blk_block);
            }
            merged_block = head_block;
        }

        chunk_merged_block(self.field_id, merged_block, max_block_size)
    }

    pub fn into_compacting_block_metas(self) -> Vec<CompactingBlockMeta> {
        self.blk_metas
    }

    pub fn is_empty(&self) -> bool {
        self.blk_metas.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blk_metas.len()
    }
}

impl Display for CompactingBlockMetaGroup {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{field_id: {}, blk_metas: [", self.field_id)?;
        if !self.blk_metas.is_empty() {
            write!(f, "{}", &self.blk_metas[0])?;
            for b in self.blk_metas.iter().skip(1) {
                write!(f, ", {}", b)?;
            }
        }
        write!(f, "]}}")
    }
}

struct CompactingBlockMetaGroupsVecDeque<'a>(&'a VecDeque<CompactingBlockMetaGroup>);

impl<'a> Display for CompactingBlockMetaGroupsVecDeque<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut iter = self.0.iter();
        if let Some(d) = iter.next() {
            write!(f, "{}", d)?;
            for d in iter {
                write!(f, ", {d}")?;
            }
        }
        Ok(())
    }
}

fn chunk_merged_block(
    field_id: FieldId,
    data_block: DataBlock,
    max_block_size: usize,
) -> Result<Vec<CompactingBlock>> {
    let mut merged_blks = Vec::new();
    if max_block_size == 0 || data_block.len() < max_block_size {
        // Data block elements less than max_block_size, do not encode it.
        // Try to merge with the next CompactingBlockMetaGroup.
        merged_blks.push(CompactingBlock::decoded(0, field_id, data_block));
    } else {
        // Data block is so big that split into multi CompactingBlock
        let len = data_block.len();
        let mut start = 0;
        let mut end = len.min(max_block_size);
        while start < len {
            // Encode decoded data blocks into chunks.
            let encoded_blk =
                EncodedDataBlock::encode(&data_block, start, end).map_err(|e| Error::WriteTsm {
                    source: tsm::WriteTsmError::Encode { source: e },
                })?;
            merged_blks.push(CompactingBlock::encoded(0, field_id, encoded_blk));

            start = end;
            end = len.min(start + max_block_size);
        }
    }

    Ok(merged_blks)
}

/// Temporary compacting data block.
/// - priority: When merging two (timestamp, value) pair with the same
/// timestamp from two data blocks, pair from data block with lower
/// priority will be discarded.
#[derive(Debug, PartialEq)]
pub(crate) enum CompactingBlock {
    Decoded {
        priority: usize,
        field_id: FieldId,
        data_block: DataBlock,
    },
    Encoded {
        priority: usize,
        field_id: FieldId,
        data_block: EncodedDataBlock,
    },
    Raw {
        priority: usize,
        meta: BlockMeta,
        raw: Vec<u8>,
    },
}

impl Display for CompactingBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactingBlock::Decoded {
                priority,
                field_id,
                data_block,
            } => {
                write!(f, "p: {priority}, f: {field_id}, block: {data_block}")
            }
            CompactingBlock::Encoded {
                priority,
                field_id,
                data_block,
            } => {
                write!(f, "p: {priority}, f: {field_id}, block: {data_block}")
            }
            CompactingBlock::Raw {
                priority,
                meta,
                raw,
            } => {
                write!(
                    f,
                    "p: {priority}, f: {}, block: {}: {{ len: {}, min_ts: {}, max_ts: {}, raw_len: {} }}",
                    meta.field_id(),
                    meta.field_type(),
                    meta.count(),
                    meta.min_ts(),
                    meta.max_ts(),
                    raw.len(),
                )
            }
        }
    }
}

impl CompactingBlock {
    pub fn decoded(priority: usize, field_id: FieldId, data_block: DataBlock) -> CompactingBlock {
        Self::Decoded {
            priority,
            field_id,
            data_block,
        }
    }

    pub fn encoded(
        priority: usize,
        field_id: FieldId,
        data_block: EncodedDataBlock,
    ) -> CompactingBlock {
        Self::Encoded {
            priority,
            field_id,
            data_block,
        }
    }

    pub fn raw(priority: usize, meta: BlockMeta, raw: Vec<u8>) -> CompactingBlock {
        CompactingBlock::Raw {
            priority,
            meta,
            raw,
        }
    }

    pub fn decode(self) -> Result<DataBlock> {
        match self {
            CompactingBlock::Decoded { data_block, .. } => Ok(data_block),
            CompactingBlock::Encoded { data_block, .. } => {
                data_block.decode().context(error::DecodeSnafu)
            }
            CompactingBlock::Raw { raw, meta, .. } => {
                tsm::decode_data_block(&raw, meta.field_type(), meta.val_off() - meta.offset())
                    .context(error::ReadTsmSnafu)
            }
        }
    }

    /// Decode data block and return the intersected segment with out_time_range.
    pub fn decode_opt(self, out_time_range: &TimeRange) -> Result<Option<DataBlock>> {
        let data_block = match self {
            CompactingBlock::Decoded { data_block, .. } => data_block,
            CompactingBlock::Encoded { data_block, .. } => {
                data_block.decode().context(error::DecodeSnafu)?
            }
            CompactingBlock::Raw { raw, meta, .. } => {
                tsm::decode_data_block(&raw, meta.field_type(), meta.val_off() - meta.offset())
                    .context(error::ReadTsmSnafu)?
            }
        };
        if data_block
            .time_range()
            .is_some_and(|(min_ts, _max_ts)| min_ts < out_time_range.max_ts)
        {
            Ok(data_block.intersection(out_time_range))
        } else {
            Ok(None)
        }
    }

    pub fn len(&self) -> usize {
        match self {
            CompactingBlock::Decoded { data_block, .. } => data_block.len(),
            CompactingBlock::Encoded { data_block, .. } => data_block.count as usize,
            CompactingBlock::Raw { meta, .. } => meta.count() as usize,
        }
    }
}

pub(crate) struct CompactingFile {
    pub i: usize,
    pub tsm_reader: Arc<TsmReader>,
    pub index_iter: BufferedIterator<IndexIterator>,
    pub field_id: FieldId,
}

impl CompactingFile {
    pub(crate) fn new(i: usize, tsm_reader: Arc<TsmReader>) -> Option<Self> {
        let mut index_iter = BufferedIterator::new(tsm_reader.index_iterator());
        index_iter
            .peek()
            .map(|idx_meta| idx_meta.field_id())
            .map(|field_id| Self {
                i,
                tsm_reader,
                index_iter,
                field_id,
            })
    }

    /// Fetch the next index meta of tsm-file, field_id may be changed.
    pub(crate) fn next(&mut self) -> Option<&IndexMeta> {
        if let Some(idx_meta) = self.index_iter.next() {
            self.field_id = idx_meta.field_id();
            Some(idx_meta)
        } else {
            None
        }
    }

    pub(crate) fn peek(&mut self) -> Option<&IndexMeta> {
        self.index_iter.peek()
    }
}

impl Eq for CompactingFile {}

impl PartialEq for CompactingFile {
    fn eq(&self, other: &Self) -> bool {
        self.tsm_reader.file_id() == other.tsm_reader.file_id() && self.field_id == other.field_id
    }
}

impl Ord for CompactingFile {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.field_id.cmp(&other.field_id).reverse()
    }
}

impl PartialOrd for CompactingFile {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) struct CompactIterator {
    tsm_readers: Vec<Arc<TsmReader>>,
    compacting_files: BinaryHeap<CompactingFile>,
    /// Maximum values in generated CompactingBlock
    max_data_block_size: usize,
    /// Decode a data block even though it doesn't need to merge with others,
    /// return CompactingBlock::DataBlock rather than CompactingBlock::Raw .
    decode_non_overlap_blocks: bool,

    /// Temporarily stored index of `TsmReader` in self.tsm_readers,
    /// and if `TsmReader` is a delta file,
    /// and `BlockMetaIterator` of current field_id.
    tmp_tsm_blk_meta_iters: Vec<(usize, BlockMetaIterator)>,
    /// When a TSM file at index i is ended, finished_idxes[i] is set to true.
    finished_readers: Vec<bool>,
    /// How many finished_idxes is set to true.
    finished_reader_cnt: usize,
    curr_fid: Option<FieldId>,

    merging_blk_meta_groups: VecDeque<CompactingBlockMetaGroup>,
}

/// To reduce construction code
impl Default for CompactIterator {
    fn default() -> Self {
        Self {
            tsm_readers: Default::default(),
            compacting_files: Default::default(),
            max_data_block_size: 0,
            decode_non_overlap_blocks: false,
            tmp_tsm_blk_meta_iters: Default::default(),
            finished_readers: Default::default(),
            finished_reader_cnt: Default::default(),
            curr_fid: Default::default(),
            merging_blk_meta_groups: Default::default(),
        }
    }
}

impl CompactIterator {
    pub(crate) fn new(
        tsm_readers: Vec<Arc<TsmReader>>,
        max_data_block_size: usize,
        decode_non_overlap_blocks: bool,
    ) -> Self {
        let mut compacting_tsm_readers = Vec::with_capacity(tsm_readers.len());
        let mut compacting_files = BinaryHeap::with_capacity(tsm_readers.len());
        let mut compacting_tsm_file_idx = 0_usize;
        for tsm_reader in tsm_readers {
            if let Some(cf) = CompactingFile::new(compacting_tsm_file_idx, tsm_reader.clone()) {
                compacting_tsm_readers.push(tsm_reader);
                compacting_files.push(cf);
                compacting_tsm_file_idx += 1;
            }
        }

        Self {
            tsm_readers: compacting_tsm_readers,
            compacting_files,
            max_data_block_size,
            decode_non_overlap_blocks,
            tmp_tsm_blk_meta_iters: Vec::with_capacity(compacting_tsm_file_idx),
            finished_readers: vec![false; compacting_tsm_file_idx],
            ..Default::default()
        }
    }

    /// Update tmp_tsm_blks and tmp_tsm_blk_tsm_reader_idx for field id in next iteration.
    fn next_field_id(&mut self) {
        trace!("===============================");
        self.curr_fid = None;

        if let Some(f) = self.compacting_files.peek() {
            if self.curr_fid.is_none() {
                trace!(
                    "selected new field {:?} from file {} as current field id",
                    f.field_id,
                    f.tsm_reader.file_id()
                );
                self.curr_fid = Some(f.field_id);
            }
        } else {
            // TODO finished
            trace!("no file to select, mark finished");
            self.finished_reader_cnt += 1;
        }
        self.tmp_tsm_blk_meta_iters.clear();
        let mut loop_file_i;
        while let Some(mut f) = self.compacting_files.pop() {
            loop_file_i = f.i;
            if self.curr_fid.is_some_and(|fid| fid == f.field_id) {
                if let Some(idx_meta) = f.peek() {
                    self.tmp_tsm_blk_meta_iters
                        .push((loop_file_i, idx_meta.block_iterator()));
                    trace!("merging idx_meta({}): field_id: {}, field_type: {:?}, block_count: {}, time_range: {:?}",
                        self.tmp_tsm_blk_meta_iters.len(),
                        idx_meta.field_id(),
                        idx_meta.field_type(),
                        idx_meta.block_count(),
                        idx_meta.time_range()
                    );
                    f.next();
                    self.compacting_files.push(f);
                } else {
                    // This tsm-file has been finished, do not push it back.
                    trace!("file {loop_file_i} is finished.");
                    self.finished_readers[loop_file_i] = true;
                    self.finished_reader_cnt += 1;
                }
            } else {
                // This tsm-file do not need to compact at this time, push it back.
                self.compacting_files.push(f);
                break;
            }
        }
    }

    /// Collect merging `DataBlock`s.
    fn fetch_merging_block_meta_groups(&mut self) -> bool {
        if self.tmp_tsm_blk_meta_iters.is_empty() {
            return false;
        }
        let field_id = match self.curr_fid {
            Some(fid) => fid,
            None => return false,
        };

        let mut blk_metas: Vec<CompactingBlockMeta> =
            Vec::with_capacity(self.tmp_tsm_blk_meta_iters.len());
        // Get all block_meta, and check if it's tsm file has a related tombstone file.
        for (tsm_reader_idx, blk_iter) in self.tmp_tsm_blk_meta_iters.iter_mut() {
            for blk_meta in blk_iter.by_ref() {
                let tsm_reader_ptr = self.tsm_readers[*tsm_reader_idx].clone();
                blk_metas.push(CompactingBlockMeta::new(
                    *tsm_reader_idx,
                    tsm_reader_ptr,
                    blk_meta,
                ));
            }
        }
        // Sort by field_id, min_ts and max_ts.
        blk_metas.sort();

        let mut blk_meta_groups: Vec<CompactingBlockMetaGroup> =
            Vec::with_capacity(blk_metas.len());
        for blk_meta in blk_metas {
            blk_meta_groups.push(CompactingBlockMetaGroup::new(field_id, blk_meta));
        }
        // Compact blk_meta_groups.
        let mut i = 0;
        loop {
            let mut head_idx = i;
            // Find the first non-empty as head.
            for (off, bmg) in blk_meta_groups[i..].iter().enumerate() {
                if !bmg.is_empty() {
                    head_idx += off;
                    break;
                }
            }
            if head_idx >= blk_meta_groups.len() - 1 {
                // There no other blk_meta_group to merge with the last one.
                break;
            }
            let mut head = blk_meta_groups[head_idx].clone();
            i = head_idx + 1;
            for bmg in blk_meta_groups[i..].iter_mut() {
                if bmg.is_empty() {
                    continue;
                }
                if head.overlaps(bmg) {
                    head.append(bmg);
                }
            }
            blk_meta_groups[head_idx] = head;
        }
        let blk_meta_groups: VecDeque<CompactingBlockMetaGroup> = blk_meta_groups
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect();

        trace!(
            "selected merging meta groups: {}",
            CompactingBlockMetaGroupsVecDeque(&blk_meta_groups),
        );

        self.merging_blk_meta_groups = blk_meta_groups;

        true
    }
}

impl CompactIterator {
    pub(crate) async fn next(&mut self) -> Option<CompactingBlockMetaGroup> {
        if let Some(g) = self.merging_blk_meta_groups.pop_front() {
            return Some(g);
        }

        // For each tsm-file, get next index reader for current iteration field id
        self.next_field_id();

        trace!(
            "selected {} blocks meta iterators",
            self.tmp_tsm_blk_meta_iters.len()
        );
        if self.tmp_tsm_blk_meta_iters.is_empty() {
            trace!("iteration field_id {:?} is finished", self.curr_fid);
            self.curr_fid = None;
            return None;
        }

        // Get all of block_metas of this field id, and merge these blocks
        self.fetch_merging_block_meta_groups();

        if let Some(g) = self.merging_blk_meta_groups.pop_front() {
            return Some(g);
        }
        None
    }

    pub fn curr_fid(&self) -> Option<FieldId> {
        self.curr_fid
    }
}

/// Returns if r1 (min_ts, max_ts) overlaps r2 (min_ts, max_ts)
fn overlaps_tuples(r1: (i64, i64), r2: (i64, i64)) -> bool {
    r1.0 <= r2.1 && r1.1 >= r2.0
}

pub async fn run_compaction_job(
    request: CompactReq,
    kernel: Arc<GlobalContext>,
) -> Result<Option<(VersionEdit, HashMap<ColumnFileId, Arc<BloomFilter>>)>> {
    info!("Compaction: Running compaction job on {request}");

    if request.files.is_empty() {
        // Nothing to compact
        return Ok(None);
    }

    // Buffers all tsm-files and it's indexes for this compaction
    let tsf_id = request.ts_family_id;
    let mut tsm_readers = Vec::new();
    for col_file in request.files.iter() {
        let tsm_reader = request.version.get_tsm_reader(col_file.file_path()).await?;
        tsm_readers.push(tsm_reader);
    }

    let max_block_size = TseriesFamily::MAX_DATA_BLOCK_SIZE as usize;
    let mut iter = CompactIterator::new(tsm_readers, max_block_size, false);
    let tsm_dir = request
        .storage_opt
        .tsm_dir(&request.tenant_database, tsf_id);
    let mut tsm_writer = tsm::new_tsm_writer(&tsm_dir, kernel.file_id_next(), false, 0).await?;
    info!(
        "Compaction: File: {} been created (level: {}).",
        tsm_writer.sequence(),
        request.out_level
    );
    let mut version_edit = VersionEdit::new(tsf_id);
    let mut file_metas: HashMap<ColumnFileId, Arc<BloomFilter>> = HashMap::new();

    let mut previous_merged_block: Option<CompactingBlock> = None;
    let mut fid = iter.curr_fid;
    while let Some(blk_meta_group) = iter.next().await {
        trace!("===============================");
        if fid.is_some() && fid != iter.curr_fid {
            // Iteration of next field id, write previous merged block.
            if let Some(blk) = previous_merged_block.take() {
                // Write the small previous merged block.
                if write_tsm(
                    &mut tsm_writer,
                    blk,
                    &mut file_metas,
                    &mut version_edit,
                    &request,
                )
                .await?
                {
                    tsm_writer =
                        tsm::new_tsm_writer(&tsm_dir, kernel.file_id_next(), false, 0).await?;
                    info!(
                        "Compaction: File: {} been created (level: {}).",
                        tsm_writer.sequence(),
                        request.out_level
                    );
                }
            }
        }

        fid = iter.curr_fid;
        let mut compacting_blks = blk_meta_group
            .merge(previous_merged_block.take(), max_block_size)
            .await?;
        if compacting_blks.len() == 1 && compacting_blks[0].len() < max_block_size {
            // The only one data block too small, try to extend the next compacting blocks.
            previous_merged_block = Some(compacting_blks.remove(0));
            continue;
        }

        let last_blk_idx = compacting_blks.len() - 1;
        for (i, blk) in compacting_blks.into_iter().enumerate() {
            if i == last_blk_idx && blk.len() < max_block_size {
                // The last data block too small, try to extend to
                // the next compacting blocks (current field id).
                previous_merged_block = Some(blk);
                break;
            }
            if write_tsm(
                &mut tsm_writer,
                blk,
                &mut file_metas,
                &mut version_edit,
                &request,
            )
            .await?
            {
                tsm_writer = tsm::new_tsm_writer(&tsm_dir, kernel.file_id_next(), false, 0).await?;
                info!(
                    "Compaction: File: {} been created (level: {}).",
                    tsm_writer.sequence(),
                    request.out_level
                );
            }
        }
    }
    if let Some(blk) = previous_merged_block {
        let _max_file_size_exceed = write_tsm(
            &mut tsm_writer,
            blk,
            &mut file_metas,
            &mut version_edit,
            &request,
        )
        .await?;
    }
    if !tsm_writer.finished() {
        finish_write_tsm(
            &mut tsm_writer,
            &mut file_metas,
            &mut version_edit,
            &request,
            request.version.max_level_ts(),
        )
        .await?;
    }

    for file in request.files {
        version_edit.del_file(file.level(), file.file_id(), file.is_delta());
    }

    info!(
        "Compaction: Compact finished, version edits: {:?}",
        version_edit
    );
    Ok(Some((version_edit, file_metas)))
}

async fn write_tsm(
    tsm_writer: &mut TsmWriter,
    blk: CompactingBlock,
    file_metas: &mut HashMap<ColumnFileId, Arc<BloomFilter>>,
    version_edit: &mut VersionEdit,
    request: &CompactReq,
) -> Result<bool> {
    let write_ret = match blk {
        CompactingBlock::Decoded {
            field_id: fid,
            data_block: b,
            ..
        } => tsm_writer.write_block(fid, &b).await,
        CompactingBlock::Encoded {
            field_id,
            data_block,
            ..
        } => tsm_writer.write_encoded_block(field_id, &data_block).await,
        CompactingBlock::Raw { meta, raw, .. } => tsm_writer.write_raw(&meta, &raw).await,
    };
    if let Err(e) = write_ret {
        match e {
            tsm::WriteTsmError::WriteIO { source } => {
                // TODO try re-run compaction on other time.
                error!("Failed compaction: IO error when write tsm: {:?}", source);
                return Err(Error::IO { source });
            }
            tsm::WriteTsmError::Encode { source } => {
                // TODO try re-run compaction on other time.
                error!(
                    "Failed compaction: encoding error when write tsm: {:?}",
                    source
                );
                return Err(Error::Encode { source });
            }
            tsm::WriteTsmError::MaxFileSizeExceed { .. } => {
                finish_write_tsm(
                    tsm_writer,
                    file_metas,
                    version_edit,
                    request,
                    request.version.max_level_ts(),
                )
                .await?;
                return Ok(true);
            }
            tsm::WriteTsmError::Finished { path } => {
                error!(
                    "Trying to write by a finished tsm writer: {}",
                    path.display()
                );
            }
        }
    }

    Ok(false)
}

async fn finish_write_tsm(
    tsm_writer: &mut TsmWriter,
    file_metas: &mut HashMap<ColumnFileId, Arc<BloomFilter>>,
    version_edit: &mut VersionEdit,
    request: &CompactReq,
    max_level_ts: Timestamp,
) -> Result<()> {
    tsm_writer
        .write_index()
        .await
        .context(error::WriteTsmSnafu)?;
    tsm_writer.finish().await.context(error::WriteTsmSnafu)?;
    file_metas.insert(
        tsm_writer.sequence(),
        Arc::new(tsm_writer.bloom_filter_cloned()),
    );
    info!(
        "Compaction: File: {} write finished (level: {}, {} B).",
        tsm_writer.sequence(),
        request.out_level,
        tsm_writer.size()
    );

    let cm = new_compact_meta(tsm_writer, request.ts_family_id, request.out_level);
    version_edit.add_file(cm, max_level_ts);

    Ok(())
}

fn new_compact_meta(
    tsm_writer: &TsmWriter,
    tsf_id: TseriesFamilyId,
    level: LevelId,
) -> CompactMeta {
    CompactMeta {
        file_id: tsm_writer.sequence(),
        file_size: tsm_writer.size(),
        tsf_id,
        level,
        min_ts: tsm_writer.min_ts(),
        max_ts: tsm_writer.max_ts(),
        high_seq: 0,
        low_seq: 0,
        is_delta: false,
    }
}

#[cfg(test)]
pub mod test {
    use core::panic;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use cache::ShardedAsyncCache;
    use minivec::MiniVec;
    use models::codec::Encoding;
    use models::predicate::domain::TimeRange;
    use models::{FieldId, PhysicalDType as ValueType, Timestamp};

    use super::{chunk_merged_block, run_compaction_job, CompactReq, CompactingBlock};
    use crate::context::GlobalContext;
    use crate::file_system::file_manager;
    use crate::kv_option::Options;
    use crate::summary::VersionEdit;
    use crate::tseries_family::{ColumnFile, LevelInfo, Version};
    use crate::tsm::codec::DataBlockEncoding;
    use crate::tsm::{self, DataBlock, EncodedDataBlock, TsmReader, TsmTombstone};
    use crate::{file_utils, ColumnFileId, LevelId};

    #[test]
    fn test_chunk_merged_block() {
        let data_block = DataBlock::U64 {
            ts: vec![0, 1, 2, 10, 11, 12, 100, 101, 102, 1000, 1001, 1002],
            val: vec![0, 3, 6, 30, 33, 36, 300, 303, 306, 3000, 3003, 3006],
            enc: DataBlockEncoding::default(),
        };
        let field_id = 1;
        // Trying to chunk with no chunk size
        {
            let chunks = chunk_merged_block(field_id, data_block.clone(), 0).unwrap();
            assert_eq!(chunks.len(), 1);
            assert_eq!(
                chunks[0],
                CompactingBlock::decoded(0, 1, data_block.clone())
            );
        }
        // Trying to chunk with too big chunk size
        {
            let chunks = chunk_merged_block(field_id, data_block.clone(), 100).unwrap();
            assert_eq!(chunks.len(), 1);
            assert_eq!(
                chunks[0],
                CompactingBlock::decoded(0, 1, data_block.clone())
            );
        }
        // Trying to chunk with chunk size that can divide data block exactly
        {
            let chunks = chunk_merged_block(field_id, data_block.clone(), 4).unwrap();
            assert_eq!(chunks.len(), 3);
            assert_eq!(
                chunks[0],
                CompactingBlock::encoded(
                    0,
                    field_id,
                    EncodedDataBlock::encode(&data_block, 0, 4).unwrap()
                )
            );
            assert_eq!(
                chunks[1],
                CompactingBlock::encoded(
                    0,
                    field_id,
                    EncodedDataBlock::encode(&data_block, 4, 8).unwrap()
                )
            );
            assert_eq!(
                chunks[2],
                CompactingBlock::encoded(
                    0,
                    field_id,
                    EncodedDataBlock::encode(&data_block, 8, 12).unwrap()
                )
            );
        }
        // Trying to chunk with chunk size that cannot divide data block exactly
        {
            let chunks = chunk_merged_block(field_id, data_block.clone(), 5).unwrap();
            assert_eq!(chunks.len(), 3);
            assert_eq!(
                chunks[0],
                CompactingBlock::encoded(
                    0,
                    field_id,
                    EncodedDataBlock::encode(&data_block, 0, 5).unwrap()
                )
            );
            assert_eq!(
                chunks[1],
                CompactingBlock::encoded(
                    0,
                    field_id,
                    EncodedDataBlock::encode(&data_block, 5, 10).unwrap()
                )
            );
            assert_eq!(
                chunks[2],
                CompactingBlock::encoded(
                    0,
                    field_id,
                    EncodedDataBlock::encode(&data_block, 10, 12).unwrap()
                )
            );
        }
    }

    pub async fn write_data_blocks_to_column_file(
        dir: impl AsRef<Path>,
        data: Vec<HashMap<FieldId, Vec<DataBlock>>>,
    ) -> (u64, Vec<Arc<ColumnFile>>) {
        if !file_manager::try_exists(&dir) {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let mut cfs = Vec::new();
        let mut file_seq = 0;
        for (i, d) in data.iter().enumerate() {
            file_seq = i as u64 + 1;
            let mut writer = tsm::new_tsm_writer(&dir, file_seq, false, 0).await.unwrap();
            for (fid, data_blks) in d.iter() {
                for blk in data_blks.iter() {
                    writer.write_block(*fid, blk).await.unwrap();
                }
            }
            writer.write_index().await.unwrap();
            writer.finish().await.unwrap();
            let mut cf = ColumnFile::new(
                file_seq,
                2,
                TimeRange::new(writer.min_ts(), writer.max_ts()),
                writer.size(),
                false,
                writer.path(),
            );
            cf.set_field_id_filter(Arc::new(writer.into_bloom_filter()));
            cfs.push(Arc::new(cf));
        }
        (file_seq + 1, cfs)
    }

    pub async fn read_data_blocks_from_column_file(
        path: impl AsRef<Path>,
    ) -> HashMap<FieldId, Vec<DataBlock>> {
        let tsm_reader = TsmReader::open(path).await.unwrap();
        let mut data: HashMap<FieldId, Vec<DataBlock>> = HashMap::new();
        for idx in tsm_reader.index_iterator() {
            let field_id = idx.field_id();
            for blk_meta in idx.block_iterator() {
                let blk = tsm_reader.get_data_block(&blk_meta).await.unwrap();
                data.entry(field_id).or_default().push(blk);
            }
        }
        data
    }

    fn get_result_file_path(
        dir: impl AsRef<Path>,
        version_edit: VersionEdit,
        level: LevelId,
    ) -> PathBuf {
        if version_edit.has_file_id && !version_edit.add_files.is_empty() {
            if let Some(f) = version_edit
                .add_files
                .into_iter()
                .find(|f| f.level == level)
            {
                if level == 0 {
                    return file_utils::make_delta_file(dir, f.file_id);
                } else {
                    return file_utils::make_tsm_file(dir, f.file_id);
                }
            }
            panic!("VersionEdit::add_files doesn't contain any file matches level-{level}.");
        }
        panic!("VersionEdit::add_files is empty, no file to read.");
    }

    /// Compare DataBlocks in path with the expected_Data using assert_eq.
    pub async fn check_column_file(
        dir: impl AsRef<Path>,
        version_edit: VersionEdit,
        expected_data: HashMap<FieldId, Vec<DataBlock>>,
        expected_data_level: LevelId,
    ) {
        let path = get_result_file_path(dir, version_edit, expected_data_level);
        let data = read_data_blocks_from_column_file(path).await;
        let mut data_field_ids = data.keys().copied().collect::<Vec<_>>();
        data_field_ids.sort();
        let mut expected_data_field_ids = expected_data.keys().copied().collect::<Vec<_>>();
        expected_data_field_ids.sort();
        assert_eq!(data_field_ids, expected_data_field_ids);

        for (k, v) in expected_data.iter() {
            let data_blks = data.get(k).unwrap();
            if v.len() != data_blks.len() {
                let v_str = format_data_blocks(v.as_slice());
                let data_blks_str = format_data_blocks(data_blks.as_slice());
                panic!("fid={k}, v.len != data_blks.len:\n          v={v_str}\n  data_blks={data_blks_str}")
            }
            assert_eq!(v.len(), data_blks.len());
            for (v_idx, v_blk) in v.iter().enumerate() {
                assert_eq!(data_blks.get(v_idx).unwrap(), v_blk);
            }
        }
    }

    pub fn create_options(base_dir: String) -> Arc<Options> {
        let mut config = config::get_config_for_test();
        config.storage.path = base_dir.clone();
        config.log.path = base_dir;
        Arc::new(Options::from(&config))
    }

    pub fn prepare_compaction(
        tenant_database: Arc<String>,
        opt: Arc<Options>,
        next_file_id: ColumnFileId,
        files: Vec<Arc<ColumnFile>>,
        max_level_ts: Timestamp,
    ) -> (CompactReq, Arc<GlobalContext>) {
        let version = Arc::new(Version::new(
            1,
            tenant_database.clone(),
            opt.storage.clone(),
            1,
            LevelInfo::init_levels(tenant_database.clone(), 0, opt.storage.clone()),
            max_level_ts,
            Arc::new(ShardedAsyncCache::create_lru_sharded_cache(1)),
        ));
        let compact_req = CompactReq {
            ts_family_id: 1,
            tenant_database,
            storage_opt: opt.storage.clone(),
            files,
            version,
            in_level: 1,
            out_level: 2,
            max_ts: Timestamp::MAX,
        };
        let context = Arc::new(GlobalContext::new());
        context.set_file_id(next_file_id);

        (compact_req, context)
    }

    fn format_data_blocks(data_blocks: &[DataBlock]) -> String {
        format!(
            "[{}]",
            data_blocks
                .iter()
                .map(|b| format!("{}", b))
                .collect::<Vec<String>>()
                .join(", ")
        )
    }

    /// Test compaction with ordered data.
    #[tokio::test]
    async fn test_compaction_fast() {
        #[rustfmt::skip]
        let data = vec![
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: DataBlockEncoding::default() }]),
                (2, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: DataBlockEncoding::default() }]),
                (3, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: DataBlockEncoding::default() }]),
            ]),
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: DataBlockEncoding::default() }]),
                (2, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: DataBlockEncoding::default() }]),
                (3, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: DataBlockEncoding::default() }]),
            ]),
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: DataBlockEncoding::default() }]),
                (2, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: DataBlockEncoding::default() }]),
                (3, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: DataBlockEncoding::default() }]),
            ]),
        ];
        #[rustfmt::skip]
        let expected_data = HashMap::from([
            (1, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: DataBlockEncoding::default() }]),
            (2, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: DataBlockEncoding::default() }]),
            (3, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: DataBlockEncoding::default() }]),
        ]);

        let dir = "/tmp/test/compaction/0";
        let _ = std::fs::remove_dir_all(dir);
        let tenant_database = Arc::new("cnosdb.dba".to_string());
        let opt = create_options(dir.to_string());
        let dir = opt.storage.tsm_dir(&tenant_database, 1);
        let max_level_ts = 9;

        let (next_file_id, files) = write_data_blocks_to_column_file(&dir, data).await;
        let (compact_req, kernel) =
            prepare_compaction(tenant_database, opt, next_file_id, files, max_level_ts);
        let out_level = compact_req.out_level;
        let (version_edit, _) = run_compaction_job(compact_req, kernel)
            .await
            .unwrap()
            .unwrap();
        check_column_file(dir, version_edit, expected_data, out_level).await;
    }

    const INT_BLOCK_ENCODING: DataBlockEncoding =
        DataBlockEncoding::new(Encoding::Delta, Encoding::Delta);

    #[tokio::test]
    async fn test_compaction_1() {
        #[rustfmt::skip]
        let data = vec![
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: INT_BLOCK_ENCODING }]),
                (2, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: INT_BLOCK_ENCODING }]),
                (3, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: INT_BLOCK_ENCODING }]),
            ]),
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: INT_BLOCK_ENCODING }]),
                (2, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: INT_BLOCK_ENCODING }]),
                (3, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: INT_BLOCK_ENCODING }]),
            ]),
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: INT_BLOCK_ENCODING }]),
                (2, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: INT_BLOCK_ENCODING }]),
                (3, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: INT_BLOCK_ENCODING }]),
            ]),
        ];
        #[rustfmt::skip]
        let expected_data = HashMap::from([
            (1, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: INT_BLOCK_ENCODING }]),
            (2, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: INT_BLOCK_ENCODING }]),
            (3, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: INT_BLOCK_ENCODING }]),
        ]);

        let dir = "/tmp/test/compaction/1";
        let _ = std::fs::remove_dir_all(dir);
        let tenant_database = Arc::new("cnosdb.dba".to_string());
        let opt = create_options(dir.to_string());
        let dir = opt.storage.tsm_dir(&tenant_database, 1);
        let max_level_ts = 9;

        let (next_file_id, files) = write_data_blocks_to_column_file(&dir, data).await;
        let (compact_req, kernel) =
            prepare_compaction(tenant_database, opt, next_file_id, files, max_level_ts);
        let out_level = compact_req.out_level;
        let (version_edit, _) = run_compaction_job(compact_req, kernel)
            .await
            .unwrap()
            .unwrap();
        check_column_file(dir, version_edit, expected_data, out_level).await;
    }

    /// Test compact with duplicate timestamp.
    #[tokio::test]
    async fn test_compaction_2() {
        #[rustfmt::skip]
        let data = vec![
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4], val: vec![1, 2, 3, 5], enc: INT_BLOCK_ENCODING }]),
                (3, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4], val: vec![1, 2, 3, 5], enc: INT_BLOCK_ENCODING }]),
                (4, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: INT_BLOCK_ENCODING }]),
            ]),
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: INT_BLOCK_ENCODING }]),
                (2, vec![DataBlock::I64 { ts: vec![4, 5, 6], val: vec![4, 5, 6], enc: INT_BLOCK_ENCODING }]),
                (3, vec![DataBlock::I64 { ts: vec![4, 5, 6, 7], val: vec![4, 5, 6, 8], enc: INT_BLOCK_ENCODING }]),
            ]),
            HashMap::from([
                (1, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: INT_BLOCK_ENCODING }]),
                (2, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: INT_BLOCK_ENCODING }]),
                (3, vec![DataBlock::I64 { ts: vec![7, 8, 9], val: vec![7, 8, 9], enc: INT_BLOCK_ENCODING }]),
            ]),
        ];
        #[rustfmt::skip]
        let expected_data = HashMap::from([
            (1, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: INT_BLOCK_ENCODING }]),
            (2, vec![DataBlock::I64 { ts: vec![4, 5, 6, 7, 8, 9], val: vec![4, 5, 6, 7, 8, 9], enc: INT_BLOCK_ENCODING }]),
            (3, vec![DataBlock::I64 { ts: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], val: vec![1, 2, 3, 4, 5, 6, 7, 8, 9], enc: INT_BLOCK_ENCODING }]),
            (4, vec![DataBlock::I64 { ts: vec![1, 2, 3], val: vec![1, 2, 3], enc: INT_BLOCK_ENCODING }]),
        ]);

        let dir = "/tmp/test/compaction/2";
        let _ = std::fs::remove_dir_all(dir);
        let tenant_database = Arc::new("cnosdb.dba".to_string());
        let opt = create_options(dir.to_string());
        let dir = opt.storage.tsm_dir(&tenant_database, 1);
        let max_level_ts = 9;

        let (next_file_id, files) = write_data_blocks_to_column_file(&dir, data).await;
        let (compact_req, kernel) =
            prepare_compaction(tenant_database, opt, next_file_id, files, max_level_ts);
        let out_level = compact_req.out_level;
        let (version_edit, _) = run_compaction_job(compact_req, kernel)
            .await
            .unwrap()
            .unwrap();
        check_column_file(dir, version_edit, expected_data, out_level).await;
    }

    /// Returns a generated `DataBlock` with default value and specified size, `DataBlock::ts`
    /// is all the time-ranges in data_descriptors.
    ///
    /// The default value is different for each ValueType:
    /// - Unsigned: 1
    /// - Integer: 1
    /// - String: "1"
    /// - Float: 1.0
    /// - Boolean: true
    /// - Unknown: will create a panic
    pub fn generate_data_block(
        value_type: ValueType,
        data_descriptors: Vec<(i64, i64)>,
    ) -> DataBlock {
        match value_type {
            ValueType::Unsigned => {
                let mut ts_vec: Vec<Timestamp> = Vec::with_capacity(1000);
                let mut val_vec: Vec<u64> = Vec::with_capacity(1000);
                for (min_ts, max_ts) in data_descriptors {
                    for ts in min_ts..max_ts + 1 {
                        ts_vec.push(ts);
                        val_vec.push(1_u64);
                    }
                }
                DataBlock::U64 {
                    ts: ts_vec,
                    val: val_vec,
                    enc: DataBlockEncoding::new(Encoding::Delta, Encoding::Delta),
                }
            }
            ValueType::Integer => {
                let mut ts_vec: Vec<Timestamp> = Vec::with_capacity(1000);
                let mut val_vec: Vec<i64> = Vec::with_capacity(1000);
                for (min_ts, max_ts) in data_descriptors {
                    for ts in min_ts..max_ts + 1 {
                        ts_vec.push(ts);
                        val_vec.push(1_i64);
                    }
                }
                DataBlock::I64 {
                    ts: ts_vec,
                    val: val_vec,
                    enc: DataBlockEncoding::new(Encoding::Delta, Encoding::Delta),
                }
            }
            ValueType::String => {
                let word = MiniVec::from(&b"hello_world"[..]);
                let mut ts_vec: Vec<Timestamp> = Vec::with_capacity(10000);
                let mut val_vec: Vec<MiniVec<u8>> = Vec::with_capacity(10000);
                for (min_ts, max_ts) in data_descriptors {
                    for ts in min_ts..max_ts + 1 {
                        ts_vec.push(ts);
                        val_vec.push(word.clone());
                    }
                }
                DataBlock::Str {
                    ts: ts_vec,
                    val: val_vec,
                    enc: DataBlockEncoding::new(Encoding::Delta, Encoding::Snappy),
                }
            }
            ValueType::Float => {
                let mut ts_vec: Vec<Timestamp> = Vec::with_capacity(10000);
                let mut val_vec: Vec<f64> = Vec::with_capacity(10000);
                for (min_ts, max_ts) in data_descriptors {
                    for ts in min_ts..max_ts + 1 {
                        ts_vec.push(ts);
                        val_vec.push(1.0);
                    }
                }
                DataBlock::F64 {
                    ts: ts_vec,
                    val: val_vec,
                    enc: DataBlockEncoding::new(Encoding::Delta, Encoding::Gorilla),
                }
            }
            ValueType::Boolean => {
                let mut ts_vec: Vec<Timestamp> = Vec::with_capacity(10000);
                let mut val_vec: Vec<bool> = Vec::with_capacity(10000);
                for (min_ts, max_ts) in data_descriptors {
                    for ts in min_ts..max_ts + 1 {
                        ts_vec.push(ts);
                        val_vec.push(true);
                    }
                }
                DataBlock::Bool {
                    ts: ts_vec,
                    val: val_vec,
                    enc: DataBlockEncoding::new(Encoding::Delta, Encoding::BitPack),
                }
            }
            ValueType::Unknown => {
                panic!("value type is Unknown")
            }
        }
    }

    pub type TsmSchema = (
        ColumnFileId,                                    // tsm file id
        Vec<(ValueType, FieldId, Timestamp, Timestamp)>, // Data block definitions
        Vec<(FieldId, Timestamp, Timestamp)>,            // Tombstone definitions
    );

    pub async fn write_data_block_desc(
        dir: impl AsRef<Path>,
        data_desc: &[TsmSchema],
    ) -> Vec<Arc<ColumnFile>> {
        let mut column_files = Vec::new();
        for (tsm_sequence, tsm_desc, tombstone_desc) in data_desc.iter() {
            let mut tsm_writer = tsm::new_tsm_writer(&dir, *tsm_sequence, false, 0)
                .await
                .unwrap();
            for &(val_type, fid, min_ts, max_ts) in tsm_desc.iter() {
                tsm_writer
                    .write_block(fid, &generate_data_block(val_type, vec![(min_ts, max_ts)]))
                    .await
                    .unwrap();
            }
            tsm_writer.write_index().await.unwrap();
            tsm_writer.finish().await.unwrap();
            let tsm_tombstone = TsmTombstone::open(&dir, *tsm_sequence).await.unwrap();
            for (fid, min_ts, max_ts) in tombstone_desc.iter() {
                tsm_tombstone
                    .add_range(&[*fid][..], &TimeRange::new(*min_ts, *max_ts), None)
                    .await
                    .unwrap();
            }

            tsm_tombstone.flush().await.unwrap();
            column_files.push(Arc::new(ColumnFile::new(
                *tsm_sequence,
                2,
                TimeRange::new(tsm_writer.min_ts(), tsm_writer.max_ts()),
                tsm_writer.size(),
                false,
                tsm_writer.path(),
            )));
        }

        column_files
    }

    /// Test compaction without tombstones.
    #[tokio::test]
    async fn test_compaction_3() {
        #[rustfmt::skip]
        let data_desc: [TsmSchema; 3] = [
            // [( tsm_sequence, vec![ (ValueType, FieldId, Timestamp_Begin, Timestamp_end) ] )]
            (1_u64, vec![
                // 1, 1~2500
                (ValueType::Unsigned, 1_u64, 1_i64, 1000_i64),
                (ValueType::Unsigned, 1, 1001, 2000),
                (ValueType::Unsigned, 1, 2001, 2500),
                // 2, 1~1500
                (ValueType::Integer, 2, 1, 1000),
                (ValueType::Integer, 2, 1001, 1500),
                // 3, 1~1500
                (ValueType::Boolean, 3, 1, 1000),
                (ValueType::Boolean, 3, 1001, 1500),
            ], vec![]),
            (2, vec![
                // 1, 2001~4500
                (ValueType::Unsigned, 1, 2001, 3000),
                (ValueType::Unsigned, 1, 3001, 4000),
                (ValueType::Unsigned, 1, 4001, 4500),
                // 2, 1001~3000
                (ValueType::Integer, 2, 1001, 2000),
                (ValueType::Integer, 2, 2001, 3000),
                // 3, 1001~2500
                (ValueType::Boolean, 3, 1001, 2000),
                (ValueType::Boolean, 3, 2001, 2500),
                // 4, 1~1500
                (ValueType::Float, 4, 1, 1000),
                (ValueType::Float, 4, 1001, 1500),
            ], vec![]),
            (3, vec![
                // 1, 4001~6500
                (ValueType::Unsigned, 1, 4001, 5000),
                (ValueType::Unsigned, 1, 5001, 6000),
                (ValueType::Unsigned, 1, 6001, 6500),
                // 2, 3001~5000
                (ValueType::Integer, 2, 3001, 4000),
                (ValueType::Integer, 2, 4001, 5000),
                // 3, 2001~3500
                (ValueType::Boolean, 3, 2001, 3000),
                (ValueType::Boolean, 3, 3001, 3500),
                // 4. 1001~2500
                (ValueType::Float, 4, 1001, 2000),
                (ValueType::Float, 4, 2001, 2500),
            ], vec![]),
        ];
        #[rustfmt::skip]
        let expected_data: HashMap<FieldId, Vec<DataBlock>> = HashMap::from(
            [
                // 1, 1~6500
                (1, vec![
                    generate_data_block(ValueType::Unsigned, vec![(1, 1000)]),
                    generate_data_block(ValueType::Unsigned, vec![(1001, 2000)]),
                    generate_data_block(ValueType::Unsigned, vec![(2001, 3000)]),
                    generate_data_block(ValueType::Unsigned, vec![(3001, 4000)]),
                    generate_data_block(ValueType::Unsigned, vec![(4001, 5000)]),
                    generate_data_block(ValueType::Unsigned, vec![(5001, 6000)]),
                    generate_data_block(ValueType::Unsigned, vec![(6001, 6500)]),
                ]),
                // 2, 1~5000
                (2, vec![
                    generate_data_block(ValueType::Integer, vec![(1, 1000)]),
                    generate_data_block(ValueType::Integer, vec![(1001, 2000)]),
                    generate_data_block(ValueType::Integer, vec![(2001, 3000)]),
                    generate_data_block(ValueType::Integer, vec![(3001, 4000)]),
                    generate_data_block(ValueType::Integer, vec![(4001, 5000)]),
                ]),
                // 3, 1~3500
                (3, vec![
                    generate_data_block(ValueType::Boolean, vec![(1, 1000)]),
                    generate_data_block(ValueType::Boolean, vec![(1001, 2000)]),
                    generate_data_block(ValueType::Boolean, vec![(2001, 3000)]),
                    generate_data_block(ValueType::Boolean, vec![(3001, 3500)]),
                ]),
                // 4, 1~2500
                (4, vec![
                    generate_data_block(ValueType::Float, vec![(1, 1000)]),
                    generate_data_block(ValueType::Float, vec![(1001, 2000)]),
                    generate_data_block(ValueType::Float, vec![(2001, 2500)]),
                ]),
            ]
        );

        let dir = "/tmp/test/compaction/3";
        let _ = std::fs::remove_dir_all(dir);
        let tenant_database = Arc::new("cnosdb.dba".to_string());
        let opt = create_options(dir.to_string());
        let dir = opt.storage.tsm_dir(&tenant_database, 1);
        if !file_manager::try_exists(&dir) {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let max_level_ts = 6500;

        let column_files = write_data_block_desc(&dir, &data_desc).await;
        let next_file_id = 4_u64;

        let (compact_req, kernel) = prepare_compaction(
            tenant_database,
            opt,
            next_file_id,
            column_files,
            max_level_ts,
        );
        let out_level = compact_req.out_level;
        let (version_edit, _) = run_compaction_job(compact_req, kernel)
            .await
            .unwrap()
            .unwrap();

        check_column_file(dir, version_edit, expected_data, out_level).await;
    }

    /// Test compaction with tombstones
    #[tokio::test]
    async fn test_compaction_4() {
        #[rustfmt::skip]
        let data_desc: [TsmSchema; 3] = [
            // [( tsm_data:  tsm_sequence, vec![(ValueType, FieldId, Timestamp_Begin, Timestamp_end)],
            //    tombstone: vec![(FieldId, MinTimestamp, MaxTimestamp)]
            // )]
            (1, vec![
                // 1, 1~2500
                (ValueType::Unsigned, 1, 1, 1000), (ValueType::Unsigned, 1, 1001, 2000), (ValueType::Unsigned, 1, 2001, 2500),
            ], vec![(1, 1, 2), (1, 2001, 2100)]),
            (2, vec![
                // 1, 2001~4500
                // 2101~3100, 3101~4100, 4101~4499
                (ValueType::Unsigned, 1, 2001, 3000), (ValueType::Unsigned, 1, 3001, 4000), (ValueType::Unsigned, 1, 4001, 4500),
            ], vec![(1, 2001, 2100), (1, 4500, 4501)]),
            (3, vec![
                // 1, 4001~6500
                // 4001~4499, 4502~5501, 5502~6500
                (ValueType::Unsigned, 1, 4001, 5000), (ValueType::Unsigned, 1, 5001, 6000), (ValueType::Unsigned, 1, 6001, 6500),
            ], vec![(1, 4500, 4501)]),
        ];
        #[rustfmt::skip]
        let expected_data: HashMap<FieldId, Vec<DataBlock>> = HashMap::from(
            [
                // 1, 1~6500
                (1, vec![
                    generate_data_block(ValueType::Unsigned, vec![(3, 1002)]),
                    generate_data_block(ValueType::Unsigned, vec![(1003, 2000), (2101, 2102)]),
                    generate_data_block(ValueType::Unsigned, vec![(2103, 3102)]),
                    generate_data_block(ValueType::Unsigned, vec![(3103, 4102)]),
                    generate_data_block(ValueType::Unsigned, vec![(4103, 4499), (4502, 5104)]),
                    generate_data_block(ValueType::Unsigned, vec![(5105, 6104)]),
                    generate_data_block(ValueType::Unsigned, vec![(6105, 6500)]),
                ]),
            ]
        );

        let dir = "/tmp/test/compaction/4";
        let _ = std::fs::remove_dir_all(dir);
        let tenant_database = Arc::new("cnosdb.dba".to_string());
        let opt = create_options(dir.to_string());
        let dir = opt.storage.tsm_dir(&tenant_database, 1);
        if !file_manager::try_exists(&dir) {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let max_level_ts = 6500;

        let column_files = write_data_block_desc(&dir, &data_desc).await;
        let next_file_id = 4_u64;
        let (compact_req, kernel) = prepare_compaction(
            tenant_database,
            opt,
            next_file_id,
            column_files,
            max_level_ts,
        );
        let out_level = compact_req.out_level;
        let (version_edit, _) = run_compaction_job(compact_req, kernel)
            .await
            .unwrap()
            .unwrap();

        check_column_file(dir, version_edit, expected_data, out_level).await;
    }

    /// Test compaction with multi-field and tombstones.
    #[tokio::test]
    async fn test_compaction_5() {
        #[rustfmt::skip]
        let data_desc: [TsmSchema; 3] = [
            // [( tsm_data:  tsm_sequence, vec![(ValueType, FieldId, Timestamp_Begin, Timestamp_end)],
            //    tombstone: vec![(FieldId, MinTimestamp, MaxTimestamp)]
            // )]
            (1, vec![
                // 1, 1~2500
                (ValueType::Unsigned, 1, 1, 1000), (ValueType::Unsigned, 1, 1001, 2000),  (ValueType::Unsigned, 1, 2001, 2500),
                // 2, 1~1500
                (ValueType::Integer, 2, 1, 1000), (ValueType::Integer, 2, 1001, 1500),
                // 3, 1~1500
                (ValueType::Boolean, 3, 1, 1000), (ValueType::Boolean, 3, 1001, 1500),
            ], vec![
                (1, 1, 2), (1, 2001, 2100),
                (2, 1001, 1002),
                (3, 1499, 1500),
            ]),
            (2, vec![
                // 1, 2001~4500
                (ValueType::Unsigned, 1, 2001, 3000), (ValueType::Unsigned, 1, 3001, 4000), (ValueType::Unsigned, 1, 4001, 4500),
                // 2, 1001~3000
                (ValueType::Integer, 2, 1001, 2000), (ValueType::Integer, 2, 2001, 3000),
                // 3, 1001~2500
                (ValueType::Boolean, 3, 1001, 2000), (ValueType::Boolean, 3, 2001, 2500),
                // 4, 1~1500
                (ValueType::Float, 4, 1, 1000), (ValueType::Float, 4, 1001, 1500),
            ], vec![
                (1, 2001, 2100), (1, 4500, 4501),
                (2, 1001, 1002), (2, 2501, 2502),
                (3, 1499, 1500),
            ]),
            (3, vec![
                // 1, 4001~6500
                (ValueType::Unsigned, 1, 4001, 5000), (ValueType::Unsigned, 1, 5001, 6000), (ValueType::Unsigned, 1, 6001, 6500),
                // 2, 3001~5000
                (ValueType::Integer, 2, 3001, 4000), (ValueType::Integer, 2, 4001, 5000),
                // 3, 2001~3500
                (ValueType::Boolean, 3, 2001, 3000), (ValueType::Boolean, 3, 3001, 3500),
                // 4. 1001~2500
                (ValueType::Float, 4, 1001, 2000), (ValueType::Float, 4, 2001, 2500),
            ], vec![
                (1, 4500, 4501),
                (2, 4001, 4002),
            ]),
        ];
        #[rustfmt::skip]
        let expected_data: HashMap<FieldId, Vec<DataBlock>> = HashMap::from(
            [
                // 1, 1~6500
                (1, vec![
                    generate_data_block(ValueType::Unsigned, vec![(3, 1002)]),
                    generate_data_block(ValueType::Unsigned, vec![(1003, 2000), (2101, 2102)]),
                    generate_data_block(ValueType::Unsigned, vec![(2103, 3102)]),
                    generate_data_block(ValueType::Unsigned, vec![(3103, 4102)]),
                    generate_data_block(ValueType::Unsigned, vec![(4103, 4499), (4502, 5104)]),
                    generate_data_block(ValueType::Unsigned, vec![(5105, 6104)]),
                    generate_data_block(ValueType::Unsigned, vec![(6105, 6500)]),
                ]),
                // 2, 1~5000
                (2, vec![
                    generate_data_block(ValueType::Integer, vec![(1, 1000)]),
                    generate_data_block(ValueType::Integer, vec![(1003, 2002)]),
                    generate_data_block(ValueType::Integer, vec![(2003, 2500), (2503, 3004)]),
                    generate_data_block(ValueType::Integer, vec![(3005, 4000), (4003, 4006)]),
                    generate_data_block(ValueType::Integer, vec![(4007, 5000)]),
                ]),
                // 3, 1~3500
                (3, vec![
                    generate_data_block(ValueType::Boolean, vec![(1, 1000)]),
                    generate_data_block(ValueType::Boolean, vec![(1001, 1498), (1501, 2002)]),
                    generate_data_block(ValueType::Boolean, vec![(2003, 3002)]),
                    generate_data_block(ValueType::Boolean, vec![(3003, 3500)]),
                ]),
                // 4, 1~2500
                (4, vec![
                    generate_data_block(ValueType::Float, vec![(1, 1000)]),
                    generate_data_block(ValueType::Float, vec![(1001, 2000)]),
                    generate_data_block(ValueType::Float, vec![(2001, 2500)]),
                ]),
            ]
        );

        let dir = "/tmp/test/compaction/5";
        let _ = std::fs::remove_dir_all(dir);
        let tenant_database = Arc::new("cnosdb.dba".to_string());
        let opt = create_options(dir.to_string());
        let dir = opt.storage.tsm_dir(&tenant_database, 1);
        if !file_manager::try_exists(&dir) {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let max_level_ts = 6500;

        let column_files = write_data_block_desc(&dir, &data_desc).await;
        let next_file_id = 4_u64;
        let (compact_req, kernel) = prepare_compaction(
            tenant_database,
            opt,
            next_file_id,
            column_files,
            max_level_ts,
        );
        let out_level = compact_req.out_level;
        let (version_edit, _) = run_compaction_job(compact_req, kernel)
            .await
            .unwrap()
            .unwrap();

        check_column_file(dir, version_edit, expected_data, out_level).await;
    }
}

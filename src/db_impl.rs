//! db_impl contains the implementation of the database interface and high-level compaction and
//! maintenance logic.

#![allow(unused_attributes)]

use db_iter::DBIterator;

use cmp::{Cmp, InternalKeyCmp};
use env::{Env, FileLock};
use error::{err, Status, StatusCode, Result};
use filter::{BoxedFilterPolicy, InternalFilterPolicy};
use infolog::Logger;
use log::{LogReader, LogWriter};
use key_types::{parse_internal_key, InternalKey, LookupKey, ValueType};
use memtable::MemTable;
use merging_iter::MergingIter;
use options::Options;
use snapshot::{Snapshot, SnapshotList};
use table_builder::TableBuilder;
use table_cache::{table_file_name, TableCache};
use types::{parse_file_name, share, FileMetaData, FileNum, FileType, LdbIterator,
            MAX_SEQUENCE_NUMBER, NUM_LEVELS, SequenceNumber, Shared};
use version_edit::VersionEdit;
use version_set::{manifest_file_name, read_current_file, set_current_file, Compaction, VersionSet};
use version::Version;
use write_batch::WriteBatch;

use std::cmp::Ordering;
use std::io::{self, Write};
use std::mem;
use std::ops::{DerefMut, Drop};
use std::path::Path;
use std::rc::Rc;

/// DB contains the actual database implemenation. As opposed to the original, this implementation
/// is not concurrent (yet).
pub struct DB {
    name: String,
    lock: Option<FileLock>,

    internal_cmp: Rc<Box<Cmp>>,
    fpol: InternalFilterPolicy<BoxedFilterPolicy>,
    opt: Options,

    mem: MemTable,
    imm: Option<MemTable>,

    log: Option<LogWriter<Box<Write>>>,
    log_num: Option<FileNum>,
    cache: Shared<TableCache>,
    vset: Shared<VersionSet>,
    snaps: SnapshotList,

    cstats: [CompactionStats; NUM_LEVELS],
}

impl DB {
    // RECOVERY AND INITIALIZATION //

    /// new initializes a new DB object, but doesn't touch disk.
    fn new(name: &str, mut opt: Options) -> DB {
        if opt.log.is_none() {
            let log = open_info_log(opt.env.as_ref().as_ref(), name);
            opt.log = Some(share(log));
        }

        let cache = share(TableCache::new(&name, opt.clone(), opt.max_open_files - 10));
        let vset = VersionSet::new(&name, opt.clone(), cache.clone());

        DB {
            name: name.to_string(),
            lock: None,
            internal_cmp: Rc::new(Box::new(InternalKeyCmp(opt.cmp.clone()))),
            fpol: InternalFilterPolicy::new(opt.filter_policy.clone()),

            mem: MemTable::new(opt.cmp.clone()),
            imm: None,

            opt: opt,

            log: None,
            log_num: None,
            cache: cache,
            vset: share(vset),
            snaps: SnapshotList::new(),

            cstats: Default::default(),
        }
    }

    fn current(&self) -> Shared<Version> {
        self.vset.borrow().current()
    }

    /// Opens or creates* a new or existing database.
    ///
    /// *depending on the options set (create_if_missing, error_if_exists).
    fn open(name: &str, opt: Options) -> Result<DB> {
        let mut db = DB::new(name, opt);
        let mut ve = VersionEdit::new();
        let save_manifest = db.recover(&mut ve)?;

        // Create log file if an old one is not being reused.
        if db.log.is_none() {
            let lognum = db.vset.borrow_mut().new_file_number();
            let logfile =
                db.opt.env.open_writable_file(Path::new(&log_file_name(&db.name, lognum)))?;
            ve.set_log_num(lognum);
            db.log = Some(LogWriter::new(logfile));
            db.log_num = Some(lognum);
        }

        if save_manifest {
            ve.set_log_num(db.log_num.unwrap_or(0));
            db.vset.borrow_mut().log_and_apply(ve)?;
        }

        db.delete_obsolete_files()?;
        db.maybe_do_compaction()?;
        Ok(db)
    }

    /// initialize_db initializes a new database.
    fn initialize_db(&mut self) -> Result<()> {
        let mut ve = VersionEdit::new();
        ve.set_comparator_name(self.opt.cmp.id());
        ve.set_log_num(0);
        ve.set_next_file(2);
        ve.set_last_seq(0);

        {
            let manifest = manifest_file_name(&self.name, 1);
            let manifest_file = self.opt.env.open_writable_file(Path::new(&manifest))?;
            let mut lw = LogWriter::new(manifest_file);
            lw.add_record(&ve.encode())?;
            lw.flush()?;
        }
        set_current_file(&self.opt.env, &self.name, 1)
    }

    /// recover recovers from the existing state on disk. If the wrapped result is `true`, then
    /// log_and_apply() should be called after recovery has finished.
    fn recover(&mut self, ve: &mut VersionEdit) -> Result<bool> {
        self.opt.env.mkdir(Path::new(&self.name)).is_ok();
        self.acquire_lock()?;

        if let Err(e) = read_current_file(&self.opt.env, &self.name) {
            if e.code == StatusCode::NotFound && self.opt.create_if_missing {
                self.initialize_db()?;
            } else {
                return err(StatusCode::InvalidArgument,
                           "database does not exist and create_if_missing is false");
            }
        } else if self.opt.error_if_exists {
            return err(StatusCode::InvalidArgument,
                       "database already exists and error_if_exists is true");
        }

        // If save_manifest is true, the existing manifest is reused and we should log_and_apply()
        // later.
        let mut save_manifest = self.vset.borrow_mut().recover()?;

        // Recover from all log files not in the descriptor.
        let mut max_seq = 0;
        let filenames = self.opt.env.children(Path::new(&self.name))?;
        let mut expected = self.vset.borrow().live_files();
        let mut log_files = vec![];

        for file in &filenames {
            if let Ok((num, typ)) = parse_file_name(&file) {
                expected.remove(&num);
                if typ == FileType::Log && num >= self.vset.borrow().log_num {
                    log_files.push(num);
                }
            }
        }
        if !expected.is_empty() {
            log!(self.opt.log, "Missing at least these files: {:?}", expected);
            return err(StatusCode::Corruption, "missing live files (see log)");
        }

        log_files.sort();
        for i in 0..log_files.len() {
            let (save_manifest_, max_seq_) =
                self.recover_log_file(log_files[i], i == log_files.len() - 1, ve)?;
            if save_manifest_ {
                save_manifest = true;
            }
            if max_seq_ > max_seq {
                max_seq = max_seq_;
            }
            self.vset.borrow_mut().mark_file_number_used(log_files[i]);
        }

        if self.vset.borrow().last_seq < max_seq {
            self.vset.borrow_mut().last_seq = max_seq;
        }

        Ok(save_manifest)
    }

    /// recover_log_file reads a single log file into a memtable, writing new L0 tables if
    /// necessary. If is_last is true, it checks whether the log file can be reused, and sets up
    /// the database's logging handles appropriately if that's the case.
    fn recover_log_file(&mut self,
                        log_num: FileNum,
                        is_last: bool,
                        ve: &mut VersionEdit)
                        -> Result<(bool, SequenceNumber)> {
        let filename = log_file_name(&self.name, log_num);
        let logfile = self.opt.env.open_sequential_file(Path::new(&filename))?;
        // Use the user-supplied comparator; it will be wrapped inside a MemtableKeyCmp.
        let cmp: Rc<Box<Cmp>> = self.opt.cmp.clone();

        let mut logreader = LogReader::new(logfile,
                                           // checksum=
                                           true);
        log!(self.opt.log, "Recovering log file {}", filename);
        let mut scratch = vec![];
        let mut mem = MemTable::new(cmp.clone());
        let mut batch = WriteBatch::new();

        let mut compactions = 0;
        let mut max_seq = 0;
        let mut save_manifest = false;

        while let Ok(len) = logreader.read(&mut scratch) {
            if len == 0 {
                break;
            }
            if len < 12 {
                log!(self.opt.log,
                     "corruption in log file {:06}: record shorter than 12B",
                     log_num);
                continue;
            }

            batch.set_contents(&scratch);
            batch.insert_into_memtable(batch.sequence(), &mut mem);
            save_manifest = true;

            let last_seq = batch.sequence() + batch.count() as u64 - 1;
            if last_seq > max_seq {
                max_seq = last_seq
            }
            if mem.approx_mem_usage() > self.opt.write_buffer_size {
                compactions += 1;
                self.write_l0_table(&mem, ve, None)?;
                mem = MemTable::new(cmp.clone());
            }
            batch.clear();
        }

        // Check if we can reuse the last log file.
        if self.opt.reuse_logs && is_last && compactions == 0 {
            assert!(self.log.is_none());
            log!(self.opt.log, "reusing log file {}", filename);
            let oldsize = self.opt.env.size_of(Path::new(&filename))?;
            let oldfile = self.opt.env.open_appendable_file(Path::new(&filename))?;
            let lw = LogWriter::new_with_off(oldfile, oldsize);
            self.log = Some(lw);
            self.log_num = Some(log_num);
            self.mem = mem;
        } else if mem.len() > 0 {
            // Log is not reused, so write out the accumulated memtable.
            self.write_l0_table(&mem, ve, None)?;
        }

        Ok((save_manifest, max_seq))
    }

    /// delete_obsolete_files removes files that are no longer needed from the file system.
    fn delete_obsolete_files(&mut self) -> Result<()> {
        let files = self.vset.borrow().live_files();
        let filenames = self.opt.env.children(Path::new(&self.name))?;
        for name in filenames {
            if let Ok((num, typ)) = parse_file_name(&name) {
                log!(self.opt.log, "{} {:?}", num, typ);
                match typ {
                    FileType::Log => {
                        if num >= self.vset.borrow().log_num {
                            continue;
                        }
                    }
                    FileType::Descriptor => {
                        if num >= self.vset.borrow().manifest_num {
                            continue;
                        }
                    }
                    FileType::Table => {
                        if files.contains(&num) {
                            continue;
                        }
                    }
                    // NOTE: In this non-concurrent implementation, we likely never find temp
                    // files.
                    FileType::Temp => {
                        if files.contains(&num) {
                            continue;
                        }
                    }
                    FileType::Current | FileType::DBLock | FileType::InfoLog => continue,
                }

                // If we're here, delete this file.
                if typ == FileType::Table {
                    self.cache.borrow_mut().evict(num).is_ok();
                }
                log!(self.opt.log, "Deleting file type={:?} num={}", typ, num);
                if let Err(e) = self.opt
                    .env
                    .delete(Path::new(&format!("{}/{}", &self.name, &name))) {
                    log!(self.opt.log, "Deleting file num={} failed: {}", num, e);
                }
            }
        }
        Ok(())
    }

    /// acquire_lock acquires the lock file.
    fn acquire_lock(&mut self) -> Result<()> {
        let lock_r = self.opt.env.lock(Path::new(&lock_file_name(&self.name)));
        match lock_r {
            Ok(lockfile) => {
                self.lock = Some(lockfile);
                Ok(())
            }
            Err(ref e) if e.code == StatusCode::LockError => {
                err(StatusCode::LockError,
                    "database lock is held by another instance")
            }
            Err(e) => Err(e),
        }
    }

    /// release_lock releases the lock file, if it's currently held.
    fn release_lock(&mut self) -> Result<()> {
        if let Some(l) = self.lock.take() {
            self.opt.env.unlock(l)
        } else {
            Ok(())
        }
    }
}

impl DB {
    // WRITE //

    /// put adds a single entry.
    fn put(&mut self, k: &[u8], v: &[u8]) -> Result<()> {
        let mut wb = WriteBatch::new();
        wb.put(k, v);
        self.write(wb, false)
    }

    /// write writes an entire WriteBatch. sync determines whether the write should be flushed to
    /// disk.
    fn write(&mut self, batch: WriteBatch, sync: bool) -> Result<()> {
        assert!(self.log.is_some());
        let entries = batch.count() as u64;
        let log = self.log.as_mut().unwrap();
        let next = self.vset.borrow().last_seq + 1;

        batch.insert_into_memtable(next, &mut self.mem);
        log.add_record(&batch.encode(next))?;
        if sync {
            log.flush()?;
        }
        self.vset.borrow_mut().last_seq += entries;
        Ok(())
    }

    /// flush makes sure that all pending changes (e.g. from put()) are stored on disk.
    fn flush(&mut self) -> Result<()> {
        assert!(self.log.is_some());
        self.log.as_mut().unwrap().flush()
    }
}

impl DB {
    // READ //

    fn get_internal(&mut self, seq: SequenceNumber, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let current = self.current();
        let mut current = current.borrow_mut();

        // Using this lookup key will skip all entries with higher sequence numbers, because they
        // will compare "Lesser" using the InternalKeyCmp
        let lkey = LookupKey::new(key, seq);

        match self.mem.get(&lkey) {
            (Some(v), _) => return Ok(Some(v)),
            // deleted entry
            (None, true) => return Ok(None),
            // not found entry
            (None, false) => {}
        }

        if let Some(imm) = self.imm.as_ref() {
            match imm.get(&lkey) {
                (Some(v), _) => return Ok(Some(v)),
                // deleted entry
                (None, true) => return Ok(None),
                // not found entry
                (None, false) => {}
            }
        }

        if let Ok(Some((v, st))) = current.get(lkey.internal_key()) {
            if current.update_stats(st) {
                if let Err(e) = self.maybe_do_compaction() {
                    log!(self.opt.log, "error while doing compaction in get: {}", e);
                }
            }
            return Ok(Some(v));
        }
        Ok(None)
    }

    /// get_at reads the value for a given key at or before snapshot. It returns Ok(None) if the
    /// entry wasn't found, and Err(_) if an error occurred.
    pub fn get_at(&mut self, snapshot: &Snapshot, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_internal(snapshot.sequence(), key)
    }

    /// get is a simplified version of get_at(), translating errors to None.
    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let seq = self.vset.borrow().last_seq;
        if let Ok(v) = self.get_internal(seq, key) {
            v
        } else {
            None
        }
    }
}

impl DB {
    // ITERATOR //

    /// new_iter returns a DBIterator over the current state of the database. The iterator will not
    /// return elements added to the database after its creation.
    pub fn new_iter(&mut self) -> Result<DBIterator> {
        let snapshot = self.get_snapshot();
        self.new_iter_at(snapshot)
    }

    /// new_iter_at returns a DBIterator at the supplied snapshot.
    pub fn new_iter_at(&mut self, ss: Snapshot) -> Result<DBIterator> {
        Ok(DBIterator::new(self.opt.cmp.clone(),
                           self.vset.clone(),
                           self.merge_iterators()?,
                           ss))
    }

    /// merge_iterators produces a MergingIter merging the entries in the memtable, the immutable
    /// memtable, and table files from all levels.
    fn merge_iterators(&mut self) -> Result<MergingIter> {
        let mut iters: Vec<Box<LdbIterator>> = vec![];
        if self.mem.len() > 0 {
            iters.push(Box::new(self.mem.iter()));
        }
        if let Some(ref imm) = self.imm {
            if imm.len() > 0 {
                iters.push(Box::new(imm.iter()));
            }
        }

        // Add iterators for table files.
        let current = self.current();
        let current = current.borrow();
        iters.extend(current.new_iters()?);

        Ok(MergingIter::new(self.internal_cmp.clone(), iters))
    }
}

impl DB {
    // SNAPSHOTS //

    /// Returns a snapshot at the current state. The snapshot is released automatically on Drop.
    pub fn get_snapshot(&mut self) -> Snapshot {
        self.snaps.new_snapshot(self.vset.borrow().last_seq)
    }
}

impl DB {
    // STATISTICS //
    fn add_stats(&mut self, level: usize, cs: CompactionStats) {
        assert!(level < NUM_LEVELS);
        self.cstats[level].add(cs);
    }

    /// Trigger a compaction based on where this key is located in the different levels.
    fn record_read_sample<'a>(&mut self, k: InternalKey<'a>) {
        let current = self.current();
        if current.borrow_mut().record_read_sample(k) {
            if let Err(e) = self.maybe_do_compaction() {
                log!(self.opt.log, "record_read_sample: compaction failed: {}", e);
            }
        }
    }
}

impl DB {
    // COMPACTIONS //
    /// make_room_for_write checks if the memtable has become too large, and triggers a compaction
    /// if it's the case.
    fn make_room_for_write(&mut self) -> Result<()> {
        if self.mem.approx_mem_usage() < self.opt.write_buffer_size {
            Ok(())
        } else {
            // Create new memtable.
            let logn = self.vset.borrow_mut().new_file_number();
            let logf = self.opt.env.open_writable_file(Path::new(&log_file_name(&self.name, logn)));
            if logf.is_err() {
                self.vset.borrow_mut().reuse_file_number(logn);
                Err(logf.err().unwrap())
            } else {
                self.log = Some(LogWriter::new(logf.unwrap()));
                self.log_num = Some(logn);

                let mut imm = MemTable::new(self.opt.cmp.clone());
                mem::swap(&mut imm, &mut self.mem);
                self.imm = Some(imm);
                self.maybe_do_compaction()
            }
        }
    }

    /// maybe_do_compaction starts a blocking compaction if it makes sense.
    fn maybe_do_compaction(&mut self) -> Result<()> {
        if self.imm.is_none() && !self.vset.borrow().needs_compaction() {
            return Ok(());
        }
        self.start_compaction()
    }

    /// start_compaction dispatches the different kinds of compactions depending on the current
    /// state of the database.
    fn start_compaction(&mut self) -> Result<()> {
        // TODO (maybe): Support manual compactions.
        if self.imm.is_some() {
            return self.compact_memtable();
        }


        let compaction = self.vset.borrow_mut().pick_compaction();
        if compaction.is_none() {
            return Ok(());
        }
        let mut compaction = compaction.unwrap();

        if compaction.is_trivial_move() {
            assert_eq!(1, compaction.num_inputs(0));
            let f = compaction.input(0, 0);
            let num = f.num;
            let size = f.size;
            let level = compaction.level();

            compaction.edit().delete_file(level, num);
            compaction.edit().add_file(level + 1, f);

            let r = self.vset.borrow_mut().log_and_apply(compaction.into_edit());
            if let Err(e) = r {
                log!(self.opt.log, "trivial move failed: {}", e);
                Err(e)
            } else {
                log!(self.opt.log,
                     "Moved num={} bytes={} from L{} to L{}",
                     num,
                     size,
                     level,
                     level + 1);
                log!(self.opt.log,
                     "Summary: {}",
                     self.vset.borrow().current_summary());
                Ok(())
            }
        } else {
            let mut state = CompactionState::new(compaction);
            if let Err(e) = self.do_compaction_work(&mut state) {
                state.cleanup(&self.opt.env, &self.name);
                log!(self.opt.log, "Compaction work failed: {}", e);
            }
            self.install_compaction_results(state)?;
            log!(self.opt.log,
                 "Compaction finished: {}",
                 self.vset.borrow().current_summary());

            self.delete_obsolete_files()
        }
    }

    fn compact_memtable(&mut self) -> Result<()> {
        assert!(self.imm.is_some());

        let mut ve = VersionEdit::new();
        let base = self.current();

        let imm = self.imm.take().unwrap();
        if let Err(e) = self.write_l0_table(&imm, &mut ve, Some(&base.borrow())) {
            self.imm = Some(imm);
            return Err(e);
        }
        ve.set_log_num(self.log_num.unwrap_or(0));
        self.vset.borrow_mut().log_and_apply(ve)?;
        if let Err(e) = self.delete_obsolete_files() {
            log!(self.opt.log, "Error deleting obsolete files: {}", e);
        }
        Ok(())
    }

    /// write_l0_table writes the given memtable to a table file.
    fn write_l0_table(&mut self,
                      memt: &MemTable,
                      ve: &mut VersionEdit,
                      base: Option<&Version>)
                      -> Result<()> {
        let start_ts = self.opt.env.micros();
        let num = self.vset.borrow_mut().new_file_number();
        log!(self.opt.log, "Start write of L0 table {:06}", num);
        let fmd = build_table(&self.name, &self.opt, memt.iter(), num)?;
        log!(self.opt.log, "L0 table {:06} has {} bytes", num, fmd.size);

        // Wrote empty table.
        if fmd.size == 0 {
            self.vset.borrow_mut().reuse_file_number(num);
            return Ok(());
        }

        let cache_result = self.cache.borrow_mut().get_table(num);
        if let Err(e) = cache_result {
            log!(self.opt.log,
                 "L0 table {:06} not returned by cache: {}",
                 num,
                 e);
            self.opt.env.delete(Path::new(&table_file_name(&self.name, num))).is_ok();
            return Err(e);
        }

        let mut stats = CompactionStats::default();
        stats.micros = self.opt.env.micros() - start_ts;
        stats.written = fmd.size;

        let mut level = 0;
        if let Some(b) = base {
            level = b.pick_memtable_output_level(parse_internal_key(&fmd.smallest).2,
                                                 parse_internal_key(&fmd.largest).2);
        }

        self.add_stats(level, stats);
        ve.add_file(level, fmd);

        Ok(())
    }

    fn do_compaction_work(&mut self, cs: &mut CompactionState) -> Result<()> {
        let start_ts = self.opt.env.micros();
        log!(self.opt.log,
             "Compacting {} files at L{} and {} files at L{}",
             cs.compaction.num_inputs(0),
             cs.compaction.level(),
             cs.compaction.num_inputs(1),
             cs.compaction.level() + 1);
        assert!(self.vset.borrow().num_level_files(cs.compaction.level()) > 0);
        assert!(cs.builder.is_none());

        cs.smallest_seq = if self.snaps.empty() {
            self.vset.borrow().last_seq
        } else {
            self.snaps.oldest()
        };

        let mut input = self.vset.borrow().make_input_iterator(&cs.compaction);
        input.seek_to_first();

        let (mut key, mut val) = (vec![], vec![]);
        let mut last_seq_for_key = MAX_SEQUENCE_NUMBER;

        let mut have_ukey = false;
        let mut current_ukey = vec![];

        while input.valid() {
            // TODO: Do we need to do a memtable compaction here? Probably not, in the sequential
            // case.
            assert!(input.current(&mut key, &mut val));
            if cs.compaction.should_stop_before(&key) && cs.builder.is_none() {
                self.finish_compaction_output(cs, key.clone())?;
            }
            let (ktyp, seq, ukey) = parse_internal_key(&key);
            if seq == 0 {
                // Parsing failed.
                log!(self.opt.log, "Encountered seq=0 in key: {:?}", &key);
                last_seq_for_key = MAX_SEQUENCE_NUMBER;
                continue;
            }

            if !have_ukey || self.opt.cmp.cmp(ukey, &current_ukey) != Ordering::Equal {
                // First occurrence of this key.
                current_ukey.clear();
                current_ukey.extend_from_slice(ukey);
                have_ukey = true;
                last_seq_for_key = MAX_SEQUENCE_NUMBER;
            }

            // We can omit the key under the following conditions:
            if last_seq_for_key <= cs.smallest_seq {
                continue;
            }
            if ktyp == ValueType::TypeDeletion && seq <= cs.smallest_seq &&
               cs.compaction.is_base_level_for(ukey) {
                continue;
            }

            if cs.builder.is_none() {
                let fnum = self.vset.borrow_mut().new_file_number();
                let mut fmd = FileMetaData::default();
                fmd.num = fnum;

                let fname = table_file_name(&self.name, fnum);
                let f = self.opt.env.open_writable_file(Path::new(&fname))?;
                let f = Box::new(io::BufWriter::new(f));
                cs.builder = Some(TableBuilder::new(self.opt.clone(), f));
                cs.outputs.push(fmd);
            }
            if cs.builder.as_ref().unwrap().entries() == 0 {
                cs.current_output().smallest = key.clone();
            }
            cs.builder.as_mut().unwrap().add(&key, &val)?;
            // NOTE: Adjust max file size based on level.
            if cs.builder.as_ref().unwrap().size_estimate() > self.opt.max_file_size {
                self.finish_compaction_output(cs, key.clone())?;
            }

            input.advance();
        }

        if cs.builder.is_some() {
            self.finish_compaction_output(cs, key)?;
        }

        let mut stats = CompactionStats::default();
        stats.micros = self.opt.env.micros() - start_ts;
        for parent in 0..2 {
            for inp in 0..cs.compaction.num_inputs(parent) {
                stats.read += cs.compaction.input(parent, inp).size;
            }
        }
        for output in &cs.outputs {
            stats.written += output.size;
        }
        self.cstats[cs.compaction.level()].add(stats);
        Ok(())
    }

    fn finish_compaction_output(&mut self,
                                cs: &mut CompactionState,
                                largest: Vec<u8>)
                                -> Result<()> {
        assert!(cs.builder.is_some());
        let output_num = cs.current_output().num;
        assert!(output_num > 0);

        // The original checks if the input iterator has an OK status. For this, we'd need to
        // extend the LdbIterator interface though -- let's see if we can without for now.
        // (it's not good for corruptions, in any case)
        let b = cs.builder.take().unwrap();
        let entries = b.entries();
        let bytes = b.finish()?;
        cs.total_bytes += bytes;

        cs.current_output().largest = largest;
        cs.current_output().size = bytes;

        if entries > 0 {
            // Verify that table can be used. (Separating get_table() because borrowing in an if
            // let expression is dangerous).
            let r = self.cache.borrow_mut().get_table(output_num);
            if let Err(e) = r {
                log!(self.opt.log, "New table can't be read: {}", e);
                return Err(e);
            }
            log!(self.opt.log,
                 "New table num={}: keys={} size={}",
                 output_num,
                 entries,
                 bytes);
        }
        Ok(())
    }

    fn install_compaction_results(&mut self, mut cs: CompactionState) -> Result<()> {
        log!(self.opt.log,
             "Compacted {} L{} files + {} L{} files => {}B",
             cs.compaction.num_inputs(0),
             cs.compaction.level(),
             cs.compaction.num_inputs(1),
             cs.compaction.level() + 1,
             cs.total_bytes);
        cs.compaction.add_input_deletions();
        let level = cs.compaction.level();
        for output in &cs.outputs {
            cs.compaction.edit().add_file(level + 1, output.clone());
        }
        self.vset.borrow_mut().log_and_apply(cs.compaction.into_edit())
    }
}

impl Drop for DB {
    fn drop(&mut self) {
        self.release_lock().is_ok();
    }
}

struct CompactionState {
    compaction: Compaction,
    smallest_seq: SequenceNumber,
    outputs: Vec<FileMetaData>,
    builder: Option<TableBuilder<Box<Write>>>,
    total_bytes: usize,
}

impl CompactionState {
    fn new(c: Compaction) -> CompactionState {
        CompactionState {
            compaction: c,
            smallest_seq: 0,
            outputs: vec![],
            builder: None,
            total_bytes: 0,
        }
    }

    fn current_output(&mut self) -> &mut FileMetaData {
        let len = self.outputs.len();
        &mut self.outputs[len - 1]
    }

    /// cleanup cleans up after an aborted compaction.
    fn cleanup(&mut self, env: &Box<Env>, name: &str) {
        for o in self.outputs.drain(..) {
            let name = table_file_name(name, o.num);
            env.delete(Path::new(&name)).is_ok();
        }
    }
}

#[derive(Debug, Default)]
struct CompactionStats {
    micros: u64,
    read: usize,
    written: usize,
}

impl CompactionStats {
    fn add(&mut self, cs: CompactionStats) {
        self.micros += cs.micros;
        self.read += cs.read;
        self.written += cs.written;
    }
}

pub fn build_table<I: LdbIterator>(dbname: &str,
                                   opt: &Options,
                                   mut from: I,
                                   num: FileNum)
                                   -> Result<FileMetaData> {
    from.reset();
    let filename = table_file_name(dbname, num);

    let (mut kbuf, mut vbuf) = (vec![], vec![]);
    let mut firstkey = None;
    // lastkey is what remains in kbuf.

    // Clean up file if write fails at any point.
    //
    // TODO: Replace with catch {} when available.
    let r = (|| -> Result<()> {
        let f = opt.env.open_writable_file(Path::new(&filename))?;
        let f = io::BufWriter::new(f);
        let mut builder = TableBuilder::new(opt.clone(), f);
        while from.advance() {
            assert!(from.current(&mut kbuf, &mut vbuf));
            if firstkey.is_none() {
                firstkey = Some(kbuf.clone());
            }
            builder.add(&kbuf, &vbuf)?;
        }
        builder.finish()?;
        Ok(())
    })();

    if let Err(e) = r {
        opt.env.delete(Path::new(&filename)).is_ok();
        return Err(e);
    }

    let mut md = FileMetaData::default();
    if firstkey.is_none() {
        opt.env.delete(Path::new(&filename)).is_ok();
    } else {
        md.num = num;
        md.size = opt.env.size_of(Path::new(&filename))?;
        md.smallest = firstkey.unwrap();
        md.largest = kbuf;
    }
    Ok(md)
}

fn log_file_name(db: &str, num: FileNum) -> String {
    format!("{}/{:06}.log", db, num)
}

fn lock_file_name(db: &str) -> String {
    format!("{}/LOCK", db)
}

/// open_info_log opens an info log file in the given database. It transparently returns a
/// /dev/null logger in case the open fails.
fn open_info_log<E: Env + ?Sized>(env: &E, db: &str) -> Logger {
    let logfilename = format!("{}/LOG", db);
    let oldlogfilename = format!("{}/LOG.old", db);
    env.mkdir(Path::new(db)).is_ok();
    if let Ok(e) = env.exists(Path::new(&logfilename)) {
        if e {
            env.rename(Path::new(&logfilename), Path::new(&oldlogfilename)).is_ok();
        }
    }
    if let Ok(w) = env.open_writable_file(Path::new(&logfilename)) {
        Logger(w)
    } else {
        Logger(Box::new(io::sink()))
    }
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    use options;
    use key_types::LookupKey;
    use mem_env::MemEnv;
    use test_util::LdbIteratorIter;
    use version::testutil::make_version;

    /// build_db creates a database filled with the tables created by make_version().
    pub fn build_db() -> DB {
        let name = "db";
        let (v, opt) = make_version();
        let mut ve = VersionEdit::new();
        ve.set_comparator_name(opt.cmp.id());
        ve.set_log_num(0);
        // 9 files + 1 manifest we write below.
        ve.set_next_file(11);
        // 28 entries in these tables.
        ve.set_last_seq(28);

        for l in 0..NUM_LEVELS {
            for f in &v.files[l] {
                ve.add_file(l, f.borrow().clone());
            }
        }

        let manifest = manifest_file_name(name, 11);
        let manifest_file = opt.env.open_writable_file(Path::new(&manifest)).unwrap();
        let mut lw = LogWriter::new(manifest_file);
        lw.add_record(&ve.encode()).unwrap();
        lw.flush().unwrap();
        set_current_file(&opt.env, name, 11).unwrap();

        DB::open(name, opt).unwrap()
    }

    /// set_file_to_compact ensures that the specified table file will be compacted next.
    pub fn set_file_to_compact(db: &mut DB, num: FileNum) {
        let v = db.current();
        let mut v = v.borrow_mut();

        let mut ftc = None;
        for l in 0..NUM_LEVELS {
            for f in &v.files[l] {
                if f.borrow().num == num {
                    ftc = Some((f.clone(), l));
                }
            }
        }
        if let Some((f, l)) = ftc {
            v.file_to_compact = Some(f);
            v.file_to_compact_lvl = l;
        } else {
            panic!("file number not found");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testutil::*;

    use options;
    use key_types::LookupKey;
    use mem_env::MemEnv;
    use test_util::LdbIteratorIter;
    use version::testutil::make_version;

    #[test]
    fn test_db_impl_open_info_log() {
        let e = MemEnv::new();
        {
            let l = Some(share(open_info_log(&e, "abc")));
            assert!(e.exists(Path::new("abc/LOG")).unwrap());
            log!(l, "hello {}", "world");
            assert_eq!(12, e.size_of(Path::new("abc/LOG")).unwrap());
        }
        {
            let l = Some(share(open_info_log(&e, "abc")));
            assert!(e.exists(Path::new("abc/LOG.old")).unwrap());
            assert!(e.exists(Path::new("abc/LOG")).unwrap());
            assert_eq!(12, e.size_of(Path::new("abc/LOG.old")).unwrap());
            assert_eq!(0, e.size_of(Path::new("abc/LOG")).unwrap());
            log!(l, "something else");
            log!(l, "and another {}", 1);

            let mut s = String::new();
            let mut r = e.open_sequential_file(Path::new("abc/LOG")).unwrap();
            r.read_to_string(&mut s).unwrap();
            assert_eq!("something else\nand another 1\n", &s);
        }
    }

    fn build_memtable() -> MemTable {
        let mut mt = MemTable::new(options::for_test().cmp);
        let mut i = 1;
        for k in ["abc", "def", "ghi", "jkl", "mno", "aabc", "test123"].iter() {
            mt.add(i,
                   ValueType::TypeValue,
                   k.as_bytes(),
                   "looooongval".as_bytes());
            i += 1;
        }
        mt
    }

    #[test]
    fn test_db_impl_init() {
        // A sanity check for recovery and basic persistence.
        let opt = options::for_test();
        let env = opt.env.clone();

        // Several test cases with different options follow. The printlns can eventually be
        // removed.

        {
            let mut opt = opt.clone();
            opt.reuse_manifest = false;
            let _ = DB::open("otherdb", opt.clone()).unwrap();

            println!("children after: {:?}",
                     env.children(Path::new("otherdb/")).unwrap());
            assert!(env.exists(Path::new("otherdb/CURRENT")).unwrap());
            // Database is initialized and initial manifest reused.
            assert!(!env.exists(Path::new("otherdb/MANIFEST-000001")).unwrap());
            assert!(env.exists(Path::new("otherdb/MANIFEST-000002")).unwrap());
            assert!(env.exists(Path::new("otherdb/000003.log")).unwrap());
        }

        {
            let mut opt = opt.clone();
            opt.reuse_manifest = true;
            let mut db = DB::open("db", opt.clone()).unwrap();

            println!("children after: {:?}",
                     env.children(Path::new("db/")).unwrap());
            assert!(env.exists(Path::new("db/CURRENT")).unwrap());
            // Database is initialized and initial manifest reused.
            assert!(env.exists(Path::new("db/MANIFEST-000001")).unwrap());
            assert!(env.exists(Path::new("db/LOCK")).unwrap());
            assert!(env.exists(Path::new("db/000003.log")).unwrap());

            db.put("abc".as_bytes(), "def".as_bytes()).unwrap();
            db.put("abd".as_bytes(), "def".as_bytes()).unwrap();
        }

        {
            println!("children before: {:?}",
                     env.children(Path::new("db/")).unwrap());
            let mut opt = opt.clone();
            opt.reuse_manifest = false;
            let mut db = DB::open("db", opt.clone()).unwrap();

            println!("children after: {:?}",
                     env.children(Path::new("db/")).unwrap());
            // Obsolete manifest is deleted.
            assert!(!env.exists(Path::new("db/MANIFEST-000001")).unwrap());
            // New manifest is created.
            assert!(env.exists(Path::new("db/MANIFEST-000002")).unwrap());
            // Obsolete log file is deleted.
            assert!(!env.exists(Path::new("db/000003.log")).unwrap());
            // New L0 table has been added.
            assert!(env.exists(Path::new("db/000003.ldb")).unwrap());
            assert!(env.exists(Path::new("db/000004.log")).unwrap());
            // Check that entry exists and is correct. Phew, long call chain!
            let current = db.current();
            log!(opt.log, "files: {:?}", current.borrow().files);
            assert_eq!("def".as_bytes(),
                       current.borrow_mut()
                           .get(LookupKey::new("abc".as_bytes(), 1).internal_key())
                           .unwrap()
                           .unwrap()
                           .0
                           .as_slice());
            db.put("abe".as_bytes(), "def".as_bytes()).unwrap();
        }

        {
            println!("children before: {:?}",
                     env.children(Path::new("db/")).unwrap());
            // reuse_manifest above causes the old manifest to be deleted as obsolete, but no new
            // manifest is written. CURRENT becomes stale.
            let mut opt = opt.clone();
            opt.reuse_logs = true;
            let db = DB::open("db", opt).unwrap();

            println!("children after: {:?}",
                     env.children(Path::new("db/")).unwrap());
            assert!(!env.exists(Path::new("db/MANIFEST-000001")).unwrap());
            assert!(env.exists(Path::new("db/MANIFEST-000002")).unwrap());
            assert!(!env.exists(Path::new("db/MANIFEST-000005")).unwrap());
            assert!(env.exists(Path::new("db/000004.log")).unwrap());
            // 000004 should be reused, no new log file should be created.
            assert!(!env.exists(Path::new("db/000006.log")).unwrap());
            // Log is reused, so memtable should contain last written entry from above.
            assert_eq!(1, db.mem.len());
            assert_eq!("def".as_bytes(),
                       db.mem
                           .get(&LookupKey::new("abe".as_bytes(), 3))
                           .0
                           .unwrap()
                           .as_slice());
        }
    }

    #[allow(unused_variables)]
    #[test]
    fn test_db_impl_locking() {
        let opt = options::for_test();
        let db = DB::open("db", opt.clone()).unwrap();
        let want_err = Status::new(StatusCode::LockError,
                                   "database lock is held by another instance");
        assert_eq!(want_err, DB::open("db", opt.clone()).err().unwrap());
    }

    #[test]
    fn test_db_impl_build_table() {
        let mut opt = options::for_test();
        opt.block_size = 128;
        let mt = build_memtable();

        let f = build_table("db", &opt, mt.iter(), 123).unwrap();
        let path = Path::new("db/000123.ldb");

        assert_eq!(LookupKey::new("aabc".as_bytes(), 6).internal_key(),
                   f.smallest.as_slice());
        assert_eq!(LookupKey::new("test123".as_bytes(), 7).internal_key(),
                   f.largest.as_slice());
        assert_eq!(379, f.size);
        assert_eq!(123, f.num);
        assert!(opt.env.exists(path).unwrap());

        {
            // Read table back in.
            let mut tc = TableCache::new("db", opt.clone(), 100);
            let tbl = tc.get_table(123).unwrap();
            assert_eq!(mt.len(), LdbIteratorIter::wrap(&mut tbl.iter()).count());
        }

        {
            // Corrupt table; make sure it doesn't load fully.
            let mut buf = vec![];
            opt.env.open_sequential_file(path).unwrap().read_to_end(&mut buf).unwrap();
            buf[150] += 1;
            opt.env.open_writable_file(path).unwrap().write_all(&buf).unwrap();

            let mut tc = TableCache::new("db", opt.clone(), 100);
            let tbl = tc.get_table(123).unwrap();
            // The last two entries are skipped due to the corruption above.
            assert_eq!(5,
                       LdbIteratorIter::wrap(&mut tbl.iter()).map(|v| println!("{:?}", v)).count());
        }
    }

    #[allow(unused_variables)]
    #[test]
    fn test_db_impl_build_db_sanity() {
        let db = build_db();
        let env = &db.opt.env;
        let name = &db.name;

        assert!(env.exists(Path::new(&log_file_name(name, 12))).unwrap());
    }

    #[test]
    fn test_db_impl_get_from_table_with_snapshot() {
        let mut db = build_db();

        assert_eq!(28, db.vset.borrow().last_seq);

        // seq = 29
        db.put("xyy".as_bytes(), "123".as_bytes()).unwrap();
        let old_ss = db.get_snapshot();
        // seq = 30
        db.put("xyz".as_bytes(), "123".as_bytes()).unwrap();
        assert!(db.get_at(&old_ss, "xyy".as_bytes()).unwrap().is_some());
        assert!(db.get_at(&old_ss, "xyz".as_bytes()).unwrap().is_none());

        // memtable get
        assert_eq!("123".as_bytes(),
                   db.get("xyz".as_bytes()).unwrap().as_slice());
        assert!(db.get_internal(29, "xyy".as_bytes()).unwrap().is_some());
        assert!(db.get_internal(30, "xyy".as_bytes()).unwrap().is_some());

        assert!(db.get_internal(29, "xyz".as_bytes()).unwrap().is_none());
        assert!(db.get_internal(30, "xyz".as_bytes()).unwrap().is_some());

        // table get
        assert_eq!("val2".as_bytes(),
                   db.get("eab".as_bytes()).unwrap().as_slice());
        assert!(db.get_internal(3, "eab".as_bytes()).unwrap().is_none());
        assert!(db.get_internal(30, "eab".as_bytes()).unwrap().is_some());

        {
            let ss = db.get_snapshot();
            assert_eq!("val2".as_bytes(),
                       db.get_at(&ss, "eab".as_bytes()).unwrap().unwrap().as_slice());
        }
    }

    #[test]
    fn test_db_impl_compact_single_file() {
        let mut db = build_db();
        set_file_to_compact(&mut db, 4);
        db.maybe_do_compaction().unwrap();

        let env = &db.opt.env;
        let name = &db.name;
        assert!(!env.exists(Path::new(&table_file_name(name, 3))).unwrap());
        assert!(!env.exists(Path::new(&table_file_name(name, 4))).unwrap());
        assert!(!env.exists(Path::new(&table_file_name(name, 5))).unwrap());
        assert!(env.exists(Path::new(&table_file_name(name, 13))).unwrap());
    }


    #[test]
    fn test_db_impl_compaction_trivial_move() {
        let mut db = DB::open("db", options::for_test()).unwrap();

        db.put("abc".as_bytes(), "xyz".as_bytes()).unwrap();
        db.put("ab3".as_bytes(), "xyz".as_bytes()).unwrap();
        db.put("ab0".as_bytes(), "xyz".as_bytes()).unwrap();
        db.put("abz".as_bytes(), "xyz".as_bytes()).unwrap();
        assert_eq!(4, db.mem.len());
        let mut imm = MemTable::new(db.opt.cmp.clone());
        mem::swap(&mut imm, &mut db.mem);
        db.imm = Some(imm);
        db.compact_memtable().unwrap();

        println!("children after: {:?}",
                 db.opt.env.children(Path::new("db/")).unwrap());
        assert!(db.opt.env.exists(Path::new("db/000004.ldb")).unwrap());

        {
            let v = db.current();
            let mut v = v.borrow_mut();
            v.file_to_compact = Some(v.files[2][0].clone());
            v.file_to_compact_lvl = 2;
        }

        db.maybe_do_compaction().unwrap();

        {
            let v = db.current();
            let v = v.borrow_mut();
            assert_eq!(1, v.files[3].len());
        }
    }

    #[test]
    fn test_db_impl_memtable_compaction() {
        let mut opt = options::for_test();
        opt.write_buffer_size = 25;
        let mut db = DB::new("db", opt);

        // Fill up memtable.
        db.mem = build_memtable();

        // Trigger memtable compaction.
        db.make_room_for_write().unwrap();
        assert_eq!(0, db.mem.len());
        assert!(db.opt.env.exists(Path::new("db/000002.log")).unwrap());
        assert!(db.opt.env.exists(Path::new("db/000003.ldb")).unwrap());
        assert_eq!(351, db.opt.env.size_of(Path::new("db/000003.ldb")).unwrap());
        assert_eq!(7,
                   LdbIteratorIter::wrap(&mut db.cache.borrow_mut().get_table(3).unwrap().iter())
                       .count());
    }

    #[test]
    fn test_db_impl_compaction() {
        let (mut v, opt) = make_version();

        // Trigger size compaction at level 1.
        v.compaction_score = Some(2.0);
        v.compaction_level = Some(1);

        let mut db = DB::new("db", opt.clone());
        db.vset.borrow_mut().add_version(v);
        db.vset.borrow_mut().next_file_num = 10;

        db.start_compaction().unwrap();

        assert!(!opt.env.exists(Path::new("db/000003.ldb")).unwrap());
        assert!(opt.env.exists(Path::new("db/000010.ldb")).unwrap());
        assert_eq!(375, opt.env.size_of(Path::new("db/000010.ldb")).unwrap());

        let v = db.current();
        assert_eq!(0, v.borrow().files[1].len());
        assert_eq!(2, v.borrow().files[2].len());
    }

    #[test]
    fn test_db_impl_compaction_trivial() {
        let (mut v, opt) = make_version();

        let to_compact = v.files[2][0].clone();
        v.file_to_compact = Some(to_compact);
        v.file_to_compact_lvl = 2;

        let mut db = DB::new("db", opt.clone());
        db.vset.borrow_mut().add_version(v);
        db.vset.borrow_mut().next_file_num = 10;

        db.start_compaction().unwrap();

        assert!(opt.env.exists(Path::new("db/000006.ldb")).unwrap());
        assert!(!opt.env.exists(Path::new("db/000010.ldb")).unwrap());
        assert_eq!(218, opt.env.size_of(Path::new("db/000006.ldb")).unwrap());

        let v = db.current();
        assert_eq!(1, v.borrow().files[2].len());
        assert_eq!(3, v.borrow().files[3].len());
    }

    #[test]
    fn test_db_impl_compaction_state_cleanup() {
        let env: Box<Env> = Box::new(MemEnv::new());
        let name = "db";

        let stuff = "abcdefghijkl".as_bytes();
        env.open_writable_file(Path::new("db/000001.ldb")).unwrap().write_all(stuff).unwrap();
        let mut fmd = FileMetaData::default();
        fmd.num = 1;

        let mut cs = CompactionState::new(Compaction::new(&options::for_test(), 2, None));
        cs.outputs = vec![fmd];
        cs.cleanup(&env, name);

        assert!(!env.exists(Path::new("db/000001.ldb")).unwrap());
    }
}

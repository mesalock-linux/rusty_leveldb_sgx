//! db_impl contains the implementation of the database interface and high-level compaction and
//! maintenance logic.

#![allow(unused_attributes)]

use cmp::InternalKeyCmp;
use env::{Env, FileLock};
use error::{err, StatusCode, Result};
use filter::{BoxedFilterPolicy, InternalFilterPolicy};
use infolog::Logger;
use log::LogWriter;
use key_types::{parse_internal_key, ValueType};
use memtable::MemTable;
use options::Options;
use table_builder::TableBuilder;
use table_cache::{table_file_name, TableCache};
use types::{parse_file_name, share, FileMetaData, FileNum, FileType, LdbIterator,
            MAX_SEQUENCE_NUMBER, NUM_LEVELS, SequenceNumber, Shared};
use version_edit::VersionEdit;
use version_set::{Compaction, VersionSet};
use version::Version;

use std::cmp::Ordering;
use std::io::{self, Write};
use std::mem;
use std::path::Path;

/// DB contains the actual database implemenation. As opposed to the original, this implementation
/// is not concurrent (yet).
pub struct DB {
    name: String,
    lock: Option<FileLock>,

    cmp: InternalKeyCmp,
    fpol: InternalFilterPolicy<BoxedFilterPolicy>,
    opt: Options,

    mem: MemTable,
    imm: Option<MemTable>,

    log: Option<LogWriter<Box<Write>>>,
    log_num: Option<FileNum>,
    cache: Shared<TableCache>,
    vset: VersionSet,

    cstats: [CompactionStats; NUM_LEVELS],
}

impl DB {
    fn new(name: &str, mut opt: Options) -> DB {
        let cache = share(TableCache::new(&name, opt.clone(), opt.max_open_files - 10));
        let vset = VersionSet::new(&name, opt.clone(), cache.clone());

        let log = open_info_log(opt.env.as_ref().as_ref(), &name);
        opt.log = share(log);

        DB {
            name: name.to_string(),
            lock: None,
            cmp: InternalKeyCmp(opt.cmp.clone()),
            fpol: InternalFilterPolicy::new(opt.filter_policy.clone()),

            mem: MemTable::new(opt.cmp.clone()),
            imm: None,

            opt: opt,

            log: None,
            log_num: None,
            cache: cache,
            vset: vset,

            cstats: Default::default(),
        }
    }

    fn add_stats(&mut self, level: usize, cs: CompactionStats) {
        assert!(level < NUM_LEVELS);
        self.cstats[level].add(cs);
    }

    /// make_room_for_write checks if the memtable has become too large, and triggers a compaction
    /// if it's the case.
    fn make_room_for_write(&mut self) -> Result<()> {
        if self.mem.approx_mem_usage() < self.opt.write_buffer_size {
            return Ok(());
        } else {
            // Create new memtable.
            let logn = self.vset.new_file_number();
            let logf = self.opt.env.open_writable_file(Path::new(&log_file_name(&self.name, logn)));
            if logf.is_err() {
                self.vset.reuse_file_number(logn);
                logf?;
            } else {
                self.log = Some(LogWriter::new(logf.unwrap()));
                self.log_num = Some(logn);

                let mut imm = MemTable::new(self.opt.cmp.clone());
                mem::swap(&mut imm, &mut self.mem);
                self.imm = Some(imm);
                self.maybe_do_compaction();
            }

            return Ok(());
        }
    }

    /// maybe_do_compaction starts a blocking compaction if it makes sense.
    fn maybe_do_compaction(&mut self) {
        if self.imm.is_none() && !self.vset.needs_compaction() {
            return;
        }
        self.start_compaction();
    }

    /// start_compaction dispatches the different kinds of compactions depending on the current
    /// state of the database.
    fn start_compaction(&mut self) {
        // TODO (maybe): Support manual compactions.
        if self.imm.is_some() {
            if let Err(e) = self.compact_memtable() {
                log!(self.opt.log, "Error while compacting memtable: {}", e);
            }
            return;
        }

        let compaction = self.vset.pick_compaction();
        if compaction.is_none() {
            return;
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

            if let Err(e) = self.vset.log_and_apply(compaction.into_edit()) {
                log!(self.opt.log, "trivial move failed: {}", e);
            } else {
                log!(self.opt.log,
                     "Moved num={} bytes={} from L{} to L{}",
                     num,
                     size,
                     level,
                     level + 1);
                log!(self.opt.log, "Summary: {}", self.vset.current_summary());
            }
        } else {
            let state = CompactionState::new(compaction);
            if let Err(e) = self.do_compaction_work(state) {
                log!(self.opt.log, "Compaction work failed: {}", e);
            }
            self.delete_obsolete_files().is_ok();
        }
    }

    fn compact_memtable(&mut self) -> Result<()> {
        assert!(self.imm.is_some());
        let mut ve = VersionEdit::new();
        let base = self.vset.current();

        let imm = self.imm.take().unwrap();
        if let Err(e) = self.write_l0_table(&imm, &mut ve, Some(&base.borrow())) {
            self.imm = Some(imm);
            return Err(e);
        }
        ve.set_log_num(self.log_num.unwrap_or(0));
        self.vset.log_and_apply(ve)?;
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
        let num = self.vset.new_file_number();
        log!(self.opt.log, "Start write of L0 table {:06}", num);
        let fmd = build_table(&self.name, &self.opt, memt.iter(), num)?;
        log!(self.opt.log, "L0 table {:06} has {} bytes", num, fmd.size);

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

    fn do_compaction_work(&mut self, mut cs: CompactionState) -> Result<()> {
        let start_ts = self.opt.env.micros();
        log!(self.opt.log,
             "Compacting {} files at L{} and {} files at L{}",
             cs.compaction.num_inputs(0),
             cs.compaction.level(),
             cs.compaction.num_inputs(1),
             cs.compaction.level() + 1);
        assert!(self.vset.num_level_files(cs.compaction.level()) > 0);
        assert!(cs.builder.is_none());

        let mut input = self.vset.make_input_iterator(&cs.compaction);
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
                self.finish_compaction_output(&mut cs, key.clone())?;
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
                let fnum = self.vset.new_file_number();
                let mut fmd = FileMetaData::default();
                fmd.num = fnum;

                let fname = table_file_name(&self.name, fnum);
                let f = self.opt.env.open_writable_file(Path::new(&fname))?;
                cs.builder = Some(TableBuilder::new(self.opt.clone(), f));
                cs.outputs.push(fmd);
            }
            if cs.builder.as_ref().unwrap().entries() == 0 {
                cs.current_output().smallest = key.clone();
            }
            cs.builder.as_mut().unwrap().add(&key, &val)?;
            // NOTE: Adjust max file size based on level.
            if cs.builder.as_ref().unwrap().size_estimate() > self.opt.max_file_size {
                self.finish_compaction_output(&mut cs, key.clone())?;
            }

            input.advance();
        }

        if cs.builder.is_some() {
            self.finish_compaction_output(&mut cs, key)?;
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
        self.install_compaction_results(cs)?;
        log!(self.opt.log,
             "Compaction finished with {}",
             self.vset.current_summary());

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
            // Verify that table can be used.
            if let Err(e) = self.cache.borrow_mut().get_table(output_num) {
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
        self.vset.log_and_apply(cs.compaction.into_edit())
    }

    fn delete_obsolete_files(&mut self) -> Result<()> {
        let files = self.vset.live_files();
        let filenames = self.opt.env.children(Path::new(&self.name))?;

        for name in filenames {
            if let Ok((num, typ)) = parse_file_name(&name) {
                match typ {
                    FileType::Log => {
                        if num >= self.vset.log_num {
                            continue;
                        }
                    }
                    FileType::Descriptor => {
                        if num >= self.vset.manifest_num {
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
    let mut md = FileMetaData::default();

    let (mut kbuf, mut vbuf) = (vec![], vec![]);
    let mut firstkey = None;
    // lastkey is what remains in kbuf.

    // Clean up file if write fails at any point.
    //
    // TODO: Replace with catch {} when available.
    let r = (|| -> Result<()> {
        let f = opt.env.open_writable_file(Path::new(&filename))?;
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

    md.num = num;
    md.size = opt.env.size_of(Path::new(&filename))?;
    md.smallest = firstkey.unwrap();
    md.largest = kbuf;
    Ok(md)
}

fn log_file_name(db: &str, num: FileNum) -> String {
    format!("{}/{:06}.log", db, num)
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
mod tests {
    use super::*;

    use options;
    use key_types::LookupKey;
    use mem_env::MemEnv;
    use test_util::LdbIteratorIter;

    #[test]
    fn test_db_impl_open_info_log() {
        let e = MemEnv::new();
        {
            let l = share(open_info_log(&e, "abc"));
            assert!(e.exists(Path::new("abc/LOG")).unwrap());
            log!(l, "hello {}", "world");
            assert_eq!(12, e.size_of(Path::new("abc/LOG")).unwrap());
        }
        {
            let l = share(open_info_log(&e, "abc"));
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

    #[test]
    fn test_db_impl_make_room_for_write() {
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
    }
}
//!
//! VirtualFile is like a normal File, but it's not bound directly to
//! a file descriptor. Instead, the file is opened when it's read from,
//! and if too many files are open globally in the system, least-recently
//! used ones are closed.
//!
//! To track which files have been recently used, we use the clock algorithm
//! with a 'recently_used' flag on each slot.
//!
//! This is similar to PostgreSQL's virtual file descriptor facility in
//! src/backend/storage/file/fd.c
//!
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{RwLock, RwLockWriteGuard};

use once_cell::sync::OnceCell;

///
/// A virtual file descriptor. You can use this just like std::fs::File, but internally
/// the underlying file is closed if the system is low on file descriptors,
/// and re-opened when it's accessed again.
///
/// Like with std::fs::File, multiple threads can read/write the file concurrently,
/// holding just a shared reference the same VirtualFile, using the read_at() / write_at()
/// functions from the FileExt trait. But the functions from the Read/Write/Seek traits
/// require a mutable reference, because they modify the "current position".
///
/// Each VirtualFile has a physical file descriptor in the global OPEN_FILES array, at the
/// slot that 'handle points to, if the underlying file is currently open. If it's not
/// currently open, the 'handle' can still point to the slot where it was last kept. The
/// 'tag' field is used to detect whether the handle still is valid or not.
///
pub struct VirtualFile {
    /// Lazy handle to the global file descriptor cache. The slot that this points to
    /// might contain our File, or it may be empty, or it may contain a File that
    /// belongs to a different VirtualFile.
    handle: RwLock<SlotHandle>,

    /// Current file position
    pos: u64,

    /// File path and options to use to open it.
    ///
    /// Note: this only contains the options needed to re-open it. For example,
    /// if a new file is created, we only pass the create flag when it's initially
    /// opened, in the VirtualFile::create() function, and strip the flag before
    /// storing it here.
    path: PathBuf,
    open_options: OpenOptions,
}

#[derive(PartialEq, Clone, Copy)]
struct SlotHandle {
    /// Index into OPEN_FILES.slots
    index: usize,

    /// Value of 'tag' in the slot. If slot's tag doesn't match, then the slot has
    /// been recycled and no longer contains the FD for this virtual file.
    tag: u64,
}

/// OPEN_FILES is the global array that holds the physical file descriptors that
/// are currently open. Each slot in the array is protected by a separate lock,
/// so that different files can be accessed independently. The lock must be held
/// in write mode to replace the slot with a different file, but a read mode
/// is enough to operate on the file, whether you're reading or writing to it.
///
/// OPEN_FILES starts in uninitialized state, and it's initialized by
/// the virtual_file::init() function. It must be called exactly once at page
/// server startup.
static OPEN_FILES: OnceCell<OpenFiles> = OnceCell::new();

struct OpenFiles {
    slots: &'static [Slot],

    /// clock arm for the clock algorithm
    next: AtomicUsize,
}

struct Slot {
    inner: RwLock<SlotInner>,

    /// has this file been used since last clock sweep?
    recently_used: AtomicBool,
}

struct SlotInner {
    /// Counter that's incremented every time a different file is stored here.
    /// To avoid the ABA problem.
    tag: u64,

    /// the underlying file
    file: Option<File>,
}

impl OpenFiles {
    /// Find a slot to use, evicting an existing file descriptor if needed.
    ///
    /// On return, we hold a lock on the slot, and its 'tag' has been updated
    /// recently_used has been set. It's all ready for reuse.
    fn find_victim_slot(&self) -> (SlotHandle, RwLockWriteGuard<SlotInner>) {
        //
        // Run the clock algorithm to find a slot to replace.
        //
        let num_slots = self.slots.len();
        let mut retries = 0;
        let mut slot;
        let mut slot_guard;
        let index;
        loop {
            let next = self.next.fetch_add(1, Ordering::AcqRel) % num_slots;
            slot = &self.slots[next];

            // If the recently_used flag on this slot is set, continue the clock
            // sweep. Otherwise try to use this slot. If we cannot acquire the
            // lock, also continue the clock sweep.
            //
            // We only continue in this manner for a while, though. If we loop
            // through the array twice without finding a victim, just pick the
            // next slot and wait until we can reuse it. This way, we avoid
            // spinning in the extreme case that all the slots are busy with an
            // I/O operation.
            if retries < num_slots * 2 {
                if !slot.recently_used.swap(false, Ordering::Release) {
                    if let Ok(guard) = slot.inner.try_write() {
                        slot_guard = guard;
                        index = next;
                        break;
                    }
                }
                retries += 1;
            } else {
                slot_guard = slot.inner.write().unwrap();
                index = next;
                break;
            }
        }

        //
        // We now have the victim slot locked. If it was in use previously, close the
        // old file.
        //
        if let Some(old_file) = slot_guard.file.take() {
            drop(old_file);
        }

        // Prepare the slot for reuse and return it
        slot_guard.tag += 1;
        slot.recently_used.store(true, Ordering::Relaxed);
        (
            SlotHandle {
                index,
                tag: slot_guard.tag,
            },
            slot_guard,
        )
    }
}

impl VirtualFile {
    /// Open a file in read-only mode. Like File::open.
    pub fn open(path: &Path) -> Result<VirtualFile, std::io::Error> {
        Self::open_with_options(path, OpenOptions::new().read(true))
    }

    /// Create a new file for writing. If the file exists, it will be truncated.
    /// Like File::create.
    pub fn create(path: &Path) -> Result<VirtualFile, std::io::Error> {
        Self::open_with_options(
            path,
            OpenOptions::new().write(true).create(true).truncate(true),
        )
    }

    /// Open a file with given options.
    ///
    /// Note: If any custom flags were set in 'open_options' through OpenOptionsExt,
    /// they will be applied also when the file is subsequently re-opened, not only
    /// on the first time. Make sure that's sane!
    pub fn open_with_options(
        path: &Path,
        open_options: &OpenOptions,
    ) -> Result<VirtualFile, std::io::Error> {
        let (handle, mut slot_guard) = get_open_files().find_victim_slot();

        let file = open_options.open(path)?;

        // Strip all options other than read and write.
        //
        // It would perhaps be nicer to check just for the read and write flags
        // explicitly, but OpenOptions doesn't contain any functions to read flags,
        // only to set them.
        let mut reopen_options = open_options.clone();
        reopen_options.create(false);
        reopen_options.create_new(false);
        reopen_options.truncate(false);

        let vfile = VirtualFile {
            handle: RwLock::new(handle),
            pos: 0,
            path: path.to_path_buf(),
            open_options: reopen_options,
        };

        slot_guard.file.replace(file);

        Ok(vfile)
    }

    /// Call File::sync_all() on the underlying File.
    pub fn sync_all(&self) -> Result<(), Error> {
        self.with_file(|file| file.sync_all())?
    }

    /// Helper function that looks up the underlying File for this VirtualFile,
    /// opening it and evicting some other File if necessary. It calls 'func'
    /// with the physical File.
    fn with_file<F, R>(&self, mut func: F) -> Result<R, Error>
    where
        F: FnMut(&File) -> R,
    {
        let open_files = get_open_files();

        // Read the cached slot handle, and see if the slot that it points to still
        // contains our File.
        //
        // We only need to hold the lock while we read the current handle. If
        // another thread closes the file and recycles the slot for a different file,
        // we will notice that the handle we read is no longer valid and retry.
        let mut handle_guard;
        let mut handle = *self.handle.read().unwrap();
        loop {
            let slot = &open_files.slots[handle.index];
            let slot_guard = slot.inner.read().unwrap();
            if slot_guard.tag == handle.tag {
                if let Some(file) = &slot_guard.file {
                    // Found a cached file descriptor.
                    slot.recently_used.store(true, Ordering::Relaxed);
                    return Ok(func(file));
                }
            }

            // The slot didn't contain our File. Grab a write lock on handle in
            // the VirtualFile, so that no other thread will try to concurrently
            // open the same file.
            handle_guard = self.handle.write().unwrap();

            // Check if some other thread already did it while we were not
            // holding the lock.
            if *handle_guard != handle {
                handle = *handle_guard;
                continue;
            }
            break;
        }

        // We need to open the file ourselves. The handle in the VirtualFile is
        // now locked in write-mode. Find a free slot to put it in.
        let (handle, mut slot_guard) = open_files.find_victim_slot();

        // Open the physical file
        let file = self.open_options.open(&self.path)?;

        // Perform the requested operation on it
        //
        // TODO: We could downgrade the locks to read mode before calling
        // 'func', to allow a little bit more concurrency, but the standard
        // library RwLock doesn't allow downgrading without releasing the lock,
        // and that doesn't seem worth the trouble. (parking_lot RwLock would
        // allow it)
        let result = func(&file);

        // Store the File in the slot and update the handle in the VirtualFile
        // to point to it.
        slot_guard.file.replace(file);

        *handle_guard = handle;

        Ok(result)
    }
}

impl Drop for VirtualFile {
    /// If a VirtualFile is dropped, close the underlying file if it was open.
    fn drop(&mut self) {
        let handle = self.handle.get_mut().unwrap();

        // We could check with a read-lock first, to avoid waiting on an
        // unrelated I/O.
        let slot = &get_open_files().slots[handle.index];
        let mut slot_guard = slot.inner.write().unwrap();
        if slot_guard.tag == handle.tag {
            slot.recently_used.store(false, Ordering::Relaxed);
            slot_guard.file.take();
        }
    }
}

impl Read for VirtualFile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let pos = self.pos;
        let n = self.read_at(buf, pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for VirtualFile {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let pos = self.pos;
        let n = self.write_at(buf, pos)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        // flush is no-op for File (at least on unix), so we don't need to do
        // anything here either.
        Ok(())
    }
}

impl Seek for VirtualFile {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Error> {
        match pos {
            SeekFrom::Start(offset) => {
                self.pos = offset;
            }
            SeekFrom::End(offset) => {
                self.pos = self.with_file(|mut file| file.seek(SeekFrom::End(offset)))??
            }
            SeekFrom::Current(offset) => {
                let pos = self.pos as i128 + offset as i128;
                if pos < 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "offset would be negative",
                    ));
                }
                if pos > u64::MAX as i128 {
                    return Err(Error::new(ErrorKind::InvalidInput, "offset overflow"));
                }
                self.pos = pos as u64;
            }
        }
        Ok(self.pos)
    }
}

impl FileExt for VirtualFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize, Error> {
        self.with_file(|file| file.read_at(buf, offset))?
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize, Error> {
        self.with_file(|file| file.write_at(buf, offset))?
    }
}

impl OpenFiles {
    fn new(num_slots: usize) -> OpenFiles {
        let mut slots = Box::new(Vec::with_capacity(num_slots));
        for _ in 0..num_slots {
            let slot = Slot {
                recently_used: AtomicBool::new(false),
                inner: RwLock::new(SlotInner { tag: 0, file: None }),
            };
            slots.push(slot);
        }

        OpenFiles {
            next: AtomicUsize::new(0),
            slots: Box::leak(slots),
        }
    }
}

///
/// Initialize the virtual file module. This must be called once at page
/// server startup.
///
pub fn init(num_slots: usize) {
    if OPEN_FILES.set(OpenFiles::new(num_slots)).is_err() {
        panic!("virtual_file::init called twice");
    }
}

const TEST_MAX_FILE_DESCRIPTORS: usize = 10;

// Get a handle to the global slots array.
fn get_open_files() -> &'static OpenFiles {
    //
    // In unit tests, page server startup doesn't happen and no one calls
    // virtual_file::init(). Initialize it here, with a small array.
    //
    // This applies to the virtual file tests below, but all other unit
    // tests too, so the virtual file facility is always usable in
    // unit tests.
    //
    if cfg!(test) {
        OPEN_FILES.get_or_init(|| OpenFiles::new(TEST_MAX_FILE_DESCRIPTORS))
    } else {
        OPEN_FILES.get().expect("virtual_file::init not called yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::seq::SliceRandom;
    use rand::thread_rng;

    // Helper function to slurp contents of a file, starting at the current position,
    // into a string
    fn read_string<FD>(vfile: &mut FD) -> Result<String, Error>
    where
        FD: Read,
    {
        let mut buf = String::new();
        vfile.read_to_string(&mut buf)?;
        Ok(buf)
    }

    // Helper function to slurp a portion of a file into a string
    fn read_string_at<FD>(vfile: &mut FD, pos: u64, len: usize) -> Result<String, Error>
    where
        FD: FileExt,
    {
        let mut buf = Vec::new();
        buf.resize(len, 0);
        vfile.read_exact_at(&mut buf, pos)?;
        Ok(String::from_utf8(buf).unwrap())
    }

    #[test]
    fn test_virtual_files() -> Result<(), Error> {
        // The real work is done in the test_files() helper function. This
        // allows us to run the same set of tests against a native File, and
        // VirtualFile. We trust the native Files and wouldn't need to test them,
        // but this allows us to verify that the operations return the same
        // results with VirtualFiles as with native Files. (Except that with
        // native files, you will run out of file descriptors if the ulimit
        // is low enough.)
        test_files("virtual_files", |path, open_options| {
            VirtualFile::open_with_options(path, open_options)
        })
    }

    #[test]
    fn test_physical_files() -> Result<(), Error> {
        test_files("physical_files", |path, open_options| {
            open_options.open(path)
        })
    }

    fn test_files<OF, FD>(testname: &str, openfunc: OF) -> Result<(), Error>
    where
        FD: Read + Write + Seek + FileExt,
        OF: Fn(&Path, &OpenOptions) -> Result<FD, std::io::Error>,
    {
        let testdir = crate::PageServerConf::test_repo_dir(testname);
        std::fs::create_dir_all(&testdir)?;

        let path_a = testdir.join("file_a");
        let mut file_a = openfunc(
            &path_a,
            OpenOptions::new().write(true).create(true).truncate(true),
        )?;
        file_a.write_all(b"foobar")?;

        // cannot read from a file opened in write-only mode
        assert!(read_string(&mut file_a).is_err());

        // Close the file and re-open for reading
        let mut file_a = openfunc(&path_a, OpenOptions::new().read(true))?;

        // cannot write to a file opened in read-only mode
        assert!(file_a.write(b"bar").is_err());

        // Try simple read
        assert_eq!("foobar", read_string(&mut file_a)?);

        // It's positioned at the EOF now.
        assert_eq!("", read_string(&mut file_a)?);

        // Test seeks.
        assert_eq!(file_a.seek(SeekFrom::Start(1))?, 1);
        assert_eq!("oobar", read_string(&mut file_a)?);

        assert_eq!(file_a.seek(SeekFrom::End(-2))?, 4);
        assert_eq!("ar", read_string(&mut file_a)?);

        assert_eq!(file_a.seek(SeekFrom::Start(1))?, 1);
        assert_eq!(file_a.seek(SeekFrom::Current(2))?, 3);
        assert_eq!("bar", read_string(&mut file_a)?);

        assert_eq!(file_a.seek(SeekFrom::Current(-5))?, 1);
        assert_eq!("oobar", read_string(&mut file_a)?);

        // Test erroneous seeks to before byte 0
        assert!(file_a.seek(SeekFrom::End(-7)).is_err());
        assert_eq!(file_a.seek(SeekFrom::Start(1))?, 1);
        assert!(file_a.seek(SeekFrom::Current(-2)).is_err());

        // the erroneous seek should have left the position unchanged
        assert_eq!("oobar", read_string(&mut file_a)?);

        // Create another test file, and try FileExt functions on it.
        let path_b = testdir.join("file_b");
        let mut file_b = openfunc(
            &path_b,
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true),
        )?;
        file_b.write_all_at(b"BAR", 3)?;
        file_b.write_all_at(b"FOO", 0)?;

        assert_eq!(read_string_at(&mut file_b, 2, 3)?, "OBA");

        // Open a lot of files, enough to cause some evictions. (Or to be precise,
        // open the same file many times. The effect is the same.)
        //
        // leave file_a positioned at offset 1 before we start
        assert_eq!(file_a.seek(SeekFrom::Start(1))?, 1);

        let mut vfiles = Vec::new();
        for _ in 0..100 {
            let mut vfile = openfunc(&path_b, OpenOptions::new().read(true))?;
            assert_eq!("FOOBAR", read_string(&mut vfile)?);
            vfiles.push(vfile);
        }

        // make sure we opened enough files to definitely cause evictions.
        assert!(vfiles.len() > TEST_MAX_FILE_DESCRIPTORS * 2);

        // The underlying file descriptor for 'file_a' should be closed now. Try to read
        // from it again. We left the file positioned at offset 1 above.
        assert_eq!("oobar", read_string(&mut file_a)?);

        // Check that all the other FDs still work too. Use them in random order for
        // good measure.
        vfiles.as_mut_slice().shuffle(&mut thread_rng());
        for vfile in vfiles.iter_mut() {
            assert_eq!("OOBAR", read_string_at(vfile, 1, 5)?);
        }

        Ok(())
    }
}
/*! A convenient, safe, and performant API for atomic file I/O

With [`Atommap`], `open()` and `commit()` are fully atomic (and constant-time); but it only works on reflink-enabled filesystems (XFS, btrfs, bcachefs, ZFS).
[`Mmap`] works on any filesystem, but `open()` and `commit()` may not be atomic, and might be O(n) in the size of the file.

*/

use rustix::{
    fs::{AtFlags, Mode, OFlags, copy_file_range, ftruncate, ioctl_ficlone, linkat, open},
    io::Errno,
    mm::{MapFlags, MremapFlags, MsyncFlags, ProtFlags, mmap, mremap, msync, munmap},
};
use std::{ffi::c_void, fs::File, io, os::fd::AsFd, path::Path};

/// A point-in-time snapshot of a file which can be atomically written
///
/// ## Reading
///
/// Read the file contents using the `AsRef` impl.  The data you see will
/// reflect the state of the file at the time `open()` was called; writes by other
/// process are not reflected.  In other words, `Atommap` will show you a consistent
/// point-in-time snapshot of the file.
///
/// Data is not loaded eagerly into memory.  It will be read in from disk on demand.
/// For this we rely on the COW capabilities of the underlying filesystem.
///
/// ## Writing
///
/// Write the file contents using the `AsMut` impl.  Writes will be immediately
/// visible to you (when you read this `Atommap`), but will not be visible to
/// other processes reading the file until you call `commit()`.  Once you
/// call commit, all your modifications will be atomically visible to other
/// readers.
///
/// Modifications are being written back to disk all the time (asynchronously
/// by the kernel), so there may be very little I/O left to do when you actually
/// call `commit()`.  "Committing" simply makes the written changes visible
/// (after waiting for writeback to complete).
pub struct Atommap(Inner);

impl Atommap {
    /// Take an atomic snapshot of the file and map it into memory
    ///
    /// Note that changes to the snapshot will be discarded unless you call [`commit()`].
    ///
    /// This will fail with `EOPNOTSUPP` if `path` is on a filesystem which
    /// doesn't support reflinks.  For a version which works on any filesystem,
    /// see [`Mmap::open()`].
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let x = Inner::open(path, true)?;
        Ok(Self(x))
    }

    /// Atomically replace the original file with the contents of the snapshot
    pub fn commit(&mut self) -> io::Result<()> {
        self.0.commit(false)
    }

    /// Link this snapshot to the directory tree at the given path
    ///
    /// Atomic if on the same filesystem.
    pub fn link(self, path: impl AsRef<Path>) -> io::Result<()> {
        self.0.link(path)
    }

    /// Change the size of the file.  If extending, the extension is filled with zeroes.
    pub fn resize(&mut self, new_len: usize) -> io::Result<()> {
        self.0.resize(new_len)
    }
}

impl AsRef<[u8]> for Atommap {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl AsMut<[u8]> for Atommap {
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut()
    }
}

/// A snapshot of a file
///
/// The data you read might not quite be a point-in-time snapshot (writes
/// performed while the file was being opened may be partially present.)
/// Committing is not guaranteed to be atomic (concurrent writes might be
/// interleaved).
pub struct Mmap(Inner);

impl Mmap {
    /// Take a snapshot of the file and map it into memory
    ///
    /// Writes concurrent with this call may be partially present in the
    /// resulting snapshot.
    ///
    /// Note that changes to the snapshot will be discarded unless you call [`commit()`].
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let x = Inner::open(path, true)?;
        Ok(Self(x))
    }

    /// Replace the original file with the contents of the snapshot
    ///
    /// This may interleave with writes from other processes.
    pub fn commit(&mut self) -> io::Result<()> {
        self.0.commit(true)
    }

    /// Link this snapshot to the directory tree at the given path
    ///
    /// Atomic if on the same filesystem.
    pub fn link(self, path: impl AsRef<Path>) -> io::Result<()> {
        self.0.link(path)
    }

    /// Change the size of the file.  If extending, the extension is filled with zeroes.
    pub fn resize(&mut self, new_len: usize) -> io::Result<()> {
        self.0.resize(new_len)
    }
}

impl AsRef<[u8]> for Mmap {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl AsMut<[u8]> for Mmap {
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut()
    }
}

/// Returns whether it fell back
fn ficlone(fd_out: impl AsFd, fd_in: impl AsFd, len: usize, fallback: bool) -> io::Result<()> {
    match ioctl_ficlone(&fd_out, &fd_in) {
        Ok(()) => Ok(()),
        Err(Errno::OPNOTSUPP) if fallback => {
            let mut rem = len;
            while rem > 0 {
                let n = copy_file_range(&fd_in, None, &fd_out, None, rem)?;
                if n == 0 {
                    Err(io::ErrorKind::UnexpectedEof)?;
                }
                if n > rem {
                    panic!()
                }
                rem -= n;
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

struct Inner {
    original: File,
    private: File,    // Unlinked; initially a clone of `original`
    ptr: *mut c_void, // Can be null
    len: usize,       // zero iff `ptr` is null
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    fn open(path: impl AsRef<Path>, allow_fallback: bool) -> io::Result<Self> {
        let path = path.as_ref();
        let original = File::options().read(true).write(true).open(path)?;
        let len = original.metadata()?.len() as usize;
        let dir = path.parent().unwrap_or(Path::new("."));
        let private: File =
            open(dir, OFlags::TMPFILE | OFlags::RDWR, Mode::RUSR | Mode::WUSR)?.into();
        ficlone(&private, &original, len, allow_fallback)?;

        // SAFETY:
        // > If `ptr` is not null, it must be aligned...
        // `ptr` is null.
        //
        // > If there exist any Rust references referring to the memory region, or if
        // > you subsequently create a Rust reference referring to the resulting region,
        // > it is your responsibility to ensure that the Rust reference invariants are
        // > preserved, including ensuring that the memory is not mutated in a way that
        // > a Rust reference would not expect.
        //
        // I believe this is satified if the the only way to mutate the memory
        // is via this Atommap's `AsMut` impl.  The other way this memory
        // could be mutated is by modifications to the file. Since the file
        // was created with O_TMPFILE, it's impossible to create a new fd for
        // the file via the filesystem. And since we never expose our fd, it's
        // impossible to create a new fd via clone().  Therefore we hold the
        // only fd. So long as _we_ don't modify the file via that fd (which we
        // don't), the file can only be modified via the mmap. This satisfies
        // the requirements.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &private,
                0,
            )?
        };
        Ok(Self {
            original,
            private,
            ptr,
            len,
        })
    }

    fn commit(&mut self, allow_fallback: bool) -> io::Result<()> {
        unsafe {
            msync(self.ptr, self.len, MsyncFlags::SYNC)?;
        }
        ficlone(&self.original, &self.private, self.len, allow_fallback)?;
        Ok(())
    }

    // This is always atomic
    fn link(self, path: impl AsRef<Path>) -> io::Result<()> {
        linkat(
            &self.private,
            "",
            rustix::fs::CWD,
            path.as_ref(),
            AtFlags::EMPTY_PATH,
        )?;
        Ok(())
    }

    fn resize(&mut self, new_len: usize) -> io::Result<()> {
        ftruncate(&self.private, new_len as u64)?;
        unsafe {
            self.ptr = mremap(self.ptr, self.len, new_len, MremapFlags::MAYMOVE)?;
        }
        self.len = new_len;
        Ok(())
    }
}

impl AsRef<[u8]> for Inner {
    fn as_ref(&self) -> &[u8] {
        if self.len == 0 {
            &[] // core::slice::from_raw_parts rejects (null, 0)
        } else {
            unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.len) }
        }
    }
}

impl AsMut<[u8]> for Inner {
    fn as_mut(&mut self) -> &mut [u8] {
        if self.len == 0 {
            &mut [] // core::slice::from_raw_parts rejects (null, 0)
        } else {
            unsafe { core::slice::from_raw_parts_mut(self.ptr as *mut u8, self.len) }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() {
                match munmap(self.ptr, self.len) {
                    Ok(()) => (),
                    Err(e) => eprintln!("munmap failed: {e}"),
                }
            }
        }
    }
}

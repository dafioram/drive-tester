//! Sector-aligned heap buffers for unbuffered I/O.
//!
//! Windows requires buffers used with `FILE_FLAG_NO_BUFFERING` to be aligned to
//! the physical sector size. 4096 bytes satisfies 512n, 512e and 4Kn drives.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Alignment used for every I/O buffer.
pub const IO_ALIGN: usize = 4096;

/// A zero-initialised byte buffer whose start is aligned to [`IO_ALIGN`].
///
/// Sub-slices that start at index 0 (for example `&buf[..len]`) keep the
/// alignment, which is how the engines pass shorter final blocks.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// SAFETY: AlignedBuf exclusively owns its allocation, exactly like Vec<u8>.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared references only allow reads of initialised memory.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Allocates `len` zeroed bytes aligned to [`IO_ALIGN`].
    pub fn new(len: usize) -> Self {
        let size = len.max(1).div_ceil(IO_ALIGN) * IO_ALIGN;
        let layout = Layout::from_size_align(size, IO_ALIGN).expect("valid I/O buffer layout");
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        Self { ptr, len, layout }
    }

    /// Length in bytes (as requested, not the rounded allocation size).
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the buffer has zero length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: the allocation holds at least `len` initialised bytes.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with exactly this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// Reads into a buffer (aligned for unbuffered I/O) until it is full or the
/// file ends. A short read means end of file: reading again from an
/// unaligned position would fail on an unbuffered Windows handle.
pub fn read_until_eof(r: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
        if !got.is_multiple_of(IO_ALIGN) {
            break;
        }
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_aligned_and_zeroed() {
        for len in [1, 512, 4096, 1 << 20, (1 << 20) + 512] {
            let buf = AlignedBuf::new(len);
            assert_eq!(buf.as_ptr() as usize % IO_ALIGN, 0);
            assert_eq!(buf.len(), len);
            assert!(buf.iter().all(|&b| b == 0));
        }
    }
}

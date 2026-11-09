//! Small safe wrappers: owned handles, UTF-16 strings and DeviceIoControl.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// An owned Win32 handle, closed on drop.
pub struct Handle(HANDLE);

// SAFETY: Win32 file handles may be used from any thread.
unsafe impl Send for Handle {}

impl Handle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Null-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to the first null.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Opens a device or file with read/write sharing.
pub fn open(path: &str, access: u32, flags: u32) -> io::Result<Handle> {
    let w = wide(path);
    // SAFETY: `w` is null-terminated and outlives the call.
    let h = unsafe {
        CreateFileW(w.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, flags, null_mut())
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() { Err(io::Error::last_os_error()) } else { Ok(Handle(h)) }
}

/// `\\.\PhysicalDriveN`
pub fn disk_path(disk: u32) -> String {
    format!("\\\\.\\PhysicalDrive{disk}")
}

/// DeviceIoControl with raw buffers. Returns bytes returned.
///
/// # Safety
/// The pointers must be valid for the given lengths for the duration of the call.
unsafe fn ioctl_raw(
    h: &Handle,
    code: u32,
    input: *const c_void,
    in_len: usize,
    output: *mut c_void,
    out_len: usize,
) -> io::Result<u32> {
    let mut returned = 0u32;
    // SAFETY: guaranteed by the caller.
    let ok = unsafe {
        DeviceIoControl(h.raw(), code, input, in_len as u32, output, out_len as u32, &mut returned, null_mut())
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(returned) }
}

/// IOCTL with no input or output.
pub fn ioctl_none(h: &Handle, code: u32) -> io::Result<()> {
    // SAFETY: null buffers with zero lengths.
    unsafe { ioctl_raw(h, code, null(), 0, null_mut(), 0) }.map(|_| ())
}

/// IOCTL with a plain-old-data input struct and a byte output buffer.
pub fn ioctl_in_out<T: Copy>(h: &Handle, code: u32, input: &T, output: &mut [u8]) -> io::Result<u32> {
    // SAFETY: both buffers are valid for their sizes.
    unsafe { ioctl_raw(h, code, (input as *const T).cast(), size_of::<T>(), output.as_mut_ptr().cast(), output.len()) }
}

/// IOCTL with bytes in and bytes out.
pub fn ioctl_bytes(h: &Handle, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<u32> {
    // SAFETY: both buffers are valid for their sizes.
    unsafe { ioctl_raw(h, code, input.as_ptr().cast(), input.len(), output.as_mut_ptr().cast(), output.len()) }
}

/// IOCTL that uses one struct as both input and output buffer (pass-through).
pub fn ioctl_inplace<T: Copy>(h: &Handle, code: u32, buf: &mut T) -> io::Result<u32> {
    let p = (buf as *mut T).cast::<c_void>();
    // SAFETY: the same valid buffer is used for input and output.
    unsafe { ioctl_raw(h, code, p, size_of::<T>(), p, size_of::<T>()) }
}

/// IOCTL with no input and a byte output buffer.
pub fn ioctl_out(h: &Handle, code: u32, output: &mut [u8]) -> io::Result<u32> {
    // SAFETY: output is valid for its size.
    unsafe { ioctl_raw(h, code, null(), 0, output.as_mut_ptr().cast(), output.len()) }
}

pub fn u32_at(buf: &[u8], offset: usize) -> u32 {
    buf.get(offset..offset + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
}

pub fn i64_at(buf: &[u8], offset: usize) -> i64 {
    buf.get(offset..offset + 8).map(|b| i64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
}

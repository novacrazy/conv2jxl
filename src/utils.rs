use std::fs::{FileTimes, Metadata};
use std::path::Path;

/// The timestamps worth copying from a source file onto the file that replaces
/// it: modification and access time everywhere, creation time only where the
/// platform can set one (Linux has no API for it at all).
///
/// Each is collected independently, so a filesystem that cannot report one of
/// them (a Linux filesystem without `btime`, say) does not cost the others.
/// `None` only when none of them are available.
pub fn preserved_times(metadata: &Metadata) -> Option<FileTimes> {
    let mut times = FileTimes::new();
    let mut any = false;

    if let Ok(mtime) = metadata.modified() {
        times = times.set_modified(mtime);
        any = true;
    }

    if let Ok(atime) = metadata.accessed() {
        times = times.set_accessed(atime);
        any = true;
    }

    #[cfg(windows)]
    if let Ok(ctime) = metadata.created() {
        use std::os::windows::fs::FileTimesExt as _;

        times = times.set_created(ctime);
        any = true;
    }

    #[cfg(target_os = "macos")]
    if let Ok(ctime) = metadata.created() {
        use std::os::macos::fs::FileTimesExt as _;

        times = times.set_created(ctime);
        any = true;
    }

    any.then_some(times)
}

/// Set the "hidden" attribute on `path`, preserving its other attributes. A
/// no-op if it is already hidden.
///
/// Only Windows has such an attribute. Unix hides by leading dot, which would
/// mean renaming the file out from under whoever is looking for it, so there
/// this is a no-op and `--hide-truncated` simply does nothing.
#[cfg(not(windows))]
pub fn set_hidden(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

// Minimal kernel32 bindings so we don't pull in a Windows-API crate just to
// flip one file attribute.
#[cfg(windows)]
unsafe extern "system" {
    fn GetFileAttributesW(lp_file_name: *const u16) -> u32;
    fn SetFileAttributesW(lp_file_name: *const u16, dw_file_attributes: u32) -> i32;
}

#[cfg(windows)]
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
#[cfg(windows)]
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;

/// Set the Windows "hidden" attribute on `path`, preserving its other
/// attributes. A no-op if it is already hidden.
#[cfg(windows)]
pub fn set_hidden(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    // Win32 wants a NUL-terminated UTF-16 string.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };

    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(std::io::Error::last_os_error());
    }

    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        return Ok(()); // already hidden
    }

    if unsafe { SetFileAttributesW(wide.as_ptr(), attrs | FILE_ATTRIBUTE_HIDDEN) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

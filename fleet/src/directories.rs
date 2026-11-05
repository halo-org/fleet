use core::cfg_select;
use std::{fs::DirBuilder, io, path::PathBuf};

/// Creates and returns the data directory.
pub fn create_data_dir() -> io::Result<PathBuf> {
    let path = cfg_select! {
        windows => windows_data_dir(),
        target_os = "macos" => home_dir().map(|path| path.join("Library/Application Support")),
        unix => match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
            Some(path) if path.is_absolute() => Ok(path),
            _ => home_dir().map(|path| path.join(".local/share")),
        },
        _ => Err::<PathBuf, _>(io::Error::new(io::ErrorKind::Unsupported, "data directory is unavailable")),
    }?;

    let path = path.join("fleet");

    let mut directory = DirBuilder::new();
    directory.recursive(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        directory.mode(0o700);
    }

    directory.create(&path)?;

    Ok(path)
}

#[cfg(unix)]
fn home_dir() -> io::Result<PathBuf> {
    std::env::home_dir()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory is unavailable"))
}

#[cfg(windows)]
fn windows_data_dir() -> io::Result<PathBuf> {
    use core::{ptr, slice};
    use std::{ffi::OsString, os::windows::ffi::OsStringExt as _};
    use windows_sys::Win32::{
        Globalization::lstrlenW,
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
    };

    let mut path = ptr::null_mut();

    // SAFETY: The folder ID and output pointer are valid. A null token selects the current user.
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, ptr::null_mut(), &mut path) };

    let result = if result < 0 {
        Err(io::Error::other(format!(
            "could not locate Local AppData: HRESULT {result:#010x}",
        )))
    } else {
        // SAFETY: A successful call returns a null-terminated UTF-16 string.
        let length = unsafe { lstrlenW(path) };
        let length = usize::try_from(length).expect("path length is nonnegative");

        // SAFETY: The allocation contains at least length UTF-16 code units.
        let path = unsafe { slice::from_raw_parts(path, length) };

        Ok(PathBuf::from(OsString::from_wide(path)))
    };

    // SAFETY: The API allocates with the COM allocator. Freeing a null pointer is valid.
    unsafe { CoTaskMemFree(path.cast()) };

    result
}

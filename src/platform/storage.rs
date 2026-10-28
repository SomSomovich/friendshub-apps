use std::io;
use std::path::{Path, PathBuf};

const APP_DIRECTORY: &str = "FriendsHub";

/// Return the writable FriendsHub data directory for the current desktop or
/// iOS platform and create it when necessary.
///
/// Android must use [`app_data_dir_from_host`], because the host activity is
/// the source of its sandbox path. No current-working-directory fallback is
/// used: silently writing beside the executable can put account databases in
/// a read-only or shared directory.
#[cfg(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "ios",
    target_os = "linux"
))]
pub fn app_data_dir() -> io::Result<PathBuf> {
    let base = platform_data_root()?;
    ensure_application_dir(base.join(APP_DIRECTORY))
}

/// Return the writable FriendsHub data directory under a path supplied by the
/// native Android wrapper, and create it when necessary.
pub fn app_data_dir_from_host(host_data_dir: &Path) -> io::Result<PathBuf> {
    if !host_data_dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Android host data directory must be absolute",
        ));
    }
    ensure_application_dir(host_data_dir.join(APP_DIRECTORY))
}

#[cfg(target_os = "windows")]
fn platform_data_root() -> io::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(missing_platform_data_root)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn platform_data_root() -> io::Result<PathBuf> {
    home_directory().map(|home| home.join("Library").join("Application Support"))
}

#[cfg(target_os = "linux")]
fn platform_data_root() -> io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Ok(xdg);
        }
    }
    home_directory().map(|home| home.join(".local").join("share"))
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "linux"))]
fn home_directory() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(missing_platform_data_root)
}

fn ensure_application_dir(path: PathBuf) -> io::Result<PathBuf> {
    std::fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

fn missing_platform_data_root() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "operating system did not provide an absolute application-data directory",
    )
}

//! Hand an app-private downloaded attachment to a system viewer or share sheet.
//!
//! The cache is account-scoped and retained for seven days after its last
//! successful open. External viewers may read a file asynchronously, so a
//! successful handoff never deletes it immediately.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::MainWindow;

const CACHE_DIRECTORY: &str = "opened-attachments";
const CACHE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum OpenFileError {
    UnsupportedPlatform,
    InvalidPath,
    PathEncoding,
    Cache(io::Error),
    Platform(String),
}

impl fmt::Display for OpenFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("Opening downloaded files is unavailable on this platform.")
            }
            Self::InvalidPath => formatter.write_str("The downloaded file path is invalid."),
            Self::PathEncoding => formatter.write_str("The downloaded file path cannot be opened."),
            Self::Cache(error) => write!(formatter, "The private attachment cache failed: {error}"),
            Self::Platform(error) => write!(
                formatter,
                "The system file viewer could not be opened: {error}"
            ),
        }
    }
}

impl std::error::Error for OpenFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cache(error) => Some(error),
            _ => None,
        }
    }
}

/// Create a unique private destination for a successfully decrypted file.
pub fn create_download_path(data_dir: &Path, account_id: &str) -> Result<PathBuf, OpenFileError> {
    if !data_dir.is_absolute() || account_id.is_empty() {
        return Err(OpenFileError::InvalidPath);
    }
    let account_dir = account_cache_dir(data_dir, account_id);
    create_private_dir(&account_dir).map_err(OpenFileError::Cache)?;
    prune_expired_files(&account_dir);

    loop {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = account_dir.join(format!("attachment-{time:x}-{sequence:x}.bin"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(OpenFileError::Cache(error)),
        }
    }
}

/// Hand the already decrypted local path to the platform's default opener.
pub fn open_downloaded_file(path: &Path, _window: &MainWindow) -> Result<(), OpenFileError> {
    if !path.is_absolute() || !path.is_file() {
        return Err(OpenFileError::InvalidPath);
    }

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    {
        open_with_system_handler(path)
    }

    #[cfg(all(target_os = "ios", feature = "native-file-picker"))]
    {
        ios::share_file(path)
    }

    #[cfg(all(target_os = "ios", not(feature = "native-file-picker")))]
    {
        let _ = path;
        Err(OpenFileError::UnsupportedPlatform)
    }

    // Android needs a manifest-registered ContentProvider (normally AndroidX
    // FileProvider) to grant another app temporary access to an app-private
    // file. This NativeActivity build has no provider registration/package hook;
    // a file:// URI or public Downloads copy would not preserve file isolation.
    #[cfg(target_os = "android")]
    {
        let _ = path;
        Err(OpenFileError::UnsupportedPlatform)
    }

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "ios",
        target_os = "android"
    )))]
    {
        let _ = path;
        Err(OpenFileError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "windows")]
fn open_with_system_handler(path: &Path) -> Result<(), OpenFileError> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> *mut c_void;
    }

    const OPEN_VERB: &[u16] = &[b'o' as u16, b'p' as u16, b'e' as u16, b'n' as u16, 0];
    const SW_SHOWNORMAL: i32 = 1;

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: all strings are NUL-terminated UTF-16, and null optional
    // parameters select the Windows default verb/working-directory behavior.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            OPEN_VERB.as_ptr(),
            wide_path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        return Err(OpenFileError::Platform(format!(
            "Windows rejected the file-open request (ShellExecuteW code {}).",
            result as usize
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_with_system_handler(path: &Path) -> Result<(), OpenFileError> {
    std::process::Command::new("/usr/bin/open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| OpenFileError::Platform(format!("Could not start macOS `open`: {error}")))
}

#[cfg(target_os = "linux")]
fn open_with_system_handler(path: &Path) -> Result<(), OpenFileError> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| OpenFileError::Platform(format!("Could not start `xdg-open`: {error}")))
}

#[cfg(all(target_os = "ios", feature = "native-file-picker"))]
mod ios {
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_foundation::{NSArray, NSString, NSURL};
    use objc2_ui_kit::{
        UIActivityViewController, UIApplication, UISceneActivationState, UIViewController,
        UIWindowScene,
    };
    use std::path::Path;

    use super::OpenFileError;

    pub(super) fn share_file(path: &Path) -> Result<(), OpenFileError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            OpenFileError::Platform("The iOS share sheet must be opened on the main thread.".into())
        })?;
        let path = path.to_str().ok_or(OpenFileError::PathEncoding)?;

        objc2::rc::autoreleasepool(|_| {
            let presenter = presenter(mtm)?;
            if presenter.presentedViewController().is_some()
                || presenter.isBeingPresented()
                || presenter.isBeingDismissed()
            {
                return Err(OpenFileError::Platform(
                    "Another iOS system sheet is already open.".into(),
                ));
            }

            let file_url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let item = file_url.into_super().into_super();
            let items = NSArray::from_retained_slice(&[item]);
            // The activity controller and its activity-item array retain the
            // file URL after this stack frame returns. UIKit grants temporary
            // access through the share extension handoff; no file:// URL is
            // opened by another process directly.
            let activity = unsafe {
                UIActivityViewController::initWithActivityItems_applicationActivities(
                    UIActivityViewController::alloc(mtm),
                    &items,
                    None,
                )
            };

            // iPad uses a popover for activity sheets and requires a source
            // anchor. Anchoring to the current window's root view works for all
            // app layouts and avoids relying on a stale Slint widget position.
            if let Some(popover) = activity.popoverPresentationController() {
                let view = presenter.view().ok_or_else(|| {
                    OpenFileError::Platform(
                        "The iOS window has no view for anchoring the share sheet.".into(),
                    )
                })?;
                popover.setSourceView(Some(&view));
                popover.setSourceRect(view.bounds());
            }

            presenter.presentViewController_animated_completion(&activity, true, None);
            Ok(())
        })
    }

    fn presenter(mtm: MainThreadMarker) -> Result<Retained<UIViewController>, OpenFileError> {
        let app = UIApplication::sharedApplication(mtm);
        for scene in app.connectedScenes() {
            if scene.activationState() != UISceneActivationState::ForegroundActive {
                continue;
            }
            let Some(scene) = scene.downcast_ref::<UIWindowScene>() else {
                continue;
            };
            for window in scene.windows() {
                if !window.isKeyWindow() {
                    continue;
                }
                if let Some(controller) = window.rootViewController() {
                    return Ok(controller);
                }
            }
        }
        Err(OpenFileError::Platform(
            "No foreground iOS window is available for the share sheet.".into(),
        ))
    }
}

/// Remove files for an account only after that account has been safely retired.
pub fn forget_account_cache(data_dir: &Path, account_id: &str) -> io::Result<()> {
    if !data_dir.is_absolute() || account_id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected an absolute data directory and non-empty account id",
        ));
    }
    let account_dir = account_cache_dir(data_dir, account_id);
    match fs::remove_dir_all(account_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn account_cache_dir(data_dir: &Path, account_id: &str) -> PathBuf {
    let mut encoded = String::with_capacity(account_id.len() * 2);
    for byte in account_id.as_bytes() {
        use fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    data_dir.join("cache").join(CACHE_DIRECTORY).join(encoded)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        if let Some(parent) = path.parent() {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn prune_expired_files(account_dir: &Path) {
    let Ok(entries) = fs::read_dir(account_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() >= CACHE_RETENTION {
            let _ = fs::remove_file(entry.path());
        }
    }
}

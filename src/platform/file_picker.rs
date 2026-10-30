//! User-triggered native file selection and app-owned materialization.
//!
//! Call [`pick_files`] on the UI thread after showing the application window.
//! Completion runs on a worker thread; marshal UI updates through Slint's event
//! loop. Keep each [`PickedFile`] alive until its consumer finishes reading it.
//! Android uses the launcher's Storage Access Framework result bridge and
//! imports provider-owned descriptors before their temporary grants expire.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionMode {
    Single,
    Multiple,
}

#[derive(Clone, Debug)]
pub struct ExtensionFilter {
    pub label: String,
    /// Extensions without a leading dot, e.g. `png`, `jpg`, `pdf`.
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PickRequest {
    pub mode: SelectionMode,
    pub filters: Vec<ExtensionFilter>,
    /// Absolute, trusted app-owned cache directory. Files receive unique
    /// subdirectories here, so equal names never overwrite one another.
    pub cache_dir: PathBuf,
    pub title: Option<String>,
}

#[derive(Debug)]
pub enum PickOutcome {
    Cancelled,
    Selected(Vec<PickedFile>),
}

/// A complete app-owned file. Dropping it removes its private cache directory.
#[derive(Debug)]
pub struct PickedFile {
    path: PathBuf,
    file_name: OsString,
    mime_type: Option<String>,
    size_bytes: u64,
    #[cfg(feature = "native-file-picker")]
    _directory: tempfile::TempDir,
}

impl PickedFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_name(&self) -> &OsStr {
        &self.file_name
    }

    /// Platform MIME type when supplied, otherwise an extension-based hint.
    /// This is metadata, not validation of file contents.
    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

#[derive(Debug)]
pub enum PickerError {
    Disabled,
    Busy,
    WrongThread,
    InvalidRequest(&'static str),
    MissingActivityResultHook,
    UnsupportedPlatform,
    Platform(String),
    Io(std::io::Error),
}

impl fmt::Display for PickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("Native file selection is disabled."),
            Self::Busy => f.write_str("A file selection is already in progress."),
            Self::WrongThread => f.write_str("File selection requires the application UI thread."),
            Self::InvalidRequest(reason) => write!(f, "Invalid file-selection request: {reason}"),
            Self::MissingActivityResultHook => {
                f.write_str("The Android file picker bridge is not initialized.")
            }
            Self::UnsupportedPlatform => f.write_str("Native file selection is unavailable."),
            Self::Platform(reason) => write!(f, "Native file selection failed: {reason}"),
            Self::Io(error) => write!(f, "The selected file could not be imported: {error}"),
        }
    }
}

impl std::error::Error for PickerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PickerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

type Completion = Box<dyn FnOnce(Result<PickOutcome, PickerError>) + Send + 'static>;

/// Begin selection. An immediate error means no request was started and the
/// completion will not run. After successful initiation the completion runs
/// exactly once, including on cancellation or import failure. No size limit is
/// imposed; the caller decides its policy using the imported byte count.
pub fn pick_files(
    request: PickRequest,
    completion: impl FnOnce(Result<PickOutcome, PickerError>) + Send + 'static,
) -> Result<(), PickerError> {
    validate(&request)?;

    #[cfg(all(feature = "native-file-picker", target_os = "ios"))]
    {
        ios::start(request, Box::new(completion))
    }
    #[cfg(all(
        feature = "native-file-picker",
        any(target_os = "windows", target_os = "macos", target_os = "linux")
    ))]
    {
        desktop::start(request, Box::new(completion))
    }
    #[cfg(all(feature = "native-file-picker", target_os = "android"))]
    {
        android::start(request, Box::new(completion))
    }
    #[cfg(all(
        feature = "native-file-picker",
        not(any(
            target_os = "ios",
            target_os = "android",
            target_os = "windows",
            target_os = "macos",
            target_os = "linux"
        ))
    ))]
    {
        let _ = completion;
        Err(PickerError::UnsupportedPlatform)
    }
    #[cfg(not(feature = "native-file-picker"))]
    {
        let _ = completion;
        Err(PickerError::Disabled)
    }
}

/// Copy an app-generated audio file into the same private, self-cleaning cache
/// used for user-selected attachments. The caller must keep the source intact
/// until this function returns and remove it after a successful copy.
pub fn import_generated_voice_note(
    source: &Path,
    cache_dir: &Path,
) -> Result<PickedFile, PickerError> {
    if !source.is_absolute() || !cache_dir.is_absolute() {
        return Err(PickerError::InvalidRequest(
            "generated voice note and cache directory must be absolute",
        ));
    }

    #[cfg(all(
        feature = "native-file-picker",
        any(
            target_os = "ios",
            target_os = "android",
            target_os = "windows",
            target_os = "macos",
            target_os = "linux"
        )
    ))]
    {
        return import::file(source, Some("audio/ogg".to_owned()), cache_dir);
    }

    #[cfg(not(all(
        feature = "native-file-picker",
        any(
            target_os = "ios",
            target_os = "android",
            target_os = "windows",
            target_os = "macos",
            target_os = "linux"
        )
    )))]
    {
        let _ = (source, cache_dir);
        Err(PickerError::Disabled)
    }
}

fn validate(request: &PickRequest) -> Result<(), PickerError> {
    if !request.cache_dir.is_absolute() {
        return Err(PickerError::InvalidRequest(
            "cache directory must be absolute",
        ));
    }
    for filter in &request.filters {
        if filter.extensions.is_empty()
            || filter.extensions.iter().any(|extension| {
                extension.is_empty() || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        {
            return Err(PickerError::InvalidRequest(
                "filters require nonempty alphanumeric extensions without dots",
            ));
        }
    }
    Ok(())
}

#[cfg(all(
    feature = "native-file-picker",
    any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "ios",
        target_os = "android"
    )
))]
mod import {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::sync::atomic::{AtomicBool, Ordering};

    static ACTIVE: AtomicBool = AtomicBool::new(false);

    pub(super) struct ActiveRequest;

    impl ActiveRequest {
        pub(super) fn acquire() -> Result<Self, PickerError> {
            ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map(|_| Self)
                .map_err(|_| PickerError::Busy)
        }
    }

    impl Drop for ActiveRequest {
        fn drop(&mut self) {
            ACTIVE.store(false, Ordering::Release);
        }
    }

    pub(super) fn file(
        source: &Path,
        mime_type: Option<String>,
        cache_dir: &Path,
    ) -> Result<PickedFile, PickerError> {
        let file_name = source
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or(PickerError::InvalidRequest("selected file has no filename"))?
            .to_os_string();
        let mut input = File::open(source)?;
        if !input.metadata()?.is_file() {
            return Err(PickerError::InvalidRequest(
                "selection is not a regular file",
            ));
        }
        reader(&mut input, file_name, mime_type, cache_dir)
    }

    pub(super) fn reader(
        input: &mut impl std::io::Read,
        file_name: OsString,
        mime_type: Option<String>,
        cache_dir: &Path,
    ) -> Result<PickedFile, PickerError> {
        std::fs::create_dir_all(cache_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(cache_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let directory = tempfile::Builder::new()
            .prefix("selected-")
            .tempdir_in(cache_dir)?;
        let path = directory.path().join(&file_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&path)?;
        let size_bytes = std::io::copy(input, &mut output)?;
        // Errors above remove partially copied files through TempDir's guard.
        drop(output);
        Ok(PickedFile {
            mime_type: mime_type.or_else(|| {
                mime_guess::from_path(&file_name)
                    .first_raw()
                    .map(str::to_owned)
            }),
            path,
            file_name,
            size_bytes,
            _directory: directory,
        })
    }
}

#[cfg(all(
    feature = "native-file-picker",
    any(target_os = "windows", target_os = "macos", target_os = "linux")
))]
mod desktop {
    use super::*;

    pub(super) fn start(request: PickRequest, completion: Completion) -> Result<(), PickerError> {
        let active = import::ActiveRequest::acquire()?;
        std::thread::Builder::new()
            .name("native-file-picker".into())
            .spawn(move || {
                let result = select(request);
                drop(active);
                completion(result);
            })?;
        Ok(())
    }

    fn select(request: PickRequest) -> Result<PickOutcome, PickerError> {
        let mut dialog = native_dialog::DialogBuilder::file();
        if let Some(title) = request.title {
            dialog = dialog.set_title(title);
        }
        for filter in request.filters {
            dialog = dialog.add_filter(filter.label, filter.extensions);
        }
        let paths = match request.mode {
            SelectionMode::Single => dialog
                .open_single_file()
                .show()
                .map(|path| path.into_iter().collect()),
            SelectionMode::Multiple => dialog.open_multiple_file().show(),
        }
        .map_err(|error| PickerError::Platform(error.to_string()))?;
        if paths.is_empty() {
            return Ok(PickOutcome::Cancelled);
        }
        paths
            .iter()
            .map(|path| import::file(path, None, &request.cache_dir))
            .collect::<Result<Vec<_>, _>>()
            .map(PickOutcome::Selected)
    }
}

#[cfg(all(feature = "native-file-picker", target_os = "ios"))]
#[path = "file_picker/ios.rs"]
mod ios;

#[cfg(all(feature = "native-file-picker", target_os = "android"))]
#[path = "file_picker/android.rs"]
mod android;

#[cfg(all(feature = "native-file-picker", target_os = "android"))]
pub use android::{initialize as initialize_android, on_result as android_activity_result};

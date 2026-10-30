//! Hand an app-private downloaded attachment to a system viewer or share sheet.
//!
//! The cache is account-scoped and retained for seven days after its last
//! successful open. External viewers may read a file asynchronously, so a
//! successful handoff never deletes it immediately.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
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

/// Copy a generated Ogg voice note into the account-scoped cache used by
/// external players. The temporary file keeps the `.ogg` extension so the
/// platform can choose an audio-capable application.
pub fn copy_voice_preview(
    source: &Path,
    data_dir: &Path,
    account_id: &str,
) -> Result<PathBuf, OpenFileError> {
    if !source.is_absolute() || !source.is_file() {
        return Err(OpenFileError::InvalidPath);
    }
    let mut input = fs::File::open(source).map_err(OpenFileError::Cache)?;
    let mut signature = [0u8; 4];
    input
        .read_exact(&mut signature)
        .map_err(OpenFileError::Cache)?;
    if &signature != b"OggS" {
        return Err(OpenFileError::InvalidPath);
    }

    let path = create_download_path(data_dir, account_id)?;
    if let Err(error) = fs::copy(source, &path) {
        let _ = fs::remove_file(&path);
        return Err(OpenFileError::Cache(error));
    }
    let mut preview_path = path.clone();
    preview_path.set_extension("ogg");
    if preview_path.exists() {
        let _ = fs::remove_file(&path);
        return Err(OpenFileError::Cache(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "voice preview path already exists",
        )));
    }
    if let Err(error) = fs::rename(&path, &preview_path) {
        let _ = fs::remove_file(&path);
        return Err(OpenFileError::Cache(error));
    }
    Ok(preview_path)
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

    #[cfg(all(target_os = "android", feature = "native-file-picker"))]
    {
        android::open_file(path)
    }

    #[cfg(all(target_os = "android", not(feature = "native-file-picker")))]
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

/// Retain the current Android launcher for temporary attachment URI grants.
///
/// Call this after Slint has initialized its Android activity. Re-registering a
/// recreated activity from the same JVM replaces the retained activity.
///
/// # Safety
/// `java_vm` must be this process's JavaVM pointer. `activity` must be a live
/// global JNI reference owned by AndroidApp for the duration of this call.
#[cfg(target_os = "android")]
pub unsafe fn initialize_android(
    java_vm: *mut std::ffi::c_void,
    activity: *mut std::ffi::c_void,
) -> Result<(), OpenFileError> {
    #[cfg(feature = "native-file-picker")]
    {
        // SAFETY: the caller supplies the initialized AndroidApp pointers.
        unsafe { android::initialize(java_vm, activity) }
    }
    #[cfg(not(feature = "native-file-picker"))]
    {
        let _ = (java_vm, activity);
        Err(OpenFileError::UnsupportedPlatform)
    }
}

#[cfg(all(target_os = "android", feature = "native-file-picker"))]
mod android {
    use std::ffi::c_void;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use jni::objects::{GlobalRef, JClass, JObject, JValue};
    use jni::{JavaVM, errors::Error as JniError};

    use super::OpenFileError;

    const PROVIDER_CLASS: &str = "com.somuchsystem.friendshub.AttachmentProvider";
    const URI_METHOD: &str = "uriForFile";
    const URI_SIGNATURE: &str = "(Landroid/content/Context;Ljava/lang/String;)Landroid/net/Uri;";
    const FLAG_GRANT_READ_URI_PERMISSION: i32 = 1;

    struct AndroidContext {
        vm: JavaVM,
        activity: GlobalRef,
        provider: GlobalRef,
    }

    static CONTEXT: OnceLock<Mutex<AndroidContext>> = OnceLock::new();

    pub(super) unsafe fn initialize(
        java_vm: *mut c_void,
        activity: *mut c_void,
    ) -> Result<(), OpenFileError> {
        if java_vm.is_null() || activity.is_null() {
            return Err(initialization_error());
        }
        // SAFETY: the initialized AndroidApp provides the process JVM.
        let vm = unsafe { JavaVM::from_raw(java_vm.cast()) }.map_err(|_| initialization_error())?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|_| initialization_error())?;
        if env.exception_check().map_err(|_| initialization_error())? {
            return Err(initialization_error());
        }
        let result = env.with_local_frame(12, |env| {
            // SAFETY: this borrows AndroidApp's global reference; only the new
            // GlobalRefs below are owned by this service.
            let activity = unsafe { JObject::from_raw(activity.cast()) };
            let loader = env
                .call_method(
                    &activity,
                    "getClassLoader",
                    "()Ljava/lang/ClassLoader;",
                    &[],
                )?
                .l()?;
            let name = env.new_string(PROVIDER_CLASS)?;
            let class = JClass::from(
                env.call_method(
                    loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(name.as_ref())],
                )?
                .l()?,
            );
            env.get_static_method_id(&class, URI_METHOD, URI_SIGNATURE)?;
            Ok::<_, JniError>((env.new_global_ref(&activity)?, env.new_global_ref(class)?))
        });
        if result.is_err() {
            let _ = env.exception_clear();
        }
        let (activity, provider) = result.map_err(|_| initialization_error())?;
        drop(env);

        if let Some(context) = CONTEXT.get() {
            let mut context = context.lock().map_err(|_| initialization_error())?;
            if context.vm.get_java_vm_pointer() != vm.get_java_vm_pointer() {
                return Err(initialization_error());
            }
            context.activity = activity;
            context.provider = provider;
            return Ok(());
        }
        CONTEXT
            .set(Mutex::new(AndroidContext {
                vm,
                activity,
                provider,
            }))
            .map_err(|_| initialization_error())
    }

    pub(super) fn open_file(path: &Path) -> Result<(), OpenFileError> {
        let path = path.to_str().ok_or(OpenFileError::PathEncoding)?;
        let context = CONTEXT.get().ok_or_else(initialization_error)?;
        let (vm_pointer, activity, provider) = {
            let context = context.lock().map_err(|_| initialization_error())?;
            (
                context.vm.get_java_vm_pointer(),
                context.activity.clone(),
                context.provider.clone(),
            )
        };
        // SAFETY: CONTEXT retains the process JVM and both global references.
        let vm = unsafe { JavaVM::from_raw(vm_pointer) }.map_err(|_| initialization_error())?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|_| initialization_error())?;
        if env.exception_check().map_err(|_| initialization_error())? {
            return Err(initialization_error());
        }
        let result = env.with_local_frame(24, |env| {
            let path = env.new_string(path)?;
            let provider: &JClass<'_> = provider.as_obj().into();
            let uri = env
                .call_static_method(
                    provider,
                    URI_METHOD,
                    URI_SIGNATURE,
                    &[
                        JValue::Object(activity.as_obj()),
                        JValue::Object(path.as_ref()),
                    ],
                )?
                .l()?;
            if uri.is_null() {
                return Err(JniError::NullPtr("attachment content URI"));
            }
            let resolver = env
                .call_method(
                    activity.as_obj(),
                    "getContentResolver",
                    "()Landroid/content/ContentResolver;",
                    &[],
                )?
                .l()?;
            let mut mime_type = env
                .call_method(
                    resolver,
                    "getType",
                    "(Landroid/net/Uri;)Ljava/lang/String;",
                    &[JValue::Object(&uri)],
                )?
                .l()?;
            if mime_type.is_null() {
                mime_type = env.new_string("application/octet-stream")?.into();
            }
            let action = env.new_string("android.intent.action.VIEW")?;
            let intent = env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(action.as_ref())],
            )?;
            env.call_method(
                &intent,
                "setDataAndType",
                "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;",
                &[JValue::Object(&uri), JValue::Object(&mime_type)],
            )?
            .l()?;
            env.call_method(
                &intent,
                "addFlags",
                "(I)Landroid/content/Intent;",
                &[JValue::Int(FLAG_GRANT_READ_URI_PERMISSION)],
            )?
            .l()?;
            let label = env.new_string("FriendsHub attachment")?;
            let clip = env
                .call_static_method(
                    "android/content/ClipData",
                    "newRawUri",
                    "(Ljava/lang/CharSequence;Landroid/net/Uri;)Landroid/content/ClipData;",
                    &[JValue::Object(label.as_ref()), JValue::Object(&uri)],
                )?
                .l()?;
            env.call_method(
                &intent,
                "setClipData",
                "(Landroid/content/ClipData;)V",
                &[JValue::Object(&clip)],
            )?
            .v()?;
            env.call_method(
                activity.as_obj(),
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValue::Object(&intent)],
            )?
            .v()
        });
        if result.is_err() {
            // Avoid leaking file paths or Java exception data in diagnostics,
            // and leave no pending exception on the attached application thread.
            let _ = env.exception_clear();
        }
        result.map_err(|_| {
            OpenFileError::Platform("Android could not open a viewer for this attachment.".into())
        })
    }

    fn initialization_error() -> OpenFileError {
        OpenFileError::Platform("Android attachment sharing has not been initialized.".into())
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
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
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

//! Native clipboard writes for explicit UI copy actions.
//!
//! Call [`copy_text`] from the application's event-loop thread. Desktop
//! clipboard ownership is retained on that thread so Linux can serve the
//! contents until they are replaced or the application exits.

use std::fmt;

/// A clipboard failure that contains no copied text or platform exception data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardError {
    Unavailable,
    Busy,
    WrongThread,
    NotInitialized,
    WriteFailed,
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "The system clipboard is unavailable.",
            Self::Busy => "The system clipboard is busy. Try copying again.",
            Self::WrongThread => "Clipboard access requires the application UI thread.",
            Self::NotInitialized => "The system clipboard has not been initialized.",
            Self::WriteFailed => "The text could not be copied to the system clipboard.",
        })
    }
}

impl std::error::Error for ClipboardError {}

/// Copy UTF-8 text after an explicit user action.
///
/// The result reports the native write operation; it never reads clipboard
/// contents back. iOS requires its main UI thread. Desktop callers must use
/// the long-lived event-loop thread to retain Linux clipboard ownership.
pub fn copy_text(text: &str) -> Result<(), ClipboardError> {
    #[cfg(all(
        feature = "native-clipboard",
        any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux",
            target_os = "ios",
            target_os = "android"
        )
    ))]
    {
        implementation::copy_text(text)
    }

    #[cfg(not(all(
        feature = "native-clipboard",
        any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux",
            target_os = "ios",
            target_os = "android"
        )
    )))]
    {
        let _ = text;
        Err(ClipboardError::Unavailable)
    }
}

#[cfg(all(
    feature = "native-clipboard",
    any(target_os = "windows", target_os = "macos", target_os = "linux")
))]
mod implementation {
    use std::cell::RefCell;

    use super::ClipboardError;

    thread_local! {
        static CLIPBOARD: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };
    }

    pub(super) fn copy_text(text: &str) -> Result<(), ClipboardError> {
        CLIPBOARD.with(|clipboard| {
            let mut clipboard = clipboard
                .try_borrow_mut()
                .map_err(|_| ClipboardError::Busy)?;
            if clipboard.is_none() {
                *clipboard = Some(arboard::Clipboard::new().map_err(map_error)?);
            }
            clipboard
                .as_mut()
                .ok_or(ClipboardError::Unavailable)?
                .set_text(text)
                .map_err(map_error)
        })
    }

    fn map_error(error: arboard::Error) -> ClipboardError {
        match error {
            arboard::Error::ClipboardOccupied => ClipboardError::Busy,
            arboard::Error::ClipboardNotSupported => ClipboardError::Unavailable,
            _ => ClipboardError::WriteFailed,
        }
    }
}

#[cfg(all(feature = "native-clipboard", target_os = "ios"))]
mod implementation {
    use objc2::MainThreadMarker;
    use objc2_foundation::NSString;
    use objc2_ui_kit::UIPasteboard;

    use super::ClipboardError;

    pub(super) fn copy_text(text: &str) -> Result<(), ClipboardError> {
        let _main_thread = MainThreadMarker::new().ok_or(ClipboardError::WrongThread)?;
        objc2::rc::autoreleasepool(|_| {
            let text = NSString::from_str(text);
            let pasteboard = UIPasteboard::generalPasteboard();
            // SAFETY: UIKit is called only after the main-thread check. The
            // setter copies this valid NSString before the retained value drops.
            unsafe { pasteboard.setString(Some(&text)) };
        });
        Ok(())
    }
}

/// Retain an application context for Android clipboard writes.
///
/// Invoke this after Slint has initialized its Android activity. Repeated
/// registration for the same JVM and Application is allowed.
///
/// # Safety
/// `java_vm` must be the process JavaVM pointer. `activity` must be a live JNI
/// global reference to an Android Activity for the duration of this call.
#[cfg(target_os = "android")]
pub unsafe fn initialize_android(
    java_vm: *mut std::ffi::c_void,
    activity: *mut std::ffi::c_void,
) -> Result<(), ClipboardError> {
    #[cfg(feature = "native-clipboard")]
    {
        // SAFETY: the caller provides the JVM and live Activity global ref.
        unsafe { implementation::initialize(java_vm, activity) }
    }
    #[cfg(not(feature = "native-clipboard"))]
    {
        let _ = (java_vm, activity);
        Err(ClipboardError::Unavailable)
    }
}

#[cfg(all(feature = "native-clipboard", target_os = "android"))]
mod implementation {
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};

    use jni::objects::{GlobalRef, JObject, JValue};
    use jni::{JavaVM, errors::Error as JniError};

    use super::ClipboardError;

    struct AndroidClipboard {
        vm: JavaVM,
        application: GlobalRef,
    }

    static CONTEXT: OnceLock<AndroidClipboard> = OnceLock::new();
    static INITIALIZATION: Mutex<()> = Mutex::new(());

    pub(super) unsafe fn initialize(
        java_vm: *mut c_void,
        activity: *mut c_void,
    ) -> Result<(), ClipboardError> {
        if java_vm.is_null() || activity.is_null() {
            return Err(ClipboardError::NotInitialized);
        }
        let _initialization = INITIALIZATION
            .lock()
            .map_err(|_| ClipboardError::NotInitialized)?;
        // SAFETY: these pointers are supplied by the initialized AndroidApp.
        let vm = unsafe { JavaVM::from_raw(java_vm.cast()) }
            .map_err(|_| ClipboardError::NotInitialized)?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|_| ClipboardError::Unavailable)?;
        if env
            .exception_check()
            .map_err(|_| ClipboardError::Unavailable)?
        {
            return Err(ClipboardError::Unavailable);
        }
        let application = env.with_local_frame(8, |env| {
            // SAFETY: this borrows the host's live global reference; JObject
            // itself does not delete it. Only the new Application GlobalRef
            // below is owned by this service.
            let activity = unsafe { JObject::from_raw(activity.cast()) };
            let application = env
                .call_method(
                    &activity,
                    "getApplicationContext",
                    "()Landroid/content/Context;",
                    &[],
                )?
                .l()?;
            if application.is_null() {
                return Err(JniError::NullPtr("application context"));
            }
            env.new_global_ref(&application)
        });
        if application.is_err() {
            let _ = env.exception_clear();
        }
        let application = application.map_err(|_| ClipboardError::Unavailable)?;
        if let Some(existing) = CONTEXT.get() {
            if existing.vm.get_java_vm_pointer() != vm.get_java_vm_pointer() {
                return Err(ClipboardError::NotInitialized);
            }
            let same_context =
                env.is_same_object(existing.application.as_obj(), application.as_obj());
            if same_context.is_err() {
                let _ = env.exception_clear();
            }
            return if same_context.unwrap_or(false) {
                Ok(())
            } else {
                Err(ClipboardError::NotInitialized)
            };
        }
        drop(env);
        CONTEXT
            .set(AndroidClipboard { vm, application })
            .map_err(|_| ClipboardError::NotInitialized)
    }

    pub(super) fn copy_text(text: &str) -> Result<(), ClipboardError> {
        let context = CONTEXT.get().ok_or(ClipboardError::NotInitialized)?;
        let mut env = context
            .vm
            .attach_current_thread()
            .map_err(|_| ClipboardError::Unavailable)?;
        if env
            .exception_check()
            .map_err(|_| ClipboardError::Unavailable)?
        {
            return Err(ClipboardError::Unavailable);
        }
        let result = env.with_local_frame(24, |env| {
            let service = env.new_string("clipboard")?;
            let manager = env
                .call_method(
                    context.application.as_obj(),
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValue::Object(service.as_ref())],
                )?
                .l()?;
            if manager.is_null() {
                return Err(JniError::NullPtr("clipboard service"));
            }
            let label = env.new_string("FriendsHub")?;
            let value = env.new_string(text)?;
            let clip = env
                .call_static_method(
                    "android/content/ClipData",
                    "newPlainText",
                    "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                    &[
                        JValue::Object(label.as_ref()),
                        JValue::Object(value.as_ref()),
                    ],
                )?
                .l()?;
            if clip.is_null() {
                return Err(JniError::NullPtr("clipboard data"));
            }
            // Callers can copy tokens and recovery codes. The stable key also
            // works below API 33, where EXTRA_IS_SENSITIVE has no Java field.
            let extras = env.new_object("android/os/PersistableBundle", "()V", &[])?;
            let sensitive_key = env.new_string("android.content.extra.IS_SENSITIVE")?;
            env.call_method(
                &extras,
                "putBoolean",
                "(Ljava/lang/String;Z)V",
                &[JValue::Object(sensitive_key.as_ref()), JValue::Bool(1)],
            )?
            .v()?;
            let description = env
                .call_method(
                    &clip,
                    "getDescription",
                    "()Landroid/content/ClipDescription;",
                    &[],
                )?
                .l()?;
            if description.is_null() {
                return Err(JniError::NullPtr("clipboard description"));
            }
            env.call_method(
                &description,
                "setExtras",
                "(Landroid/os/PersistableBundle;)V",
                &[JValue::Object(&extras)],
            )?
            .v()?;
            env.call_method(
                &manager,
                "setPrimaryClip",
                "(Landroid/content/ClipData;)V",
                &[JValue::Object(&clip)],
            )?
            .v()
        });
        if result.is_err() {
            // JNI exceptions belong to this failed platform operation. Clear
            // them before returning to the Slint event loop without exposing
            // the Java exception or the copied text in diagnostics.
            let _ = env.exception_clear();
        }
        result.map_err(|_| ClipboardError::WriteFailed)
    }
}

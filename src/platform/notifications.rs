//! Native notification permission actions.
//!
//! Push delivery and local notification delivery are separate services. This
//! module only reports or changes an operating-system permission when the
//! current launcher exposes the required native API.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionState {
    Granted,
    Denied,
    Prompt,
    Unsupported,
}

impl PermissionState {
    pub fn as_ui_value(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Prompt => "prompt",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestOutcome {
    Started,
    AlreadyGranted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicrophonePermissionState {
    Granted,
    Prompt,
    SettingsRequired,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicrophoneRequestOutcome {
    Requested,
    AlreadyGranted,
    SettingsRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationError {
    Unsupported,
    NotInitialized,
    NativeCallFailed,
}

impl fmt::Display for NotificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "system notification permissions are unavailable on this platform",
            Self::NotInitialized => "the native notification context has not been initialized",
            Self::NativeCallFailed => "the operating system rejected the notification action",
        })
    }
}

impl std::error::Error for NotificationError {}

/// Return an OS permission state only when the platform can verify it.
pub fn permission_state() -> PermissionState {
    #[cfg(target_os = "android")]
    {
        #[cfg(feature = "native-clipboard")]
        {
            return android::permission_state().unwrap_or(PermissionState::Unsupported);
        }
        #[cfg(not(feature = "native-clipboard"))]
        {
            return PermissionState::Unsupported;
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        PermissionState::Unsupported
    }
}

/// Ask Android to show its runtime notification permission dialog.
///
/// The result is asynchronous. Callers must keep showing a pending state until
/// a later `permission_state` call confirms the grant or denial.
pub fn request_permission() -> Result<RequestOutcome, NotificationError> {
    #[cfg(target_os = "android")]
    {
        #[cfg(feature = "native-clipboard")]
        {
            return android::request_permission();
        }
    }

    Err(NotificationError::Unsupported)
}

/// Open this app's notification settings when the platform launcher supports it.
pub fn open_system_settings() -> Result<(), NotificationError> {
    #[cfg(target_os = "android")]
    {
        #[cfg(feature = "native-clipboard")]
        {
            return android::open_system_settings();
        }
    }

    Err(NotificationError::Unsupported)
}

/// Read Android's RECORD_AUDIO runtime permission without assuming a grant.
pub fn microphone_permission_state() -> MicrophonePermissionState {
    #[cfg(target_os = "android")]
    {
        #[cfg(feature = "native-clipboard")]
        {
            return android::microphone_permission_state()
                .unwrap_or(MicrophonePermissionState::Unsupported);
        }
        #[cfg(not(feature = "native-clipboard"))]
        {
            return MicrophonePermissionState::Unsupported;
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        MicrophonePermissionState::Unsupported
    }
}

/// Request Android microphone access. Call from the Slint/Activity thread.
pub fn request_microphone_permission() -> Result<MicrophoneRequestOutcome, NotificationError> {
    #[cfg(target_os = "android")]
    {
        #[cfg(feature = "native-clipboard")]
        {
            return android::request_microphone_permission();
        }
    }

    Err(NotificationError::Unsupported)
}

/// Open Android app settings so the user can enable a previously denied
/// microphone permission.
pub fn open_microphone_settings() -> Result<(), NotificationError> {
    #[cfg(target_os = "android")]
    {
        #[cfg(feature = "native-clipboard")]
        {
            return android::open_microphone_settings();
        }
    }

    Err(NotificationError::Unsupported)
}

/// Initialize the Android activity bridge after Slint has initialized its
/// native activity.
///
/// # Safety
/// `java_vm` must be the process JavaVM pointer. `activity` must be a live JNI
/// global reference owned by the Slint Android host.
#[cfg(target_os = "android")]
pub unsafe fn initialize_android(
    java_vm: *mut std::ffi::c_void,
    activity: *mut std::ffi::c_void,
) -> Result<(), NotificationError> {
    #[cfg(feature = "native-clipboard")]
    {
        // SAFETY: the caller provides the JavaVM and live Activity references.
        unsafe { android::initialize(java_vm, activity) }
    }
    #[cfg(not(feature = "native-clipboard"))]
    {
        let _ = (java_vm, activity);
        Err(NotificationError::Unsupported)
    }
}

#[cfg(all(target_os = "android", feature = "native-clipboard"))]
mod android {
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};

    use jni::JavaVM;
    use jni::errors::Error as JniError;
    use jni::objects::{GlobalRef, JObject, JValue};

    use super::{
        MicrophonePermissionState, MicrophoneRequestOutcome, NotificationError, PermissionState,
        RequestOutcome,
    };

    const NOTIFICATION_PERMISSION: &str = "android.permission.POST_NOTIFICATIONS";
    const PERMISSION_PREFERENCES: &str = "friendshub_notification_permissions";
    const REQUESTED_KEY: &str = "post_notifications_requested";
    const REQUEST_CODE: i32 = 0x4648;
    const MICROPHONE_PERMISSION: &str = "android.permission.RECORD_AUDIO";
    const MICROPHONE_PREFERENCES: &str = "friendshub_microphone_permissions";
    const MICROPHONE_REQUESTED_KEY: &str = "record_audio_requested";
    const MICROPHONE_REQUEST_CODE: i32 = 0x4649;

    struct AndroidContext {
        vm: JavaVM,
        activity: GlobalRef,
    }

    static CONTEXT: OnceLock<AndroidContext> = OnceLock::new();
    static INITIALIZATION: Mutex<()> = Mutex::new(());

    pub(super) unsafe fn initialize(
        java_vm: *mut c_void,
        activity: *mut c_void,
    ) -> Result<(), NotificationError> {
        if java_vm.is_null() || activity.is_null() {
            return Err(NotificationError::NotInitialized);
        }

        let _initialization = INITIALIZATION
            .lock()
            .map_err(|_| NotificationError::NotInitialized)?;
        // SAFETY: these pointers come from the initialized AndroidApp.
        let vm = unsafe { JavaVM::from_raw(java_vm.cast()) }
            .map_err(|_| NotificationError::NotInitialized)?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|_| NotificationError::NativeCallFailed)?;
        if env
            .exception_check()
            .map_err(|_| NotificationError::NativeCallFailed)?
        {
            return Err(NotificationError::NativeCallFailed);
        }

        let activity_ref = env.with_local_frame(8, |env| {
            // SAFETY: this borrows the host's live JNI global reference; the
            // JObject itself does not delete it.
            let activity = unsafe { JObject::from_raw(activity.cast()) };
            env.new_global_ref(activity)
        });
        if activity_ref.is_err() {
            let _ = env.exception_clear();
        }
        let activity = activity_ref.map_err(|_| NotificationError::NativeCallFailed)?;

        if let Some(existing) = CONTEXT.get() {
            if existing.vm.get_java_vm_pointer() != vm.get_java_vm_pointer() {
                return Err(NotificationError::NotInitialized);
            }
            let same_activity = env
                .is_same_object(existing.activity.as_obj(), activity.as_obj())
                .unwrap_or(false);
            return if same_activity {
                Ok(())
            } else {
                Err(NotificationError::NotInitialized)
            };
        }

        drop(env);
        CONTEXT
            .set(AndroidContext { vm, activity })
            .map_err(|_| NotificationError::NotInitialized)
    }

    pub(super) fn permission_state() -> Result<PermissionState, NotificationError> {
        let context = CONTEXT.get().ok_or(NotificationError::NotInitialized)?;
        let mut env = context
            .vm
            .attach_current_thread()
            .map_err(|_| NotificationError::NativeCallFailed)?;
        let result = env.with_local_frame(16, |env| {
            let sdk = env
                .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
                .i()?;
            if sdk < 33 {
                return Ok(if notifications_enabled(env, context.activity.as_obj())? {
                    PermissionState::Granted
                } else {
                    PermissionState::Denied
                });
            }

            let permission = env.new_string(NOTIFICATION_PERMISSION)?;
            let grant = env
                .call_method(
                    context.activity.as_obj(),
                    "checkSelfPermission",
                    "(Ljava/lang/String;)I",
                    &[JValue::Object(permission.as_ref())],
                )?
                .i()?;
            if grant != 0 {
                return Ok(if request_was_attempted(env, context.activity.as_obj())? {
                    PermissionState::Denied
                } else {
                    PermissionState::Prompt
                });
            }
            Ok(if notifications_enabled(env, context.activity.as_obj())? {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            })
        });
        clear_exception_on_error(&mut env, &result);
        result.map_err(|_| NotificationError::NativeCallFailed)
    }

    pub(super) fn request_permission() -> Result<RequestOutcome, NotificationError> {
        let context = CONTEXT.get().ok_or(NotificationError::NotInitialized)?;
        let mut env = context
            .vm
            .attach_current_thread()
            .map_err(|_| NotificationError::NativeCallFailed)?;
        let sdk = env
            .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
            .and_then(|sdk| sdk.i())
            .map_err(|_| {
                let _ = env.exception_clear();
                NotificationError::NativeCallFailed
            })?;
        if sdk < 33 {
            return Err(NotificationError::Unsupported);
        }
        let result = env.with_local_frame(16, |env| {
            let permission = env.new_string(NOTIFICATION_PERMISSION)?;
            let grant = env
                .call_method(
                    context.activity.as_obj(),
                    "checkSelfPermission",
                    "(Ljava/lang/String;)I",
                    &[JValue::Object(permission.as_ref())],
                )?
                .i()?;
            if grant == 0 {
                return Ok(RequestOutcome::AlreadyGranted);
            }

            let requested_permissions =
                env.new_object_array(1, "java/lang/String", &JObject::null())?;
            env.set_object_array_element(&requested_permissions, 0, permission)?;
            env.call_method(
                context.activity.as_obj(),
                "requestPermissions",
                "([Ljava/lang/String;I)V",
                &[
                    JValue::Object(requested_permissions.as_ref()),
                    JValue::Int(REQUEST_CODE),
                ],
            )?;
            mark_request_attempted(env, context.activity.as_obj())?;
            Ok(RequestOutcome::Started)
        });
        clear_exception_on_error(&mut env, &result);
        result.map_err(|_| NotificationError::NativeCallFailed)
    }

    pub(super) fn open_system_settings() -> Result<(), NotificationError> {
        let context = CONTEXT.get().ok_or(NotificationError::NotInitialized)?;
        let mut env = context
            .vm
            .attach_current_thread()
            .map_err(|_| NotificationError::NativeCallFailed)?;
        let result = env.with_local_frame(16, |env| {
            let sdk = env
                .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
                .i()?;
            let action = env.new_string(if sdk >= 26 {
                "android.settings.APP_NOTIFICATION_SETTINGS"
            } else {
                "android.settings.APPLICATION_DETAILS_SETTINGS"
            })?;
            let intent = env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(action.as_ref())],
            )?;
            if sdk >= 26 {
                let key = env.new_string("android.provider.extra.APP_PACKAGE")?;
                let package = env
                    .call_method(
                        context.activity.as_obj(),
                        "getPackageName",
                        "()Ljava/lang/String;",
                        &[],
                    )?
                    .l()?;
                env.call_method(
                    &intent,
                    "putExtra",
                    "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                    &[JValue::Object(key.as_ref()), JValue::Object(&package)],
                )?;
            }
            env.call_method(
                context.activity.as_obj(),
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValue::Object(&intent)],
            )?;
            Ok(())
        });
        clear_exception_on_error(&mut env, &result);
        result.map_err(|_| NotificationError::NativeCallFailed)
    }

    pub(super) fn microphone_permission_state()
    -> Result<MicrophonePermissionState, NotificationError> {
        let context = CONTEXT.get().ok_or(NotificationError::NotInitialized)?;
        let mut env = context
            .vm
            .attach_current_thread()
            .map_err(|_| NotificationError::NativeCallFailed)?;
        let result = env.with_local_frame(16, |env| {
            let sdk = env
                .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
                .i()?;
            if sdk < 23 {
                return Ok(MicrophonePermissionState::Granted);
            }
            let permission = env.new_string(MICROPHONE_PERMISSION)?;
            let grant = env
                .call_method(
                    context.activity.as_obj(),
                    "checkSelfPermission",
                    "(Ljava/lang/String;)I",
                    &[JValue::Object(permission.as_ref())],
                )?
                .i()?;
            if grant == 0 {
                return Ok(MicrophonePermissionState::Granted);
            }
            let attempted = microphone_request_was_attempted(env, context.activity.as_obj())?;
            let can_request_again =
                microphone_should_show_rationale(env, context.activity.as_obj())?;
            Ok(if attempted && !can_request_again {
                MicrophonePermissionState::SettingsRequired
            } else {
                MicrophonePermissionState::Prompt
            })
        });
        clear_exception_on_error(&mut env, &result);
        result.map_err(|_| NotificationError::NativeCallFailed)
    }

    pub(super) fn request_microphone_permission()
    -> Result<MicrophoneRequestOutcome, NotificationError> {
        let context = CONTEXT.get().ok_or(NotificationError::NotInitialized)?;
        let mut env = context
            .vm
            .attach_current_thread()
            .map_err(|_| NotificationError::NativeCallFailed)?;
        let result = env.with_local_frame(16, |env| {
            let sdk = env
                .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
                .i()?;
            if sdk < 23 {
                return Ok(MicrophoneRequestOutcome::AlreadyGranted);
            }
            let permission = env.new_string(MICROPHONE_PERMISSION)?;
            let grant = env
                .call_method(
                    context.activity.as_obj(),
                    "checkSelfPermission",
                    "(Ljava/lang/String;)I",
                    &[JValue::Object(permission.as_ref())],
                )?
                .i()?;
            if grant == 0 {
                return Ok(MicrophoneRequestOutcome::AlreadyGranted);
            }
            if microphone_request_was_attempted(env, context.activity.as_obj())?
                && !microphone_should_show_rationale(env, context.activity.as_obj())?
            {
                return Ok(MicrophoneRequestOutcome::SettingsRequired);
            }

            let requested_permissions =
                env.new_object_array(1, "java/lang/String", &JObject::null())?;
            env.set_object_array_element(&requested_permissions, 0, permission)?;
            env.call_method(
                context.activity.as_obj(),
                "requestPermissions",
                "([Ljava/lang/String;I)V",
                &[
                    JValue::Object(requested_permissions.as_ref()),
                    JValue::Int(MICROPHONE_REQUEST_CODE),
                ],
            )?;
            mark_microphone_request_attempted(env, context.activity.as_obj())?;
            Ok(MicrophoneRequestOutcome::Requested)
        });
        clear_exception_on_error(&mut env, &result);
        result.map_err(|_| NotificationError::NativeCallFailed)
    }

    pub(super) fn open_microphone_settings() -> Result<(), NotificationError> {
        let context = CONTEXT.get().ok_or(NotificationError::NotInitialized)?;
        let mut env = context
            .vm
            .attach_current_thread()
            .map_err(|_| NotificationError::NativeCallFailed)?;
        let result = env.with_local_frame(16, |env| {
            let action = env.new_string("android.settings.APPLICATION_DETAILS_SETTINGS")?;
            let intent = env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(action.as_ref())],
            )?;
            let package = env
                .call_method(
                    context.activity.as_obj(),
                    "getPackageName",
                    "()Ljava/lang/String;",
                    &[],
                )?
                .l()?;
            let scheme = env.new_string("package:")?;
            let uri_value = env
                .call_method(
                    &scheme,
                    "concat",
                    "(Ljava/lang/String;)Ljava/lang/String;",
                    &[JValue::Object(&package)],
                )?
                .l()?;
            let uri = env
                .call_static_method(
                    "android/net/Uri",
                    "parse",
                    "(Ljava/lang/String;)Landroid/net/Uri;",
                    &[JValue::Object(&uri_value)],
                )?
                .l()?;
            env.call_method(
                &intent,
                "setData",
                "(Landroid/net/Uri;)Landroid/content/Intent;",
                &[JValue::Object(&uri)],
            )?;
            env.call_method(
                context.activity.as_obj(),
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValue::Object(&intent)],
            )?;
            Ok(())
        });
        clear_exception_on_error(&mut env, &result);
        result.map_err(|_| NotificationError::NativeCallFailed)
    }

    fn notifications_enabled(
        env: &mut jni::JNIEnv<'_>,
        activity: &JObject<'_>,
    ) -> jni::errors::Result<bool> {
        let service_name = env.new_string("notification")?;
        let service = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(service_name.as_ref())],
            )?
            .l()?;
        if service.is_null() {
            return Ok(false);
        }
        env.call_method(&service, "areNotificationsEnabled", "()Z", &[])?
            .z()
    }

    fn request_was_attempted(
        env: &mut jni::JNIEnv<'_>,
        activity: &JObject<'_>,
    ) -> jni::errors::Result<bool> {
        let preferences_name = env.new_string(PERMISSION_PREFERENCES)?;
        let preferences = env
            .call_method(
                activity,
                "getSharedPreferences",
                "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
                &[JValue::Object(preferences_name.as_ref()), JValue::Int(0)],
            )?
            .l()?;
        let key = env.new_string(REQUESTED_KEY)?;
        env.call_method(
            preferences,
            "getBoolean",
            "(Ljava/lang/String;Z)Z",
            &[JValue::Object(key.as_ref()), JValue::Bool(0)],
        )?
        .z()
    }

    fn mark_request_attempted(
        env: &mut jni::JNIEnv<'_>,
        activity: &JObject<'_>,
    ) -> jni::errors::Result<()> {
        let preferences_name = env.new_string(PERMISSION_PREFERENCES)?;
        let preferences = env
            .call_method(
                activity,
                "getSharedPreferences",
                "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
                &[JValue::Object(preferences_name.as_ref()), JValue::Int(0)],
            )?
            .l()?;
        let editor = env
            .call_method(
                &preferences,
                "edit",
                "()Landroid/content/SharedPreferences$Editor;",
                &[],
            )?
            .l()?;
        let key = env.new_string(REQUESTED_KEY)?;
        env.call_method(
            &editor,
            "putBoolean",
            "(Ljava/lang/String;Z)Landroid/content/SharedPreferences$Editor;",
            &[JValue::Object(key.as_ref()), JValue::Bool(1)],
        )?;
        env.call_method(&editor, "apply", "()V", &[])?;
        Ok(())
    }

    fn microphone_request_was_attempted(
        env: &mut jni::JNIEnv<'_>,
        activity: &JObject<'_>,
    ) -> jni::errors::Result<bool> {
        let preferences_name = env.new_string(MICROPHONE_PREFERENCES)?;
        let preferences = env
            .call_method(
                activity,
                "getSharedPreferences",
                "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
                &[JValue::Object(preferences_name.as_ref()), JValue::Int(0)],
            )?
            .l()?;
        let key = env.new_string(MICROPHONE_REQUESTED_KEY)?;
        env.call_method(
            preferences,
            "getBoolean",
            "(Ljava/lang/String;Z)Z",
            &[JValue::Object(key.as_ref()), JValue::Bool(0)],
        )?
        .z()
    }

    fn microphone_should_show_rationale(
        env: &mut jni::JNIEnv<'_>,
        activity: &JObject<'_>,
    ) -> jni::errors::Result<bool> {
        let permission = env.new_string(MICROPHONE_PERMISSION)?;
        env.call_method(
            activity,
            "shouldShowRequestPermissionRationale",
            "(Ljava/lang/String;)Z",
            &[JValue::Object(permission.as_ref())],
        )?
        .z()
    }

    fn mark_microphone_request_attempted(
        env: &mut jni::JNIEnv<'_>,
        activity: &JObject<'_>,
    ) -> jni::errors::Result<()> {
        let preferences_name = env.new_string(MICROPHONE_PREFERENCES)?;
        let preferences = env
            .call_method(
                activity,
                "getSharedPreferences",
                "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
                &[JValue::Object(preferences_name.as_ref()), JValue::Int(0)],
            )?
            .l()?;
        let editor = env
            .call_method(
                &preferences,
                "edit",
                "()Landroid/content/SharedPreferences$Editor;",
                &[],
            )?
            .l()?;
        let key = env.new_string(MICROPHONE_REQUESTED_KEY)?;
        env.call_method(
            &editor,
            "putBoolean",
            "(Ljava/lang/String;Z)Landroid/content/SharedPreferences$Editor;",
            &[JValue::Object(key.as_ref()), JValue::Bool(1)],
        )?;
        env.call_method(&editor, "apply", "()V", &[])?;
        Ok(())
    }

    fn clear_exception_on_error<T>(env: &mut jni::JNIEnv<'_>, result: &jni::errors::Result<T>) {
        if result.is_err() {
            let _ = env.exception_clear();
        }
    }
}

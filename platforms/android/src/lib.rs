#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    let app_for_data_dir = app.clone();
    if let Err(error) = slint::android::init(app) {
        eprintln!("Failed to initialize Slint on Android: {error}");
        return;
    }

    // SAFETY: AndroidApp owns these JVM/Activity pointers, and the clipboard
    // service retains its own Application global reference during this call.
    if let Err(error) = unsafe {
        friendshub_apps::platform::clipboard::initialize_android(
            app_for_data_dir.vm_as_ptr(),
            app_for_data_dir.activity_as_ptr(),
        )
    } {
        eprintln!("Failed to initialize Android clipboard: {error}");
    }

    // SAFETY: AndroidApp keeps the JVM/Activity pointers live during this call;
    // the picker keeps its own global reference to the launcher subclass.
    if let Err(error) = unsafe {
        friendshub_apps::platform::file_picker::initialize_android(
            app_for_data_dir.vm_as_ptr(),
            app_for_data_dir.activity_as_ptr(),
        )
    } {
        eprintln!("Failed to initialize Android file picker: {error}");
    }

    // SAFETY: AndroidApp owns these JVM/Activity pointers, and the attachment
    // provider bridge retains its own global references for ACTION_VIEW grants.
    if let Err(error) = unsafe {
        friendshub_apps::platform::open_file::initialize_android(
            app_for_data_dir.vm_as_ptr(),
            app_for_data_dir.activity_as_ptr(),
        )
    } {
        eprintln!("Failed to initialize Android attachment sharing: {error}");
    }

    // SAFETY: AndroidApp owns these JVM/Activity pointers, and the
    // notifications module retains a global activity reference for settings
    // permission and intent operations.
    if let Err(error) = unsafe {
        friendshub_apps::platform::notifications::initialize_android(
            app_for_data_dir.vm_as_ptr(),
            app_for_data_dir.activity_as_ptr(),
        )
    } {
        eprintln!("Failed to initialize Android notification settings: {error}");
    }

    let status = friendshub_core::ffi::fh_android_init_from_ndk_context();
    if status != 0 {
        eprintln!("Failed to initialize FriendsHub core Android context (status {status})");
        return;
    }

    let Some(data_dir) = app_for_data_dir.internal_data_path() else {
        eprintln!("Failed to get Android app internal data path");
        return;
    };

    if let Err(error) = friendshub_apps::run_with_data_dir(data_dir) {
        eprintln!("Failed to start FriendsHub on Android: {error}");
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_somuchsystem_friendshub_FriendsHubActivity_nativeOnFilePickerResult(
    env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    request_id: jni::sys::jlong,
    status: jni::sys::jint,
    names: jni::objects::JObjectArray<'_>,
    mime_types: jni::objects::JObjectArray<'_>,
    descriptors: jni::objects::JIntArray<'_>,
    message: jni::objects::JString<'_>,
) {
    friendshub_apps::platform::file_picker::android_activity_result(
        env,
        request_id,
        status,
        names,
        mime_types,
        descriptors,
        message,
    );
}

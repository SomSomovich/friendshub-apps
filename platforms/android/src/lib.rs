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

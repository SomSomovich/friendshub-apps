#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    if let Err(error) = slint::android::init(app) {
        eprintln!("Failed to initialize Slint on Android: {error}");
        return;
    }

    if let Err(error) = friendshub_apps::run() {
        eprintln!("Failed to start FriendsHub on Android: {error}");
    }
}

pub mod app;
pub mod core;
pub mod features;
pub mod platform;

slint::include_modules!();

use std::error::Error;
use std::path::PathBuf;

use app::auth::AuthController;
use app::community as community_controller;
use app::settings as settings_controller;
use app::{AppContext, navigate};
use platform::accounts::AccountStore;

/// Start the desktop or iOS application using the platform application-data directory.
pub fn run() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "android")]
    {
        Err("Android must provide its activity data directory".into())
    }

    #[cfg(not(target_os = "android"))]
    {
        run_with_data_dir(platform::storage::app_data_dir()?)
    }
}

/// Start FriendsHub using a platform-provided application-data directory.
///
/// Android passes `ApplicationInfo.dataDir`; other platforms pass the
/// directory returned by [`platform::storage::app_data_dir`].
pub fn run_with_data_dir(data_dir: PathBuf) -> Result<(), Box<dyn Error>> {
    let data_dir = prepare_data_dir(data_dir)?;
    let accounts = AccountStore::new(&data_dir)?;
    let main_window = MainWindow::new()?;
    let context = AppContext::new(main_window.as_weak(), data_dir, accounts, None);
    let auth = AuthController::new(context.clone());

    settings_controller::initialize(&context);

    {
        let context = context.clone();
        let auth = auth.clone();
        main_window.on_navigate_requested(move |path| {
            let path = path.to_string();
            if path == "/login" || path == "/" {
                auth.cancel_twofactor();
            }
            navigate(&context, &path);
        });
    }
    {
        let auth = auth.clone();
        main_window.on_login_requested(move |fh_number, password| {
            auth.login(fh_number.to_string(), password.to_string());
        });
    }
    {
        let auth = auth.clone();
        main_window.on_register_requested(move |password| {
            auth.register(password.to_string());
        });
    }
    {
        let auth = auth.clone();
        main_window.on_twofactor_submit_requested(move |code, backup_mode| {
            auth.submit_twofactor(code.to_string(), backup_mode);
        });
    }
    {
        let context = context.clone();
        main_window.on_dating_profile_save_requested(move |draft| {
            let _ = community_controller::dating_profile_save_requested(&context, draft);
        });
    }
    {
        let context = context.clone();
        main_window.on_dating_search_requested(move |filters| {
            let _ = community_controller::dating_search_requested(&context, filters);
        });
    }
    {
        let context = context.clone();
        main_window.on_create_group_requested(
            move |title, description, handle, is_public, members| {
                let _ = community_controller::create_group_requested(
                    &context,
                    title,
                    description,
                    handle,
                    is_public,
                    members,
                );
            },
        );
    }
    {
        let context = context.clone();
        main_window.on_create_channel_requested(
            move |title,
                  description,
                  handle,
                  is_public,
                  discussion_kind,
                  discussion_title,
                  discussion_handle,
                  selected_group_id| {
                let _ = community_controller::create_channel_requested(
                    &context,
                    title,
                    description,
                    handle,
                    is_public,
                    discussion_kind,
                    discussion_title,
                    discussion_handle,
                    selected_group_id,
                );
            },
        );
    }
    {
        let context = context.clone();
        let auth = auth.clone();
        main_window.on_ui_action(move |namespace, action, arg1, arg2, arg3| {
            dispatch_ui_action(
                &context,
                &auth,
                namespace.as_str(),
                action.as_str(),
                arg1.as_str(),
                arg2.as_str(),
                arg3.as_str(),
            );
        });
    }

    auth.bootstrap();
    main_window.run()?;
    Ok(())
}

fn prepare_data_dir(data_dir: PathBuf) -> std::io::Result<PathBuf> {
    #[cfg(target_os = "android")]
    {
        platform::storage::app_data_dir_from_host(&data_dir)
    }

    #[cfg(not(target_os = "android"))]
    {
        if !data_dir.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "application data directory must be absolute",
            ));
        }
        std::fs::create_dir_all(&data_dir)?;
        Ok(data_dir)
    }
}

fn dispatch_ui_action(
    context: &AppContext,
    auth: &AuthController,
    namespace: &str,
    action: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
) {
    if namespace == "landing" {
        match action {
            "theme-toggle" => {
                if let Some(window) = context.ui.upgrade() {
                    let next = if window.get_light_mode() {
                        "dark"
                    } else {
                        "light"
                    };
                    let _ = settings_controller::handle(
                        context,
                        "appearance",
                        "set-theme",
                        next,
                        "",
                        "",
                    );
                }
                return;
            }
            "language-toggle" => {
                if let Some(window) = context.ui.upgrade() {
                    let next = if window.get_language() == "ru" {
                        "en"
                    } else {
                        "ru"
                    };
                    let _ = settings_controller::handle(
                        context,
                        "appearance",
                        "set-language",
                        next,
                        "",
                        "",
                    );
                }
                return;
            }
            _ => {}
        }
    }

    if namespace == "auth" && action == "logout" {
        auth.logout();
        return;
    }
    if namespace == "connect" {
        match action {
            "retry" => auth.bootstrap(),
            "continue-offline" => {
                if context.active_session().is_some() {
                    navigate(context, "/app");
                }
            }
            _ => {}
        }
        return;
    }

    let handled = app::chat::handle(context, namespace, action, arg1, arg2, arg3)
        || app::community::handle(context, namespace, action, arg1, arg2, arg3)
        || app::settings::handle(context, namespace, action, arg1, arg2, arg3)
        || app::media::handle(context, namespace, action, arg1, arg2, arg3);
    if !handled {
        // Arguments can contain passwords, TOTP codes, bot tokens, or message
        // plaintext. Keep diagnostic output limited to the static action name.
        eprintln!("No native handler for UI action {namespace}/{action}");
    }
}

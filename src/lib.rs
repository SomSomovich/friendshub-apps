slint::include_modules!();

pub fn run() -> Result<(), slint::PlatformError> {
    let main_window = MainWindow::new()?;
    main_window.run()
}

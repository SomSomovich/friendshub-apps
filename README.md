# Нативные приложения FriendsHub

Общее приложение и пользовательский интерфейс создаются на Rust и Slint. Платформенные пакеты в `platforms/` используют одну библиотеку `friendshub-apps`, поэтому UI и интеграция с `friendshub-core` не дублируются.

## Структура

```text
friendshub-apps/
├── Cargo.toml                 # корневой пакет и workspace
├── Cargo.lock                 # зафиксированные версии и ревизии Git-зависимостей
├── build.rs                   # компиляция Slint UI
├── src/lib.rs                 # общий запуск приложения
├── ui/main-window.slint       # общий Slint-интерфейс
└── platforms/
    ├── windows/               # Windows entry point
    ├── macos/                 # macOS entry point
    ├── ios/                   # iOS entry point
    ├── android/               # Android cdylib entry point
    └── linux/                 # Linux entry point
```

`friendshub-core` подключается непосредственно из `https://github.com/SomSomovich/friendshub-core`. Cargo сохраняет разрешённую Git-ревизию в `Cargo.lock`; сборка с `--locked` использует именно её.

## Запуск и проверка на Linux

```sh
cargo run -p friendshub-linux
cargo check --workspace
```

Окно пока пустое и имеет заголовок FriendsHub. Продуктовые экраны не входят в этот начальный каркас.

## Сборка под целевые ОС

Перед cross-compilation установите Rust target и системный toolchain для целевой ОС. Команды ниже показывают Cargo-пакет и target triple; их выполнение само по себе не создаёт установщик или пакет для магазина приложений.

```sh
cargo build -p friendshub-windows --target x86_64-pc-windows-msvc
cargo build -p friendshub-macos --target aarch64-apple-darwin
cargo build -p friendshub-ios --target aarch64-apple-ios
cargo build -p friendshub-android --target aarch64-linux-android
cargo build -p friendshub-linux --target x86_64-unknown-linux-gnu
```

Для Windows нужен MSVC toolchain; для macOS и iOS — macOS с Apple toolchain, а для iOS также Xcode. Для Android нужны Android NDK/Clang и соответствующий Rust target. Для Linux необходимы библиотеки оконной системы, используемой Slint backend. Xcode project генерируется из `platforms/ios/project.yml`.

Для iOS используется Winit backend со Skia renderer; соответствующая feature включена только для `target_os = "ios"`.

## Сборки GitHub Actions

Workflow [`.github/workflows/release.yml`](.github/workflows/release.yml) собирает приложения на GitHub-hosted runners при push, pull request и ручном запуске. Он проверяет Linux x64/arm64, Windows x64/arm64, macOS Intel/Apple Silicon, iOS arm64 для устройства и симулятора, а также Android `arm64-v8a`, `armeabi-v7a` и `x86_64`. Push тега `v*` дополнительно публикует собранные архивы в GitHub Release.

Workflow собирает исполняемые файлы Windows, macOS и Linux, arm64 executable для iOS Simulator, неподписанный arm64 iOS device IPA и Android APK для `arm64-v8a`, `armeabi-v7a` и `x86_64`.

### Подпись Android APK

На `pull_request` собирается APK с временной debug-подписью. На остальных запусках workflow использует release-подпись, если настроены GitHub Actions Secrets; тег `v*` завершается ошибкой без release-ключа и не публикует debug APK как релиз.

Созданный локальный ключ хранится в игнорируемом `.superpowers/signing/`. Пароль сохранён в `friendshub-android-release.txt` с ограниченными правами доступа. Не добавляйте файлы из этого каталога в Git.

В настройках репозитория `SomSomovich/friendshub-apps` добавьте Actions Secrets:

- `ANDROID_RELEASE_KEYSTORE_B64` — Base64 содержимого `friendshub-android-release.p12`;
- `ANDROID_RELEASE_KEYSTORE_PASSWORD` — пароль keystore и ключа.

В Linux Base64 можно получить командой `base64 -w 0 .superpowers/signing/friendshub-android-release.p12`; в macOS — `base64 .superpowers/signing/friendshub-android-release.p12 | tr -d '\n'`.

На pull request release-ключ не передаётся шагу сборки. Для tag-релиза workflow требует release secrets и проверяет APK командой `apksigner verify`.

### iOS IPA

Android использует applicationId `com.somuchsystem.friendshub`. Workflow создаёт `friendshub-ios-unsigned.ipa` для arm64 устройства с Bundle ID `com.somuch-system.friendshub`; сборка CI отключает code signing. Xcode project описан в `platforms/ios/project.yml` и открывается после генерации через `xcodegen generate`. Для симулятора отдельная matrix job сохраняет arm64 executable.

//! UIKit's document picker imports provider documents before delivering URLs.
//! We retain each URL and its security scope until our worker finishes copying.

use super::*;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSString, NSURL, NSURLContentTypeKey};
use objc2_ui_kit::{
    UIApplication, UIDocumentPickerDelegate, UIDocumentPickerViewController,
    UISceneActivationState, UIViewController, UIWindowScene,
};
use objc2_uniform_type_identifiers::UTType;
use std::cell::RefCell;
use std::sync::mpsc::{SyncSender, sync_channel};

type Selection = Result<Vec<ScopedUrl>, PickerError>;

struct DelegateState {
    sender: RefCell<Option<SyncSender<Selection>>>,
}

define_class!(
    // NSObject has no additional subclassing requirements. All UIKit calls and
    // delegate access are confined to the main thread by MainThreadOnly.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateState]
    struct PickerDelegate;

    unsafe impl NSObjectProtocol for PickerDelegate {}

    unsafe impl UIDocumentPickerDelegate for PickerDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn picked(&self, _controller: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            let result = if urls.is_empty() {
                Err(PickerError::Platform(
                    "The document picker returned no file URLs.".into(),
                ))
            } else {
                urls.iter().map(ScopedUrl::new).collect()
            };
            self.finish(result);
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn cancelled(&self, _controller: &UIDocumentPickerViewController) {
            self.finish(Ok(Vec::new()));
        }
    }
);

struct ActivePicker {
    _controller: Retained<UIDocumentPickerViewController>,
    _delegate: Retained<PickerDelegate>,
}

thread_local! {
    // UIDocumentPickerViewController retains its delegate weakly.
    static PICKER: RefCell<Option<ActivePicker>> = const { RefCell::new(None) };
}

impl PickerDelegate {
    fn finish(&self, result: Selection) {
        // Retain this delegate through the end of the method, including removing
        // the thread-local owner. UIKit may have only its weak delegate pointer.
        let _keep_alive = PICKER.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|picker| picker._delegate.clone())
        });
        if let Some(sender) = self.ivars().sender.borrow_mut().take() {
            let _ = sender.send(result);
        }
        PICKER.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

struct ScopedUrl {
    url: Retained<NSURL>,
    scoped: bool,
}

impl ScopedUrl {
    fn new(url: Retained<NSURL>) -> Result<Self, PickerError> {
        if !url.isFileURL() {
            return Err(PickerError::Platform(
                "The selected URL is not a local document.".into(),
            ));
        }
        // A URL already inside the app's sandbox can return false; actual
        // readability is checked by opening it on the worker. Pair every true
        // result with stopAccessing, including import and worker failures.
        let scoped = unsafe { url.startAccessingSecurityScopedResource() };
        Ok(Self { url, scoped })
    }

    fn import(&self, cache_dir: &Path) -> Result<PickedFile, PickerError> {
        let path = self.url.to_file_path().ok_or_else(|| {
            PickerError::Platform("The selected document has no filesystem path.".into())
        })?;
        let mut resource = None;
        // The resource key returns a UTType. Missing metadata is normal;
        // MIME guessing by the preserved extension remains the fallback.
        let mime_type = unsafe {
            self.url
                .getResourceValue_forKey_error(&mut resource, NSURLContentTypeKey)
                .ok()
                .and_then(|()| resource)
                .and_then(|value| value.downcast::<UTType>().ok())
                .and_then(|kind| kind.preferredMIMEType())
                .map(|mime| mime.to_string())
        };
        super::import::file(&path, mime_type, cache_dir)
    }
}

impl Drop for ScopedUrl {
    fn drop(&mut self) {
        if self.scoped {
            // NSURL is Send + Sync; security scope ownership follows the URL
            // to the import worker, and remains active until copying finishes.
            unsafe { self.url.stopAccessingSecurityScopedResource() };
        }
    }
}

pub(super) fn start(request: PickRequest, completion: Completion) -> Result<(), PickerError> {
    let mtm = MainThreadMarker::new().ok_or(PickerError::WrongThread)?;
    let active = super::import::ActiveRequest::acquire()?;
    let presenter = presenter(mtm)?;

    let mut types = Vec::new();
    for filter in &request.filters {
        for extension in &filter.extensions {
            let kind = UTType::typeWithFilenameExtension(&NSString::from_str(extension)).ok_or(
                PickerError::InvalidRequest("iOS cannot resolve an extension filter"),
            )?;
            types.push(kind);
        }
    }
    if types.is_empty() {
        types.push(
            UTType::typeWithIdentifier(&NSString::from_str("public.item")).ok_or_else(|| {
                PickerError::Platform("iOS has no public.item document type.".into())
            })?,
        );
    }

    // asCopy=true asks the document provider to import into this application's
    // sandbox; no external security-scoped URL escapes our completion callback.
    let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
        UIDocumentPickerViewController::alloc(mtm),
        &NSArray::from_retained_slice(&types),
        true,
    );
    picker.setAllowsMultipleSelection(request.mode == SelectionMode::Multiple);
    picker.setModalInPresentation(true);
    if let Some(title) = &request.title {
        picker.setTitle(Some(&NSString::from_str(title)));
    }
    let (sender, receiver) = sync_channel(1);
    let delegate = PickerDelegate::alloc(mtm).set_ivars(DelegateState {
        sender: RefCell::new(Some(sender)),
    });
    // SAFETY: NSObject's initializer has no extra requirements, and the
    // allocated instance already has its Rust ivars initialized.
    let delegate: Retained<PickerDelegate> = unsafe { msg_send![super(delegate), init] };
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    std::thread::Builder::new()
        .name("native-file-import".into())
        .spawn(move || {
            let result = receiver.recv()
                .map_err(|_| PickerError::Platform("The document picker closed without a result.".into()))
                .and_then(|selection| selection)
                .and_then(|urls| {
                    if urls.is_empty() {
                        return Ok(PickOutcome::Cancelled);
                    }
                    if request.mode == SelectionMode::Single && urls.len() != 1 {
                        return Err(PickerError::Platform("The document picker returned multiple files for a single-file request.".into()));
                    }
                    urls.iter()
                        .map(|url| url.import(&request.cache_dir))
                        .collect::<Result<Vec<_>, _>>()
                        .map(PickOutcome::Selected)
                });
            drop(active);
            completion(result);
        })?;
    PICKER.with(|slot| {
        *slot.borrow_mut() = Some(ActivePicker {
            _controller: picker.clone(),
            _delegate: delegate,
        });
    });
    presenter.presentViewController_animated_completion(&picker, true, None);
    Ok(())
}

fn presenter(mtm: MainThreadMarker) -> Result<Retained<UIViewController>, PickerError> {
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
                // Presenting during another modal transition can silently fail
                // in UIKit and leave a pending request forever. Surface busy.
                if controller.presentedViewController().is_some()
                    || controller.isBeingPresented()
                    || controller.isBeingDismissed()
                {
                    return Err(PickerError::Busy);
                }
                return Ok(controller);
            }
        }
    }
    Err(PickerError::Platform(
        "No foreground iOS window is available.".into(),
    ))
}

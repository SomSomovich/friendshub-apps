//! SAF integration for the small NativeActivity launcher subclass.
//!
//! The Java callback holds the provider descriptors open until `on_result`
//! returns. We duplicate them before importing, and never retain a URI grant.

use std::collections::BTreeSet;
use std::ffi::{OsString, c_void};
use std::fs::File;
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::objects::{GlobalRef, JIntArray, JObject, JObjectArray, JString, JValue};
use jni::{JNIEnv, JavaVM};

use super::{Completion, PickOutcome, PickRequest, PickerError, SelectionMode, import};

const LAUNCH_METHOD: &str = "openDocuments";
const LAUNCH_SIGNATURE: &str = "(JZ[Ljava/lang/String;Ljava/lang/String;)V";
const RESULT_CANCELLED: i32 = 0;
const RESULT_SELECTED: i32 = 1;
const RESULT_ERROR: i32 = 2;

struct AndroidContext {
    vm: JavaVM,
    activity: GlobalRef,
}

struct PendingRequest {
    id: i64,
    request: PickRequest,
    completion: Completion,
    active: import::ActiveRequest,
}

struct SelectedSource {
    file: File,
    file_name: OsString,
    mime_type: Option<String>,
}

static CONTEXT: OnceLock<Mutex<AndroidContext>> = OnceLock::new();
static INITIALIZATION: Mutex<()> = Mutex::new(());
static PENDING: Mutex<Option<PendingRequest>> = Mutex::new(None);
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Register the current launcher activity after Slint initializes Android.
/// A recreated activity replaces the old global reference for the same JVM.
///
/// # Safety
/// `java_vm` must be this process's JavaVM and `activity` a live global JNI
/// reference owned by the supplied AndroidApp for the duration of this call.
pub unsafe fn initialize(java_vm: *mut c_void, activity: *mut c_void) -> Result<(), PickerError> {
    if java_vm.is_null() || activity.is_null() {
        return Err(PickerError::MissingActivityResultHook);
    }
    let _initialization = INITIALIZATION
        .lock()
        .map_err(|_| platform_error("The Android file picker could not be initialized."))?;
    // SAFETY: the initialized AndroidApp provides these pointers.
    let vm = unsafe { JavaVM::from_raw(java_vm.cast()) }
        .map_err(|_| PickerError::MissingActivityResultHook)?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|_| PickerError::MissingActivityResultHook)?;
    let result = env.with_local_frame(8, |env| {
        // SAFETY: JObject only borrows the host's global reference. The service
        // owns only the fresh GlobalRef created below.
        let activity = unsafe { JObject::from_raw(activity.cast()) };
        let class = env.get_object_class(&activity)?;
        env.get_method_id(class, LAUNCH_METHOD, LAUNCH_SIGNATURE)?;
        env.new_global_ref(activity)
    });
    if result.is_err() {
        let _ = env.exception_clear();
    }
    let activity = result.map_err(|_| PickerError::MissingActivityResultHook)?;
    drop(env);

    if let Some(context) = CONTEXT.get() {
        let mut context = context
            .lock()
            .map_err(|_| platform_error("The Android file picker could not be initialized."))?;
        if context.vm.get_java_vm_pointer() != vm.get_java_vm_pointer() {
            return Err(PickerError::MissingActivityResultHook);
        }
        context.activity = activity;
        return Ok(());
    }
    CONTEXT
        .set(Mutex::new(AndroidContext { vm, activity }))
        .map_err(|_| platform_error("The Android file picker could not be initialized."))
}

pub(super) fn start(request: PickRequest, completion: Completion) -> Result<(), PickerError> {
    let active = import::ActiveRequest::acquire()?;
    let context = CONTEXT
        .get()
        .ok_or(PickerError::MissingActivityResultHook)?;
    let (vm_pointer, activity) = {
        let context = context
            .lock()
            .map_err(|_| platform_error("The Android file picker is unavailable."))?;
        (context.vm.get_java_vm_pointer(), context.activity.clone())
    };
    // SAFETY: CONTEXT retains the process JVM and the activity GlobalRef.
    let vm = unsafe { JavaVM::from_raw(vm_pointer) }
        .map_err(|_| PickerError::MissingActivityResultHook)?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|_| platform_error("The Android file picker could not be opened."))?;
    let mime_types = mime_types(&request);
    let multiple = request.mode == SelectionMode::Multiple;
    let title = request.title.clone().unwrap_or_default();
    let id = REQUEST_SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1) as i64;
    {
        let mut pending = PENDING
            .lock()
            .map_err(|_| platform_error("The Android file picker is unavailable."))?;
        if pending.is_some() {
            return Err(PickerError::Busy);
        }
        *pending = Some(PendingRequest {
            id,
            request,
            completion,
            active,
        });
    }

    let result = env.with_local_frame(16, |env| -> jni::errors::Result<()> {
        let types =
            env.new_object_array(mime_types.len() as i32, "java/lang/String", JObject::null())?;
        for (index, mime_type) in mime_types.iter().enumerate() {
            let mime_type = env.new_string(mime_type)?;
            env.set_object_array_element(&types, index as i32, mime_type)?;
        }
        let title = env.new_string(title)?;
        env.call_method(
            activity.as_obj(),
            LAUNCH_METHOD,
            LAUNCH_SIGNATURE,
            &[
                JValue::Long(id),
                JValue::Bool(u8::from(multiple)),
                JValue::Object(types.as_ref()),
                JValue::Object(title.as_ref()),
            ],
        )?
        .v()
    });
    if result.is_err() {
        let _ = env.exception_clear();
        // An immediate launch failure does not run the completion. If a host
        // called back synchronously first, preserve the successful initiation.
        if take_pending(id).is_some() {
            return Err(platform_error(
                "The Android file picker could not be opened.",
            ));
        }
    }
    Ok(())
}

/// Receive a result from FriendsHubActivity on its document-import worker.
/// Descriptor integers are borrowed only until this call returns; the Java
/// caller closes them in `finally`, including when the request is stale.
pub fn on_result(
    mut env: JNIEnv<'_>,
    request_id: i64,
    status: i32,
    names: JObjectArray<'_>,
    mime_types: JObjectArray<'_>,
    descriptors: JIntArray<'_>,
    message: JString<'_>,
) {
    let Some(pending) = take_pending(request_id) else {
        return;
    };
    let result = match status {
        RESULT_CANCELLED => Ok(PickOutcome::Cancelled),
        RESULT_SELECTED => {
            selected_sources(&mut env, names, mime_types, descriptors).and_then(|sources| {
                if sources.is_empty() {
                    return Err(platform_error("The document provider returned no files."));
                }
                if pending.request.mode == SelectionMode::Single && sources.len() != 1 {
                    return Err(platform_error(
                        "The document provider returned more than one file.",
                    ));
                }
                sources
                    .into_iter()
                    .map(|mut source| {
                        import::reader(
                            &mut source.file,
                            source.file_name,
                            source.mime_type,
                            &pending.request.cache_dir,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(PickOutcome::Selected)
            })
        }
        RESULT_ERROR => {
            let reason = if message.is_null() {
                None
            } else {
                env.get_string(&message).ok().map(String::from)
            };
            Err(platform_error(reason.as_deref().unwrap_or(
                "The document provider could not open the selected files.",
            )))
        }
        _ => Err(platform_error(
            "The Android file picker returned an invalid result.",
        )),
    };
    let _ = env.exception_clear();
    let PendingRequest {
        completion, active, ..
    } = pending;
    drop(active);
    completion(result);
}

fn selected_sources(
    env: &mut JNIEnv<'_>,
    names: JObjectArray<'_>,
    mime_types: JObjectArray<'_>,
    descriptors: JIntArray<'_>,
) -> Result<Vec<SelectedSource>, PickerError> {
    if names.is_null() || mime_types.is_null() || descriptors.is_null() {
        return Err(platform_error(
            "The document provider returned incomplete file data.",
        ));
    }
    let count = env
        .get_array_length(&descriptors)
        .map_err(|_| platform_error("The selected files could not be imported."))?;
    if env.get_array_length(&names).ok() != Some(count)
        || env.get_array_length(&mime_types).ok() != Some(count)
    {
        return Err(platform_error(
            "The document provider returned inconsistent file data.",
        ));
    }
    let mut raw_descriptors = vec![0; count as usize];
    env.get_int_array_region(&descriptors, 0, &mut raw_descriptors)
        .map_err(|_| platform_error("The selected files could not be imported."))?;
    let mut sources = Vec::with_capacity(count as usize);
    for (index, descriptor) in raw_descriptors.into_iter().enumerate() {
        if descriptor < 0 {
            return Err(platform_error(
                "The document provider returned an invalid file descriptor.",
            ));
        }
        // SAFETY: Java keeps every ParcelFileDescriptor open across this entire
        // callback. Duplication transfers only the new descriptor to Rust.
        let descriptor = unsafe { BorrowedFd::borrow_raw(descriptor) }.try_clone_to_owned()?;
        let (name, mime_type) = env
            .with_local_frame(8, |env| -> jni::errors::Result<_> {
                let name = JString::from(env.get_object_array_element(&names, index as i32)?);
                let mime_type =
                    JString::from(env.get_object_array_element(&mime_types, index as i32)?);
                let name = if name.is_null() {
                    String::new()
                } else {
                    env.get_string(&name)?.into()
                };
                let mime_type = if mime_type.is_null() {
                    String::new()
                } else {
                    env.get_string(&mime_type)?.into()
                };
                Ok((name, mime_type))
            })
            .map_err(|_| platform_error("The selected file metadata could not be read."))?;
        sources.push(SelectedSource {
            file: File::from(descriptor),
            file_name: safe_file_name(&name),
            mime_type: valid_mime_type(&mime_type).then_some(mime_type),
        });
    }
    Ok(sources)
}

fn take_pending(id: i64) -> Option<PendingRequest> {
    let mut pending = PENDING.lock().ok()?;
    if pending.as_ref().is_some_and(|pending| pending.id == id) {
        pending.take()
    } else {
        None
    }
}

fn mime_types(request: &PickRequest) -> Vec<String> {
    let mut types = BTreeSet::new();
    for extension in request.filters.iter().flat_map(|filter| &filter.extensions) {
        let Some(mime_type) = mime_guess::from_ext(extension).first_raw() else {
            return vec!["*/*".to_owned()];
        };
        types.insert(mime_type.to_owned());
    }
    if types.is_empty() {
        vec!["*/*".to_owned()]
    } else {
        types.into_iter().collect()
    }
}

fn safe_file_name(name: &str) -> OsString {
    let mut safe = String::new();
    for character in name.chars() {
        let character = if character.is_control() || matches!(character, '/' | '\\') {
            '_'
        } else {
            character
        };
        if safe.len() + character.len_utf8() > 200 {
            break;
        }
        safe.push(character);
    }
    if safe.trim().is_empty() || matches!(safe.as_str(), "." | "..") {
        safe = "attachment".to_owned();
    }
    safe.into()
}

fn valid_mime_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else {
        return false;
    };
    !kind.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && value.len() <= 255
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'+' | b'.' | b'_')
        })
}

fn platform_error(message: &str) -> PickerError {
    PickerError::Platform(message.to_owned())
}

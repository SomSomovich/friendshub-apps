package com.somuchsystem.friendshub;

import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ContentResolver;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;

import java.io.IOException;
import java.util.ArrayList;
import java.util.LinkedHashSet;

/** NativeActivity plus the activity-result callback absent from the NDK glue. */
public final class FriendsHubActivity extends NativeActivity {
    private static final int DOCUMENT_REQUEST = 0x4648;
    private static final int CANCELLED = 0;
    private static final int SELECTED = 1;
    private static final int ERROR = 2;
    private static final String SAVED_REQUEST = "friendshub.document-request";

    static {
        // NativeActivity also loads this library through its manifest metadata.
        // System.loadLibrary registers it with the JVM for the JNI callback.
        System.loadLibrary("friendshub_android");
    }

    private long documentRequest;

    private static native void nativeOnFilePickerResult(
            long request, int status, String[] names, String[] mimeTypes,
            int[] descriptors, String message);

    @Override
    protected void onCreate(Bundle state) {
        if (state != null) {
            documentRequest = state.getLong(SAVED_REQUEST, 0);
        }
        super.onCreate(state);
    }

    @Override
    protected void onSaveInstanceState(Bundle state) {
        state.putLong(SAVED_REQUEST, documentRequest);
        super.onSaveInstanceState(state);
    }

    /** Called through JNI from the Slint application thread. */
    public void openDocuments(long request, boolean multiple, String[] mimeTypes, String title) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (isFinishing() || isDestroyed()) {
                    deliver(request, ERROR, "The Android activity is no longer available.");
                    return;
                }
                if (documentRequest != 0) {
                    deliver(request, ERROR, "A document selection is already in progress.");
                    return;
                }
                documentRequest = request;
                try {
                    Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                    intent.addCategory(Intent.CATEGORY_OPENABLE);
                    intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                    intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multiple);
                    if (mimeTypes != null && mimeTypes.length == 1) {
                        intent.setType(mimeTypes[0]);
                    } else {
                        intent.setType("*/*");
                        if (mimeTypes != null && mimeTypes.length > 1) {
                            intent.putExtra(Intent.EXTRA_MIME_TYPES, mimeTypes);
                        }
                    }
                    if (title != null && !title.isEmpty()) {
                        intent.putExtra(Intent.EXTRA_TITLE, title);
                    }
                    startActivityForResult(intent, DOCUMENT_REQUEST);
                } catch (RuntimeException failure) {
                    documentRequest = 0;
                    deliver(request, ERROR, "The Android document picker could not be opened.");
                }
            }
        });
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != DOCUMENT_REQUEST || documentRequest == 0) {
            return;
        }
        long request = documentRequest;
        documentRequest = 0;
        if (resultCode != RESULT_OK || data == null) {
            deliver(request, CANCELLED, null);
            return;
        }
        try {
            LinkedHashSet<Uri> uris = new LinkedHashSet<>();
            Uri selected = data.getData();
            if (selected != null) {
                uris.add(selected);
            }
            ClipData clips = data.getClipData();
            if (clips != null) {
                for (int index = 0; index < clips.getItemCount(); index++) {
                    Uri uri = clips.getItemAt(index).getUri();
                    if (uri != null) {
                        uris.add(uri);
                    }
                }
            }
            // Provider I/O and Rust's private-cache import must not block Android's
            // UI thread. No persistable URI permission is requested or retained.
            new Thread(new Runnable() {
                @Override
                public void run() {
                    importSelection(request, uris);
                }
            }, "friendshub-document-import").start();
        } catch (RuntimeException failure) {
            deliver(request, ERROR, "The document provider returned an invalid selection.");
        }
    }

    @Override
    protected void onDestroy() {
        if (documentRequest != 0 && !isChangingConfigurations()) {
            long request = documentRequest;
            documentRequest = 0;
            deliver(request, CANCELLED, null);
        }
        super.onDestroy();
    }

    private void importSelection(long request, LinkedHashSet<Uri> uris) {
        ArrayList<ParcelFileDescriptor> opened = new ArrayList<>();
        try {
            if (uris.isEmpty()) {
                nativeOnFilePickerResult(request, ERROR, null, null, null,
                        "The document provider returned no files.");
                return;
            }
            ContentResolver resolver = getApplicationContext().getContentResolver();
            String[] names = new String[uris.size()];
            String[] mimeTypes = new String[uris.size()];
            int[] descriptors = new int[uris.size()];
            int index = 0;
            for (Uri uri : uris) {
                if (!"content".equals(uri.getScheme())) {
                    throw new IOException("The provider returned a non-content URI.");
                }
                ParcelFileDescriptor descriptor = resolver.openFileDescriptor(uri, "r");
                if (descriptor == null) {
                    throw new IOException("The provider returned no descriptor.");
                }
                opened.add(descriptor);
                descriptors[index] = descriptor.getFd();
                names[index] = displayName(resolver, uri);
                try {
                    mimeTypes[index] = resolver.getType(uri);
                } catch (RuntimeException ignored) {
                    mimeTypes[index] = null;
                }
                index++;
            }
            // These descriptors remain owned by Java. Rust duplicates them and
            // finishes importing into its private cache before returning.
            nativeOnFilePickerResult(request, SELECTED, names, mimeTypes, descriptors, null);
        } catch (IOException | RuntimeException failure) {
            nativeOnFilePickerResult(request, ERROR, null, null, null,
                    "The document provider could not open the selected files.");
        } finally {
            for (ParcelFileDescriptor descriptor : opened) {
                try {
                    descriptor.close();
                } catch (IOException ignored) {
                    // Rust has already imported or rejected this descriptor.
                }
            }
        }
    }

    private static String displayName(ContentResolver resolver, Uri uri) {
        try (Cursor cursor = resolver.query(uri,
                new String[] { OpenableColumns.DISPLAY_NAME }, null, null, null)) {
            if (cursor != null && cursor.moveToFirst() && !cursor.isNull(0)) {
                String name = cursor.getString(0);
                if (name != null && !name.isEmpty()) {
                    return name;
                }
            }
        } catch (RuntimeException ignored) {
            // Display names are optional metadata, never a source filesystem path.
        }
        return "attachment";
    }

    private static void deliver(long request, int status, String message) {
        new Thread(new Runnable() {
            @Override
            public void run() {
                nativeOnFilePickerResult(request, status, null, null, null, message);
            }
        }, "friendshub-document-result").start();
    }
}

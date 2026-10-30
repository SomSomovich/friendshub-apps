package com.somuchsystem.friendshub;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.content.Context;
import android.content.pm.ProviderInfo;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;
import android.system.ErrnoException;
import android.system.Os;
import android.system.OsConstants;
import android.webkit.MimeTypeMap;

import java.io.BufferedInputStream;
import java.io.File;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.net.URLConnection;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

/** Temporary, read-only access to the private downloaded-attachment cache. */
public final class AttachmentProvider extends ContentProvider {
    private static final String AUTHORITY = "com.somuchsystem.friendshub.attachments";
    private static final String PREFIX = "attachment";

    @Override
    public void attachInfo(Context context, ProviderInfo info) {
        if (info.exported || !info.grantUriPermissions || !AUTHORITY.equals(info.authority)) {
            throw new SecurityException("Attachment provider configuration is invalid.");
        }
        super.attachInfo(context, info);
    }

    @Override
    public boolean onCreate() {
        return true;
    }

    /** Called through JNI; never accepts a path outside the attachment cache. */
    public static Uri uriForFile(Context context, String path) throws IOException {
        if (path == null || !new File(path).isAbsolute()) {
            throw new SecurityException("Attachment path must be absolute.");
        }
        for (String component : path.split("/", -1)) {
            if (".".equals(component) || "..".equals(component)) {
                throw new SecurityException("Attachment path traversal is forbidden.");
            }
        }
        File root = cacheRoot(context);
        File file = containedFile(root, new File(path));
        String relative = file.getPath().substring(root.getPath().length() + 1);
        String[] segments = relative.split("/", -1);
        if (segments.length != 2 || !validAccount(segments[0]) || !validSegment(segments[1])) {
            throw new SecurityException("Attachment path is invalid.");
        }
        return new Uri.Builder().scheme("content").authority(AUTHORITY)
                .appendPath(PREFIX).appendPath(segments[0]).appendPath(segments[1]).build();
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        if (!"r".equals(mode)) {
            throw new SecurityException("Attachments are read-only.");
        }
        try {
            File root = cacheRoot(providerContext());
            File file = resolve(root, uri);
            ParcelFileDescriptor descriptor = ParcelFileDescriptor.open(file,
                    ParcelFileDescriptor.MODE_READ_ONLY);
            try {
                // Validate the opened object too: a path replaced between
                // canonicalization and open must not escape the private root.
                File opened = new File("/proc/self/fd/" + descriptor.getFd()).getCanonicalFile();
                if (!opened.equals(file) || !isWithin(root, opened)
                        || !OsConstants.S_ISREG(Os.fstat(descriptor.getFileDescriptor()).st_mode)) {
                    throw new SecurityException("Attachment file changed while opening.");
                }
                return descriptor;
            } catch (IOException | ErrnoException | RuntimeException failure) {
                descriptor.close();
                throw failure;
            }
        } catch (IOException | ErrnoException failure) {
            throw new FileNotFoundException("The private attachment is unavailable.");
        }
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection,
            String[] selectionArgs, String sortOrder) {
        File file = resolveForMetadata(uri);
        long size;
        try (ParcelFileDescriptor descriptor = openFile(uri, "r")) {
            size = descriptor.getStatSize();
        } catch (IOException failure) {
            throw new IllegalArgumentException("The private attachment is unavailable.");
        }
        String[] requested = projection == null
                ? new String[] { OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE } : projection;
        ArrayList<String> columns = new ArrayList<>();
        ArrayList<Object> values = new ArrayList<>();
        for (String column : requested) {
            if (OpenableColumns.DISPLAY_NAME.equals(column)) {
                columns.add(column);
                values.add(file.getName());
            } else if (OpenableColumns.SIZE.equals(column)) {
                columns.add(column);
                values.add(size);
            }
        }
        MatrixCursor cursor = new MatrixCursor(columns.toArray(new String[0]), 1);
        cursor.addRow(values);
        return cursor;
    }

    @Override
    public String getType(Uri uri) {
        File file = resolveForMetadata(uri);
        String name = file.getName();
        int dot = name.lastIndexOf('.');
        if (dot >= 0 && dot < name.length() - 1) {
            String type = MimeTypeMap.getSingleton().getMimeTypeFromExtension(
                    name.substring(dot + 1).toLowerCase(Locale.ROOT));
            if (type != null && !"application/octet-stream".equals(type)) {
                return type;
            }
        }
        try (BufferedInputStream stream = new BufferedInputStream(
                new ParcelFileDescriptor.AutoCloseInputStream(openFile(uri, "r")))) {
            String detected = URLConnection.guessContentTypeFromStream(stream);
            if (detected != null) {
                return detected;
            }
        } catch (IOException ignored) {
            // A deleted or unreadable cache file is rejected again by openFile.
        }
        return "application/octet-stream";
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        throw new UnsupportedOperationException("Attachments are read-only.");
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] selectionArgs) {
        throw new UnsupportedOperationException("Attachments are read-only.");
    }

    @Override
    public int delete(Uri uri, String selection, String[] selectionArgs) {
        throw new UnsupportedOperationException("Attachments are read-only.");
    }

    private Context providerContext() {
        Context context = getContext();
        if (context == null) {
            throw new IllegalStateException("Attachment provider is not initialized.");
        }
        return context;
    }

    private File resolveForMetadata(Uri uri) {
        try {
            return resolve(cacheRoot(providerContext()), uri);
        } catch (IOException failure) {
            throw new IllegalArgumentException("The private attachment is unavailable.");
        }
    }

    private static File resolve(File root, Uri uri) throws IOException {
        if (!"content".equals(uri.getScheme()) || !AUTHORITY.equals(uri.getAuthority())
                || uri.getQuery() != null || uri.getFragment() != null) {
            throw new SecurityException("Attachment URI is invalid.");
        }
        List<String> segments = uri.getPathSegments();
        if (segments.size() != 3 || !PREFIX.equals(segments.get(0))
                || !validAccount(segments.get(1)) || !validSegment(segments.get(2))) {
            throw new SecurityException("Attachment URI path is invalid.");
        }
        return containedFile(root, new File(new File(root, segments.get(1)), segments.get(2)));
    }

    private static File cacheRoot(Context context) throws IOException {
        File files = context.getFilesDir().getCanonicalFile();
        // Rust's Android storage adapter places all app data under
        // <filesDir>/FriendsHub, including the attachment cache.
        File root = new File(new File(new File(files, "FriendsHub"), "cache"), "opened-attachments");
        File canonical = root.getCanonicalFile();
        if (!root.equals(canonical) || !isWithin(files, canonical)) {
            throw new SecurityException("Attachment cache path is invalid.");
        }
        return canonical;
    }

    private static File containedFile(File root, File source) throws IOException {
        File file = source.getCanonicalFile();
        if (!isWithin(root, file) || !file.isFile()) {
            throw new SecurityException("Attachment path is outside the private cache.");
        }
        return file;
    }

    private static boolean isWithin(File root, File file) {
        return file.getPath().startsWith(root.getPath() + File.separator);
    }

    private static boolean validAccount(String value) {
        if (value.isEmpty() || value.length() % 2 != 0) {
            return false;
        }
        for (int index = 0; index < value.length(); index++) {
            char character = value.charAt(index);
            if ((character < '0' || character > '9') && (character < 'a' || character > 'f')) {
                return false;
            }
        }
        return true;
    }

    private static boolean validSegment(String value) {
        return !value.isEmpty() && !".".equals(value) && !"..".equals(value)
                && value.indexOf('/') < 0 && value.indexOf('\\') < 0 && value.indexOf('\0') < 0;
    }
}

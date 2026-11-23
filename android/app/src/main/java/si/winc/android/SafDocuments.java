package si.winc.android;

import android.content.ContentResolver;
import android.content.Context;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract.Document;
import android.provider.OpenableColumns;
import android.system.ErrnoException;
import android.system.Os;
import android.system.OsConstants;
import android.util.Log;

import java.io.ByteArrayOutputStream;
import java.io.FileDescriptor;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Arrays;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.function.IntConsumer;

/** Uses the provider URI as the source of truth and the private file as an editor mirror. */
final class SafDocuments {
    private static final int LIMIT = 1024 * 1024;
    private static final int CHUNK = 16 * 1024;
    private static final int SAVED = 0;
    private static final int UNWRITTEN = 1;
    private static final int CONFLICT = 2;
    private static final int INVALID_TEXT = 3;
    private static final int UNCERTAIN = 4;
    private final ContentResolver resolver;
    private final Path directory;
    private final ExecutorService worker = Executors.newSingleThreadExecutor();

    interface OpenResult {
        void opened(String path, String uri, boolean writable);
        void failed(int status);
    }

    private record Metadata(String name, boolean supportsWrite) {}

    SafDocuments(Context context) {
        resolver = context.getContentResolver();
        directory = context.getFilesDir().toPath().resolve("winc/documents");
    }

    void open(Uri uri, int grants, OpenResult result) {
        worker.execute(() -> {
            try {
                if (!ContentResolver.SCHEME_CONTENT.equals(uri.getScheme())) {
                    throw new IOException("Document picker returned a non-content URI");
                }
                byte[] bytes = read(uri);
                validateText(bytes);
                Metadata metadata = metadata(uri);
                Files.createDirectories(directory);
                Path entry = Files.createTempDirectory(directory, "document-");
                Path mirrorDirectory = entry.resolve("mirror");
                Path mirror = mirrorDirectory.resolve(metadata.name());
                try {
                    Files.createDirectory(mirrorDirectory);
                    Files.write(mirror, bytes);
                } catch (IOException exception) {
                    Files.deleteIfExists(mirror);
                    Files.deleteIfExists(mirrorDirectory);
                    Files.deleteIfExists(entry);
                    throw exception;
                }
                boolean writable = metadata.supportsWrite()
                        && (grants & Intent.FLAG_GRANT_WRITE_URI_PERMISSION) != 0;
                result.opened(mirror.toString(), uri.toString(), writable);
            } catch (UnsupportedText failure) {
                result.failed(2);
            } catch (IOException | RuntimeException failure) {
                Log.w("Winc", "Unable to open system document", failure);
                result.failed(1);
            }
        });
    }

    void save(String source, String baseline, String staged, IntConsumer result) {
        worker.execute(() -> result.accept(write(Uri.parse(source), Paths.get(staged),
                Paths.get(baseline))));
    }

    void export(Uri uri, String source, String excludedUri, IntConsumer result) {
        worker.execute(() -> {
            if (uri.equals(Uri.parse(excludedUri))) {
                result.accept(UNWRITTEN);
                return;
            }
            result.accept(write(uri, Paths.get(source), null));
        });
    }

    private int write(Uri uri, Path staged, Path baseline) {
        boolean modifying = false;
        try {
            if (!ContentResolver.SCHEME_CONTENT.equals(uri.getScheme())) {
                return UNWRITTEN;
            }
            if (baseline != null && Files.size(staged) > LIMIT) {
                return INVALID_TEXT;
            }
            byte[] intended = Files.readAllBytes(staged);
            if (baseline != null) {
                validateText(intended);
            }
            byte[] original = baseline == null ? new byte[0] : Files.readAllBytes(baseline);
            try {
                if (!Arrays.equals(read(uri), original)) {
                    return CONFLICT;
                }
            } catch (UnsupportedText failure) {
                return CONFLICT;
            }
            // Opening with "rw" avoids truncation. Validate and write through one descriptor.
            try (ParcelFileDescriptor parcel = resolver.openFileDescriptor(uri, "rw")) {
                if (parcel == null) {
                    return UNWRITTEN;
                }
                FileDescriptor file = parcel.getFileDescriptor();
                if (!OsConstants.S_ISREG(Os.fstat(file).st_mode)) {
                    return UNWRITTEN;
                }
                if (!matches(file, original)) {
                    return CONFLICT;
                }
                if (!Arrays.equals(original, intended)) {
                    modifying = true;
                    for (int offset = 0; offset < intended.length;) {
                        int written = Os.pwrite(file, intended, offset,
                                Math.min(CHUNK, intended.length - offset), offset);
                        if (written <= 0) {
                            throw new IOException("Document provider made no write progress");
                        }
                        offset += written;
                    }
                    if (intended.length < original.length) {
                        // Check the tail before removing it. A provider may allow other writers.
                        byte[] intermediate = original.clone();
                        System.arraycopy(intended, 0, intermediate, 0, intended.length);
                        if (!matches(file, intermediate)) {
                            return UNCERTAIN;
                        }
                        Os.ftruncate(file, intended.length);
                    }
                    Os.fsync(file);
                    if (!matches(file, intended)) {
                        return UNCERTAIN;
                    }
                }
                parcel.checkError();
            }
            return Arrays.equals(read(uri, intended.length), intended)
                    ? SAVED : modifying ? UNCERTAIN : CONFLICT;
        } catch (UnsupportedText failure) {
            return modifying ? UNCERTAIN : INVALID_TEXT;
        } catch (IOException | ErrnoException | RuntimeException failure) {
            Log.w("Winc", "Unable to verify system document write (" +
                    failure.getClass().getSimpleName() + ")");
            return modifying ? UNCERTAIN : UNWRITTEN;
        }
    }

    private static boolean matches(FileDescriptor file, byte[] expected)
            throws ErrnoException, IOException {
        if (Os.fstat(file).st_size != expected.length) {
            return false;
        }
        byte[] buffer = new byte[Math.min(CHUNK, expected.length)];
        for (int offset = 0; offset < expected.length;) {
            int size = Os.pread(file, buffer, 0, Math.min(buffer.length, expected.length - offset),
                    offset);
            if (size <= 0) {
                return false;
            }
            for (int index = 0; index < size; index++) {
                if (buffer[index] != expected[offset + index]) {
                    return false;
                }
            }
            offset += size;
        }
        return Os.fstat(file).st_size == expected.length;
    }

    private byte[] read(Uri uri) throws IOException {
        return read(uri, LIMIT);
    }

    private byte[] read(Uri uri, int limit) throws IOException {
        try (InputStream stream = resolver.openInputStream(uri)) {
            if (stream == null) {
                throw new FileNotFoundException("Document provider returned no input stream");
            }
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            byte[] buffer = new byte[16 * 1024];
            for (int size; (size = stream.read(buffer)) != -1;) {
                if (size > limit - bytes.size()) {
                    throw new UnsupportedText();
                }
                bytes.write(buffer, 0, size);
            }
            return bytes.toByteArray();
        }
    }

    private static void validateText(byte[] bytes) throws UnsupportedText {
        if (bytes.length > LIMIT) {
            throw new UnsupportedText();
        }
        for (byte value : bytes) {
            if (value == 0) {
                throw new UnsupportedText();
            }
        }
        try {
            StandardCharsets.UTF_8.newDecoder().decode(ByteBuffer.wrap(bytes));
        } catch (CharacterCodingException failure) {
            throw new UnsupportedText();
        }
    }

    private Metadata metadata(Uri uri) {
        String[] projection = {OpenableColumns.DISPLAY_NAME, Document.COLUMN_FLAGS};
        try (Cursor cursor = resolver.query(uri, projection, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int name = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                int flags = cursor.getColumnIndex(Document.COLUMN_FLAGS);
                boolean supportsWrite = flags < 0 || cursor.isNull(flags)
                        || (cursor.getInt(flags) & Document.FLAG_SUPPORTS_WRITE) != 0;
                return new Metadata(safeName(name < 0 ? null : cursor.getString(name)), supportsWrite);
            }
        }
        return new Metadata("document.txt", true);
    }

    private static String safeName(String name) {
        if (name == null) {
            return "document.txt";
        }
        String safe = name.replace('/', '_').replace('\\', '_').replaceAll("\\p{Cntrl}", "_");
        int length = Math.min(60, safe.codePointCount(0, safe.length()));
        safe = safe.substring(0, safe.offsetByCodePoints(0, length));
        return safe.trim().isEmpty() || safe.equals(".") || safe.equals("..")
                ? "document.txt" : safe;
    }

    void close() {
        worker.shutdown();
    }

    private static final class UnsupportedText extends IOException {}
}

package si.winc.android;

import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.graphics.Insets;
import android.os.Bundle;
import android.os.SystemClock;
import android.util.Log;
import android.view.Gravity;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.PopupMenu;

import androidx.activity.OnBackPressedCallback;
import androidx.activity.result.ActivityResultLauncher;
import androidx.activity.result.contract.ActivityResultContracts;
import androidx.core.view.WindowCompat;

import com.google.androidgamesdk.GameActivity;

public final class WincActivity extends GameActivity {
    private static final int OPEN = 1;
    private static final int SAVE = 2;
    private static final int KEYBOARD = 3;
    private static final int UNDO = 4;
    private static final int INPUT = 5;
    private static final int OPENED = 6;
    private static final int OPEN_ENDED = 7;
    private static final int SAVED = 8;
    private static final int PAUSED = 9;
    private static final int LAYOUT = 11;
    private static final int EXPORT_DRAFT = 12;
    private static final int EXPORT_ORIGINAL = 13;
    private static final int EXPORT_ENDED = 14;
    private static final int INPUT_RECENTERED = 15;

    static {
        System.loadLibrary("winc_android");
    }

    private ActivityResultLauncher<Intent> picker;
    private ActivityResultLauncher<Intent> exporter;
    private SafDocuments documents;
    private EditorInput input;
    private SurfaceView surface;
    private long pickToken;
    private Export pendingExport;
    private long started;

    private record Export(long token, String path, String excludedUri) {}

    private native void sendNative(
            int kind, long token, long sequence, int a, int b, int c, int d, String text,
            String extra);

    private void send(int kind) {
        complete(kind, 0, 0);
    }

    private void complete(int kind, long token, int status) {
        sendNative(kind, token, 0, status, 0, 0, 0, "", "");
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        started = SystemClock.elapsedRealtime();
        super.onCreate(savedInstanceState);
        documents = new SafDocuments(this);
        picker = registerForActivityResult(
                new ActivityResultContracts.StartActivityForResult(), result -> {
                    long token = pickToken;
                    pickToken = 0;
                    Intent data = result.getData();
                    if (token == 0) {
                        return;
                    }
                    if (result.getResultCode() != RESULT_OK || data == null || data.getData() == null) {
                        complete(OPEN_ENDED, token, 0);
                        return;
                    }
                    documents.open(data.getData(), data.getFlags(), new SafDocuments.OpenResult() {
                        @Override
                        public void opened(String path, String uri, boolean writable) {
                            sendNative(OPENED, token, 0, writable ? 1 : 0, 0, 0, 0, path, uri);
                        }

                        @Override
                        public void failed(int status) {
                            complete(OPEN_ENDED, token, status);
                        }
                    });
                });
        exporter = registerForActivityResult(
                new ActivityResultContracts.StartActivityForResult(), result -> {
                    Export request = pendingExport;
                    pendingExport = null;
                    if (request == null) {
                        return;
                    }
                    Intent data = result.getData();
                    if (result.getResultCode() != RESULT_OK || data == null || data.getData() == null) {
                        complete(EXPORT_ENDED, request.token(), -1);
                        return;
                    }
                    documents.export(data.getData(), request.path(), request.excludedUri(),
                            status -> complete(EXPORT_ENDED, request.token(), status));
                });

        installControls();
        getOnBackPressedDispatcher().addCallback(this, new OnBackPressedCallback(true) {
            @Override
            public void handleOnBackPressed() {
                if (input.hasFocus()) {
                    hideInput();
                } else {
                    moveTaskToBack(true);
                }
            }
        });
    }

    private void installControls() {
        WindowCompat.setDecorFitsSystemWindows(getWindow(), false);
        FrameLayout content = findViewById(android.R.id.content);
        View game = content.getChildAt(0);
        surface = findSurface(game);

        int toolbarHeight = Math.round(48 * getResources().getDisplayMetrics().density);
        FrameLayout.LayoutParams gameLayout = (FrameLayout.LayoutParams) game.getLayoutParams();
        gameLayout.topMargin = toolbarHeight;
        game.setLayoutParams(gameLayout);

        LinearLayout toolbar = new LinearLayout(this);
        toolbar.setGravity(Gravity.CENTER_VERTICAL);
        button(toolbar, R.string.open).setOnClickListener(view -> send(OPEN));
        button(toolbar, R.string.save).setOnClickListener(view -> send(SAVE));
        button(toolbar, R.string.undo).setOnClickListener(view -> send(UNDO));
        button(toolbar, R.string.keyboard).setOnClickListener(view -> send(KEYBOARD));
        button(toolbar, R.string.more).setOnClickListener(this::showOptions);
        content.addView(toolbar, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, toolbarHeight, Gravity.TOP));

        input = new EditorInput(this, (token, sequence, anchor, head, from, to, text) ->
                sendNative(INPUT, token, sequence, anchor, head, from, to, text, ""));
        content.addView(input, new FrameLayout.LayoutParams(1, 1, Gravity.BOTTOM | Gravity.LEFT));
        content.setOnApplyWindowInsetsListener((view, insets) -> {
            Insets safe = insets.getInsets(WindowInsets.Type.systemBars()
                    | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime());
            if (view.getPaddingLeft() != safe.left || view.getPaddingTop() != safe.top
                    || view.getPaddingRight() != safe.right || view.getPaddingBottom() != safe.bottom) {
                view.setPadding(safe.left, safe.top, safe.right, safe.bottom);
            }
            return insets;
        });
        surface.addOnLayoutChangeListener((view, left, top, right, bottom, oldLeft, oldTop,
                oldRight, oldBottom) -> send(LAYOUT));
        content.requestApplyInsets();
    }

    private Button button(LinearLayout toolbar, int label) {
        Button button = new Button(this);
        button.setText(label);
        button.setAllCaps(false);
        button.setMinWidth(0);
        button.setMinimumWidth(0);
        toolbar.addView(button, new LinearLayout.LayoutParams(0, -1, 1));
        return button;
    }

    private void showOptions(View anchor) {
        PopupMenu menu = new PopupMenu(this, anchor);
        menu.getMenu().add(0, EXPORT_DRAFT, 0, R.string.export_draft);
        menu.getMenu().add(0, EXPORT_ORIGINAL, 1, R.string.export_original);
        menu.setOnMenuItemClickListener(item -> {
            send(item.getItemId());
            return true;
        });
        menu.show();
    }

    private static SurfaceView findSurface(View view) {
        if (view instanceof SurfaceView surface) {
            return surface;
        }
        if (view instanceof ViewGroup group) {
            for (int i = 0; i < group.getChildCount(); i++) {
                SurfaceView surface = findSurface(group.getChildAt(i));
                if (surface != null) {
                    return surface;
                }
            }
        }
        return null;
    }

    // Called from the native winit thread.
    private void pickDocument(long token) {
        runOnUiThread(() -> {
            pickToken = token;
            Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                    .setType("*/*").addCategory(Intent.CATEGORY_OPENABLE)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION
                            | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
            try {
                picker.launch(intent);
            } catch (ActivityNotFoundException exception) {
                pickToken = 0;
                complete(OPEN_ENDED, token, 1);
            }
        });
    }

    private void saveDocument(long token, String uri, String baseline, String staged) {
        documents.save(uri, baseline, staged, status -> complete(SAVED, token, status));
    }

    private void exportDocument(long token, String path, String excludedUri, String name) {
        runOnUiThread(() -> {
            pendingExport = new Export(token, path, excludedUri);
            Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT)
                    .setType("text/plain").addCategory(Intent.CATEGORY_OPENABLE)
                    .putExtra(Intent.EXTRA_TITLE, name)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION
                            | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
            try {
                exporter.launch(intent);
            } catch (ActivityNotFoundException exception) {
                pendingExport = null;
                complete(EXPORT_ENDED, token, 1);
            }
        });
    }

    private void updateInput(long token, String text, int anchor, int head, boolean show) {
        runOnUiThread(() -> input.reset(token, text, anchor, head, show));
    }

    private void recenterInput(long previous, long sequence, long token, String text,
                               int anchor, int head) {
        runOnUiThread(() -> {
            boolean applied = input.recenter(previous, sequence, token, text, anchor, head);
            complete(INPUT_RECENTERED, token, applied ? 0 : 1);
        });
    }

    private void hideInput() {
        runOnUiThread(() -> {
            input.dismiss();
            surface.requestFocus();
        });
    }

    private void firstFrame(long rasterMillis) {
        Log.i("Winc", "First editor frame: " + (SystemClock.elapsedRealtime() - started)
                + " ms from Activity.onCreate, " + rasterMillis + " ms from android_main");
    }

    @Override
    protected void onPause() {
        send(PAUSED);
        super.onPause();
    }

    @Override
    protected void onDestroy() {
        documents.close();
        super.onDestroy();
    }
}

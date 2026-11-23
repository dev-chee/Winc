package si.winc.android;

import android.content.Context;
import android.text.Editable;
import android.text.InputType;
import android.text.TextWatcher;
import android.view.KeyEvent;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputConnectionWrapper;
import android.view.inputmethod.InputMethodManager;
import android.view.inputmethod.TextAttribute;

import androidx.annotation.RequiresApi;
import androidx.appcompat.widget.AppCompatEditText;

/** A native input connection for the custom Rust editor, with a bounded editable context. */
final class EditorInput extends AppCompatEditText {
    interface Listener {
        void changed(long token, long sequence, int anchor, int head, int from, int to, String text);
    }

    private record Snapshot(int anchor, int head, int from, int to, String text) {}

    private final InputMethodManager ime;
    private final Listener listener;
    private long token;
    private long sequence;
    private Snapshot last;
    private boolean updating;
    private boolean dispatching;

    EditorInput(Context context, Listener listener) {
        super(context);
        this.listener = listener;
        ime = context.getSystemService(InputMethodManager.class);
        setAlpha(0f);
        setImportantForAccessibility(IMPORTANT_FOR_ACCESSIBILITY_NO);
        setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE);
        setImeOptions(EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_FLAG_NO_FULLSCREEN);
        setFocusableInTouchMode(true);
        addTextChangedListener(new TextWatcher() {
            @Override
            public void beforeTextChanged(CharSequence text, int start, int count, int after) {}

            @Override
            public void onTextChanged(CharSequence text, int start, int before, int count) {}

            @Override
            public void afterTextChanged(Editable text) {
                textChanged();
            }
        });
    }

    void reset(long generation, String text, int anchor, int head, boolean show) {
        updating = true;
        token = generation;
        sequence = 0;
        setText(text);
        setSelection(anchor, head);
        last = snapshot();
        updating = false;
        if (show) {
            requestFocus();
        }
        if (hasFocus()) {
            ime.restartInput(this);
            if (show) {
                ime.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT);
            }
        }
    }

    boolean recenter(long previous, long expectedSequence, long generation,
                     String text, int anchor, int head) {
        publish(previous);
        if (token != previous || sequence != expectedSequence
                || (last.from() >= 0 && last.to() > last.from())) {
            return false;
        }
        reset(generation, text, anchor, head, false);
        return true;
    }

    void dismiss() {
        token = 0;
        ime.hideSoftInputFromWindow(getWindowToken(), 0);
        clearFocus();
    }

    private Snapshot snapshot() {
        Editable text = getText();
        return new Snapshot(getSelectionStart(), getSelectionEnd(),
                BaseInputConnection.getComposingSpanStart(text),
                BaseInputConnection.getComposingSpanEnd(text), text.toString());
    }

    private void publish(long generation) {
        if (generation == 0 || generation != token || updating) {
            return;
        }
        Snapshot next = snapshot();
        if (next.anchor() >= 0 && next.head() >= 0 && !next.equals(last)) {
            last = next;
            listener.changed(generation, ++sequence, next.anchor(), next.head(), next.from(),
                    next.to(), next.text());
        }
    }

    private void textChanged() {
        if (token != 0 && !updating && !dispatching) {
            long generation = token;
            post(() -> publish(generation));
        }
    }

    @Override
    protected void onSelectionChanged(int start, int end) {
        super.onSelectionChanged(start, end);
        textChanged();
    }

    @Override
    public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
        InputConnection connection = super.onCreateInputConnection(outAttrs);
        if (connection == null) {
            return null;
        }
        long generation = token;
        return new InputConnectionWrapper(connection, false) {
            private boolean edit(java.util.function.BooleanSupplier operation) {
                if (generation == 0 || generation != token) {
                    return false;
                }
                dispatching = true;
                boolean result;
                try {
                    result = operation.getAsBoolean();
                } finally {
                    dispatching = false;
                }
                publish(generation);
                return result;
            }

            @Override
            public boolean setComposingText(CharSequence text, int position) {
                return edit(() -> super.setComposingText(text, position));
            }

            @Override
            @RequiresApi(33)
            public boolean setComposingText(CharSequence text, int position, TextAttribute attributes) {
                return edit(() -> super.setComposingText(text, position, attributes));
            }

            @Override
            public boolean setComposingRegion(int start, int end) {
                return edit(() -> super.setComposingRegion(start, end));
            }

            @Override
            @RequiresApi(33)
            public boolean setComposingRegion(int start, int end, TextAttribute attributes) {
                return edit(() -> super.setComposingRegion(start, end, attributes));
            }

            @Override
            public boolean finishComposingText() {
                return edit(super::finishComposingText);
            }

            @Override
            public boolean commitText(CharSequence text, int position) {
                return edit(() -> super.commitText(text, position));
            }

            @Override
            @RequiresApi(33)
            public boolean commitText(CharSequence text, int position, TextAttribute attributes) {
                return edit(() -> super.commitText(text, position, attributes));
            }

            @Override
            @RequiresApi(34)
            public boolean replaceText(int start, int end, CharSequence text, int position, TextAttribute attributes) {
                return edit(() -> super.replaceText(start, end, text, position, attributes));
            }

            @Override
            public boolean deleteSurroundingText(int before, int after) {
                return edit(() -> super.deleteSurroundingText(before, after));
            }

            @Override
            public boolean deleteSurroundingTextInCodePoints(int before, int after) {
                return edit(() -> super.deleteSurroundingTextInCodePoints(before, after));
            }

            @Override
            public boolean setSelection(int start, int end) {
                return edit(() -> super.setSelection(start, end));
            }

            @Override
            public boolean sendKeyEvent(KeyEvent event) {
                return edit(() -> super.sendKeyEvent(event));
            }

            @Override
            public boolean performContextMenuAction(int action) {
                return edit(() -> super.performContextMenuAction(action));
            }
        };
    }
}

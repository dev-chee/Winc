# Winc Android prototype

This debug APK runs Winc's Rust editor on a single Android activity. It opens one
UTF-8 text document of at most 1 MiB, edits it with the Android keyboard, and saves
it back to the selected document. The app keeps the active editor in memory while
the activity goes to the background or its drawing surface is recreated.
A full Activity recreation starts a fresh editor in the same process.

## Build and install

Use JDK 17, Android SDK Platform 36, Build Tools 36.0.0, and NDK
`28.2.13676358`. Set `JAVA_HOME` and `ANDROID_HOME` to the installed toolchains.

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk --version 4.1.2 --locked
cd android
sh ./gradlew :app:assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

On Windows use `gradlew.bat`. For an external NDK installation, use
`-Pwinc.ndkPath=<path> -Pwinc.ndkVersion=<revision>`. The linker is explicitly
configured for 16 KiB load-segment alignment even with an older NDK.

## Verify on a phone

1. Copy a UTF-8 text fixture with several screens of lines to Downloads. Include
   Chinese and emoji in the fixture. Launch the app and choose it with **Open**.
2. Scroll, tap to place the caret, and open **Keyboard**. Check Chinese
   composition and commit, selection replacement, backspace, line breaks, and
   **Undo**. Check that the keyboard leaves the caret visible.
3. Switch to another app and return. Rotate the screen and verify that content,
   unsaved edits, and the caret survive. Check the `Winc` log tag for first-frame
   timings and use `adb shell dumpsys meminfo si.winc.android.poc` for memory.
4. Delete part of the fixture and tap **Save**. Reopen it in the system Files
   app to confirm the exact saved bytes and truncation. Cancel the picker and
   check that the open document is still present. Modify the source externally
   and confirm that Winc reports a conflict and keeps local edits.
5. For a provider that offers read-only access or revocable grants, exercise
   both cases and confirm that unsuccessful saves leave the editor dirty. Use
   **More → Save a copy** to export the current edits and **More → Export last
   saved version** to export the baseline. Reopen both files to verify them.

The APK should pass `zipalign -c -P 16 -v 4 app-debug.apk`. Inspect the packaged
`libwinc_android.so` with `llvm-readelf -lW` and check that all `LOAD` segments
are aligned to at least `0x4000`.

The prototype stores the last confirmed version and staged drafts in the app's
private directory. Save uses a seekable read/write descriptor and compares the
baseline on that descriptor before writing. Winc truncates only after writing
the replacement, then flushes and verifies the provider's bytes. Storage Access
Framework does not provide an atomic compare-and-replace operation across
providers. If the provider rejects read/write access or the result is uncertain,
the editor can export the draft and the last confirmed version separately.
Project folders, desktop tools, and session recovery after Activity destruction
or process death are outside the prototype.

## Device validation on November 23, 2026

- Pixel 2 XL on Android 11 (API 30). Opened a 222-line fixture in the system
  picker, displayed Chinese and emoji, scrolled to line 203, and placed the
  caret by touch. A test Android IME exercised composition, commit, selection
  replacement, backspace, newline, and undo through the system input connection.
- Switched to the launcher and returned, then rotated the screen in both
  directions before saving. Provider read-back matched the expected UTF-8 bytes
  exactly after shortening the file from 1901 bytes to 1891 bytes.
- Cancelling the picker preserved the current document. Read-only access,
  revoking the URI grant, and an external edit left the provider's bytes intact
  and preserved unsaved edits in Winc.
- Two startup samples reached the first editor frame in 522 ms and 360 ms from
  `Activity.onCreate`. The loaded-editor sample reported 102692 KiB total PSS
  with the test keyboard shown. These are debug-build measurements.
- The updated APK kept the source bytes unchanged when Save was tapped during
  Chinese composition. Committing the selected replacement saved `你好` and
  shortened the fixture from 1901 bytes to 1891 bytes. An external edit survived
  a conflicting save. Both exports reopened from Downloads, with SHA-256
  matching the staged edit and the last confirmed version respectively.
- A separate fixture containing Chinese and emoji retained CRLF byte for byte
  after selecting text, committing a replacement and inserting a line break
  through the Android input connection.
- Changing the system font scale recreated the Activity and produced another
  first frame under the same process ID.
- The Windows test run for the updated code passed 1066 tests across the affected
  app, renderer, and localization crates, including the literal insertion,
  CRLF, UTF-16, and input context regressions. The app passed `cargo check` for
  x86-64 Linux and ARM64 macOS, cross-checked from Windows.

The delivered APK was built with NDK r28c. Android lint passed with five
warnings. Signature verification, `zipalign -P 16`, and all four ELF `LOAD`
segment alignments passed.
`release/out/winc-android-poc-arm64-debug.apk` is 44820188 bytes and has SHA-256
`c8825db01f560c29960b6bd9c404b4d587ee0aa576f3725c4e466852bccf8130`.

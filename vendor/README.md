# Vendored dependencies

`winit` is the published 0.30.13 crate. The Android build releases the
event-loop reservation after the old Activity's backend drops, and exits
the loop when Android destroys that Activity. The other platforms retain
their event-loop policy. See [winit issue 3325](https://github.com/rust-windowing/winit/issues/3325).

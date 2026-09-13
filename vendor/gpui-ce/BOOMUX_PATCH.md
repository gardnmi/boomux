# Boomux GPUI patch

Based on the crates.io gpui-ce 0.2.2 release (Apache-2.0).

Adds `Window::with_slanted_content_mask` and primitive clipping in `scene.rs`.
This paint-only mask preserves layout and glyph geometry. Only primitives crossing
an edge are split into rectangular masks. Work is bounded to 512 bands per
primitive; fully inside/outside primitives take a constant-time path. The GPU
primitive ABI and platform shaders are unchanged. Nested slanted masks replace
the previous slant during their scope; existing rectangular clipping remains.

Upstream changes are confined to src/scene.rs, src/window.rs, and the new
src/slanted_mask.rs (plus removal of two trailing spaces in upstream
src/_accessibility.rs documentation). The standalone geometry tests avoid building upstream's
unrelated dev dependencies:

```sh
rustc --edition 2024 --test vendor/gpui-ce/src/slanted_mask.rs -o /tmp/boomux-slanted-mask-tests
/tmp/boomux-slanted-mask-tests
```

Original crates.io checksum:
`b0af79c6659e0fea67773cfbd751fc5c8e51be00139827f365d7d1237a468a4d`.


Local geometry microbenchmark: 19,200 glyph rectangles at 1920×1280, centered
half-width reveal with 307.2px slant, 200 iterations, rustc -O: 0.072ms/frame
and 10,424 emitted masks/frame. This excludes scene insertion and GPU rendering;
it is not an end-to-end animation frame-time measurement. Normal Desktop CI
includes the standalone geometry tests via theme_picker.rs.

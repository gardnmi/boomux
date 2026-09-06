# Portable CPU patch

Source: libghostty-vt-sys 0.2.1 from crates.io, upstream
https://github.com/uzaaft/libghostty-rs at
46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0.
The upstream LICENSE is included.

The only upstream code change adds `-Dcpu=baseline` to the vendored Zig build.
Keep the generated bindings and pinned Ghostty revision unchanged.
Remove this override when a reviewed upstream release provides portable native
builds.

Without this option, Zig selects the build host's CPU for native builds. The
linked compiler-rt exports optimized memset/memcpy routines into the whole app,
so unsupported instructions can crash font loading before a terminal is opened.
CI run 34010759760 emitted an AVX-512 register-source vpbroadcastb in memset and
failed with SIGILL in both display jobs. The exact artifact reproduced locally;
the core's call stack reached memset through fontconfig-parser and cosmic-text.

X11 smoke coverage runs Desktop with QEMU's Nehalem CPU (no AVX), while Wayland
runs natively. The CI cache key changed to exclude prior native-CPU artifacts.

//! Display-independent checks used before activating a downloaded release.

pub fn check() -> Result<(), String> {
    let mut missing = Vec::new();
    for library in [
        c"libfontconfig.so.1",
        c"libwayland-client.so.0",
        c"libX11.so.6",
        c"libxcb.so.1",
        c"libxcb-shape.so.0",
        c"libxcb-xfixes.so.0",
        c"libxkbcommon.so.0",
        c"libxkbcommon-x11.so.0",
        c"libvulkan.so.1",
    ] {
        // These are fixed system libraries. No symbols or borrowed pointers
        // escape the matching open/close pair.
        unsafe {
            let handle = libc::dlopen(library.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if handle.is_null() {
                missing.push(library.to_string_lossy().into_owned());
            } else {
                libc::dlclose(handle);
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Missing graphics libraries: {}.\nUbuntu/Debian packages: libfontconfig1 libwayland-client0 libx11-6 libxcb1 libxcb-shape0 libxcb-xfixes0 libxkbcommon0 libxkbcommon-x11-0 libvulkan1.\nArch packages: fontconfig wayland libx11 libxcb libxkbcommon libxkbcommon-x11 vulkan-icd-loader.\nAlso install the Vulkan driver for your GPU. No packages have been installed automatically.",
            missing.join(", ")
        ))
    }
}

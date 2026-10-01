/// Called on GPUI's UI thread, after releasing the Workspace borrow. The view
/// comes from GPUI's live AppKit window handle and is never retained or stored.
pub(crate) fn discard_marked_text(window: &impl raw_window_handle::HasWindowHandle) {
    use objc2::{msg_send, runtime::AnyObject};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: GPUI owns this live NSView; Window and this deferred callback run
    // on the AppKit main thread. inputContext is a borrowed NSTextInputContext.
    unsafe {
        let view = handle.ns_view.as_ptr().cast::<AnyObject>();
        let context: *mut AnyObject = msg_send![view, inputContext];
        if !context.is_null() {
            let _: () = msg_send![context, discardMarkedText];
        }
    }
}

//! One AppKit adjustment: making `Cmd+Q` ask before quitting.
//!
//! winit builds the macOS application menu itself, and its Quit item is wired
//! straight to `terminate:` with `Cmd+Q` as the key equivalent. Menu key
//! equivalents are matched before the event reaches the key window, so the
//! process dies before any Rust in this app runs — no `CloseRequested`, no
//! keystroke, no chance to ask about unsaved work. That was measured, not
//! assumed: with a dirty buffer, `Cmd+Q` killed the process outright.
//!
//! The fix is one selector. Retargeting the item to `performClose:` sends it
//! down the responder chain to the key window, which closes, which is exactly
//! the path the window's own close button takes — and that path already routes
//! through `Message::CloseRequested` and the confirmation. Quit then behaves the
//! same however it's asked for.
//!
//! Deliberately *not* done by replacing the application delegate: winit owns
//! that, and taking it over to implement `applicationShouldTerminate:` would put
//! us in the middle of its event handling for no extra benefit.

#[cfg(target_os = "macos")]
pub fn route_quit_through_window_close() {
    use objc2::sel;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;

    // `None` means we aren't on the main thread, where AppKit state must be
    // touched. The caller runs from `update`, which iced drives from the winit
    // event loop, so this holds — but it's checked rather than assumed.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(menubar) = app.mainMenu() else {
        return;
    };
    // The application menu is the first item, and Quit lives in its submenu.
    // Located by *selector* rather than by title or index, so a localised or
    // reordered menu still matches.
    for i in 0..menubar.numberOfItems() {
        let Some(top) = menubar.itemAtIndex(i) else {
            continue;
        };
        let Some(submenu) = top.submenu() else {
            continue;
        };
        for j in 0..submenu.numberOfItems() {
            let Some(item) = submenu.itemAtIndex(j) else {
                continue;
            };
            if item.action() == Some(sel!(terminate:)) {
                unsafe { item.setAction(Some(sel!(performClose:))) };
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn route_quit_through_window_close() {}

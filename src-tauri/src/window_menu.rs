//! Ask the window manager to open its own window menu — the one a titlebar
//! right-click shows, with "Always on Top", workspace moves, etc.
//!
//! Linux-only. On Wayland this is the *only* way to offer a working
//! keep-above: compositors ignore app-side always-on-top requests, but apply
//! the toggle themselves when picked from their own menu. GTK exposes the
//! request as `gdk_window_show_window_menu()` (xdg_toplevel.show_window_menu
//! on Wayland, `_GTK_SHOW_WINDOW_MENU` on X11); it needs a button event, so
//! we synthesize one carrying the pointer device and the click position. The
//! compositor takes the input serial from the seat's real right-click that
//! just happened, not from our synthetic event.

#[cfg(target_os = "linux")]
pub fn show(window: tauri::WebviewWindow, x: f64, y: f64) -> bool {
    use gdk::glib::translate::{ToGlibPtr, ToGlibPtrMut};
    use gdk::prelude::*;
    use gtk::prelude::*;
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();
    let win = window.clone();
    // GTK is single-threaded; commands run off the main thread.
    let queued = window
        .run_on_main_thread(move || {
            let shown = (|| -> Option<bool> {
                let gdk_win = win.gtk_window().ok()?.window()?;
                let device = gdk_win.display().default_seat()?.pointer()?;
                let (root_x, root_y) = gdk_win.root_coords(x as i32, y as i32);

                let mut event = gdk::Event::new(gdk::EventType::ButtonPress);
                unsafe {
                    let raw: *mut gdk::ffi::GdkEvent = event.to_glib_none_mut().0;
                    let ev = raw as *mut gdk::ffi::GdkEventButton;
                    // The event owns its window reference (released on free).
                    (*ev).window = ToGlibPtr::<*mut gdk::ffi::GdkWindow>::to_glib_full(&gdk_win);
                    (*ev).time = gdk::ffi::GDK_CURRENT_TIME as u32;
                    (*ev).x = x;
                    (*ev).y = y;
                    (*ev).x_root = root_x as f64;
                    (*ev).y_root = root_y as f64;
                    (*ev).button = 3;
                }
                event.set_device(Some(&device));

                Some(gdk_win.show_window_menu(&mut event))
            })()
            .unwrap_or(false);
            let _ = tx.send(shown);
        })
        .is_ok();

    queued && rx.recv_timeout(Duration::from_millis(500)).unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub fn show(_window: tauri::WebviewWindow, _x: f64, _y: f64) -> bool {
    false
}

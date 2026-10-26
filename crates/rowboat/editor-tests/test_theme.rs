//! Keep production component bindings and the initial palette, without live I/O.
use gpui_omarchy::ActiveTheme;

pub(crate) fn init(cx: &mut gpui_kit::App) {
    // Omarchy 0.2.1 has no separate public component-only initializer. Explicit
    // Theme::apply is its supported API for stopping system-theme tracking.
    // Do both in the same App update: cancel the watcher task before it can be
    // polled and register a foreground waker with notify-rs's external thread.
    gpui_omarchy::init(cx);
    cx.omarchy().clone().apply(cx);
}

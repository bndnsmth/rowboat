//! Indeterminate activity, mounted only while the owning operation is pending.
use gpui_kit::{prelude::*, *};
use gpui_omarchy::ActiveTheme;
use std::{f32::consts::TAU, time::Duration};

#[derive(Clone, Copy)]
enum Current {
    Rail,
    Wave,
    Status,
}

// One shared, gently eased current: no task, model mutation, or result-data copy.
// GPUI's animation wrapper also honors the platform's reduced-motion setting.
fn current(kind: Current, window: &Window, cx: &App) -> AnyElement {
    let accent = cx.omarchy().accent;
    let (id, width, height) = match kind {
        Current::Rail => ("loading-rail", None, 3.),
        Current::Wave => ("loading-wave", Some(156.), 36.),
        Current::Status => ("loading-status", Some(22.), 14.),
    };
    let element = div()
        .id(id)
        .debug_selector(move || id.into())
        .relative()
        .flex_shrink_0()
        .h(px(height))
        .overflow_hidden()
        .map(|d| match width {
            Some(width) => d.w(px(width)),
            None => d.w_full().bg(accent.opacity(0.10)),
        });
    let paint = move |d: Stateful<Div>, phase: f32| match kind {
        Current::Rail => {
            // A tapered ribbon travels out of the clipped rail before wrapping.
            // Its length is indeterminate; it never represents percent complete.
            let head = -0.30 + 1.60 * ease_in_out(phase);
            d.children((0..20).map(move |i| {
                let position = i as f32 / 19.;
                let envelope = (std::f32::consts::PI * position).sin().powi(2);
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(relative(head - position * 0.30))
                    .w(relative(0.017))
                    .bg(accent.opacity(0.12 + 0.76 * envelope))
            }))
        }
        Current::Wave | Current::Status => {
            let compact = matches!(kind, Current::Status);
            let count = if compact { 3 } else { 9 };
            let step = if compact { 8. } else { 18. };
            let dash = if compact { 5. } else { 12. };
            let amplitude = if compact { 2. } else { 5. };
            d.children((0..count).map(move |i| {
                let wave = (TAU * (phase - i as f32 * 0.075)).sin();
                div()
                    .absolute()
                    .left(px(i as f32 * step))
                    .top(px(height / 2. - 1.5 + wave * amplitude))
                    .w(px(dash))
                    .h(px(3.))
                    .rounded_full()
                    .bg(accent.opacity(0.48 + 0.30 * wave))
            }))
        }
    };
    if !window.is_visible() || cx.reduce_motion() {
        return paint(element, 0.45).into_any_element();
    }
    element
        .with_animation(
            "current-motion",
            Animation::new(Duration::from_millis(2400))
                .repeat_synced()
                .with_max_fps(30.),
            paint,
        )
        .into_any_element()
}

pub fn status(window: &Window, cx: &App) -> AnyElement {
    current(Current::Status, window, cx)
}

pub fn rail(window: &Window, cx: &App) -> AnyElement {
    current(Current::Rail, window, cx)
}

pub fn header(label: &'static str, window: &Window, cx: &App) -> AnyElement {
    crate::view::column()
        .flex_shrink_0()
        .child(
            div()
                .px_3()
                .py_1()
                .text_size(px(11.))
                .text_color(cx.omarchy().secondary)
                .child(label),
        )
        .child(rail(window, cx))
        .into_any_element()
}

pub fn waiting(label: &'static str, window: &Window, cx: &App) -> AnyElement {
    let theme = cx.omarchy();
    crate::view::column()
        .id("query-loading")
        .debug_selector(|| "query-loading".into())
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap_2()
        .py_4()
        .child(current(Current::Wave, window, cx))
        .child(
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.bright)
                .child(label),
        )
        .child(div().text_size(px(12.)).text_color(theme.secondary).child(
            if label == "Finishing query…" {
                "Waiting for query cleanup"
            } else {
                "Results will appear as they arrive"
            },
        ))
        .into_any_element()
}

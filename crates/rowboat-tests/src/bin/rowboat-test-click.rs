//! Local native-GUI test helper. No daemon, device privileges, or host installation.
//! Coordinates are global layout positions, with explicit layout extents.
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("rowboat-test-click requires a Linux Wayland test session")
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, ensure};
    use wayland_client::{
        Connection, Dispatch, QueueHandle, delegate_noop,
        protocol::{wl_pointer, wl_registry},
    };
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

    #[derive(Default)]
    struct State {
        manager: Option<ZwlrVirtualPointerManagerV1>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
                && interface == "zwlr_virtual_pointer_manager_v1"
            {
                state.manager = Some(registry.bind(name, version.min(2), qh, ()));
            }
        }
    }
    delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
    delegate_noop!(State: ignore ZwlrVirtualPointerV1);

    pub fn run() -> Result<()> {
        let mut args = std::env::args().skip(1).collect::<Vec<_>>();
        let horizontal = args.iter().any(|arg| arg == "--horizontal");
        let wheel = args.iter().any(|arg| arg == "--wheel");
        args.retain(|arg| arg != "--horizontal" && arg != "--wheel");
        let values = args
            .into_iter()
            .map(|value| value.parse::<i32>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            (4..=6).contains(&values.len()),
            "Usage: rowboat-test-click X Y LAYOUT_WIDTH LAYOUT_HEIGHT [SIGNED_SCROLL_PIXELS [--horizontal] [--wheel] | DRAG_X DRAG_Y] (positive scroll down/right; focus the task-owned test window first)"
        );
        ensure!(
            !(horizontal || wheel) || values.len() == 5,
            "Scroll flags require a scroll distance"
        );
        let [x, y, width, height] = &values[..4] else {
            unreachable!()
        };
        ensure!(
            *width > 0 && *height > 0 && *x >= 0 && *y >= 0 && x < width && y < height,
            "Coordinates must lie within explicit layout extents"
        );
        if values.len() == 6 {
            ensure!(
                values[4] >= 0 && values[5] >= 0 && values[4] < *width && values[5] < *height,
                "Drag destination must lie within explicit layout extents"
            );
        }
        let connection =
            Connection::connect_to_env().context("Connect to the test Wayland session")?;
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        let manager = state
            .manager
            .take()
            .context("Compositor does not provide the virtual-pointer test protocol")?;
        let pointer = manager.create_virtual_pointer(None, &handle, ());
        pointer.motion_absolute(0, *x as u32, *y as u32, *width as u32, *height as u32);
        pointer.frame();
        if values.len() == 6 {
            pointer.button(1, 0x110, wl_pointer::ButtonState::Pressed);
            pointer.frame();
            queue.roundtrip(&mut state)?;
            for step in 1..=12 {
                let dx = *x + (values[4] - *x) * step / 12;
                let dy = *y + (values[5] - *y) * step / 12;
                pointer.motion_absolute(
                    step as u32 + 1,
                    dx as u32,
                    dy as u32,
                    *width as u32,
                    *height as u32,
                );
                pointer.frame();
                queue.roundtrip(&mut state)?;
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            pointer.button(15, 0x110, wl_pointer::ButtonState::Released);
            pointer.frame();
        } else if let Some(distance) = values.get(4) {
            let axis = if horizontal {
                wl_pointer::Axis::HorizontalScroll
            } else {
                wl_pointer::Axis::VerticalScroll
            };
            if wheel {
                pointer.axis_source(wl_pointer::AxisSource::Wheel);
                pointer.axis_discrete(1, axis, *distance as f64, distance.signum());
            } else {
                pointer.axis(1, axis, *distance as f64);
            }
            pointer.frame();
            pointer.axis_stop(2, axis);
            pointer.frame();
        } else {
            pointer.button(1, 0x110, wl_pointer::ButtonState::Pressed);
            pointer.frame();
            pointer.button(2, 0x110, wl_pointer::ButtonState::Released);
            pointer.frame();
        }
        queue.roundtrip(&mut state)?;
        pointer.destroy();
        connection.flush()?;
        Ok(())
    }
}

use rd_proto::InputEvent;

/// Where a picture pixel lands on the Windows virtual desktop.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct DesktopTarget {
    pub origin_x: i32,
    pub origin_y: i32,
    pub desktop_w: u32,
    pub desktop_h: u32,
    pub encoded_w: u32,
    pub encoded_h: u32,
}

/// Map a coordinate in the encoded picture onto a desktop pixel.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn desktop_pixel(x: u16, y: u16, target: DesktopTarget) -> (i32, i32) {
    let encoded_w = target.encoded_w.max(1);
    let encoded_h = target.encoded_h.max(1);
    let x = u32::from(x).min(encoded_w - 1);
    let y = u32::from(y).min(encoded_h - 1);
    let desktop_x = (u64::from(x) * u64::from(target.desktop_w) / u64::from(encoded_w)) as i32;
    let desktop_y = (u64::from(y) * u64::from(target.desktop_h) / u64::from(encoded_h)) as i32;
    (target.origin_x + desktop_x, target.origin_y + desktop_y)
}

/// `MOUSEEVENTF_ABSOLUTE` coordinate across the virtual screen, 0..=65535.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn virtual_absolute(
    desktop_x: i32,
    desktop_y: i32,
    virt_left: i32,
    virt_top: i32,
    virt_w: i32,
    virt_h: i32,
) -> (i32, i32) {
    let width = virt_w.max(1);
    let height = virt_h.max(1);
    let x = (desktop_x - virt_left).clamp(0, width - 1);
    let y = (desktop_y - virt_top).clamp(0, height - 1);
    let ax = (i64::from(x) * 65535) / i64::from((width - 1).max(1));
    let ay = (i64::from(y) * 65535) / i64::from((height - 1).max(1));
    (ax as i32, ay as i32)
}

pub fn inject(events: &[InputEvent], target: DesktopTarget) {
    if events.is_empty() || target.encoded_w == 0 || target.encoded_h == 0 {
        return;
    }
    #[cfg(windows)]
    {
        inject_windows(events, target);
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        tracing::debug!(
            count = events.len(),
            "input injection is implemented with SendInput on Windows"
        );
    }
}

#[cfg(windows)]
fn inject_windows(events: &[InputEvent], target: DesktopTarget) {
    use std::mem::size_of;

    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
        MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
        MOUSEEVENTF_XUP, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    let (virt_left, virt_top, virt_w, virt_h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    let mut batch = Vec::with_capacity(events.len());
    for event in events {
        match *event {
            InputEvent::MouseMove { x, y } => {
                let (desktop_x, desktop_y) = desktop_pixel(x, y, target);
                let (dx, dy) =
                    virtual_absolute(desktop_x, desktop_y, virt_left, virt_top, virt_w, virt_h);
                batch.push(mouse_input(
                    dx,
                    dy,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ));
            }
            InputEvent::MouseButton { button, pressed } => {
                let (flags, data) = match (button, pressed) {
                    (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (0, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (3, true) => (MOUSEEVENTF_XDOWN, 0x0001),
                    (3, false) => (MOUSEEVENTF_XUP, 0x0001),
                    (4, true) => (MOUSEEVENTF_XDOWN, 0x0002),
                    (4, false) => (MOUSEEVENTF_XUP, 0x0002),
                    _ => continue,
                };
                batch.push(mouse_input(0, 0, data, flags));
            }
            InputEvent::Wheel { dx, dy } => {
                if dy != 0 {
                    batch.push(mouse_input(0, 0, dy as u16 as u32, MOUSEEVENTF_WHEEL));
                }
                if dx != 0 {
                    batch.push(mouse_input(0, 0, dx as u16 as u32, MOUSEEVENTF_HWHEEL));
                }
            }
            InputEvent::Key { vk, scan, pressed } => {
                let mut flags = windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0);
                if !pressed {
                    flags |= KEYEVENTF_KEYUP;
                }
                if scan != 0 {
                    flags |= windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_SCANCODE;
                }
                batch.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(vk),
                            wScan: scan,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
    }
    if batch.is_empty() {
        return;
    }
    let sent = unsafe { SendInput(&batch, size_of::<INPUT>() as i32) };
    if sent == 0 {
        tracing::warn!("SendInput rejected the input batch");
    }
}

#[cfg(windows)]
fn mouse_input(
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_MOUSE, MOUSEINPUT};
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_the_encoded_picture_onto_a_second_monitor() {
        let target = DesktopTarget {
            origin_x: 1920,
            origin_y: 0,
            desktop_w: 1920,
            desktop_h: 1080,
            encoded_w: 960,
            encoded_h: 540,
        };
        assert_eq!(desktop_pixel(480, 270, target), (2880, 540));
    }

    #[test]
    fn absolute_coordinates_span_the_virtual_screen() {
        assert_eq!(virtual_absolute(0, 0, 0, 0, 1920, 1080), (0, 0));
        let (x, y) = virtual_absolute(1919, 1079, 0, 0, 1920, 1080);
        assert_eq!((x, y), (65535, 65535));
    }
}

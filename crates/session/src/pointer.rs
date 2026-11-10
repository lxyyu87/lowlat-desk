/// Map a window position onto host capture pixels, preserving aspect ratio
/// and ignoring the letterbox bars.
pub fn map_pointer(
    x: f32,
    y: f32,
    window_w: u32,
    window_h: u32,
    frame_w: u32,
    frame_h: u32,
) -> Option<(u16, u16)> {
    if window_w == 0 || window_h == 0 || frame_w == 0 || frame_h == 0 {
        return None;
    }
    let scale = (window_w as f32 / frame_w as f32).min(window_h as f32 / frame_h as f32);
    if scale <= 0.0 {
        return None;
    }
    let content_w = frame_w as f32 * scale;
    let content_h = frame_h as f32 * scale;
    let origin_x = (window_w as f32 - content_w) * 0.5;
    let origin_y = (window_h as f32 - content_h) * 0.5;
    if x < origin_x || y < origin_y || x >= origin_x + content_w || y >= origin_y + content_h {
        return None;
    }
    let frame_x = ((x - origin_x) / scale).clamp(0.0, frame_w.saturating_sub(1) as f32);
    let frame_y = ((y - origin_y) / scale).clamp(0.0, frame_h.saturating_sub(1) as f32);
    Some((frame_x as u16, frame_y as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_maps_the_center_and_ignores_bars() {
        let (x, y) = map_pointer(200.0, 100.0, 400, 200, 200, 100).unwrap();
        assert_eq!((x, y), (100, 50));
        assert!(map_pointer(10.0, 100.0, 400, 200, 100, 100).is_none());
    }
}

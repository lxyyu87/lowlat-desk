//! Tightly packed BGRA/RGBA and NV12 helpers.
//!
//! Screen capture arrives as BGRA. Media Foundation's H.264 encoder takes
//! NV12. The client presents RGBA to the window.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelError(&'static str);

impl std::fmt::Display for PixelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PixelError {}

/// Even dimensions, optionally scaled so the width does not exceed `max_width`.
/// `max_width == 0` keeps the source size (still forced even).
pub fn fit_capture_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    let mut w = width.max(2);
    let mut h = height.max(2);
    if max_width >= 2 && w > max_width {
        h = ((h as u64 * max_width as u64) / w as u64).max(2) as u32;
        w = max_width;
    }
    w &= !1;
    h &= !1;
    (w.max(2), h.max(2))
}

pub fn bgra_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

pub fn nv12_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn yuv601(r: i32, g: i32, b: i32) -> (u8, u8, u8) {
    // BT.601 limited range, the range H.264 decoders assume by default.
    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (clamp_u8(y), clamp_u8(u), clamp_u8(v))
}

/// `src_stride` is bytes per row and may be larger than `width * 4`.
pub fn bgra_to_nv12(
    src: &[u8],
    width: u32,
    height: u32,
    src_stride: usize,
    dst: &mut [u8],
) -> Result<(), PixelError> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(PixelError("NV12 requires even dimensions"));
    }
    let w = width as usize;
    let h = height as usize;
    if src_stride < w * 4 || src.len() < src_stride * h || dst.len() < nv12_len(width, height) {
        return Err(PixelError("buffer is smaller than the frame"));
    }
    let (y_plane, uv_plane) = dst.split_at_mut(w * h);
    for y in 0..h {
        let row = &src[y * src_stride..y * src_stride + w * 4];
        for x in 0..w {
            let px = &row[x * 4..x * 4 + 4];
            let (yy, _, _) = yuv601(px[2] as i32, px[1] as i32, px[0] as i32);
            y_plane[y * w + x] = yy;
        }
    }
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let mut u_acc = 0i32;
            let mut v_acc = 0i32;
            for dy in 0..2 {
                let row = &src[(y + dy) * src_stride..(y + dy) * src_stride + w * 4];
                for dx in 0..2 {
                    let px = &row[(x + dx) * 4..(x + dx) * 4 + 4];
                    let (_, u, v) = yuv601(px[2] as i32, px[1] as i32, px[0] as i32);
                    u_acc += u as i32;
                    v_acc += v as i32;
                }
            }
            let uv_index = (y / 2) * w + x;
            uv_plane[uv_index] = (u_acc / 4) as u8;
            uv_plane[uv_index + 1] = (v_acc / 4) as u8;
        }
    }
    Ok(())
}

pub fn nv12_to_rgba(src: &[u8], width: u32, height: u32, dst: &mut [u8]) -> Result<(), PixelError> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(PixelError("NV12 requires even dimensions"));
    }
    let w = width as usize;
    let h = height as usize;
    if src.len() < nv12_len(width, height) || dst.len() < bgra_len(width, height) {
        return Err(PixelError("buffer is smaller than the frame"));
    }
    let y_plane = &src[..w * h];
    let uv_plane = &src[w * h..];
    for y in 0..h {
        for x in 0..w {
            let yy = y_plane[y * w + x] as i32;
            let uv_index = (y / 2) * w + (x & !1);
            let u = uv_plane[uv_index] as i32 - 128;
            let v = uv_plane[uv_index + 1] as i32 - 128;
            let c = yy - 16;
            let r = (298 * c + 409 * v + 128) >> 8;
            let g = (298 * c - 100 * u - 208 * v + 128) >> 8;
            let b = (298 * c + 516 * u + 128) >> 8;
            let o = (y * w + x) * 4;
            dst[o] = clamp_u8(r);
            dst[o + 1] = clamp_u8(g);
            dst[o + 2] = clamp_u8(b);
            dst[o + 3] = 255;
        }
    }
    Ok(())
}

pub fn bgra_to_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    src_stride: usize,
    dst: &mut [u8],
) -> Result<(), PixelError> {
    let w = width as usize;
    let h = height as usize;
    if src_stride < w * 4 || src.len() < src_stride * h || dst.len() < w * h * 4 {
        return Err(PixelError("buffer is smaller than the frame"));
    }
    for y in 0..h {
        let row = &src[y * src_stride..y * src_stride + w * 4];
        let out = &mut dst[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            out[x * 4] = row[x * 4 + 2];
            out[x * 4 + 1] = row[x * 4 + 1];
            out[x * 4 + 2] = row[x * 4];
            out[x * 4 + 3] = row[x * 4 + 3];
        }
    }
    Ok(())
}

/// Box-filter scale of tightly packed or strided BGRA. Output is tightly packed.
pub fn scale_bgra(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: usize,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, PixelError> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Err(PixelError("zero dimension"));
    }
    let sw = src_w as usize;
    let sh = src_h as usize;
    if src_stride < sw * 4 || src.len() < src_stride * sh {
        return Err(PixelError("buffer is smaller than the frame"));
    }
    if src_w == dst_w && src_h == dst_h && src_stride == sw * 4 {
        return Ok(src.to_vec());
    }
    let dw = dst_w as usize;
    let dh = dst_h as usize;
    let mut dst = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = ((y + 1) * sh / dh).max(y0 + 1).min(sh);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = ((x + 1) * sw / dw).max(x0 + 1).min(sw);
            let mut acc = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1 {
                let row = &src[sy * src_stride..sy * src_stride + sw * 4];
                for sx in x0..x1 {
                    let px = &row[sx * 4..sx * 4 + 4];
                    acc[0] += px[0] as u32;
                    acc[1] += px[1] as u32;
                    acc[2] += px[2] as u32;
                    acc[3] += px[3] as u32;
                    count += 1;
                }
            }
            let o = (y * dw + x) * 4;
            for channel in 0..4 {
                dst[o + channel] = (acc[channel] / count.max(1)) as u8;
            }
        }
    }
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_scales_and_forces_even_sizes() {
        assert_eq!(fit_capture_size(1920, 1080, 1280), (1280, 720));
        assert_eq!(fit_capture_size(1919, 1080, 0), (1918, 1080));
        assert_eq!(fit_capture_size(100, 100, 64), (64, 64));
    }

    #[test]
    fn red_bgra_roundtrips_through_nv12_approximately() {
        let width = 2u32;
        let height = 2u32;
        let bgra = [
            0u8, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
        ];
        let mut nv12 = vec![0u8; nv12_len(width, height)];
        bgra_to_nv12(&bgra, width, height, 8, &mut nv12).unwrap();
        assert_eq!(nv12[0], 82);
        let mut rgba = vec![0u8; bgra_len(width, height)];
        nv12_to_rgba(&nv12, width, height, &mut rgba).unwrap();
        assert!(rgba[0] > 200, "red channel {}", rgba[0]);
        assert!(rgba[2] < 40, "blue channel {}", rgba[2]);
    }

    #[test]
    fn scale_averages_a_two_by_two_block() {
        let src = [0u8, 0, 0, 255, 100, 0, 0, 255, 0, 0, 0, 255, 100, 0, 0, 255];
        let dst = scale_bgra(&src, 2, 2, 8, 1, 1).unwrap();
        assert_eq!(dst, vec![50, 0, 0, 255]);
    }
}

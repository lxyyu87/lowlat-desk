//! VideoToolbox H.264 decode for the macOS client.
#![allow(deprecated)]
//!
//! The session is created from the SPS and PPS carried on a keyframe. Modern
//! macOS uses a hardware decoder when one is available and falls back to
//! Apple's software decoder inside VideoToolbox. If the session cannot be
//! created, this module drops through to OpenH264.

use std::ffi::c_void;
use std::ptr::null_mut;

use anyhow::{bail, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMBlockBuffer, CMBlockBufferCreateWithMemoryBlock, CMBlockBufferReplaceDataBytes,
    CMFormatDescription, CMSampleBuffer, CMSampleBufferCreate, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferIsPlanar,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession,
};
use rd_proto::{iter_annex_b_nals, nal_type};

use crate::engine::{DecodedPicture, VideoDecoder};

const START: &[u8] = &[0, 0, 0, 1];
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");

struct FrameSlot {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    ready: bool,
}

struct VtSession {
    session: CFRetained<VTDecompressionSession>,
    _format: CFRetained<CMFormatDescription>,
    slot: Box<FrameSlot>,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

pub struct VtDecoder {
    inner: Option<VtSession>,
    failed: bool,
    #[cfg(feature = "software-openh264")]
    software: Option<crate::soft::OpenH264Decoder>,
}

// VideoToolbox sessions are used from the decode thread only.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    pub fn new() -> Self {
        Self {
            inner: None,
            failed: false,
            #[cfg(feature = "software-openh264")]
            software: None,
        }
    }

    fn decode_vt(&mut self, annex_b: &[u8]) -> Result<Option<DecodedPicture>> {
        let (sps, pps, slices) = split_access_unit(annex_b)?;
        if self.inner.is_none() {
            if sps.is_empty() || pps.is_empty() {
                bail!("videotoolbox is waiting for SPS/PPS");
            }
            self.inner = Some(VtSession::create(&sps, &pps)?);
        } else if !sps.is_empty() && !pps.is_empty() {
            let changed = self
                .inner
                .as_ref()
                .is_some_and(|session| session.sps != sps || session.pps != pps);
            if changed {
                self.inner = Some(VtSession::create(&sps, &pps)?);
            }
        }
        let session = self.inner.as_mut().expect("session");
        if slices.is_empty() {
            return Ok(None);
        }
        session.decode_slices(&slices)?;
        if !session.slot.ready {
            return Ok(None);
        }
        session.slot.ready = false;
        Ok(Some(DecodedPicture {
            width: session.slot.width,
            height: session.slot.height,
            rgba: std::mem::take(&mut session.slot.rgba),
        }))
    }
}

impl Default for VtDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoDecoder for VtDecoder {
    fn name(&self) -> &str {
        if self.failed {
            "openh264"
        } else {
            "videotoolbox"
        }
    }

    fn is_hardware(&self) -> bool {
        !self.failed
    }

    fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedPicture>> {
        if self.failed {
            return self.decode_software(annex_b);
        }
        match self.decode_vt(annex_b) {
            Ok(picture) => Ok(picture),
            Err(err) => {
                tracing::warn!(error = %err, "videotoolbox decode failed; using openh264");
                self.failed = true;
                self.inner = None;
                self.decode_software(annex_b)
            }
        }
    }
}

impl VtDecoder {
    fn decode_software(&mut self, annex_b: &[u8]) -> Result<Option<DecodedPicture>> {
        #[cfg(feature = "software-openh264")]
        {
            if self.software.is_none() {
                self.software = Some(crate::soft::OpenH264Decoder::open()?);
            }
            return self
                .software
                .as_mut()
                .expect("software decoder")
                .decode(annex_b);
        }
        #[cfg(not(feature = "software-openh264"))]
        {
            let _ = annex_b;
            bail!("videotoolbox failed and this build has no software decoder");
        }
    }
}

impl VtSession {
    fn create(sps: &[u8], pps: &[u8]) -> Result<Self> {
        let mut slot = Box::new(FrameSlot {
            rgba: Vec::new(),
            width: 0,
            height: 0,
            ready: false,
        });
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_decoded_frame),
            decompressionOutputRefCon: (&mut *slot as *mut FrameSlot).cast(),
        };
        let format = format_description(sps, pps)?;
        let mut raw: *mut VTDecompressionSession = null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                None,
                &callback,
                std::ptr::NonNull::new(&mut raw).expect("out pointer"),
            )
        };
        if status != 0 || raw.is_null() {
            bail!("VTDecompressionSessionCreate failed ({status})");
        }
        let session =
            unsafe { CFRetained::from_raw(std::ptr::NonNull::new(raw).expect("session")) };
        Ok(Self {
            session,
            _format: format,
            slot,
            sps: sps.to_vec(),
            pps: pps.to_vec(),
        })
    }

    fn decode_slices(&mut self, slices: &[Vec<u8>]) -> Result<()> {
        self.slot.ready = false;
        let sample = sample_buffer(&self._format, slices)?;
        let mut info = VTDecodeInfoFlags(0);
        let status = unsafe {
            self.session
                .decode_frame(&sample, VTDecodeFrameFlags(0), null_mut(), &mut info)
        };
        if status != 0 {
            bail!("VTDecompressionSessionDecodeFrame failed ({status})");
        }
        Ok(())
    }
}

fn format_description(sps: &[u8], pps: &[u8]) -> Result<CFRetained<CMFormatDescription>> {
    let pointers = [
        std::ptr::NonNull::new(sps.as_ptr() as *mut u8).expect("sps"),
        std::ptr::NonNull::new(pps.as_ptr() as *mut u8).expect("pps"),
    ];
    let sizes = [sps.len(), pps.len()];
    let mut raw: *const CMFormatDescription = std::ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            std::ptr::NonNull::new(pointers.as_ptr() as *mut _).expect("pointers"),
            std::ptr::NonNull::new(sizes.as_ptr() as *mut usize).expect("sizes"),
            4,
            std::ptr::NonNull::new(&mut raw).expect("format out"),
        )
    };
    if status != 0 || raw.is_null() {
        bail!("CMVideoFormatDescriptionCreateFromH264ParameterSets failed ({status})");
    }
    let format = unsafe {
        CFRetained::from_raw(
            std::ptr::NonNull::new(raw as *mut CMFormatDescription).expect("format"),
        )
    };
    Ok(format)
}

fn sample_buffer(
    format: &CMFormatDescription,
    slices: &[Vec<u8>],
) -> Result<CFRetained<CMSampleBuffer>> {
    let mut avcc = Vec::new();
    for nal in slices {
        avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        avcc.extend_from_slice(nal);
    }
    let mut block_raw: *mut CMBlockBuffer = null_mut();
    let status = unsafe {
        CMBlockBufferCreateWithMemoryBlock(
            None,
            null_mut(),
            avcc.len(),
            None,
            std::ptr::null(),
            0,
            avcc.len(),
            0,
            std::ptr::NonNull::new(&mut block_raw).expect("block out"),
        )
    };
    if status != 0 || block_raw.is_null() {
        bail!("CMBlockBufferCreateWithMemoryBlock failed ({status})");
    }
    let block = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(block_raw).expect("block")) };
    // Copy into the block the CoreMedia allocator just created. A null memory
    // block with a null allocator asks CoreMedia to allocate the bytes.
    let source =
        std::ptr::NonNull::new(avcc.as_ptr() as *mut std::ffi::c_void).expect("avcc bytes");
    let replaced = unsafe { CMBlockBufferReplaceDataBytes(source, &block, 0, avcc.len()) };
    if replaced != 0 {
        bail!("CMBlockBufferReplaceDataBytes failed ({replaced})");
    }
    let sizes = [avcc.len()];
    let mut sample_raw: *mut CMSampleBuffer = null_mut();
    let status = unsafe {
        CMSampleBufferCreate(
            None,
            Some(&*block),
            true,
            None,
            null_mut(),
            Some(format),
            1,
            0,
            std::ptr::null(),
            1,
            sizes.as_ptr(),
            std::ptr::NonNull::new(&mut sample_raw).expect("sample out"),
        )
    };
    if status != 0 || sample_raw.is_null() {
        bail!("CMSampleBufferCreate failed ({status})");
    }
    Ok(unsafe { CFRetained::from_raw(std::ptr::NonNull::new(sample_raw).expect("sample")) })
}

fn split_access_unit(annex_b: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>)> {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    let mut slices = Vec::new();
    let source = if rd_proto::is_annex_b(annex_b) {
        annex_b.to_vec()
    } else {
        rd_proto::normalize_access_unit(annex_b)
    };
    for nal in iter_annex_b_nals(&source) {
        let Some(header) = nal.first().copied() else {
            continue;
        };
        match nal_type(header) {
            7 => sps = nal.to_vec(),
            8 => pps = nal.to_vec(),
            1 | 5 | 6 | 9 => slices.push(nal.to_vec()),
            _ => slices.push(nal.to_vec()),
        }
    }
    let _ = START;
    Ok((sps, pps, slices))
}

unsafe extern "C-unwind" fn on_decoded_frame(
    refcon: *mut c_void,
    _source: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVPixelBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    if refcon.is_null() || status != 0 || image.is_null() {
        return;
    }
    let slot = &mut *refcon.cast::<FrameSlot>();
    let pixel = &*image;
    let read_only = CVPixelBufferLockFlags(1);
    if CVPixelBufferLockBaseAddress(pixel, read_only) != 0 {
        return;
    }
    let width = CVPixelBufferGetWidth(pixel) as u32;
    let height = CVPixelBufferGetHeight(pixel) as u32;
    if width == 0 || height == 0 {
        let _ = CVPixelBufferUnlockBaseAddress(pixel, read_only);
        return;
    }
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    let format = CVPixelBufferGetPixelFormatType(pixel);
    let copied = if format == BGRA && !CVPixelBufferIsPlanar(pixel) {
        copy_bgra(pixel, width, height, &mut rgba)
    } else if format == NV12_VIDEO || format == NV12_FULL {
        copy_nv12(pixel, width, height, &mut rgba)
    } else if !CVPixelBufferIsPlanar(pixel) {
        copy_bgra(pixel, width, height, &mut rgba)
    } else {
        false
    };
    let _ = CVPixelBufferUnlockBaseAddress(pixel, read_only);
    if copied {
        slot.rgba = rgba;
        slot.width = width;
        slot.height = height;
        slot.ready = true;
    }
}

fn copy_bgra(pixel: &CVPixelBuffer, width: u32, height: u32, rgba: &mut [u8]) -> bool {
    let base = CVPixelBufferGetBaseAddress(pixel);
    if base.is_null() {
        return false;
    }
    let stride = CVPixelBufferGetBytesPerRow(pixel);
    let w = width as usize;
    let h = height as usize;
    for y in 0..h {
        let row = unsafe { std::slice::from_raw_parts(base.cast::<u8>().add(y * stride), w * 4) };
        let dst = &mut rgba[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            dst[x * 4] = row[x * 4 + 2];
            dst[x * 4 + 1] = row[x * 4 + 1];
            dst[x * 4 + 2] = row[x * 4];
            dst[x * 4 + 3] = 255;
        }
    }
    true
}

fn copy_nv12(pixel: &CVPixelBuffer, width: u32, height: u32, rgba: &mut [u8]) -> bool {
    let y_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 0);
    let uv_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 1);
    if y_base.is_null() || uv_base.is_null() {
        return false;
    }
    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, 0);
    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, 1);
    let w = width as usize;
    let h = height as usize;
    for y in 0..h {
        let y_row = unsafe { std::slice::from_raw_parts(y_base.cast::<u8>().add(y * y_stride), w) };
        let uv_row =
            unsafe { std::slice::from_raw_parts(uv_base.cast::<u8>().add((y / 2) * uv_stride), w) };
        for x in 0..w {
            let yy = y_row[x] as i32;
            let u = uv_row[x & !1] as i32 - 128;
            let v = uv_row[(x & !1) + 1] as i32 - 128;
            let c = yy - 16;
            let r = (298 * c + 409 * v + 128) >> 8;
            let g = (298 * c - 100 * u - 208 * v + 128) >> 8;
            let b = (298 * c + 516 * u + 128) >> 8;
            let o = (y * w + x) * 4;
            rgba[o] = r.clamp(0, 255) as u8;
            rgba[o + 1] = g.clamp(0, 255) as u8;
            rgba[o + 2] = b.clamp(0, 255) as u8;
            rgba[o + 3] = 255;
        }
    }
    true
}

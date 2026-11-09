//! Media Foundation H.264 encode and decode.
//!
//! Hardware path: `MFTEnumEx` with `MFT_ENUM_FLAG_HARDWARE` finds the encoder
//! or decoder the GPU driver registered (NVENC, Intel Quick Sync, AMD AMF).
//! Those transforms are asynchronous.
//!
//! Software path: `CLSID_CMSH264EncoderMFT` / `CLSID_CMSH264DecoderMFT`, the
//! Microsoft transforms shipped with desktop Windows. The encoder is forced
//! into low-latency mode with B-frames disabled and a short GOP.

use std::mem::ManuallyDrop;
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rd_pixel::{bgra_to_nv12, nv12_to_rgba};
use rd_proto::{contains_idr, normalize_access_unit};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFAttributes, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, MEError,
    METransformHaveOutput, METransformNeedInput, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint, MFStartup, MFTEnumEx,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_DECODER,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};

use crate::engine::{DecodedPicture, EncodeConfig, EncodedAccessUnit, VideoDecoder, VideoEncoder};

/// `{6CA50344-051A-4DED-9779-A43305165E35}`
const CLSID_CMSH264_ENCODER: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);
/// `{62CE7E72-4C71-4D20-B15D-452831A87D9D}`
const CLSID_CMSH264_DECODER: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);
/// `CODECAPI_AVEncMPVDefaultBPictureCount`
const CODECAPI_B_PICTURE_COUNT: GUID = GUID::from_u128(0x8d390aac_dc5c_4200_b57f_814d04babab2);

const OUTPUT_WAIT: Duration = Duration::from_millis(40);

fn pack_u32_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn frame_time_100ns(frame_index: u64, fps: u32) -> i64 {
    (frame_index as i64 * 10_000_000) / fps.max(1) as i64
}

fn frame_duration_100ns(fps: u32) -> i64 {
    10_000_000 / fps.max(1) as i64
}

fn mf_runtime_init() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
    }
    Ok(())
}

fn variant_u32(value: u32) -> VARIANT {
    let mut var = VARIANT::default();
    var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
        vt: VT_UI4,
        wReserved1: 0,
        wReserved2: 0,
        wReserved3: 0,
        Anonymous: VARIANT_0_0_0 { ulVal: value },
    });
    var
}

fn variant_bool(value: bool) -> VARIANT {
    let mut var = VARIANT::default();
    var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
        vt: VT_BOOL,
        wReserved1: 0,
        wReserved2: 0,
        wReserved3: 0,
        Anonymous: VARIANT_0_0_0 {
            boolVal: if value {
                VARIANT_TRUE
            } else {
                windows::Win32::Foundation::VARIANT_FALSE
            },
        },
    });
    var
}

fn set_codec(api: &ICodecAPI, guid: &GUID, value: VARIANT) {
    unsafe {
        let _ = api.SetValue(guid, &value);
    }
}

fn enumerate(category: GUID, subtype: GUID, input: bool) -> Result<Vec<IMFActivate>> {
    unsafe {
        let info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        let (input_info, output_info) = if input {
            (Some(&info), None)
        } else {
            (None, Some(&info))
        };
        let input_ptr = input_info.map(|info| info as *const MFT_REGISTER_TYPE_INFO);
        let output_ptr = output_info.map(|info| info as *const MFT_REGISTER_TYPE_INFO);
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            input_ptr,
            output_ptr,
            &mut activates,
            &mut count,
        )
        .context("MFTEnumEx")?;
        let mut out = Vec::with_capacity(count as usize);
        if !activates.is_null() {
            for index in 0..count as usize {
                if let Some(activate) = (*activates.add(index)).take() {
                    out.push(activate);
                }
            }
            CoTaskMemFree(Some(activates as *const _));
        }
        Ok(out)
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut value = windows::core::PWSTR::null();
        let mut len = 0u32;
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len)
            .is_err()
            || value.is_null()
        {
            return "media foundation h264".into();
        }
        let name = value
            .to_string()
            .unwrap_or_else(|_| "media foundation h264".into());
        CoTaskMemFree(Some(value.as_ptr() as *const _));
        name
    }
}

struct Engine {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    input_id: u32,
    output_id: u32,
    provides_samples: bool,
    output_size: u32,
    need_input: u32,
    async_mft: bool,
    name: String,
    hardware: bool,
}

// The COM pointers are only used from the thread that owns the encoder or
// decoder. Media Foundation objects created in the MTA are safe to move there.
unsafe impl Send for Engine {}

impl Engine {
    fn wrap(transform: IMFTransform, name: String, hardware: bool) -> Result<Self> {
        unsafe {
            let mut async_mft = false;
            if let Ok(attrs) = transform.GetAttributes() {
                async_mft = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
                if async_mft {
                    attrs
                        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .context("MF_TRANSFORM_ASYNC_UNLOCK")?;
                }
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let mut input_ids = [0u32; 1];
            let mut output_ids = [0u32; 1];
            let (input_id, output_id) =
                match transform.GetStreamIDs(&mut input_ids, &mut output_ids) {
                    Ok(()) => (input_ids[0], output_ids[0]),
                    Err(_) => (0, 0),
                };
            let events = if async_mft {
                Some(transform.cast().context("IMFMediaEventGenerator")?)
            } else {
                None
            };
            Ok(Self {
                codec: transform.cast().ok(),
                transform,
                events,
                input_id,
                output_id,
                provides_samples: false,
                output_size: 0,
                need_input: 0,
                async_mft,
                name,
                hardware,
            })
        }
    }

    fn refresh_output_info(&mut self) -> Result<()> {
        unsafe {
            let info = self
                .transform
                .GetOutputStreamInfo(self.output_id)
                .context("GetOutputStreamInfo")?;
            self.provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            self.output_size = info.cbSize;
        }
        Ok(())
    }

    fn begin(&self) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("BEGIN_STREAMING")?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("START_OF_STREAM")?;
        }
        Ok(())
    }

    fn force_keyframe(&self) {
        if let Some(api) = &self.codec {
            set_codec(api, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(1));
        }
    }

    fn process(&mut self, sample: IMFSample) -> Result<Vec<IMFSample>> {
        let mut outputs = Vec::new();
        if self.async_mft {
            let events = self
                .events
                .clone()
                .context("async mft has no event generator")?;
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                if self.need_input > 0 {
                    self.need_input -= 1;
                    unsafe {
                        self.transform
                            .ProcessInput(self.input_id, &sample, 0)
                            .context("ProcessInput")?;
                    }
                    break;
                }
                if !self.poll_event(&events, &mut outputs)? {
                    if Instant::now() >= deadline {
                        bail!("timed out waiting for METransformNeedInput");
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let grace = Instant::now() + OUTPUT_WAIT;
            let mut produced = false;
            loop {
                let before = outputs.len();
                if self.poll_event(&events, &mut outputs)? {
                    produced |= outputs.len() > before;
                } else if produced || Instant::now() >= grace {
                    break;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        } else {
            unsafe {
                self.transform
                    .ProcessInput(self.input_id, &sample, 0)
                    .context("ProcessInput")?;
            }
            while self.pull_output(&mut outputs)? {}
        }
        Ok(outputs)
    }

    fn poll_event(
        &mut self,
        events: &IMFMediaEventGenerator,
        outputs: &mut Vec<IMFSample>,
    ) -> Result<bool> {
        let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
        let event = match event {
            Ok(event) => event,
            Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(err) => return Err(err).context("GetEvent"),
        };
        let met = unsafe { event.GetType() }.context("IMFMediaEvent::GetType")?;
        if met == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if met == METransformHaveOutput.0 as u32 {
            let _ = self.pull_output(outputs)?;
        } else if met == MEError.0 as u32 {
            bail!("media foundation transform signalled MEError");
        }
        Ok(true)
    }

    fn pull_output(&mut self, outputs: &mut Vec<IMFSample>) -> Result<bool> {
        unsafe {
            loop {
                let provided = if self.provides_samples {
                    None
                } else {
                    Some(self.allocate_output_sample()?)
                };
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.output_id,
                    pSample: ManuallyDrop::new(provided),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
                let sample = ManuallyDrop::take(&mut buffers[0].pSample);
                let _events = ManuallyDrop::take(&mut buffers[0].pEvents);
                match result {
                    Ok(()) => {
                        if let Some(sample) = sample {
                            outputs.push(sample);
                        }
                        return Ok(true);
                    }
                    Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                    Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let new_type = self
                            .transform
                            .GetOutputAvailableType(self.output_id, 0)
                            .context("GetOutputAvailableType after stream change")?;
                        self.transform
                            .SetOutputType(self.output_id, &new_type, 0)
                            .context("SetOutputType after stream change")?;
                        self.refresh_output_info()?;
                        continue;
                    }
                    Err(err) => return Err(err).context("ProcessOutput"),
                }
            }
        }
    }

    fn allocate_output_sample(&self) -> Result<IMFSample> {
        unsafe {
            let size = if self.output_size > 0 {
                self.output_size
            } else {
                1 << 20
            };
            let buffer = MFCreateMemoryBuffer(size).context("MFCreateMemoryBuffer")?;
            let sample = MFCreateSample().context("MFCreateSample")?;
            sample.AddBuffer(&buffer).context("AddBuffer")?;
            Ok(sample)
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.input_id as usize);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

fn memory_sample(bytes: &[u8], time: i64, duration: i64) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32).context("MFCreateMemoryBuffer")?;
        let mut ptr_out = ptr::null_mut();
        buffer.Lock(&mut ptr_out, None, None).context("Lock")?;
        if !ptr_out.is_null() {
            ptr::copy_nonoverlapping(bytes.as_ptr(), ptr_out, bytes.len());
        }
        buffer.Unlock().context("Unlock")?;
        buffer
            .SetCurrentLength(bytes.len() as u32)
            .context("SetCurrentLength")?;
        let sample = MFCreateSample().context("MFCreateSample")?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .context("ConvertToContiguousBuffer")?;
        let mut data = ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut data, None, Some(&mut len))
            .context("Lock output")?;
        let bytes = if data.is_null() || len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, len as usize).to_vec()
        };
        buffer.Unlock().context("Unlock output")?;
        Ok(bytes)
    }
}

fn sample_is_clean(sample: &IMFSample) -> bool {
    unsafe {
        let Ok(attrs): Result<IMFAttributes, _> = sample.cast() else {
            return false;
        };
        attrs.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0
    }
}

fn configure_encoder_types(engine: &Engine, config: &EncodeConfig) -> Result<()> {
    unsafe {
        let output = match engine.transform.GetOutputAvailableType(engine.output_id, 0) {
            Ok(media_type) => media_type,
            Err(_) => MFCreateMediaType().context("MFCreateMediaType")?,
        };
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)?;
        output.SetUINT64(
            &MF_MT_FRAME_SIZE,
            pack_u32_pair(config.width, config.height),
        )?;
        output.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(config.fps, 1))?;
        output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
        output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
        engine
            .transform
            .SetOutputType(engine.output_id, &output, 0)
            .context("SetOutputType")?;

        let input = MFCreateMediaType().context("MFCreateMediaType")?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        input.SetUINT64(
            &MF_MT_FRAME_SIZE,
            pack_u32_pair(config.width, config.height),
        )?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(config.fps, 1))?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, config.width)?;
        engine
            .transform
            .SetInputType(engine.input_id, &input, 0)
            .context("SetInputType")?;
    }
    Ok(())
}

fn apply_low_latency(engine: &Engine, config: &EncodeConfig) {
    let Some(api) = &engine.codec else {
        return;
    };
    set_codec(
        api,
        &CODECAPI_AVEncCommonRateControlMode,
        variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
    );
    set_codec(
        api,
        &CODECAPI_AVEncCommonMeanBitRate,
        variant_u32(config.bitrate_bps),
    );
    set_codec(api, &CODECAPI_AVLowLatencyMode, variant_bool(true));
    set_codec(api, &CODECAPI_B_PICTURE_COUNT, variant_u32(0));
    set_codec(
        api,
        &CODECAPI_AVEncMPVGOPSize,
        variant_u32(config.gop.max(1)),
    );
}

pub struct MfEncoder {
    engine: Engine,
    width: u32,
    height: u32,
    fps: u32,
    frames: u64,
    nv12: Vec<u8>,
    parameter_sets: Vec<u8>,
}

impl MfEncoder {
    pub fn open(config: &EncodeConfig) -> Result<Self> {
        mf_runtime_init()?;
        let mut errors = Vec::new();
        if let Ok(activates) = enumerate(MFT_CATEGORY_VIDEO_ENCODER, MFVideoFormat_H264, false) {
            for activate in activates {
                let name = friendly_name(&activate);
                match Self::activate(activate, &name, true, config) {
                    Ok(encoder) => return Ok(encoder),
                    Err(err) => errors.push(format!("{name}: {err:#}")),
                }
            }
        }
        match Self::software(config) {
            Ok(encoder) => return Ok(encoder),
            Err(err) => errors.push(format!("microsoft software encoder: {err:#}")),
        }
        bail!(
            "no media foundation H.264 encoder ({})",
            if errors.is_empty() {
                "none registered".into()
            } else {
                errors.join("; ")
            }
        )
    }

    fn software(config: &EncodeConfig) -> Result<Self> {
        let transform: IMFTransform = unsafe {
            CoCreateInstance(&CLSID_CMSH264_ENCODER, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance CLSID_CMSH264EncoderMFT")?
        };
        Self::from_transform(transform, "microsoft h264 software".into(), false, config)
    }

    fn activate(
        activate: IMFActivate,
        name: &str,
        hardware: bool,
        config: &EncodeConfig,
    ) -> Result<Self> {
        let transform: IMFTransform =
            unsafe { activate.ActivateObject().context("ActivateObject")? };
        Self::from_transform(transform, name.to_string(), hardware, config)
    }

    fn from_transform(
        transform: IMFTransform,
        name: String,
        hardware: bool,
        config: &EncodeConfig,
    ) -> Result<Self> {
        let mut engine = Engine::wrap(transform, name, hardware)?;
        apply_low_latency(&engine, config);
        configure_encoder_types(&engine, config)?;
        engine.refresh_output_info()?;
        engine.begin()?;
        let nv12_len = rd_pixel::nv12_len(config.width, config.height);
        Ok(Self {
            engine,
            width: config.width,
            height: config.height,
            fps: config.fps,
            frames: 0,
            nv12: vec![0u8; nv12_len],
            parameter_sets: Vec::new(),
        })
    }

    fn remember_sequence_header(&mut self) {
        unsafe {
            let Ok(media_type) = self
                .engine
                .transform
                .GetOutputCurrentType(self.engine.output_id)
            else {
                return;
            };
            let Ok(size) = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
                return;
            };
            if size == 0 {
                return;
            }
            let mut blob = vec![0u8; size as usize];
            if media_type
                .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
                .is_ok()
            {
                if let Some(annex) = rd_proto::avcc_record_to_annex_b(&blob) {
                    self.parameter_sets = annex;
                }
            }
        }
    }
}

impl VideoEncoder for MfEncoder {
    fn name(&self) -> &str {
        &self.engine.name
    }

    fn is_hardware(&self) -> bool {
        self.engine.hardware
    }

    fn force_keyframe(&mut self) {
        self.engine.force_keyframe();
    }

    fn encode(&mut self, bgra: &[u8]) -> Result<Option<EncodedAccessUnit>> {
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() != expected {
            bail!("bgra frame is {} bytes, expected {expected}", bgra.len());
        }
        bgra_to_nv12(
            bgra,
            self.width,
            self.height,
            expected / self.height as usize,
            &mut self.nv12,
        )?;
        let sample = memory_sample(
            &self.nv12,
            frame_time_100ns(self.frames, self.fps),
            frame_duration_100ns(self.fps),
        )?;
        let outputs = self.engine.process(sample)?;
        self.frames += 1;
        self.remember_sequence_header();
        let mut annex = Vec::new();
        let mut clean = false;
        for sample in &outputs {
            clean |= sample_is_clean(sample);
            let bytes = sample_bytes(sample)?;
            annex.extend_from_slice(&normalize_access_unit(&bytes));
        }
        if annex.is_empty() {
            return Ok(None);
        }
        let mut is_keyframe = clean || contains_idr(&annex);
        if is_keyframe
            && !rd_proto::contains_parameter_sets(&annex)
            && !self.parameter_sets.is_empty()
        {
            let mut with_sets = self.parameter_sets.clone();
            with_sets.extend_from_slice(&annex);
            annex = with_sets;
        }
        if contains_idr(&annex) {
            is_keyframe = true;
        }
        Ok(Some(EncodedAccessUnit {
            annex_b: annex,
            is_keyframe,
        }))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Nv12,
    Rgb32,
    Unknown,
}

pub struct MfDecoder {
    engine: Option<Engine>,
    hint: (u32, u32, u32),
    output: OutputKind,
}

impl MfDecoder {
    pub fn open() -> Result<Self> {
        mf_runtime_init()?;
        // Construct once so a missing MFT fails over to OpenH264 at startup.
        let mut decoder = Self {
            engine: None,
            hint: (0, 0, 60),
            output: OutputKind::Unknown,
        };
        decoder.ensure_engine()?;
        Ok(decoder)
    }

    fn ensure_engine(&mut self) -> Result<()> {
        if self.engine.is_some() {
            return Ok(());
        }
        let (width, height, fps) = self.hint;
        let mut errors = Vec::new();
        if let Ok(activates) = enumerate(MFT_CATEGORY_VIDEO_DECODER, MFVideoFormat_H264, true) {
            for activate in &activates {
                let name = friendly_name(activate);
                match start_decoder(activate, &name, true, width, height, fps) {
                    Ok((engine, output)) => {
                        self.output = output;
                        self.engine = Some(engine);
                        return Ok(());
                    }
                    Err(err) => errors.push(format!("{name}: {err:#}")),
                }
            }
        }
        match start_software_decoder(width, height, fps) {
            Ok((engine, output)) => {
                self.output = output;
                self.engine = Some(engine);
                Ok(())
            }
            Err(err) => {
                errors.push(format!("microsoft software decoder: {err:#}"));
                bail!("no media foundation H.264 decoder ({})", errors.join("; "))
            }
        }
    }
}

fn start_software_decoder(width: u32, height: u32, fps: u32) -> Result<(Engine, OutputKind)> {
    let transform: IMFTransform = unsafe {
        CoCreateInstance(&CLSID_CMSH264_DECODER, None, CLSCTX_INPROC_SERVER)
            .context("CoCreateInstance CLSID_CMSH264DecoderMFT")?
    };
    finish_decoder(
        transform,
        "microsoft h264 software".into(),
        false,
        width,
        height,
        fps,
    )
}

fn start_decoder(
    activate: &IMFActivate,
    name: &str,
    hardware: bool,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(Engine, OutputKind)> {
    let transform: IMFTransform = unsafe { activate.ActivateObject().context("ActivateObject")? };
    finish_decoder(transform, name.to_string(), hardware, width, height, fps)
}

fn finish_decoder(
    transform: IMFTransform,
    name: String,
    hardware: bool,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(Engine, OutputKind)> {
    let mut engine = Engine::wrap(transform, name, hardware)?;
    unsafe {
        let input = MFCreateMediaType().context("MFCreateMediaType")?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if width > 0 && height > 0 {
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps.max(1), 1))?;
        }
        engine
            .transform
            .SetInputType(engine.input_id, &input, 0)
            .context("decoder SetInputType")?;
    }
    let output = select_decoder_output(&engine, width, height)?;
    engine.refresh_output_info()?;
    engine.begin()?;
    Ok((engine, output))
}

fn select_decoder_output(engine: &Engine, width: u32, height: u32) -> Result<OutputKind> {
    unsafe {
        let mut chosen: Option<(IMFMediaType, OutputKind)> = None;
        for index in 0..16 {
            let Ok(media_type) = engine
                .transform
                .GetOutputAvailableType(engine.output_id, index)
            else {
                break;
            };
            let subtype = media_type
                .GetGUID(&MF_MT_SUBTYPE)
                .unwrap_or(GUID::from_u128(0));
            if subtype == MFVideoFormat_RGB32
                && chosen
                    .as_ref()
                    .is_none_or(|(_, kind)| *kind != OutputKind::Rgb32)
            {
                chosen = Some((media_type, OutputKind::Rgb32));
            } else if subtype == MFVideoFormat_NV12 && chosen.is_none() {
                chosen = Some((media_type, OutputKind::Nv12));
            }
        }
        let Some((media_type, kind)) = chosen else {
            bail!("decoder has neither RGB32 nor NV12 output");
        };
        if width > 0 && height > 0 {
            let _ = media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height));
        }
        engine
            .transform
            .SetOutputType(engine.output_id, &media_type, 0)
            .context("decoder SetOutputType")?;
        Ok(kind)
    }
}

impl VideoDecoder for MfDecoder {
    fn name(&self) -> &str {
        self.engine
            .as_ref()
            .map(|engine| engine.name.as_str())
            .unwrap_or("media foundation")
    }

    fn is_hardware(&self) -> bool {
        self.engine.as_ref().is_some_and(|engine| engine.hardware)
    }

    fn set_stream_info(&mut self, width: u32, height: u32, fps: u32) {
        self.hint = (width, height, fps);
    }

    fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedPicture>> {
        self.ensure_engine()?;
        let (outputs, negotiated) = {
            let engine = self.engine.as_mut().context("decoder missing")?;
            let sample = memory_sample(&normalize_access_unit(annex_b), 0, 0)?;
            let outputs = engine.process(sample)?;
            let negotiated = negotiated_output(engine);
            (outputs, negotiated)
        };
        if let Some(info) = &negotiated {
            self.output = info.kind;
        }
        let info = negotiated.unwrap_or(OutputInfo {
            kind: self.output,
            width: self.hint.0,
            height: self.hint.1,
            stride: 0,
        });
        let mut picture = None;
        for sample in &outputs {
            let bytes = sample_bytes(sample)?;
            if bytes.is_empty() {
                continue;
            }
            picture = Some(picture_from_output(&info, &bytes)?);
        }
        Ok(picture)
    }
}

struct OutputInfo {
    kind: OutputKind,
    width: u32,
    height: u32,
    /// Row stride in bytes. Zero means the buffer is tightly packed.
    /// Negative means the image is stored bottom-up.
    stride: i32,
}

fn negotiated_output(engine: &Engine) -> Option<OutputInfo> {
    unsafe {
        let media_type = engine
            .transform
            .GetOutputCurrentType(engine.output_id)
            .ok()?;
        let subtype = media_type.GetGUID(&MF_MT_SUBTYPE).ok()?;
        let kind = if subtype == MFVideoFormat_RGB32 {
            OutputKind::Rgb32
        } else if subtype == MFVideoFormat_NV12 {
            OutputKind::Nv12
        } else {
            return None;
        };
        let packed = media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
        let width = (packed >> 32) as u32;
        let height = (packed & 0xffff_ffff) as u32;
        if width == 0 || height == 0 {
            return None;
        }
        let stride = media_type
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|value| value as i32)
            .unwrap_or(0);
        Some(OutputInfo {
            kind,
            width,
            height,
            stride,
        })
    }
}

fn picture_from_output(info: &OutputInfo, bytes: &[u8]) -> Result<DecodedPicture> {
    if info.width == 0 || info.height == 0 {
        bail!(
            "decoder output is {} bytes and has no frame size",
            bytes.len()
        );
    }
    let packed = match info.kind {
        OutputKind::Rgb32 => pack_rows(bytes, info.width, info.height, 4, info.stride)?,
        OutputKind::Nv12 => {
            let tight = pack_nv12(bytes, info.width, info.height, info.stride)?;
            let mut rgba = vec![0u8; info.width as usize * info.height as usize * 4];
            nv12_to_rgba(&tight, info.width, info.height, &mut rgba)?;
            return Ok(DecodedPicture {
                width: info.width,
                height: info.height,
                rgba,
            });
        }
        OutputKind::Unknown => bail!("decoder output format was not negotiated"),
    };
    let mut rgba = vec![0u8; info.width as usize * info.height as usize * 4];
    rd_pixel::bgra_to_rgba(
        &packed,
        info.width,
        info.height,
        info.width as usize * 4,
        &mut rgba,
    )?;
    Ok(DecodedPicture {
        width: info.width,
        height: info.height,
        rgba,
    })
}

/// Copy a possibly strided, possibly bottom-up image into a tightly packed buffer.
fn pack_rows(bytes: &[u8], width: u32, height: u32, bpp: usize, stride: i32) -> Result<Vec<u8>> {
    let w = width as usize * bpp;
    let h = height as usize;
    let row_stride = if stride == 0 { w as i32 } else { stride };
    let abs_stride = row_stride.unsigned_abs() as usize;
    if abs_stride < w {
        bail!("decoder stride {row_stride} is narrower than the frame");
    }
    let need = abs_stride * h;
    if bytes.len() < need {
        bail!(
            "decoder output is {} bytes, expected at least {need} for {width}x{height}",
            bytes.len()
        );
    }
    let mut packed = vec![0u8; w * h];
    for y in 0..h {
        let src_y = if row_stride < 0 { h - 1 - y } else { y };
        let start = src_y * abs_stride;
        packed[y * w..(y + 1) * w].copy_from_slice(&bytes[start..start + w]);
    }
    Ok(packed)
}

fn pack_nv12(bytes: &[u8], width: u32, height: u32, stride: i32) -> Result<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    let row_stride = if stride == 0 { w as i32 } else { stride };
    if row_stride == w as i32 && bytes.len() >= rd_pixel::nv12_len(width, height) {
        return Ok(bytes[..rd_pixel::nv12_len(width, height)].to_vec());
    }
    let abs_stride = row_stride.unsigned_abs() as usize;
    if abs_stride < w || !height.is_multiple_of(2) {
        bail!("decoder NV12 stride {row_stride} does not fit {width}x{height}");
    }
    let y_bytes = abs_stride * h;
    let uv_rows = h / 2;
    if bytes.len() < y_bytes + abs_stride * uv_rows {
        bail!(
            "decoder NV12 output is {} bytes for {width}x{height} stride {row_stride}",
            bytes.len()
        );
    }
    let mut packed = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        let src_y = if row_stride < 0 { h - 1 - y } else { y };
        let start = src_y * abs_stride;
        packed[y * w..(y + 1) * w].copy_from_slice(&bytes[start..start + w]);
    }
    let uv_base = if row_stride < 0 { 0 } else { y_bytes };
    for y in 0..uv_rows {
        let src_y = if row_stride < 0 { uv_rows - 1 - y } else { y };
        let start = uv_base + src_y * abs_stride;
        let dst = w * h + y * w;
        packed[dst..dst + w].copy_from_slice(&bytes[start..start + w]);
    }
    Ok(packed)
}

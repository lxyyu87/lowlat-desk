use anyhow::Result;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
use openh264::{nal_units, OpenH264API};
use rd_proto::contains_idr;

use crate::engine::{DecodedPicture, EncodeConfig, EncodedAccessUnit, VideoDecoder, VideoEncoder};

pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    yuv: YUVBuffer,
    force_idr: bool,
}

impl OpenH264Encoder {
    pub fn open(config: &EncodeConfig) -> Result<Self> {
        let settings = EncoderConfig::new()
            .bitrate(BitRate::from_bps(config.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(config.fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .skip_frames(false)
            .scene_change_detect(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.gop))
            .num_threads(1);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), settings)
            .map_err(|err| anyhow::anyhow!("openh264 encoder: {err}"))?;
        Ok(Self {
            encoder,
            width: config.width,
            height: config.height,
            yuv: YUVBuffer::new(config.width as usize, config.height as usize),
            force_idr: true,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn name(&self) -> &str {
        "openh264"
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn encode(&mut self, bgra: &[u8]) -> Result<Option<EncodedAccessUnit>> {
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() != expected {
            anyhow::bail!("bgra frame is {} bytes, expected {expected}", bgra.len());
        }
        if self.force_idr {
            self.encoder.force_intra_frame();
            self.force_idr = false;
        }
        let source = BgraSliceU8::new(bgra, (self.width as usize, self.height as usize));
        self.yuv.read_bgra8(source);
        let bitstream = self
            .encoder
            .encode(&self.yuv)
            .map_err(|err| anyhow::anyhow!("openh264 encode: {err}"))?;
        let annex_b = bitstream.to_vec();
        if annex_b.is_empty() {
            anyhow::bail!("openh264 produced an empty bitstream");
        }
        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I)
            || contains_idr(&annex_b);
        Ok(Some(EncodedAccessUnit {
            annex_b,
            is_keyframe,
        }))
    }
}

pub struct OpenH264Decoder {
    decoder: Decoder,
}

impl OpenH264Decoder {
    pub fn open() -> Result<Self> {
        let decoder = Decoder::with_api_config(
            OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::default(),
        )
        .map_err(|err| anyhow::anyhow!("openh264 decoder: {err}"))?;
        Ok(Self { decoder })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn name(&self) -> &str {
        "openh264"
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedPicture>> {
        let mut picture = None;
        for nal in nal_units(annex_b) {
            match self.decoder.decode(nal) {
                Ok(Some(yuv)) => {
                    let (width, height) = yuv.dimensions();
                    let mut rgba = vec![0u8; yuv.rgba8_len()];
                    yuv.write_rgba8(&mut rgba);
                    picture = Some(DecodedPicture {
                        width: width as u32,
                        height: height as u32,
                        rgba,
                    });
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(error = %err, "openh264 skipped a nal");
                }
            }
        }
        Ok(picture)
    }
}

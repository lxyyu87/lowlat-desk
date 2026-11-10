use anyhow::{bail, Result};

/// One decoded picture, tightly packed RGBA8.
#[derive(Debug, Clone)]
pub struct DecodedPicture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// One encoded access unit, Annex-B.
#[derive(Debug, Clone)]
pub struct EncodedAccessUnit {
    pub annex_b: Vec<u8>,
    pub is_keyframe: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub gop: u32,
}

impl EncodeConfig {
    pub fn validate(self) -> Result<()> {
        if self.width < 2
            || self.height < 2
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            bail!(
                "encoder size must be even (got {}x{})",
                self.width,
                self.height
            );
        }
        if !(1..=240).contains(&self.fps) {
            bail!("fps must be 1..=240");
        }
        if self.bitrate_bps < 50_000 {
            bail!("bitrate is too low");
        }
        if self.gop == 0 {
            bail!("gop must be at least 1");
        }
        Ok(())
    }
}

pub trait VideoEncoder: Send {
    fn name(&self) -> &str;
    fn is_hardware(&self) -> bool;
    /// The next `encode` call emits an IDR, with SPS/PPS when the codec does.
    fn force_keyframe(&mut self);
    /// `Ok(None)` means an asynchronous encoder accepted the frame and has not
    /// produced the access unit yet. The caller should not treat that as loss.
    fn encode(&mut self, bgra: &[u8]) -> Result<Option<EncodedAccessUnit>>;
}

pub trait VideoDecoder: Send {
    fn name(&self) -> &str;
    fn is_hardware(&self) -> bool;
    fn set_stream_info(&mut self, _width: u32, _height: u32, _fps: u32) {}
    fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedPicture>>;
}

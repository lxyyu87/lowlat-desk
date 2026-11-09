//! H.264 codecs for the phase 1 LAN stream.
//!
//! Preference order:
//! - Windows: a Media Foundation hardware encoder/decoder (NVENC, Quick Sync,
//!   or AMF, whichever the driver registered), then the built-in Microsoft
//!   software H.264 MFT, then OpenH264.
//! - macOS decode: VideoToolbox, which uses hardware when the machine has it
//!   and Apple's software decoder otherwise, then OpenH264.
//! - Everywhere else: OpenH264. It is Baseline, so it never emits B-frames.
//!
//! The Annex-B access unit is the boundary. Packetization lives in `rd-proto`.

mod engine;

#[cfg(feature = "software-openh264")]
mod soft;

#[cfg(windows)]
mod mf;

#[cfg(target_os = "macos")]
mod vt;

use anyhow::Result;

pub use engine::{DecodedPicture, EncodeConfig, EncodedAccessUnit, VideoDecoder, VideoEncoder};

pub fn open_encoder(config: EncodeConfig) -> Result<Box<dyn VideoEncoder>> {
    config.validate()?;
    #[cfg(windows)]
    {
        match mf::MfEncoder::open(&config) {
            Ok(encoder) => {
                tracing::info!(
                    name = encoder.name(),
                    hardware = encoder.is_hardware(),
                    "selected encoder"
                );
                return Ok(Box::new(encoder));
            }
            Err(err) => {
                tracing::warn!(error = %err, "media foundation encoder unavailable");
            }
        }
    }
    open_software_encoder(config)
}

pub fn open_decoder() -> Result<Box<dyn VideoDecoder>> {
    #[cfg(windows)]
    {
        match mf::MfDecoder::open() {
            Ok(decoder) => {
                tracing::info!(
                    name = decoder.name(),
                    hardware = decoder.is_hardware(),
                    "selected decoder"
                );
                return Ok(Box::new(decoder));
            }
            Err(err) => {
                tracing::warn!(error = %err, "media foundation decoder unavailable");
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        tracing::info!(name = "videotoolbox", "selected decoder");
        return Ok(Box::new(vt::VtDecoder::new()));
    }
    #[cfg(not(target_os = "macos"))]
    open_software_decoder()
}

fn open_software_encoder(config: EncodeConfig) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(feature = "software-openh264")]
    {
        let encoder = soft::OpenH264Encoder::open(&config)?;
        tracing::info!(name = encoder.name(), hardware = false, "selected encoder");
        Ok(Box::new(encoder))
    }
    #[cfg(not(feature = "software-openh264"))]
    {
        let _ = config;
        anyhow::bail!("this build has no software H.264 encoder");
    }
}

#[cfg(not(target_os = "macos"))]
fn open_software_decoder() -> Result<Box<dyn VideoDecoder>> {
    #[cfg(feature = "software-openh264")]
    {
        let decoder = soft::OpenH264Decoder::open()?;
        tracing::info!(name = decoder.name(), hardware = false, "selected decoder");
        Ok(Box::new(decoder))
    }
    #[cfg(not(feature = "software-openh264"))]
    {
        anyhow::bail!("this build has no software H.264 decoder");
    }
}

#[cfg(all(test, feature = "software-openh264"))]
mod tests {
    use std::net::UdpSocket;
    use std::time::Instant;

    use rd_proto::{Fragmenter, Reassembler, VideoFrame, CODEC_H264};

    use super::*;

    fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        for px in bgra.as_chunks_mut::<4>().0 {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        bgra
    }

    fn split_color(width: u32, height: u32) -> Vec<u8> {
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let o = (y * width as usize + x) * 4;
                if x < width as usize / 2 {
                    bgra[o + 2] = 255;
                } else {
                    bgra[o] = 255;
                }
                bgra[o + 3] = 255;
            }
        }
        bgra
    }

    #[test]
    fn software_roundtrip_keeps_large_color_regions() {
        let config = EncodeConfig {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_bps: 2_000_000,
            gop: 15,
        };
        let mut encoder = open_encoder(config).unwrap();
        assert!(!encoder.is_hardware());
        let mut decoder = open_decoder().unwrap();
        let bgra = split_color(64, 64);
        encoder.force_keyframe();
        let encoded = encoder.encode(&bgra).unwrap().expect("picture");
        assert!(encoded.is_keyframe);
        assert!(rd_proto::contains_idr(&encoded.annex_b));
        let picture = decoder.decode(&encoded.annex_b).unwrap().expect("picture");
        assert_eq!((picture.width, picture.height), (64, 64));
        let left = &picture.rgba[(32 * 64 + 8) * 4..][..4];
        let right = &picture.rgba[(32 * 64 + 56) * 4..][..4];
        assert!(
            left[0] > left[2] + 40,
            "left pixel should stay red: {left:?}"
        );
        assert!(
            right[2] > right[0] + 40,
            "right pixel should stay blue: {right:?}"
        );
    }

    #[test]
    fn later_keyframe_decodes_on_a_fresh_decoder() {
        let config = EncodeConfig {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_bps: 1_000_000,
            gop: 30,
        };
        let mut encoder = open_encoder(config).unwrap();
        let bgra = solid(64, 64, 0, 180, 0);
        let mut cache = rd_proto::ParameterSetCache::default();
        let mut second = None;
        for index in 0..3 {
            if index == 0 || index == 2 {
                encoder.force_keyframe();
            }
            let encoded = encoder.encode(&bgra).unwrap().expect("picture");
            cache.observe(&encoded.annex_b);
            let wire = cache.ensure(&encoded.annex_b, encoded.is_keyframe);
            if index == 2 {
                assert!(encoded.is_keyframe);
                second = Some(wire);
            }
        }
        let mut fresh = open_decoder().unwrap();
        let picture = fresh
            .decode(second.as_ref().unwrap())
            .unwrap()
            .expect("idr must be independently decodable");
        assert_eq!(picture.width, 64);
    }

    #[test]
    fn udp_reassembly_survives_a_dropped_fragment() {
        let config = EncodeConfig {
            width: 64,
            height: 48,
            fps: 20,
            bitrate_bps: 800_000,
            gop: 10,
        };
        let mut encoder = open_encoder(config).unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();

        let mut fragmenter = Fragmenter::new();
        let hello = fragmenter
            .control(
                rd_proto::kind::HELLO_ACK,
                &rd_proto::HelloAck {
                    width: 64,
                    height: 48,
                    fps: 20,
                    codec: CODEC_H264,
                }
                .encode(),
            )
            .unwrap();
        sender.send(&hello).unwrap();

        encoder.force_keyframe();
        let first = encoder
            .encode(&split_color(64, 48))
            .unwrap()
            .expect("picture");
        let packets = fragmenter
            .packetize(&VideoFrame {
                frame_id: 1,
                is_keyframe: first.is_keyframe,
                capture_us: 100,
                send_us: 180,
                annex_b: first.annex_b,
            })
            .unwrap();
        for (index, packet) in packets.iter().enumerate() {
            if index == packets.len() / 2 {
                continue;
            }
            sender.send(packet).unwrap();
        }
        encoder.force_keyframe();
        let recovery = encoder
            .encode(&solid(64, 48, 0, 0, 220))
            .unwrap()
            .expect("picture");
        for packet in fragmenter
            .packetize(&VideoFrame {
                frame_id: 2,
                is_keyframe: true,
                capture_us: 200,
                send_us: 260,
                annex_b: recovery.annex_b,
            })
            .unwrap()
        {
            sender.send(&packet).unwrap();
        }

        let mut asm = Reassembler::new();
        let mut decoder = open_decoder().unwrap();
        let mut requested = false;
        let mut decoded = None;
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while Instant::now() < deadline && decoded.is_none() {
            match receiver.recv(&mut buf) {
                Ok(n) => {
                    let outcome = asm.push(&buf[..n], Instant::now()).unwrap();
                    requested |= outcome.request_keyframe;
                    if let Some(frame) = asm.take_latest() {
                        if let Some(picture) = decoder.decode(&frame.data).unwrap() {
                            decoded = Some((frame, picture));
                        }
                    }
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    let outcome = asm.poll(Instant::now()).unwrap();
                    requested |= outcome.request_keyframe;
                    if let Some(frame) = asm.take_latest() {
                        if let Some(picture) = decoder.decode(&frame.data).unwrap() {
                            decoded = Some((frame, picture));
                        }
                    }
                }
                Err(err) => panic!("{err}"),
            }
        }
        assert!(requested, "a dropped fragment must request a keyframe");
        let (frame, picture) = decoded.expect("recovery keyframe should decode");
        assert!(frame.is_keyframe);
        assert_eq!(frame.frame_id, 2);
        assert_eq!(frame.send_us - frame.capture_us, 60);
        assert_eq!((picture.width, picture.height), (64, 48));
    }
}

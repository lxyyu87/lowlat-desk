use anyhow::{Context, Result};
use rd_pixel::{bgra_len, fit_capture_size, scale_bgra};

/// A captured desktop image, tightly packed BGRA.
pub struct Captured {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub trait FrameSource {
    fn size(&self) -> (u32, u32);
    /// `Ok(None)` means the desktop did not change before the timeout.
    fn poll(&mut self, timeout_ms: u32) -> Result<Option<Captured>>;
}

pub struct TestPattern {
    width: u32,
    height: u32,
    tick: u64,
    bgra: Vec<u8>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = fit_capture_size(width.max(2), height.max(2), 0);
        Self {
            width,
            height,
            tick: 0,
            bgra: vec![0; bgra_len(width, height)],
        }
    }
}

impl FrameSource for TestPattern {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn poll(&mut self, _timeout_ms: u32) -> Result<Option<Captured>> {
        self.tick = self.tick.wrapping_add(1);
        let w = self.width as usize;
        let h = self.height as usize;
        let bar = (self.tick as usize * 6) % w.max(1);
        for y in 0..h {
            for x in 0..w {
                let offset = (y * w + x) * 4;
                if x.abs_diff(bar) < 10 {
                    self.bgra[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
                } else if x < w / 2 {
                    self.bgra[offset..offset + 4].copy_from_slice(&[30, 40, 180, 255]);
                } else {
                    self.bgra[offset..offset + 4].copy_from_slice(&[180, 90, 30, 255]);
                }
            }
        }
        Ok(Some(Captured {
            bgra: self.bgra.clone(),
            width: self.width,
            height: self.height,
        }))
    }
}

#[cfg(windows)]
mod dxgi {
    use super::{Captured, FrameSource};
    use anyhow::{bail, Context, Result};
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_SDK_VERSION,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::{
        Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1,
        IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
        DXGI_OUTDUPL_FRAME_INFO,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    pub struct DxgiCapture {
        display: u32,
        state: Option<State>,
    }

    struct State {
        // The device owns the context and the duplication. It is not read again.
        #[allow(dead_code)]
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
        staging: ID3D11Texture2D,
        width: u32,
        height: u32,
    }

    impl DxgiCapture {
        pub fn open(display: u32) -> Result<Self> {
            let mut capture = Self {
                display,
                state: None,
            };
            capture.reopen()?;
            Ok(capture)
        }

        fn reopen(&mut self) -> Result<()> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            self.state = Some(State::open(self.display)?);
            let (w, h) = self.state.as_ref().unwrap().size();
            tracing::info!(
                display = self.display,
                width = w,
                height = h,
                "dxgi duplication ready"
            );
            Ok(())
        }
    }

    impl FrameSource for DxgiCapture {
        fn size(&self) -> (u32, u32) {
            self.state.as_ref().map(State::size).unwrap_or((0, 0))
        }

        fn poll(&mut self, timeout_ms: u32) -> Result<Option<Captured>> {
            if self.state.is_none() {
                self.reopen()?;
            }
            match self.state.as_mut().unwrap().acquire(timeout_ms) {
                Ok(frame) => Ok(frame),
                Err(err) if access_lost(&err) => {
                    tracing::warn!(error = %err, "desktop duplication lost; recreating");
                    self.state = None;
                    self.reopen()?;
                    Ok(None)
                }
                Err(err) => Err(err),
            }
        }
    }

    fn access_lost(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| {
            cause
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|win| win.code() == DXGI_ERROR_ACCESS_LOST)
        })
    }

    impl State {
        fn open(display: u32) -> Result<Self> {
            unsafe {
                let levels = [D3D_FEATURE_LEVEL_11_0];
                let mut device = None;
                let mut context = None;
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .context("D3D11CreateDevice")?;
                let device = device.context("D3D11 device")?;
                let context = context.context("D3D11 context")?;
                let dxgi: IDXGIDevice = device.cast().context("IDXGIDevice")?;
                let adapter: IDXGIAdapter = dxgi.GetParent().context("IDXGIAdapter")?;
                let output: IDXGIOutput = adapter
                    .EnumOutputs(display)
                    .with_context(|| format!("EnumOutputs({display})"))?;
                let output1: IDXGIOutput1 = output.cast().context("IDXGIOutput1")?;
                let duplication = output1
                    .DuplicateOutput(&device)
                    .context("DuplicateOutput")?;
                let desc = output.GetDesc().context("output desc")?;
                let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
                if width < 2 || height < 2 {
                    bail!("desktop size {width}x{height} is too small");
                }
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut staging = None;
                device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                    .context("CreateTexture2D staging")?;
                Ok(Self {
                    device,
                    context,
                    duplication,
                    staging: staging.context("staging texture")?,
                    width,
                    height,
                })
            }
        }

        fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        fn acquire(&mut self, timeout_ms: u32) -> Result<Option<Captured>> {
            unsafe {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                match self
                    .duplication
                    .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
                {
                    Ok(()) => {}
                    Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                    Err(err) => return Err(err).context("AcquireNextFrame"),
                }
                let release = ReleaseGuard {
                    duplication: &self.duplication,
                };
                if info.AccumulatedFrames == 0 {
                    return Ok(None);
                }
                let Some(resource) = resource else {
                    return Ok(None);
                };
                let texture: ID3D11Texture2D = resource.cast().context("desktop texture")?;
                let source: ID3D11Resource = texture.cast().context("source resource")?;
                let staging: ID3D11Resource = self.staging.cast().context("staging resource")?;
                self.context.CopyResource(&staging, &source);
                self.context.Flush();
                drop(release);
                let mut mapped =
                    windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("Map staging")?;
                let stride = mapped.RowPitch as usize;
                let bytes = std::slice::from_raw_parts(
                    mapped.pData.cast::<u8>(),
                    stride * self.height as usize,
                );
                let bgra = if stride == self.width as usize * 4 {
                    bytes.to_vec()
                } else {
                    let mut tight = vec![0u8; self.width as usize * self.height as usize * 4];
                    for y in 0..self.height as usize {
                        let src = &bytes[y * stride..y * stride + self.width as usize * 4];
                        let dst_off = y * self.width as usize * 4;
                        tight[dst_off..dst_off + src.len()].copy_from_slice(src);
                    }
                    tight
                };
                self.context.Unmap(&staging, 0);
                Ok(Some(Captured {
                    bgra,
                    width: self.width,
                    height: self.height,
                }))
            }
        }
    }

    struct ReleaseGuard<'a> {
        duplication: &'a IDXGIOutputDuplication,
    }

    impl Drop for ReleaseGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}

#[cfg(windows)]
pub use dxgi::DxgiCapture;

/// Downscale a captured desktop image to the encoder size.
pub fn prepare_frame(captured: &Captured, max_width: u32) -> Result<(Vec<u8>, u32, u32)> {
    let (width, height) = fit_capture_size(captured.width, captured.height, max_width);
    if width == captured.width && height == captured.height {
        return Ok((captured.bgra.clone(), width, height));
    }
    let scaled = scale_bgra(
        &captured.bgra,
        captured.width,
        captured.height,
        captured.width as usize * 4,
        width,
        height,
    )
    .context("downscale")?;
    Ok((scaled, width, height))
}

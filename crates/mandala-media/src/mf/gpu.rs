//! Turning a decoded texture into a tile-sized frame, on the GPU.
//!
//! Media Foundation will hand back a frame in system memory if asked, and it
//! is a tempting thing to ask for: the pixels arrive ready to upload and
//! nothing here has to know about Direct3D. It is also what makes the whole
//! pipeline abandon the hardware decoder, because a frame in system memory is
//! not something the decoder can produce without leaving the GPU.
//!
//! Measured on a 3840x2160 HEVC clip, two streams at once: asking for RGB in
//! system memory gave 32 frames a second each and kept ten processor cores
//! busy. Taking the decoder's own texture and doing the conversion here gave
//! 270 frames a second each for four tenths of one core -- eight times the
//! frames for a twenty-sixth of the processor. VLC, for comparison, plays one
//! such clip in about a tenth of a core, so this is the right order of
//! magnitude rather than a lucky measurement.
//!
//! What crosses the bus is the finished tile, a megabyte or so, rather than
//! the 4K frame it came from -- or rather than the whole decode, which is what
//! the system-memory request really costs.

use crate::frame::{Frame, Layout};
use anyhow::{Context, Result, anyhow};
use std::time::Duration;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::{IMFDXGIBuffer, IMFSample};
use windows::core::Interface;

/// Converts decoded textures into tile-sized frames.
///
/// Everything expensive is built once: the processor, the render target, and
/// the staging texture the finished tile is read back through. Only the input
/// view is per frame, because the decoder hands out a different slice of its
/// own texture array each time.
pub struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    context: ID3D11DeviceContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    target: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    size: (u32, u32),
    /// Set when the processor would only write BGRA, so the channels have to
    /// be exchanged on the way out.
    swap_rb: bool,
}

// Used from the one worker thread that owns the stream, like the reader beside
// it. The Direct3D device is shared and marked multithread-protected, which is
// what makes that safe.
unsafe impl Send for Converter {}

impl Converter {
    /// Builds a converter from `source` pixels to `target` pixels.
    pub fn new(device: &ID3D11Device, source: (u32, u32), target: (u32, u32)) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice =
                device.cast().context("this device has no video support")?;
            let context = device.GetImmediateContext().context("no immediate context")?;
            let video_context: ID3D11VideoContext = context.cast().context("no video context")?;

            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputWidth: source.0,
                InputHeight: source.1,
                OutputWidth: target.0,
                OutputHeight: target.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                ..Default::default()
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&content)
                .context("no video processor for this conversion")?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .context("could not create the video processor")?;

            // RGBA if the hardware will write it, so nothing has to exchange
            // channels afterwards; BGRA is the one every driver supports.
            let rgba_ok =
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_R8G8B8A8_UNORM).is_ok_and(
                    |support| support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0,
                );
            let format =
                if rgba_ok { DXGI_FORMAT_R8G8B8A8_UNORM } else { DXGI_FORMAT_B8G8R8A8_UNORM };

            let describe = |usage: D3D11_USAGE, bind: u32, cpu: u32| D3D11_TEXTURE2D_DESC {
                Width: target.0,
                Height: target.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: usage,
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: 0,
            };

            let mut rendered = None;
            device
                .CreateTexture2D(
                    &describe(D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET.0 as u32, 0),
                    None,
                    Some(&mut rendered),
                )
                .context("could not create the render target")?;
            let rendered = rendered.ok_or_else(|| anyhow!("no render target"))?;

            let mut staging = None;
            device
                .CreateTexture2D(
                    &describe(D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32),
                    None,
                    Some(&mut staging),
                )
                .context("could not create the staging texture")?;
            let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;

            let mut output_view = None;
            video_device
                .CreateVideoProcessorOutputView(
                    &rendered,
                    &enumerator,
                    &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        ..Default::default()
                    },
                    Some(&mut output_view),
                )
                .context("could not create the output view")?;
            let output_view = output_view.ok_or_else(|| anyhow!("no output view"))?;

            Ok(Self {
                video_device,
                video_context,
                context,
                enumerator,
                processor,
                target: rendered,
                staging,
                output_view,
                size: target,
                swap_rb: !rgba_ok,
            })
        }
    }

    /// Whether a sample is a texture this converter can take.
    ///
    /// A stream can fall back to system memory partway through -- a lost
    /// device, a format the decoder will not accelerate -- and a converter
    /// handed a system-memory buffer would fail on every frame rather than
    /// letting the caller take the ordinary path.
    pub fn accepts(sample: &IMFSample) -> bool {
        unsafe {
            sample.GetBufferByIndex(0).is_ok_and(|buffer| buffer.cast::<IMFDXGIBuffer>().is_ok())
        }
    }

    /// Converts and scales one decoded frame, and reads the result back.
    pub fn convert(&self, sample: &IMFSample, timestamp: Duration) -> Result<Frame> {
        unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer =
                buffer.cast().context("this frame is not a texture after all")?;

            // The decoder writes into one texture array and hands out a slice
            // of it per frame, so which slice has to travel with the texture.
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(
                &ID3D11Texture2D::IID,
                &mut texture as *mut _ as *mut *mut std::ffi::c_void,
            )
            .context("the frame's texture could not be read")?;
            let texture = texture.ok_or_else(|| anyhow!("no texture in the frame"))?;
            let slice = dxgi.GetSubresourceIndex().unwrap_or(0);

            let mut input_view = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    &texture,
                    &self.enumerator,
                    &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
                        },
                        ..Default::default()
                    },
                    Some(&mut input_view),
                )
                .context("could not view the decoded frame")?;

            let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::transmute_copy(&input_view),
                ..Default::default()
            }];
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams)
                .context("the conversion failed")?;

            // Only now is the frame small enough to be worth moving.
            self.context.CopyResource(&self.staging, &self.target);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("could not read the finished tile back")?;
            let bytes = std::slice::from_raw_parts(
                mapped.pData as *const u8,
                mapped.RowPitch as usize * self.size.1 as usize,
            );
            let frame = Frame::from_packed(
                bytes,
                Layout {
                    stride: mapped.RowPitch as usize,
                    bottom_up: false,
                    swap_rb: self.swap_rb,
                },
                self.size.0,
                self.size.1,
                timestamp,
            );
            self.context.Unmap(&self.staging, 0);
            frame
        }
    }
}

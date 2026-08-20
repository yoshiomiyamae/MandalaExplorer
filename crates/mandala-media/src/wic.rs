//! Still decoding through Windows Imaging Component.
//!
//! The `image` crate decodes the formats it decodes. WIC decodes whatever
//! codecs Windows has, which is how HEIC arrives here without dragging libheif
//! and a C toolchain in behind it -- and it keeps the promise that there is no
//! runtime to install.
//!
//! What Windows has is not fixed. HEIC needs the HEIF Image Extension *and*
//! the HEVC Video Extension, because the pictures inside a `.heic` are coded
//! with HEVC, and the second of those is a paid download. So this module can
//! say whether the machine it is running on can do it, rather than assuming.

use crate::com::ensure_thread_com;
use crate::frame::Frame;
use crate::sizing::fit_within;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::Duration;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::HSTRING;

/// Decodes an image and scales it down to fit `max`.
///
/// Scaling happens inside WIC rather than afterwards, so a 48-megapixel phone
/// photo never exists as 192 MB of RGBA on the way to a 256-pixel tile.
pub fn load_thumbnail(path: &Path, max: (u32, u32)) -> Result<Frame> {
    ensure_thread_com();
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER)
                .context("creating the WIC factory")?;

        let decoder = factory
            .CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                windows::Win32::Foundation::GENERIC_READ,
                // The metadata is not wanted, and reading it on demand means a
                // file that has none costs nothing.
                WICDecodeMetadataCacheOnDemand,
            )
            .with_context(|| format!("no WIC decoder for {}", path.display()))?;

        let source = decoder
            .GetFrame(0)
            .with_context(|| format!("reading the first frame of {}", path.display()))?;

        let mut width = 0u32;
        let mut height = 0u32;
        source.GetSize(&mut width, &mut height).context("reading the image size")?;
        if width == 0 || height == 0 {
            bail!("{} decoded to nothing", path.display());
        }

        let (w, h) = fit_within((width, height), max);

        // Scale first, convert second: the scaler works in the source's own
        // pixel format, so converting first would only mean converting pixels
        // that are about to be thrown away.
        let scaler = factory.CreateBitmapScaler().context("creating the scaler")?;
        scaler
            .Initialize(&source, w, h, WICBitmapInterpolationModeFant)
            .context("initialising the scaler")?;

        let converter = factory.CreateFormatConverter().context("creating the converter")?;
        converter
            .Initialize(
                &scaler,
                // The one format the rest of the program speaks.
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .context("converting to RGBA")?;

        let stride = w * 4;
        let mut rgba = vec![0u8; (stride * h) as usize];
        converter.CopyPixels(std::ptr::null(), stride, &mut rgba).context("copying pixels")?;

        Ok(Frame { width: w, height: h, rgba, timestamp: Duration::ZERO })
    }
}

/// Whether this machine can decode HEIF, and so `.heic` and `.heif` files.
///
/// Asked of WIC rather than answered from a list of Windows versions: the
/// codec arrives as a Store extension that can be installed or not on any of
/// them, and the only honest answer is whether a decoder exists right now.
pub fn heif_available() -> bool {
    ensure_thread_com();
    unsafe {
        let Ok(factory): Result<IWICImagingFactory, _> =
            CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER)
        else {
            return false;
        };
        // Asking for a decoder by container format succeeds only when one is
        // registered for it. An earlier version asked CreateComponentInfo,
        // which wants a component CLSID rather than a container format, and so
        // answered no on a machine that could decode HEIF perfectly well.
        factory.CreateDecoder(&GUID_ContainerFormatHeif, std::ptr::null()).is_ok()
    }
}

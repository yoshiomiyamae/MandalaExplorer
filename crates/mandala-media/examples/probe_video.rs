//! Reports what a video is, and where its frames go.
//!
//! Written because "playback is stuttery" has several causes that look
//! identical from the outside. Five plausible guesses were wrong here before
//! this measured the right thing, and the thing that mattered turned out not to
//! be how fast frames decode but how many of them reach the screen.
//!
//! ```
//! cargo run --release -p mandala-media --example probe_video -- "C:\some\clip.mov" 512 4
//! ```
//!
//! The second argument is the tile size to decode for, the third how many
//! copies to play at once -- which is the question a grid actually asks, and
//! one that a single stream keeping up says nothing about.

#![cfg(windows)]

use mandala_media::backend::{Advance, MediaBackend};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{GUID, HSTRING};

/// The subtypes worth naming. Everything else prints as its GUID, which is
/// still enough to look up.
fn codec_name(subtype: &GUID) -> String {
    let known: [(GUID, &str); 8] = [
        (MFVideoFormat_H264, "H.264"),
        (MFVideoFormat_HEVC, "HEVC"),
        (MFVideoFormat_HEVC_ES, "HEVC (elementary stream)"),
        (MFVideoFormat_MPEG2, "MPEG-2"),
        (MFVideoFormat_MP4V, "MPEG-4 part 2"),
        (MFVideoFormat_WMV3, "WMV9"),
        (MFVideoFormat_MJPG, "Motion JPEG"),
        (MFVideoFormat_AV1, "AV1"),
    ];
    known
        .iter()
        .find(|(guid, _)| guid == subtype)
        .map(|(_, name)| (*name).to_owned())
        .unwrap_or_else(|| format!("{subtype:?}"))
}

/// Processor time this process has used, kernel and user together.
///
/// Frames per second says nothing about this. Two pipelines can deliver the
/// same number of frames while one of them burns a core doing it, which is
/// exactly the complaint that started this: a fifth of the GPU and all of the
/// processor.
fn cpu_time() -> Duration {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    unsafe {
        let mut created = Default::default();
        let mut exited = Default::default();
        let mut kernel = Default::default();
        let mut user = Default::default();
        if GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user)
            .is_err()
        {
            return Duration::ZERO;
        }
        let hns = |t: windows::Win32::Foundation::FILETIME| {
            ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
        };
        Duration::from_nanos((hns(kernel) + hns(user)) * 100)
    }
}

/// Runs `work` and reports what it cost in processor time per frame.
fn cpu_per_frame(label: &str, work: impl FnOnce() -> anyhow::Result<f64>) -> anyhow::Result<()> {
    let before = cpu_time();
    let started = Instant::now();
    let fps = work()?;
    let cpu = cpu_time().saturating_sub(before);
    let wall = started.elapsed();
    // Cores' worth: one means a whole processor kept busy for the duration.
    println!(
        "  {label:<28} {fps:6.1} fps   {:6.1} ms of CPU per frame   {:.2} cores",
        if fps > 0.0 { cpu.as_secs_f64() * 1000.0 / (fps * wall.as_secs_f64()) } else { 0.0 },
        cpu.as_secs_f64() / wall.as_secs_f64()
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(
        std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: probe_video <file>"))?,
    );
    let tile: u32 = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(512);
    let streams: usize = std::env::args().nth(3).and_then(|n| n.parse().ok()).unwrap_or(1);

    println!("file      {}", path.display());
    if let Ok(meta) = std::fs::metadata(&path) {
        println!("size      {:.1} MB", meta.len() as f64 / 1_048_576.0);
    }
    describe(&path)?;

    // Hardware decoding, settled by taking it away. The performance counter for
    // the video-decode engine reads zero even while decoding H.264 at hundreds
    // of frames a second, so it cannot answer this; a decoder that collapses
    // when the Direct3D device is withheld was using it.
    println!("---- is the GPU decoding? ----");
    for with_d3d in [true, false] {
        let rate = raw_rate(&path, tile, with_d3d)?;
        let label = if with_d3d { "with a D3D device" } else { "without one" };
        println!("  {label:<26} {rate:.1} fps");
    }

    // The measurement that matters, and the one that took longest to reach.
    // Decoding fast is not the same as showing frames: a stream asked for the
    // present moment that cannot reach it spends its whole budget on frames it
    // discards on the way, and falls further behind with every call.
    println!("---- frames decoded, against frames shown ({streams} at once) ----");
    println!(
        "  {:<26} {:.1} fps each",
        "shown, chasing the clock",
        shown_rate(&path, tile, streams, true)?
    );
    println!(
        "  {:<26} {:.1} fps each",
        "shown, taking what arrives",
        shown_rate(&path, tile, streams, false)?
    );

    // Where the processor time goes. The reader alone hands back a sample and
    // is done with it; the app then copies every frame out of the buffer into
    // one of its own, which is work the frame rate hides completely.
    println!("---- what it costs the processor ({streams} at once) ----");
    cpu_per_frame("reader alone", || raw_rate_together(&path, tile, streams))?;
    cpu_per_frame("reader, then our copy", || decoded_rate(&path, tile, streams))?;

    // What the pipeline is asked to produce, priced in processor time rather
    // than in frames. Asking for RGB in system memory means something has to
    // convert and scale a 4K frame and hand it back across the bus; asking for
    // the decoder's own format at its own size asks for none of that.
    list_decoders();

    // The one combination never tried: no advanced video processing, and the
    // decoder's own format, so nothing in the pipeline is obliged to produce a
    // frame in system memory. If the buffers come back as textures the decode
    // stayed on the GPU, and the processor time should say so.
    println!("---- leaving the frames on the GPU ----");
    cpu_per_frame("NV12, no processing", || gpu_rate(&path, streams))?;

    // The design worth having: decode into a texture, convert and shrink it on
    // the GPU, and copy back only the tile-sized result. Everything the app
    // needs, without asking the pipeline for anything that would push it off
    // the hardware decoder.
    cpu_per_frame("...then GPU convert, small readback", || {
        gpu_convert_rate(&path, tile, streams)
    })?;

    println!("---- what the output format costs ({streams} at once) ----");
    for (label, want) in [
        ("NV12, no scale (3840)", Some((MFVideoFormat_NV12, 3840))),
        ("NV12, scaled to tile", Some((MFVideoFormat_NV12, tile))),
        ("RGB32, scaled to tile", Some((MFVideoFormat_RGB32, tile))),
    ] {
        cpu_per_frame(label, || raw_rate_as(&path, want, streams))?;
    }
    Ok(())
}

/// Reads without asking for anything the GPU cannot hand back directly.
///
/// Reports whether the buffers really are textures, since that is the whole
/// question: a sample whose buffer answers to IMFDXGIBuffer never left the
/// GPU, and one that does not has been copied across the bus.
fn gpu_rate(path: &Path, streams: usize) -> anyhow::Result<f64> {
    use windows::core::Interface;
    let reported = std::sync::atomic::AtomicBool::new(false);
    let started = Instant::now();
    let counts: Vec<u32> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..streams)
            .map(|_| {
                let reported = &reported;
                scope.spawn(move || unsafe {
                    let mut attributes = None;
                    if MFCreateAttributes(&mut attributes, 2).is_err() {
                        return 0;
                    }
                    let attributes = attributes.unwrap();
                    // Deliberately no ENABLE_ADVANCED_VIDEO_PROCESSING.
                    if let Some(device) = mandala_media::mf::d3d::shared_device() {
                        let _ =
                            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, device.manager());
                    }
                    let Ok(reader) =
                        MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes)
                    else {
                        return 0;
                    };
                    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
                    // NV12 at whatever size the decoder already produces.
                    if let Ok(native) = reader.GetNativeMediaType(stream, 0)
                        && let Ok(output) = MFCreateMediaType()
                    {
                        let _ = native.CopyAllItems(&output);
                        let _ = output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
                        if let Err(e) = reader.SetCurrentMediaType(stream, None, &output) {
                            eprintln!("    NV12 refused: {e}");
                            return 0;
                        }
                    }

                    let mut frames = 0u32;
                    while started.elapsed() < Duration::from_secs(4) {
                        let mut flags = 0u32;
                        let mut sample = None;
                        if reader
                            .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                            .is_err()
                            || flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0
                        {
                            break;
                        }
                        if let Some(sample) = sample {
                            if !reported.swap(true, std::sync::atomic::Ordering::Relaxed)
                                && let Ok(buffer) = sample.GetBufferByIndex(0)
                            {
                                let on_gpu = buffer.cast::<IMFDXGIBuffer>().is_ok();
                                println!(
                                    "  buffers are {}",
                                    if on_gpu { "GPU textures" } else { "system memory" }
                                );
                            }
                            frames += 1;
                        }
                    }
                    frames
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
    });
    Ok(counts.iter().sum::<u32>() as f64 / streams as f64 / started.elapsed().as_secs_f64())
}

/// Decodes to a texture, converts and scales it on the GPU, and reads back
/// only the tile-sized result.
///
/// This is the shape the app wants: the frame stays where the decoder put it
/// until it is small, and what crosses the bus is a megabyte rather than a
/// 4K frame -- or rather than the whole decode, which is what asking for a
/// system-memory frame costs.
fn gpu_convert_rate(path: &Path, tile: u32, streams: usize) -> anyhow::Result<f64> {
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;

    let started = Instant::now();
    let counts: Vec<u32> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..streams)
            .map(|_| {
                scope.spawn(move || unsafe {
                    let Some(shared) = mandala_media::mf::d3d::shared_device() else { return 0 };
                    let device = shared.device();

                    let mut attributes = None;
                    if MFCreateAttributes(&mut attributes, 2).is_err() {
                        return 0;
                    }
                    let attributes = attributes.unwrap();
                    let _ = attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, shared.manager());
                    let Ok(reader) =
                        MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes)
                    else {
                        return 0;
                    };
                    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

                    let Ok(native) = reader.GetNativeMediaType(stream, 0) else { return 0 };
                    let Ok(output) = MFCreateMediaType() else { return 0 };
                    let _ = native.CopyAllItems(&output);
                    let _ = output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
                    if reader.SetCurrentMediaType(stream, None, &output).is_err() {
                        return 0;
                    }
                    let Ok(size) = native.GetUINT64(&MF_MT_FRAME_SIZE) else { return 0 };
                    let (src_w, src_h) = ((size >> 32) as u32, size as u32);

                    // --- the conversion pipeline, built once -----------------
                    let Ok(video_device) = device.cast::<ID3D11VideoDevice>() else { return 0 };
                    let Ok(context) = device.GetImmediateContext() else { return 0 };
                    let Ok(video_context) = context.cast::<ID3D11VideoContext>() else { return 0 };

                    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                        InputWidth: src_w,
                        InputHeight: src_h,
                        OutputWidth: tile,
                        OutputHeight: tile,
                        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                        ..Default::default()
                    };
                    let Ok(enumerator) = video_device.CreateVideoProcessorEnumerator(&content)
                    else {
                        return 0;
                    };
                    let Ok(processor) = video_device.CreateVideoProcessor(&enumerator, 0) else {
                        return 0;
                    };

                    let describe = |usage, cpu| D3D11_TEXTURE2D_DESC {
                        Width: tile,
                        Height: tile,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: usage,
                        BindFlags: if usage == D3D11_USAGE_DEFAULT {
                            D3D11_BIND_RENDER_TARGET.0 as u32
                        } else {
                            0
                        },
                        CPUAccessFlags: cpu,
                        MiscFlags: 0,
                    };
                    let mut target = None;
                    if device
                        .CreateTexture2D(&describe(D3D11_USAGE_DEFAULT, 0), None, Some(&mut target))
                        .is_err()
                    {
                        return 0;
                    }
                    let Some(target) = target else { return 0 };
                    let mut staging = None;
                    if device
                        .CreateTexture2D(
                            &describe(D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32),
                            None,
                            Some(&mut staging),
                        )
                        .is_err()
                    {
                        return 0;
                    }
                    let Some(staging) = staging else { return 0 };

                    let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        ..Default::default()
                    };
                    let mut out_view = None;
                    if video_device
                        .CreateVideoProcessorOutputView(
                            &target,
                            &enumerator,
                            &out_desc,
                            Some(&mut out_view),
                        )
                        .is_err()
                    {
                        return 0;
                    }
                    let Some(out_view) = out_view else { return 0 };

                    let mut frames = 0u32;
                    let mut pixels = vec![0u8; (tile * tile * 4) as usize];
                    while started.elapsed() < Duration::from_secs(4) {
                        let mut flags = 0u32;
                        let mut sample = None;
                        if reader
                            .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                            .is_err()
                            || flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0
                        {
                            break;
                        }
                        let Some(sample) = sample else { continue };
                        let Ok(buffer) = sample.GetBufferByIndex(0) else { continue };
                        let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() else { continue };
                        let mut texture: Option<ID3D11Texture2D> = None;
                        if dxgi
                            .GetResource(
                                &ID3D11Texture2D::IID,
                                &mut texture as *mut _ as *mut *mut std::ffi::c_void,
                            )
                            .is_err()
                        {
                            continue;
                        }
                        let Some(texture) = texture else { continue };
                        let slice = dxgi.GetSubresourceIndex().unwrap_or(0);

                        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
                            },
                            ..Default::default()
                        };
                        let mut in_view = None;
                        if video_device
                            .CreateVideoProcessorInputView(
                                &texture,
                                &enumerator,
                                &in_desc,
                                Some(&mut in_view),
                            )
                            .is_err()
                        {
                            continue;
                        }

                        let streams_desc = [D3D11_VIDEO_PROCESSOR_STREAM {
                            Enable: true.into(),
                            pInputSurface: std::mem::transmute_copy(&in_view),
                            ..Default::default()
                        }];
                        if video_context
                            .VideoProcessorBlt(&processor, &out_view, 0, &streams_desc)
                            .is_err()
                        {
                            continue;
                        }
                        context.CopyResource(&staging, &target);

                        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                        if context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_err() {
                            continue;
                        }
                        for row in 0..tile as usize {
                            let from =
                                (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                            let to = row * tile as usize * 4;
                            std::ptr::copy_nonoverlapping(
                                from,
                                pixels.as_mut_ptr().add(to),
                                tile as usize * 4,
                            );
                        }
                        context.Unmap(&staging, 0);
                        frames += 1;
                    }
                    frames
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
    });
    Ok(counts.iter().sum::<u32>() as f64 / streams as f64 / started.elapsed().as_secs_f64())
}

/// Lists the HEVC decoders Windows offers, hardware ones first.
///
/// The point is whether a hardware decoder exists at all. If one does and the
/// pipeline is still burning processor time, the pipeline is not using it; if
/// none does, then no amount of configuring will conjure one.
fn list_decoders() {
    unsafe {
        for (codec, subtype) in [("HEVC", MFVideoFormat_HEVC), ("H.264", MFVideoFormat_H264)] {
            // H.264 is the control. If it finds a hardware decoder and HEVC does
            // not, the answer is about this machine's codecs; if neither does, the
            // answer is about this enumeration being wrong.
            for (label, flags) in [
                ("hardware", MFT_ENUM_FLAG_HARDWARE),
                ("hardware async", MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT),
                ("software", MFT_ENUM_FLAG_SYNCMFT),
            ] {
                let input = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: subtype,
                };
                let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count = 0u32;
                let found = MFTEnumEx(
                    MFT_CATEGORY_VIDEO_DECODER,
                    flags,
                    Some(&input),
                    None,
                    &mut activates,
                    &mut count,
                );
                if found.is_err() {
                    println!("  {codec:<6} {label:<15} enumeration failed");
                    continue;
                }
                println!("  {codec:<6} {label:<15} {count} decoder(s)");
                for i in 0..count as isize {
                    let Some(activate) = (*activates.offset(i)).as_ref() else { continue };
                    let mut name = windows::core::PWSTR::null();
                    let mut len = 0u32;
                    if activate
                        .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
                        .is_ok()
                    {
                        println!(
                            "                            {}",
                            name.to_string().unwrap_or_default()
                        );
                        windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as *const _));
                    }
                }
                windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
            }
        }
    }
}

/// Prints what the file holds, from its own headers.
fn describe(path: &Path) -> anyhow::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;

        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let native = reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)?;

        let size = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let rate = native.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        println!("codec     {}", codec_name(&native.GetGUID(&MF_MT_SUBTYPE)?));
        println!("frame     {}x{}", (size >> 32) as u32, size as u32);
        println!("rate      {:.2} fps", (rate >> 32) as u32 as f64 / (rate as u32).max(1) as f64);
        // For HEVC, 1 is Main and 2 is Main 10; ten-bit costs noticeably more.
        if let Ok(profile) = native.GetUINT32(&MF_MT_VIDEO_PROFILE) {
            println!("profile   {profile}");
        }
    }
    Ok(())
}

/// As `raw_rate`, but with the same number of readers running at once as the
/// path it is being compared against. Comparing one stream with four measures
/// concurrency rather than the copy.
fn raw_rate_together(path: &Path, tile: u32, streams: usize) -> anyhow::Result<f64> {
    let started = Instant::now();
    let counts: Vec<u32> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..streams)
            .map(|_| {
                scope.spawn(move || unsafe {
                    let Ok(reader) = open_reader(path, tile) else { return 0 };
                    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
                    let mut frames = 0u32;
                    while started.elapsed() < Duration::from_secs(4) {
                        let mut flags = 0u32;
                        let mut sample = None;
                        if reader
                            .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                            .is_err()
                            || flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0
                        {
                            break;
                        }
                        if sample.is_some() {
                            frames += 1;
                        }
                    }
                    frames
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
    });
    Ok(counts.iter().sum::<u32>() as f64 / streams as f64 / started.elapsed().as_secs_f64())
}

/// Reads with a chosen output format, or with none set at all.
fn raw_rate_as(path: &Path, want: Option<(GUID, u32)>, streams: usize) -> anyhow::Result<f64> {
    let started = Instant::now();
    let counts: Vec<u32> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..streams)
            .map(|_| {
                scope.spawn(move || unsafe {
                    let mut attributes = None;
                    if MFCreateAttributes(&mut attributes, 2).is_err() {
                        return 0;
                    }
                    let attributes = attributes.unwrap();
                    let _ =
                        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1);
                    if let Some(device) = mandala_media::mf::d3d::shared_device() {
                        let _ =
                            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, device.manager());
                    }
                    let Ok(reader) =
                        MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes)
                    else {
                        return 0;
                    };
                    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
                    if let Some((subtype, size)) = want
                        && let Ok(output) = MFCreateMediaType()
                    {
                        let _ = output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
                        let _ = output.SetGUID(&MF_MT_SUBTYPE, &subtype);
                        let _ = output
                            .SetUINT64(&MF_MT_FRAME_SIZE, ((size as u64) << 32) | size as u64);
                        let _ = reader.SetCurrentMediaType(stream, None, &output);
                    } else {
                        // Nothing set: the decoder's own format, whatever it is.
                        if let Ok(native) = reader.GetNativeMediaType(stream, 0) {
                            let _ = reader.SetCurrentMediaType(stream, None, &native);
                        }
                    }

                    let mut frames = 0u32;
                    while started.elapsed() < Duration::from_secs(4) {
                        let mut flags = 0u32;
                        let mut sample = None;
                        if reader
                            .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                            .is_err()
                            || flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0
                        {
                            break;
                        }
                        if sample.is_some() {
                            frames += 1;
                        }
                    }
                    frames
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
    });
    Ok(counts.iter().sum::<u32>() as f64 / streams as f64 / started.elapsed().as_secs_f64())
}

/// A reader configured the way the app configures one.
unsafe fn open_reader(path: &Path, tile: u32) -> anyhow::Result<IMFSourceReader> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2)?;
        let attributes = attributes.unwrap();
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        if let Some(device) = mandala_media::mf::d3d::shared_device() {
            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, device.manager())?;
        }
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, ((tile as u64) << 32) | tile as u64)?;
        let _ = reader.SetCurrentMediaType(stream, None, &output);
        Ok(reader)
    }
}

/// Frames a bare Source Reader produces per second, reading straight through.
fn raw_rate(path: &Path, tile: u32, with_d3d: bool) -> anyhow::Result<f64> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2)?;
        let attributes = attributes.unwrap();
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        if with_d3d && let Some(device) = mandala_media::mf::d3d::shared_device() {
            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, device.manager())?;
        }

        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

        // The same output the app asks for, so this measures the same work.
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, ((tile as u64) << 32) | tile as u64)?;
        let _ = reader.SetCurrentMediaType(stream, None, &output);

        let started = Instant::now();
        let mut frames = 0u32;
        while started.elapsed() < Duration::from_secs(2) {
            let mut flags = 0u32;
            let mut sample = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if sample.is_some() {
                frames += 1;
            }
        }
        Ok(frames as f64 / started.elapsed().as_secs_f64())
    }
}

/// Frames per second the decoder produces through the app's own stream type,
/// asked for one at a time so nothing is ever skipped.
fn decoded_rate(path: &Path, tile: u32, streams: usize) -> anyhow::Result<f64> {
    each_second(streams, |backend, started| {
        let Ok(mut video) = backend.open_video(path, (tile, tile)) else { return 0 };
        let mut frames = 0u32;
        let mut at = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(4) {
            at += Duration::from_millis(1);
            match video.advance_to(at) {
                Ok(Advance::Frame(_)) => frames += 1,
                Ok(Advance::Unchanged) => {}
                _ => break,
            }
        }
        frames
    })
}

/// Frames per second that actually reach a caller, under either policy.
///
/// `chase_clock` asks for the frame belonging to this instant, which is what
/// the app did before it learned better. The alternative asks for a little past
/// the last frame it was given, and shows whatever the decoder manages.
fn shown_rate(path: &Path, tile: u32, streams: usize, chase_clock: bool) -> anyhow::Result<f64> {
    each_second(streams, move |backend, started| {
        let Ok(mut video) = backend.open_video(path, (tile, tile)) else { return 0 };
        let mut shown = 0u32;
        let mut at = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(4) {
            at = if chase_clock { started.elapsed() } else { at + Duration::from_millis(1) };
            match video.advance_to(at) {
                Ok(Advance::Frame(_)) => shown += 1,
                Ok(Advance::Unchanged) => {}
                _ => break,
            }
        }
        shown
    })
}

/// Runs `work` on `streams` threads and averages what they counted.
fn each_second<F>(streams: usize, work: F) -> anyhow::Result<f64>
where
    F: Fn(mandala_media::MediaFoundation, Instant) -> u32 + Copy + Send + Sync,
{
    let backend = mandala_media::MediaFoundation::new()?;
    let started = Instant::now();
    let counts: Vec<u32> = std::thread::scope(|scope| {
        let handles: Vec<_> =
            (0..streams).map(|_| scope.spawn(move || work(backend, started))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
    });
    Ok(counts.iter().sum::<u32>() as f64 / streams as f64 / started.elapsed().as_secs_f64())
}

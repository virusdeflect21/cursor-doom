//! MP4 recording of the cursor frames via Media Foundation's H.264 encoder.
//!
//! Only the native cursor frames are encoded (e.g. 256x160): no desktop, no
//! scaling - exactly what Doom draws on the cursor.
//!
//! Uses the SinkWriter so the encoder + MP4 muxer are handled by Windows.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFByteStream, IMFMediaType, IMFSinkWriter, MFCreateAttributes,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL,
    MFMediaType_Video, MFStartup, MFVideoFormat_H264, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, MFSTARTUP_FULL, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::engine;

/// 100ns units, matching the engine's frame cadence.
const FRAME_DURATION_100NS: i64 = 10_000_000 / engine::FPS as i64;

/// Smallest frame the Windows H.264 encoder accepts (measured: 34x36 and 36x34
/// work, 36x32 and 32x36 do not).
pub const MIN_DIMENSION: u32 = 34;

/// Integer factor the encoder needs for a given source size, so that tiny cursor
/// resolutions (16x16, 32x20, ...) can still be recorded. Pixel art upscaled by a
/// whole number stays pixel art - no interpolation, no blur.
pub fn encoder_scale(width: u32, height: u32) -> u32 {
    let smallest = width.min(height).max(1);
    if smallest >= MIN_DIMENSION {
        1
    } else {
        MIN_DIMENSION.div_ceil(smallest).max(1)
    }
}

fn werr(context: &str, err: windows::core::Error) -> String {
    format!(
        "{context}: {} (0x{:08X})",
        err.message(),
        err.code().0 as u32
    )
}

fn init_media_foundation() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| unsafe {
        // RPC_E_CHANGED_MODE simply means COM is already up in another mode.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| werr("MFStartup failed", e))
    })
    .clone()
}

pub struct Recorder {
    writer: IMFSinkWriter,
    stream_index: u32,
    /// Size of the encoded video (the source size times `scale`).
    width: u32,
    height: u32,
    /// Size of the frames the caller hands to [`Recorder::push`].
    source_width: u32,
    source_height: u32,
    /// Whole-number upscale applied to satisfy the encoder's minimum size.
    scale: u32,
    frames: u64,
    path: PathBuf,
    slot: i64,
    scratch: Vec<u8>,
}

impl Recorder {
    pub fn start(path: &Path, width: u32, height: u32) -> Result<Self, String> {
        init_media_foundation()?;

        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(format!("H.264 needs even dimensions, got {width}x{height}"));
        }
        if width == 0 || height == 0 {
            return Err(format!("cannot record a {width}x{height} frame"));
        }

        // Anything from 16x16 up is recordable: sizes the encoder refuses are
        // upscaled by a whole number, so the video shows exactly the cursor
        // pixels, just bigger.
        let (source_width, source_height) = (width, height);
        let scale = encoder_scale(width, height);
        let width = width * scale;
        let height = height * scale;

        let url = HSTRING::from(path.to_string_lossy().to_string());

        unsafe {
            let writer =
                MFCreateSinkWriterFromURL(&url, None::<&IMFByteStream>, None::<&IMFAttributes>)
                    .map_err(|e| werr("MFCreateSinkWriterFromURL failed", e))?;

            // Output description: H.264 in an MP4 container, ~35 fps.
            let out_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| werr("MFCreateMediaType failed", e))?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| werr("output major type", e))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| werr("output subtype", e))?;
            let bitrate = ((width as u64 * height as u64 * engine::FPS as u64) / 8)
                .clamp(200_000, 20_000_000) as u32;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(|e| werr("output bitrate", e))?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| werr("output interlace", e))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                .map_err(|e| werr("output frame size", e))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_RATE, ((engine::FPS as u64) << 32) | 1)
                .map_err(|e| werr("output frame rate", e))?;
            out_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                .map_err(|e| werr("output pixel aspect", e))?;

            let stream_index = writer
                .AddStream(&out_type)
                .map_err(|e| werr("SinkWriter::AddStream failed", e))?;

            // Input description: plain 32-bit BGRA, top-down (positive stride).
            let in_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| werr("MFCreateMediaType failed", e))?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| werr("input major type", e))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| werr("input subtype", e))?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| werr("input interlace", e))?;
            in_type
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)
                .map_err(|e| werr("input stride", e))?;
            in_type
                .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
                .map_err(|e| werr("input samples independent", e))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                .map_err(|e| werr("input frame size", e))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, ((engine::FPS as u64) << 32) | 1)
                .map_err(|e| werr("input frame rate", e))?;
            in_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                .map_err(|e| werr("input pixel aspect", e))?;

            writer
                .SetInputMediaType(stream_index, &in_type, None::<&IMFAttributes>)
                .map_err(|e| werr("SinkWriter::SetInputMediaType failed", e))?;
            writer
                .BeginWriting()
                .map_err(|e| werr("SinkWriter::BeginWriting failed", e))?;

            Ok(Self {
                writer,
                stream_index,
                width,
                height,
                source_width,
                source_height,
                scale,
                frames: 0,
                path: path.to_path_buf(),
                slot: 0,
                scratch: Vec::new(),
            })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// `(width, height)` of the video file itself.
    pub fn encoded_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whole-number upscale between the cursor frames and the video.
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Encodes one frame of native-resolution BGRA pixels (upscaled first when
    /// the cursor is smaller than the encoder's minimum).
    pub fn push(&mut self, bgra: &[u8], elapsed: std::time::Duration) -> Result<(), String> {
        let bytes = (self.width * self.height * 4) as usize;
        let source_bytes = (self.source_width * self.source_height * 4) as usize;
        if bgra.len() < source_bytes {
            return Err(format!(
                "frame too small: {} bytes, expected {source_bytes}",
                bgra.len()
            ));
        }

        let pixels: &[u8] = if self.scale == 1 {
            bgra
        } else {
            self.scratch.resize(bytes, 0);
            let (src_w, scale) = (self.source_width as usize, self.scale as usize);
            let (dst_w, dst_h) = (self.width as usize, self.height as usize);
            for y in 0..dst_h {
                let src_row = (y / scale) * src_w;
                let dst_row = y * dst_w;
                for x in 0..dst_w {
                    let src = (src_row + x / scale) * 4;
                    let dst = (dst_row + x) * 4;
                    self.scratch[dst..dst + 4].copy_from_slice(&bgra[src..src + 4]);
                }
            }
            &self.scratch
        };

        // Frames are placed on the wall-clock slot they were captured in, so a
        // slow machine produces a correctly timed video instead of a sped-up one
        // (the player just drops the frames we never rendered).
        let real_slot = (elapsed.as_micros() as i64 * engine::FPS as i64) / 1_000_000;
        let sample_slot = real_slot.max(self.slot);
        self.slot = sample_slot + 1;

        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes as u32)
                .map_err(|e| werr("MFCreateMemoryBuffer failed", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            buffer
                .Lock(&mut ptr, Some(&mut max_len), None)
                .map_err(|e| werr("IMFMediaBuffer::Lock failed", e))?;
            if ptr.is_null() {
                return Err("IMFMediaBuffer::Lock returned a null pointer".into());
            }
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr, bytes);
            buffer
                .Unlock()
                .map_err(|e| werr("IMFMediaBuffer::Unlock failed", e))?;
            buffer
                .SetCurrentLength(bytes as u32)
                .map_err(|e| werr("IMFMediaBuffer::SetCurrentLength failed", e))?;

            let sample = MFCreateSample().map_err(|e| werr("MFCreateSample failed", e))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| werr("IMFSample::AddBuffer failed", e))?;
            sample
                .SetSampleTime(sample_slot * FRAME_DURATION_100NS)
                .map_err(|e| werr("IMFSample::SetSampleTime failed", e))?;
            sample
                .SetSampleDuration(FRAME_DURATION_100NS)
                .map_err(|e| werr("IMFSample::SetSampleDuration failed", e))?;

            let mut attempt = 0;
            loop {
                match self.writer.WriteSample(self.stream_index, &sample) {
                    Ok(()) => break,
                    Err(e) if attempt < 200 => {
                        // Sink is momentarily full.
                        let _ = e;
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(3));
                    }
                    Err(e) => return Err(werr("SinkWriter::WriteSample failed", e)),
                }
            }
            self.frames += 1;
        }
        Ok(())
    }

    /// Flushes the MP4 (writes the index) and returns the final path.
    pub fn finish(self) -> Result<PathBuf, String> {
        unsafe {
            self.writer
                .Finalize()
                .map_err(|e| werr("SinkWriter::Finalize failed", e))?;
        }
        Ok(self.path)
    }
}

/// Reads the first frame of an MP4 back (used by `--probe` to verify recordings).
pub fn probe_first_frame(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    init_media_foundation()?;
    let url = HSTRING::from(path.to_string_lossy().to_string());

    unsafe {
        // The H.264 decoder outputs YUV; ask the source reader to insert the
        // colour converter so we can read plain RGB32 rows back.
        let mut attributes = None::<IMFAttributes>;
        MFCreateAttributes(&mut attributes, 1).map_err(|e| werr("MFCreateAttributes failed", e))?;
        let attributes = attributes.ok_or("MFCreateAttributes returned nothing")?;
        attributes
            .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
            .map_err(|e| werr("enable video processing", e))?;

        let reader =
            windows::Win32::Media::MediaFoundation::MFCreateSourceReaderFromURL(&url, &attributes)
                .map_err(|e| werr("MFCreateSourceReaderFromURL failed", e))?;

        let wanted: IMFMediaType =
            MFCreateMediaType().map_err(|e| werr("MFCreateMediaType failed", e))?;
        wanted
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| werr("probe major type", e))?;
        wanted
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| werr("probe subtype", e))?;
        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &wanted)
            .map_err(|e| werr("SourceReader::SetCurrentMediaType failed", e))?;

        let actual = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|e| werr("SourceReader::GetCurrentMediaType failed", e))?;
        let packed = actual
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|e| werr("probe frame size", e))?;
        let width = (packed >> 32) as u32;
        let height = (packed & 0xFFFF_FFFF) as u32;
        let stride = actual.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(width * 4) as i32;

        for _ in 0..90 {
            let mut sample = None::<windows::Win32::Media::MediaFoundation::IMFSample>;
            let mut flags = 0u32;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    None,
                    Some(&mut sample),
                )
                .map_err(|e| werr("SourceReader::ReadSample failed", e))?;
            let Some(sample) = sample else { continue };
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| werr("IMFSample::ConvertToContiguousBuffer failed", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| werr("IMFMediaBuffer::Lock failed", e))?;
            if ptr.is_null() || len == 0 {
                let _ = buffer.Unlock();
                continue;
            }
            let data = std::slice::from_raw_parts(ptr, len as usize);
            let mut out = vec![0u8; (width * height * 4) as usize];
            let abs_stride = stride.unsigned_abs() as usize;
            for y in 0..height as usize {
                let src_row = if stride >= 0 {
                    y
                } else {
                    height as usize - 1 - y
                };
                let src = src_row * abs_stride;
                let dst = y * width as usize * 4;
                if src + width as usize * 4 <= data.len() {
                    out[dst..dst + width as usize * 4]
                        .copy_from_slice(&data[src..src + width as usize * 4]);
                }
            }
            let _ = buffer.Unlock();
            return Ok((out, width, height));
        }
        Err("no video frame decoded".into())
    }
}

/// `Downloads/cursor-doom-YYYYMMDD-HHMMSS.mp4`, falling back to the temp dir.
pub fn new_recording_path() -> PathBuf {
    let stamp = local_stamp();
    let name = format!("cursor-doom-{stamp}.mp4");
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(name)
}

fn local_stamp() -> String {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

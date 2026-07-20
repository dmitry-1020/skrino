//! Single-source WASAPI capture (Windows) feeding the recording engine.
//!
//! One [`AudioCapturer`] owns one shared-mode capture stream on a dedicated
//! thread: either loopback of the default render endpoint (system playback) or
//! the default capture endpoint (microphone). It negotiates the device mix
//! format, hands raw packets to the pure DSP in [`super::audio_dsp`] for decode
//! to interleaved-stereo `f32`, and reports the device sample rate so the caller
//! can resample to the encoder's 48 kHz.
//!
//! All COM lives on the thread that constructs the capturer; the object never
//! crosses threads, sidestepping apartment/marshaling concerns.

use windows::Win32::Foundation::S_FALSE;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM, eCapture, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

use super::audio_dsp::{self, SampleFmt};

/// `WAVEFORMATEX::wFormatTag` for the extensible format container; the real
/// subtype then lives in `WAVEFORMATEXTENSIBLE::SubFormat`. The `windows` crate
/// does not re-export this constant under the enabled features.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Shared-mode client buffer duration (100 ns units): 200 ms of slack so a busy
/// system does not drop packets between polls.
const BUFFER_DURATION_HNS: i64 = 2_000_000;

/// Which single endpoint to record.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AudioSel {
    /// Loopback of the default render (output) device: system playback.
    Loopback,
    /// The default capture (input) device: microphone.
    Microphone,
}

/// A live shared-mode WASAPI capture stream plus its negotiated format.
pub(crate) struct AudioCapturer {
    /// Kept alive for the stream's lifetime; `Stop` runs on drop.
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: u16,
    sample_rate: u32,
    fmt: SampleFmt,
    /// Bytes per interleaved device frame (`channels * fmt.bytes()`).
    block_align: usize,
    /// True when this instance called `CoInitializeEx` and must balance it.
    owns_com: bool,
}

impl AudioCapturer {
    /// Initialize COM on the calling thread and open the requested endpoint in
    /// shared mode. On any failure the partial COM state is unwound so the caller
    /// can fall back to a video-only recording.
    pub(crate) fn new(source: AudioSel) -> Result<Self, String> {
        // The dedicated audio thread has no apartment yet, so MTA is free to
        // pick. S_FALSE means COM was already initialized on this thread; then we
        // must not balance it with CoUninitialize.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(format!("не удалось инициализировать COM для звука: {hr:?}"));
        }
        let owns_com = hr != S_FALSE;

        match Self::open(source) {
            Ok((client, capture, channels, sample_rate, fmt)) => Ok(Self {
                block_align: fmt.bytes() * channels.max(1) as usize,
                client,
                capture,
                channels,
                sample_rate,
                fmt,
                owns_com,
            }),
            Err(e) => {
                if owns_com {
                    unsafe { CoUninitialize() };
                }
                Err(e)
            }
        }
    }

    /// Device sample rate (Hz) of the captured stream.
    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Do the COM plumbing: enumerate the default endpoint, activate an
    /// `IAudioClient`, negotiate its mix format, and start the stream.
    fn open(
        source: AudioSel,
    ) -> Result<(IAudioClient, IAudioCaptureClient, u16, u32, SampleFmt), String> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("не удалось создать перечислитель устройств: {e}"))?;

            let (dataflow, streamflags) = match source {
                AudioSel::Loopback => (eRender, AUDCLNT_STREAMFLAGS_LOOPBACK),
                AudioSel::Microphone => (eCapture, 0u32),
            };

            let device: IMMDevice = enumerator
                .GetDefaultAudioEndpoint(dataflow, eConsole)
                .map_err(|e| format!("нет аудиоустройства по умолчанию: {e}"))?;

            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("не удалось активировать аудиоклиент: {e}"))?;

            let pwfx = client
                .GetMixFormat()
                .map_err(|e| format!("не удалось получить формат звука: {e}"))?;
            if pwfx.is_null() {
                return Err("устройство вернуло пустой формат звука".into());
            }

            // Read everything we need before Initialize, then free the format.
            let format = parse_format(pwfx);
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                streamflags,
                BUFFER_DURATION_HNS,
                0,
                pwfx,
                None,
            );
            CoTaskMemFree(Some(pwfx.cast()));

            let (channels, sample_rate, fmt) = format?;
            init.map_err(|e| format!("не удалось инициализировать поток звука: {e}"))?;

            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|e| format!("не удалось получить сервис захвата звука: {e}"))?;

            client
                .Start()
                .map_err(|e| format!("не удалось запустить захват звука: {e}"))?;

            Ok((client, capture, channels, sample_rate, fmt))
        }
    }

    /// Drain all currently available packets, appending decoded
    /// interleaved-stereo `f32` frames to `out`. Silent packets (WASAPI marks
    /// idle loopback spans this way) contribute the right number of zeroed
    /// frames so the downstream sample count keeps tracking real time. Returns
    /// the number of stereo frames appended.
    pub(crate) fn capture_into(&mut self, out: &mut Vec<f32>) -> Result<usize, String> {
        let mut frames_total = 0usize;
        unsafe {
            loop {
                let packet = self
                    .capture
                    .GetNextPacketSize()
                    .map_err(|e| format!("сбой чтения размера пакета звука: {e}"))?;
                if packet == 0 {
                    break;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut num_frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture
                    .GetBuffer(&mut data, &mut num_frames, &mut flags, None, None)
                    .map_err(|e| format!("сбой чтения буфера звука: {e}"))?;

                let n = num_frames as usize;
                if n > 0 {
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    if silent || data.is_null() {
                        // Emit stereo silence of the same length so gaps stay
                        // filled and the stream never stalls.
                        out.extend(std::iter::repeat_n(0.0f32, n * 2));
                    } else {
                        let raw = std::slice::from_raw_parts(data, n * self.block_align);
                        audio_dsp::decode_to_stereo_f32(raw, self.channels, self.fmt, out);
                    }
                    frames_total += n;
                }

                self.capture
                    .ReleaseBuffer(num_frames)
                    .map_err(|e| format!("сбой освобождения буфера звука: {e}"))?;
            }
        }
        Ok(frames_total)
    }
}

impl Drop for AudioCapturer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            // Release the COM interfaces before uninitializing the apartment.
            // (They are dropped as fields; CoUninitialize must come after.)
            if self.owns_com {
                CoUninitialize();
            }
        }
    }
}

/// Interpret a `WAVEFORMATEX` (possibly a `WAVEFORMATEXTENSIBLE`) into the fields
/// the pipeline needs: channel count, sample rate, and PCM encoding.
///
/// # Safety
/// `pwfx` must point at a valid `WAVEFORMATEX`, and when its `wFormatTag` is
/// `WAVE_FORMAT_EXTENSIBLE` the allocation must be a full `WAVEFORMATEXTENSIBLE`
/// (WASAPI's mix format always is). The pointer is only read, never freed here.
unsafe fn parse_format(pwfx: *const WAVEFORMATEX) -> Result<(u16, u32, SampleFmt), String> {
    // WAVEFORMATEX(EXTENSIBLE) is `packed(1)`; copy through `read_unaligned` into
    // an aligned local so field access (and `format!` borrows) are sound.
    let wfx = unsafe { pwfx.read_unaligned() };
    let channels = wfx.nChannels;
    let sample_rate = wfx.nSamplesPerSec;
    let bits = wfx.wBitsPerSample;
    let tag = wfx.wFormatTag;

    let fmt = if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = unsafe { pwfx.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
        let sub = ext.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            SampleFmt::F32
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            pcm_fmt(bits)?
        } else {
            return Err(format!("неподдерживаемый подтип звука ({bits} бит)"));
        }
    } else if u32::from(tag) == WAVE_FORMAT_IEEE_FLOAT {
        SampleFmt::F32
    } else if u32::from(tag) == WAVE_FORMAT_PCM {
        pcm_fmt(bits)?
    } else {
        return Err(format!("неподдерживаемый формат звука (тег {tag})"));
    };

    if channels == 0 || sample_rate == 0 {
        return Err("устройство сообщило некорректный формат звука".into());
    }
    Ok((channels, sample_rate, fmt))
}

/// Map integer-PCM bit depth to a supported [`SampleFmt`].
fn pcm_fmt(bits: u16) -> Result<SampleFmt, String> {
    match bits {
        16 => Ok(SampleFmt::I16),
        32 => Ok(SampleFmt::I32),
        other => Err(format!("неподдерживаемая разрядность PCM: {other} бит")),
    }
}

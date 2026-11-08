//! Windows audio endpoints through WASAPI, in shared mode.
//!
//! Every COM object here is created and used on the thread that opened it, and
//! the apartment guard is the last field so it outlives them. Nothing in this
//! module runs during a build or a test: the fixture backend is what the test
//! suite uses, and a real endpoint is opened only when an enabled voice worker
//! is told to capture or speak.
//!
//! Shared mode is deliberate. Exclusive mode would let JEAN take the operator's
//! speaker away from every other application, and a desktop companion has no
//! business doing that. The cost is that the mix format is whatever Windows
//! chose, so `audio.rs` resamples it to the engine rate.

use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, RPC_E_CHANGED_MODE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_LPWSTR;

use crate::audio::{from_mono_f32, to_mono_f32, AudioError, DeviceFormat, Pcm, SampleFormat};
use crate::device::{
    AudioBackend, AudioDevice, CancelFlag, CaptureStream, DeviceKind, RenderStream,
};

/// How long a capture read waits for the device event before returning control
/// to the caller, so cancellation is noticed even on a silent microphone.
const CAPTURE_WAIT_MS: u32 = 100;
/// Render pacing and the ceiling on waiting for a buffer to play out.
const RENDER_POLL_MS: u64 = 10;
const RENDER_DRAIN_LIMIT_MS: u64 = 10_000;

fn hr(error: windows::core::Error) -> String {
    format!("0x{:08x}", error.code().0 as u32)
}

fn audio_error(context: &str, error: windows::core::Error) -> AudioError {
    if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        AudioError::DeviceLost(format!("{context}: the endpoint was removed or reassigned"))
    } else {
        AudioError::Open(format!("{context}: {}", hr(error)))
    }
}

/// One balanced COM initialization on the calling thread.
struct Apartment {
    balanced: bool,
}

impl Apartment {
    fn acquire() -> Result<Self, AudioError> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.0 >= 0 {
            Ok(Self { balanced: true })
        } else if result == RPC_E_CHANGED_MODE {
            // Already initialized with a different model; usable, not ours to close.
            Ok(Self { balanced: false })
        } else {
            Err(AudioError::Open(format!(
                "COM initialization failed: 0x{:08x}",
                result.0 as u32
            )))
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.balanced {
            unsafe { CoUninitialize() };
        }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, AudioError> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| AudioError::Open(format!("MMDeviceEnumerator is unavailable: {}", hr(e))))
}

fn endpoint(
    devices: &IMMDeviceEnumerator,
    flow: windows::Win32::Media::Audio::EDataFlow,
    id: &str,
) -> Result<IMMDevice, AudioError> {
    unsafe {
        if id.is_empty() {
            devices
                .GetDefaultAudioEndpoint(flow, eConsole)
                .map_err(|e| audio_error("no default audio endpoint", e))
        } else {
            let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            devices
                .GetDevice(PCWSTR(wide.as_ptr()))
                .map_err(|e| audio_error(&format!("endpoint `{id}`"), e))
        }
    }
}

fn endpoint_id(device: &IMMDevice) -> String {
    unsafe {
        match device.GetId() {
            Ok(id) => {
                let text = if id.is_null() {
                    String::new()
                } else {
                    id.to_string().unwrap_or_default()
                };
                CoTaskMemFree(Some(id.as_ptr() as *const _));
                text
            }
            Err(_) => String::new(),
        }
    }
}

fn friendly_name(device: &IMMDevice) -> String {
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
            return String::new();
        };
        let Ok(mut value) = store.GetValue(&PKEY_Device_FriendlyName) else {
            return String::new();
        };
        let name = read_property(&mut value);
        let _ = PropVariantClear(&mut value);
        name
    }
}

/// Read a VT_LPWSTR property without taking a reference into the packed union.
unsafe fn read_property(value: &mut PROPVARIANT) -> String {
    let inner = &*value.Anonymous.Anonymous;
    if inner.vt != VT_LPWSTR {
        return String::new();
    }
    let text = inner.Anonymous.pwszVal;
    if text.is_null() {
        String::new()
    } else {
        text.to_string().unwrap_or_default()
    }
}

/// The mix format Windows will actually accept, owned until the stream drops.
struct MixFormat {
    pointer: *mut WAVEFORMATEX,
}

impl MixFormat {
    fn read(client: &IAudioClient) -> Result<Self, AudioError> {
        let pointer =
            unsafe { client.GetMixFormat() }.map_err(|e| audio_error("GetMixFormat", e))?;
        if pointer.is_null() {
            return Err(AudioError::UnsupportedFormat(
                "the endpoint returned a null mix format".into(),
            ));
        }
        Ok(Self { pointer })
    }

    fn device_format(&self) -> Result<DeviceFormat, AudioError> {
        // WAVEFORMATEX is packed, so every field is copied out by value before
        // it is compared; a reference into a packed struct would be misaligned.
        let raw = unsafe { &*self.pointer };
        let tag = raw.wFormatTag as u32;
        let channels = raw.nChannels;
        let sample_rate = raw.nSamplesPerSec;
        let bits = raw.wBitsPerSample;
        let extra = raw.cbSize;
        let sample = if tag == WAVE_FORMAT_EXTENSIBLE && extra >= 22 {
            let extensible = unsafe { &*(self.pointer as *const WAVEFORMATEXTENSIBLE) };
            let subformat = extensible.SubFormat;
            if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                SampleFormat::Float32
            } else if subformat == KSDATAFORMAT_SUBTYPE_PCM {
                integer_format(bits)?
            } else {
                return Err(AudioError::UnsupportedFormat(format!(
                    "extensible subformat {subformat:?} is neither PCM nor IEEE float"
                )));
            }
        } else if tag == WAVE_FORMAT_PCM {
            integer_format(bits)?
        } else {
            return Err(AudioError::UnsupportedFormat(format!(
                "wave format tag {tag} is not PCM or extensible"
            )));
        };
        if channels == 0 || sample_rate == 0 {
            return Err(AudioError::UnsupportedFormat(
                "the mix format reports zero channels or a zero sample rate".into(),
            ));
        }
        Ok(DeviceFormat {
            sample_rate,
            channels,
            format: sample,
        })
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.pointer as *const _)) };
    }
}

fn integer_format(bits: u16) -> Result<SampleFormat, AudioError> {
    match bits {
        8 => Ok(SampleFormat::UInt8),
        16 => Ok(SampleFormat::Int16),
        32 => Ok(SampleFormat::Int32),
        other => Err(AudioError::UnsupportedFormat(format!(
            "{other}-bit integer PCM is not supported; 8, 16 and 32 bit are"
        ))),
    }
}

pub struct WasapiBackend;

impl AudioBackend for WasapiBackend {
    fn devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        let _apartment = Apartment::acquire()?;
        let devices = enumerator()?;
        let mut out = Vec::new();
        for (flow, kind) in [
            (eCapture, DeviceKind::Capture),
            (eRender, DeviceKind::Render),
        ] {
            let default_id = unsafe {
                devices
                    .GetDefaultAudioEndpoint(flow, eConsole)
                    .ok()
                    .map(|d| endpoint_id(&d))
            };
            let collection = unsafe { devices.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }
                .map_err(|e| audio_error("EnumAudioEndpoints", e))?;
            let count = unsafe { collection.GetCount() }.map_err(|e| audio_error("GetCount", e))?;
            for index in 0..count {
                let device =
                    unsafe { collection.Item(index) }.map_err(|e| audio_error("Item", e))?;
                let id = endpoint_id(&device);
                out.push(AudioDevice {
                    default: default_id.as_deref() == Some(id.as_str()) && !id.is_empty(),
                    name: friendly_name(&device),
                    id,
                    kind,
                    format: None,
                });
            }
        }
        Ok(out)
    }

    fn open_capture(&self, id: &str) -> Result<Box<dyn CaptureStream>, AudioError> {
        Ok(Box::new(WasapiCapture::open(id)?))
    }

    fn open_render(&self, id: &str) -> Result<Box<dyn RenderStream>, AudioError> {
        Ok(Box::new(WasapiRender::open(id)?))
    }
}

pub struct WasapiCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: windows::Win32::Foundation::HANDLE,
    _mix: MixFormat,
    format: DeviceFormat,
    // Dropped last: the interfaces above release into COM.
    _apartment: Apartment,
}

impl WasapiCapture {
    fn open(id: &str) -> Result<Self, AudioError> {
        let apartment = Apartment::acquire()?;
        let devices = enumerator()?;
        let device = endpoint(&devices, eCapture, id)?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| audio_error("capture Activate(IAudioClient)", e))?;
        let mix = MixFormat::read(&client)?;
        let format = mix.device_format()?;
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| AudioError::Open(format!("capture event: {}", hr(e))))?;
        let initialized = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                mix.pointer,
                None,
            )
        };
        if let Err(error) = initialized {
            unsafe {
                let _ = CloseHandle(event);
            };
            return Err(audio_error("capture Initialize", error));
        }
        if let Err(error) = unsafe { client.SetEventHandle(event) } {
            unsafe {
                let _ = CloseHandle(event);
            };
            return Err(audio_error("capture SetEventHandle", error));
        }
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.map_err(|e| audio_error("capture GetService", e))?;
        if let Err(error) = unsafe { client.Start() } {
            unsafe {
                let _ = CloseHandle(event);
            };
            return Err(audio_error("capture Start", error));
        }
        Ok(Self {
            client,
            capture,
            event,
            _mix: mix,
            format,
            _apartment: apartment,
        })
    }
}

impl CaptureStream for WasapiCapture {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn read(&mut self) -> Result<Option<Pcm>, AudioError> {
        let waited = unsafe { WaitForSingleObject(self.event, CAPTURE_WAIT_MS) };
        if waited != WAIT_OBJECT_0 {
            return Ok(None);
        }
        let mut packet = Vec::new();
        loop {
            let frames = unsafe { self.capture.GetNextPacketSize() }
                .map_err(|e| audio_error("GetNextPacketSize", e))?;
            if frames == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut available = 0u32;
            let mut flags = 0u32;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut available, &mut flags, None, None)
            }
            .map_err(|e| audio_error("capture GetBuffer", e))?;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let converted = if silent || data.is_null() {
                vec![0.0; available as usize]
            } else {
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        data,
                        available as usize
                            * self.format.channels as usize
                            * self.format.format.bytes_per_sample(),
                    )
                };
                match to_mono_f32(bytes, self.format) {
                    Ok(samples) => samples,
                    Err(error) => {
                        unsafe { self.capture.ReleaseBuffer(available) }
                            .map_err(|e| audio_error("capture ReleaseBuffer", e))?;
                        return Err(error);
                    }
                }
            };
            unsafe { self.capture.ReleaseBuffer(available) }
                .map_err(|e| audio_error("capture ReleaseBuffer", e))?;
            packet.extend(converted);
        }
        if packet.is_empty() {
            return Ok(None);
        }
        Ok(Some(Pcm {
            sample_rate: self.format.sample_rate,
            samples: packet,
        }))
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

pub struct WasapiRender {
    client: IAudioClient,
    render: IAudioRenderClient,
    _mix: MixFormat,
    format: DeviceFormat,
    buffer_frames: u32,
    _apartment: Apartment,
}

impl WasapiRender {
    fn open(id: &str) -> Result<Self, AudioError> {
        let apartment = Apartment::acquire()?;
        let devices = enumerator()?;
        let device = endpoint(&devices, eRender, id)?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| audio_error("render Activate(IAudioClient)", e))?;
        let mix = MixFormat::read(&client)?;
        let format = mix.device_format()?;
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix.pointer, None) }
            .map_err(|e| audio_error("render Initialize", e))?;
        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|e| audio_error("render GetBufferSize", e))?;
        let render: IAudioRenderClient =
            unsafe { client.GetService() }.map_err(|e| audio_error("render GetService", e))?;
        unsafe { client.Start() }.map_err(|e| audio_error("render Start", e))?;
        Ok(Self {
            client,
            render,
            _mix: mix,
            format,
            buffer_frames,
            _apartment: apartment,
        })
    }
}

impl RenderStream for WasapiRender {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError> {
        let padding = unsafe { self.client.GetCurrentPadding() }
            .map_err(|e| audio_error("GetCurrentPadding", e))?;
        let free = self.buffer_frames.saturating_sub(padding) as usize;
        let count = free.min(frames.len());
        if count == 0 {
            return Ok(0);
        }
        let bytes = from_mono_f32(&frames[..count], self.format)?;
        let destination = unsafe { self.render.GetBuffer(count as u32) }
            .map_err(|e| audio_error("render GetBuffer", e))?;
        if destination.is_null() {
            return Err(AudioError::DeviceLost(
                "render GetBuffer returned null; the endpoint is gone".into(),
            ));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
            self.render.ReleaseBuffer(count as u32, 0)
        }
        .map_err(|e| audio_error("render ReleaseBuffer", e))?;
        Ok(count)
    }

    fn drain(&mut self, cancel: &CancelFlag) -> Result<(), AudioError> {
        let limit =
            std::time::Instant::now() + std::time::Duration::from_millis(RENDER_DRAIN_LIMIT_MS);
        while !cancel.is_cancelled() && std::time::Instant::now() < limit {
            let padding = unsafe { self.client.GetCurrentPadding() }
                .map_err(|e| audio_error("drain GetCurrentPadding", e))?;
            if padding == 0 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(RENDER_POLL_MS));
        }
        Ok(())
    }

    fn stop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
        }
    }
}

impl Drop for WasapiRender {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

//! Audio half: WASAPI loopback capture into the shared AudioRing.
//! Game mode records only the game's process tree (the API OBS's
//! "Capture Audio" uses), so a podcast in the background stays out.
//! Otherwise it records everything one output device plays (the Windows
//! default, or one picked in the tray menu, e.g. a SteelSeries Sonar channel).

use crate::buffer::{AudioRing, RATE};
use crate::util::{TICKS_PER_SEC, log, now_100ns};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows_core::{Interface, Ref, implement};

#[derive(Clone, Debug)]
pub enum AudioSource {
    /// only this process (and its children)
    Process(u32),
    /// everything on an output device; None = the Windows default
    Device(Option<String>),
    /// a microphone / input device; None = the Windows default
    Mic(Option<String>),
}

pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

fn list(flow: EDataFlow) -> Vec<DeviceInfo> {
    unsafe {
        let mut out = Vec::new();
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return out;
        };
        let Ok(col) = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..col.GetCount().unwrap_or(0) {
            if let Ok(d) = col.Item(i) {
                out.push(DeviceInfo { id: device_id(&d), name: device_name(&d) });
            }
        }
        out
    }
}

fn default_name(flow: EDataFlow) -> String {
    unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .and_then(|en| en.GetDefaultAudioEndpoint(flow, eConsole))
            .map(|d| device_name(&d))
            .unwrap_or_else(|_| "none".into())
    }
}

/// Active playback devices, by endpoint id and friendly name.
/// Call from a thread that has COM initialised.
pub fn list_devices() -> Vec<DeviceInfo> {
    list(eRender)
}

/// Active microphones / input devices.
pub fn list_mics() -> Vec<DeviceInfo> {
    list(eCapture)
}

/// Friendly name of the current Windows default playback device.
pub fn default_device_name() -> String {
    default_name(eRender)
}

/// Friendly name of the current Windows default microphone.
pub fn default_mic_name() -> String {
    default_name(eCapture)
}

unsafe fn device_id(d: &IMMDevice) -> String {
    unsafe {
        match d.GetId() {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            }
            Err(_) => String::new(),
        }
    }
}

unsafe fn device_name(d: &IMMDevice) -> String {
    unsafe {
        d.OpenPropertyStore(STGM_READ)
            .and_then(|ps| ps.GetValue(&PKEY_Device_FriendlyName))
            .map(|v| v.to_string())
            .unwrap_or_default()
    }
}

pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start(src: AudioSource, ring: Arc<Mutex<AudioRing>>) -> AudioCapture {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = unsafe { run(src.clone(), &ring, &stop2) } {
                log(format!("audio {src:?} failed: {e}"));
            }
        });
        AudioCapture { stop, thread: Some(thread) }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated {
    event: isize,
}

// IAgileObject has no methods: marking it lets the async activation call us
// back from any thread.
impl IAgileObject_Impl for Activated_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows_core::Result<()> {
        unsafe { SetEvent(HANDLE(self.event as *mut _)) }
    }
}

unsafe fn process_client(pid: u32) -> windows_core::Result<IAudioClient> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // ManuallyDrop: PROPVARIANT's Drop calls PropVariantClear, which would
        // CoTaskMemFree our stack-allocated blob and corrupt the heap.
        let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob.cbSize = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
        inner.Anonymous.blob.pBlobData = &mut params as *mut _ as *mut u8;

        log("audio: activating process loopback");
        let event = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler =
            Activated { event: event.0 as isize }.into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv),
            &handler,
        )?;
        if WaitForSingleObject(event, 5000) != WAIT_OBJECT_0 {
            return Err(windows_core::Error::new(
                windows_core::HRESULT(0x8000_000Au32 as i32),
                "process loopback activation timed out",
            ));
        }
        log("audio: activation completed");
        let mut hr = windows_core::HRESULT(0);
        let mut unk: Option<windows_core::IUnknown> = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        unk.ok_or_else(|| windows_core::Error::from_hresult(windows_core::HRESULT(0x80004003u32 as i32)))?
            .cast()
    }
}

unsafe fn device_client(id: Option<&str>, flow: EDataFlow) -> windows_core::Result<IAudioClient> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = match id {
            Some(id) => {
                let w = crate::util::wide(id);
                en.GetDevice(windows_core::PCWSTR(w.as_ptr()))?
            }
            None => en.GetDefaultAudioEndpoint(flow, eConsole)?,
        };
        log(format!("audio: device '{}'", device_name(&dev)));
        dev.Activate(CLSCTX_ALL, None)
    }
}

unsafe fn run(src: AudioSource, ring: &Mutex<AudioRing>, stop: &AtomicBool) -> windows_core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = match &src {
            AudioSource::Process(pid) => process_client(*pid)?,
            AudioSource::Device(id) => device_client(id.as_deref(), eRender)?,
            AudioSource::Mic(id) => device_client(id.as_deref(), eCapture)?,
        };
        // a mic is captured directly; everything else is a loopback of playback
        let loopback = if matches!(src, AudioSource::Mic(_)) { 0 } else { AUDCLNT_STREAMFLAGS_LOOPBACK };
        log("audio: client activated");
        // Ask for 48 kHz 16-bit stereo; Windows converts whatever the device runs at.
        let fmt = WAVEFORMATEX {
            wFormatTag: 1, // WAVE_FORMAT_PCM
            nChannels: 2,
            nSamplesPerSec: RATE as u32,
            nAvgBytesPerSec: RATE as u32 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            loopback
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            2_000_000, // 200 ms
            0,
            &fmt,
            None,
        )?;
        log("audio: initialized");
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        log(format!("audio {src:?} started"));

        let mut logged_first = false;
        while !stop.load(Ordering::SeqCst) {
            // loopback stays quiet during silence; the timeout keeps us responsive
            WaitForSingleObject(event, 100);
            loop {
                let n = cap.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let count = frames as usize * 2;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let pcm: Vec<i16> = if silent || data.is_null() {
                    vec![0; count]
                } else {
                    std::slice::from_raw_parts(data as *const i16, count).to_vec()
                };
                cap.ReleaseBuffer(frames)?;
                // packet time; fall back to "now minus its length" if not reported
                let t = if qpc != 0 {
                    qpc as i64
                } else {
                    now_100ns() - frames as i64 * TICKS_PER_SEC / RATE
                };
                ring.lock().unwrap().push(&pcm, t);
                if !logged_first {
                    logged_first = true;
                    log(format!("audio: first packet {frames} frames, flags {flags:#x}, qpc {qpc}"));
                }
            }
        }
        let _ = client.Stop();
        log(format!("audio {src:?} stopped"));
        Ok(())
    }
}

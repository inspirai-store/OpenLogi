//! Raw keyboard input from the G604 Bluetooth device only.
#![expect(
    unsafe_code,
    reason = "message-only window and Raw Input are Win32 APIs"
)]
use std::io;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIDI_DEVICENAME, RIM_TYPEKEYBOARD,
    RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, HWND_MESSAGE, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SetWindowLongPtrW, TranslateMessage, WM_CLOSE, WM_INPUT, WNDCLASSW,
};

/// One physical edge, already attributed to VID 046D / PID B024.
#[derive(Clone, Copy, Debug)]
pub struct RawKey {
    /// Hardware scan-code identity.
    pub input: openlogi_core::macros::MacroInput,
    /// Windows virtual key, used for F13–F18 read-back.
    pub virtual_key: u16,
    /// Physical down/up state.
    pub pressed: bool,
}
/// Owns a hidden calibration message pump. No global hook suppression here.
pub struct RawInput {
    window: isize,
    worker: Option<JoinHandle<()>>,
}
impl RawInput {
    /// Start capture and return a bounded device-only event stream.
    pub fn start() -> io::Result<(Self, Receiver<RawKey>)> {
        let (tx, rx) = mpsc::sync_channel(128);
        let (ready, wait) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("g604-raw-input".into())
            .spawn(move || {
                // SAFETY: Win32 objects are owned and destroyed by this message-pump thread.
                unsafe {
                    let name: Vec<u16> = "OpenLogi.G604.Calibration\0".encode_utf16().collect();
                    let instance = GetModuleHandleW(std::ptr::null());
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(window_proc),
                        hInstance: instance,
                        lpszClassName: name.as_ptr(),
                        ..Default::default()
                    };
                    RegisterClassW(&raw const class);
                    let window = CreateWindowExW(
                        0,
                        name.as_ptr(),
                        name.as_ptr(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        HWND_MESSAGE,
                        std::ptr::null_mut(),
                        instance,
                        std::ptr::null(),
                    );
                    if window.is_null() {
                        let _ = ready.send(Err(io::Error::last_os_error()));
                        return;
                    }
                    let sender = Box::new(tx);
                    SetWindowLongPtrW(window, GWLP_USERDATA, (&raw const *sender) as isize);
                    let device = RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 6,
                        dwFlags: RIDEV_INPUTSINK,
                        hwndTarget: window,
                    };
                    if RegisterRawInputDevices(
                        &raw const device,
                        1,
                        u32::try_from(std::mem::size_of::<RAWINPUTDEVICE>()).unwrap_or(0),
                    ) == 0
                    {
                        let _ = ready.send(Err(io::Error::last_os_error()));
                        DestroyWindow(window);
                        return;
                    }
                    let _ = ready.send(Ok(window as isize));
                    let mut message: MSG = std::mem::zeroed();
                    while GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) > 0 {
                        TranslateMessage(&raw const message);
                        DispatchMessageW(&raw const message);
                    }
                    let remove = RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 6,
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: std::ptr::null_mut(),
                    };
                    RegisterRawInputDevices(
                        &raw const remove,
                        1,
                        u32::try_from(std::mem::size_of::<RAWINPUTDEVICE>()).unwrap_or(0),
                    );
                    SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                    DestroyWindow(window);
                    drop(sender);
                }
            })?;
        match wait
            .recv()
            .map_err(|_| io::Error::other("Raw Input worker exited"))?
        {
            Ok(window) => Ok((
                Self {
                    window,
                    worker: Some(worker),
                },
                rx,
            )),
            Err(error) => {
                let _ = worker.join();
                Err(error)
            }
        }
    }
}
impl Drop for RawInput {
    fn drop(&mut self) {
        // SAFETY: window is the live message-only window returned by the worker.
        unsafe {
            PostMessageW(self.window as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLOSE {
        // SAFETY: the window procedure runs on the Raw Input worker thread;
        // PostQuitMessage only signals that thread's own message loop.
        unsafe {
            PostQuitMessage(0);
        }
        return 0;
    }
    if message == WM_INPUT {
        let _ = std::panic::catch_unwind(|| {
            // SAFETY: WM_INPUT supplies a live HRAWINPUT; raw is correctly aligned and sized.
            unsafe {
                let mut raw: RAWINPUT = std::mem::zeroed();
                let mut size = u32::try_from(std::mem::size_of::<RAWINPUT>()).unwrap_or(0);
                if GetRawInputData(
                    lparam as HRAWINPUT,
                    RID_INPUT,
                    (&raw mut raw).cast::<RAWINPUT>().cast(),
                    &raw mut size,
                    u32::try_from(std::mem::size_of::<RAWINPUTHEADER>()).unwrap_or(0),
                ) == u32::MAX
                    || raw.header.dwType != RIM_TYPEKEYBOARD
                    || !is_g604(raw.header.hDevice)
                {
                    return;
                }
                let keyboard = raw.data.keyboard;
                let sender = GetWindowLongPtrW(window, GWLP_USERDATA) as *const SyncSender<RawKey>;
                if !sender.is_null() {
                    let _ = (*sender).try_send(RawKey {
                        input: openlogi_core::macros::MacroInput::Key {
                            scan_code: keyboard.MakeCode,
                            extended: keyboard.Flags & 2 != 0,
                        },
                        virtual_key: keyboard.VKey,
                        pressed: keyboard.Flags & 1 == 0,
                    });
                }
            }
        });
    }
    // SAFETY: forward the unchanged message parameters to the system procedure.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
unsafe fn is_g604(device: HANDLE) -> bool {
    let mut length = 0;
    // SAFETY: query length first, bound allocation, then pass a writable UTF-16 buffer.
    unsafe {
        GetRawInputDeviceInfoW(
            device,
            RIDI_DEVICENAME,
            std::ptr::null_mut(),
            &raw mut length,
        );
        if length == 0 || length > 4096 {
            return false;
        }
        let mut name = vec![0u16; usize::try_from(length).unwrap_or(0) + 1];
        if GetRawInputDeviceInfoW(
            device,
            RIDI_DEVICENAME,
            name.as_mut_ptr().cast(),
            &raw mut length,
        ) == u32::MAX
        {
            return false;
        }
        let name = String::from_utf16_lossy(&name).to_ascii_uppercase();
        (name.contains("VID_046D") || name.contains("VID&02046D"))
            && (name.contains("PID_B024") || name.contains("PID&B024"))
    }
}

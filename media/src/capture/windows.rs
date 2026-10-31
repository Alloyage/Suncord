use super::ScreenSource;
use windows_sys::core::BOOL;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    Graphics::Gdi::{
        EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW,
        ENUM_CURRENT_SETTINGS, HDC, HMONITOR, MONITORINFOEXW,
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, IsIconic, IsWindowVisible,
    },
};

unsafe extern "system" fn monitor(handle: HMONITOR, _: HDC, _: *mut RECT, context: LPARAM) -> BOOL {
    let sources = unsafe { &mut *(context as *mut Vec<ScreenSource>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(handle, &mut info.monitorInfo) } != 0 {
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let rate = if unsafe {
            EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode)
        } != 0
        {
            match mode.dmDisplayFrequency {
                2..=240 => mode.dmDisplayFrequency as i32,
                _ => 60,
            }
        } else {
            60
        };
        let rect = info.monitorInfo.rcMonitor;
        sources.push(ScreenSource {
            id: format!("monitor:{}", handle as usize),
            name: format!("Screen {}", sources.len() + 1),
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            rate,
        });
    }
    1
}

unsafe extern "system" fn window(handle: HWND, context: LPARAM) -> BOOL {
    if unsafe { IsWindowVisible(handle) } == 0 || unsafe { IsIconic(handle) } != 0 {
        return 1;
    }
    let size = unsafe { GetWindowTextLengthW(handle) };
    if size <= 0 {
        return 1;
    }
    let mut title = vec![0; size as usize + 1];
    let length = unsafe { GetWindowTextW(handle, title.as_mut_ptr(), title.len() as i32) };
    let mut rect = RECT::default();
    if length > 0
        && unsafe { GetWindowRect(handle, &mut rect) } != 0
        && rect.right - rect.left >= 16
        && rect.bottom - rect.top >= 16
    {
        let sources = unsafe { &mut *(context as *mut Vec<ScreenSource>) };
        sources.push(ScreenSource {
            id: format!("window:{}", handle as usize),
            name: String::from_utf16_lossy(&title[..length as usize]),
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            rate: 60,
        });
    }
    1
}

pub(super) fn screens() -> Vec<ScreenSource> {
    let mut sources = Vec::new();
    // EnumDisplayMonitors and EnumWindows invoke callbacks synchronously on this thread.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(monitor),
            &mut sources as *mut _ as LPARAM,
        );
        EnumWindows(Some(window), &mut sources as *mut _ as LPARAM);
    }
    sources
}

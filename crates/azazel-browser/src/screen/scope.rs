//! `[screen] allow_apps`: input goes only to the applications the user listed.

/// The names the foreground application goes by: its program file (`notepad.exe`) and the name
/// the window system gives it (`Notepad`).
pub type Names = Vec<String>;

/// Whether input may go to an application with these `names` when `allowed` lists the permitted
/// ones. An empty list permits every application. A listed name matches case-insensitively,
/// whole, with or without a trailing `.exe`.
pub fn permits(allowed: &[String], names: &[String]) -> bool {
    let bare = |name: &str| {
        let name = name.trim().to_lowercase();
        name.strip_suffix(".exe").map(str::to_string).unwrap_or(name)
    };
    allowed.is_empty()
        || names.iter().any(|name| {
            let name = bare(name);
            !name.is_empty() && allowed.iter().any(|entry| bare(entry) == name)
        })
}

/// The names of the application that has the keyboard focus.
pub fn foreground() -> Result<Names, String> {
    let mut names = Names::new();
    #[cfg(windows)]
    let window = win::foreground(&mut names);
    #[cfg(not(windows))]
    let window = other::foreground(&mut names);
    window?;
    if names.iter().all(|name| name.trim().is_empty()) {
        return Err("the application in the foreground could not be identified".to_string());
    }
    Ok(names)
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    pub fn foreground(names: &mut super::Names) -> Result<(), String> {
        // SAFETY: plain queries; the process handle is closed before returning.
        let (hwnd, pid) = unsafe {
            let hwnd = GetForegroundWindow();
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            (hwnd, pid)
        };
        if hwnd.is_null() || pid == 0 {
            return Err("no window has the focus".to_string());
        }
        // SAFETY: the buffer and its length are passed together.
        let file = unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                None
            } else {
                let mut buffer = [0u16; 1024];
                let mut len = buffer.len() as u32;
                let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len);
                CloseHandle(process);
                (ok != 0).then(|| String::from_utf16_lossy(&buffer[..len as usize]))
            }
        };
        if let Some(path) = file {
            names.extend(path.rsplit(['\\', '/']).next().map(str::to_string));
        }
        // The name Windows shows for the program (its file description), which is what `windows`
        // lists as the app.
        if let Ok(windows) = xcap::Window::all()
            && let Some(own) = windows
                .iter()
                .find(|w| w.pid().is_ok_and(|p| p == pid) && w.is_focused().unwrap_or(false))
            && let Ok(app) = own.app_name()
        {
            names.push(app);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod other {
    pub fn foreground(names: &mut super::Names) -> Result<(), String> {
        let windows = xcap::Window::all().map_err(|e| e.to_string())?;
        let focused = windows
            .iter()
            .find(|w| w.is_focused().unwrap_or(false))
            .ok_or("no window has the focus")?;
        names.extend(focused.app_name().ok());
        #[cfg(target_os = "linux")]
        if let Ok(pid) = focused.pid()
            && let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe"))
        {
            names.extend(exe.file_name().map(|n| n.to_string_lossy().into_owned()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(of: &[&str]) -> Vec<String> {
        of.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_empty_list_permits_every_application() {
        assert!(permits(&[], &names(&["anything.exe"])));
    }

    #[test]
    fn the_exe_name_or_the_app_name_matches_whole_and_without_case() {
        let allowed = names(&["Notepad", "game.exe"]);
        assert!(permits(&allowed, &names(&["notepad.exe", "Editor"])));
        assert!(permits(&allowed, &names(&["Game.EXE"])));
        assert!(permits(&allowed, &names(&["x.exe", "NOTEPAD"])));
        // Whole names only: a longer name that contains a listed one does not match.
        assert!(!permits(&allowed, &names(&["notepad++.exe"])));
        assert!(!permits(&allowed, &names(&["mygame.exe"])));
        // Nothing known about the application, nothing permitted.
        assert!(!permits(&allowed, &names(&["", " "])));
        assert!(!permits(&allowed, &[]));
    }
}

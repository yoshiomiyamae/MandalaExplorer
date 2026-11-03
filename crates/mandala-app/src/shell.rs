//! Windows Shell integration. No file-operation reimplementation or private clipboard.
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub enum Action {
    Copy,
    Cut,
    Paste,
}

#[cfg(windows)]
mod native {
    use super::*;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::{
        Win32::{
            Foundation::*,
            System::{Com::*, Ole::*},
            UI::{Shell::*, WindowsAndMessaging::*},
        },
        core::{Interface, PCSTR, PCWSTR, PSTR},
    };

    struct Ole;
    impl Ole {
        fn enter() -> windows::core::Result<Self> {
            unsafe {
                OleInitialize(None)?;
            }
            Ok(Self)
        }
    }
    impl Drop for Ole {
        fn drop(&mut self) {
            unsafe {
                OleUninitialize();
            }
        }
    }
    fn wide(path: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    fn owner(frame: &impl HasWindowHandle) -> anyhow::Result<HWND> {
        match frame.window_handle()?.as_raw() {
            RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
            _ => anyhow::bail!("A Windows window is required"),
        }
    }

    struct PasteState {
        pending: std::sync::atomic::AtomicBool,
        context: eframe::egui::Context,
    }

    /// egui-winit discards Ctrl+V when the clipboard has files but no text.
    /// Capture the key without consuming it; the app still gates on text focus.
    pub struct ClipboardKeys {
        hwnd: HWND,
        state: Box<PasteState>,
    }
    unsafe extern "system" fn clipboard_key(
        hwnd: HWND,
        msg: u32,
        w: WPARAM,
        l: LPARAM,
        _: usize,
        data: usize,
    ) -> LRESULT {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetKeyState, VK_CONTROL, VK_INSERT, VK_SHIFT,
        };
        if msg == WM_KEYDOWN && l.0 & (1 << 30) == 0 {
            let paste = (w.0 == b'V' as usize && unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0)
                || (w.0 == VK_INSERT.0 as usize && unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0);
            if paste {
                let state = unsafe { &*(data as *const PasteState) };
                state.pending.store(true, std::sync::atomic::Ordering::Relaxed);
                state.context.request_repaint();
            }
        }
        unsafe { DefSubclassProc(hwnd, msg, w, l) }
    }
    impl ClipboardKeys {
        pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
            let hwnd = owner(cc)?;
            let state =
                Box::new(PasteState { pending: false.into(), context: cc.egui_ctx.clone() });
            anyhow::ensure!(
                unsafe {
                    SetWindowSubclass(
                        hwnd,
                        Some(clipboard_key),
                        0x4d414e45,
                        &*state as *const _ as usize,
                    )
                }
                .as_bool(),
                "Cannot attach file-paste key handler"
            );
            Ok(Self { hwnd, state })
        }
        pub fn take_paste(&self) -> bool {
            self.state.pending.swap(false, std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl Drop for ClipboardKeys {
        fn drop(&mut self) {
            unsafe {
                let _ = RemoveWindowSubclass(self.hwnd, Some(clipboard_key), 0x4d414e45);
            }
        }
    }
    fn item(path: &Path) -> windows::core::Result<IShellItem> {
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide(path).as_ptr()), None) }
    }
    struct Pidl(*mut Common::ITEMIDLIST);
    impl Drop for Pidl {
        fn drop(&mut self) {
            unsafe {
                CoTaskMemFree(Some(self.0.cast()));
            }
        }
    }
    fn context(paths: &[PathBuf]) -> anyhow::Result<IContextMenu> {
        anyhow::ensure!(!paths.is_empty(), "No files selected");
        let mut ids = Vec::new();
        for path in paths {
            ids.push(Pidl(unsafe { SHGetIDListFromObject(&item(path)?)? }));
        }
        let pointers: Vec<_> = ids.iter().map(|p| p.0 as *const _).collect();
        let array = unsafe { SHCreateShellItemArrayFromIDLists(&pointers)? };
        Ok(unsafe { array.BindToHandler(None, &BHID_SFUIObject)? })
    }
    struct Menu(HMENU);
    impl Drop for Menu {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyMenu(self.0);
            }
        }
    }
    fn menu(context: &IContextMenu, rename: bool) -> windows::core::Result<Menu> {
        let menu = Menu(unsafe { CreatePopupMenu()? });
        unsafe {
            context
                .QueryContextMenu(
                    menu.0,
                    0,
                    1,
                    0x7fff,
                    CMF_NORMAL | if rename { CMF_CANRENAME } else { 0 },
                )
                .ok()?;
        }
        Ok(menu)
    }
    fn invoke(context: &IContextMenu, hwnd: HWND, verb: PCSTR) -> windows::core::Result<()> {
        let info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            hwnd,
            lpVerb: verb,
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        unsafe { context.InvokeCommand(&info) }
    }

    struct Forward {
        third: Option<IContextMenu3>,
        second: Option<IContextMenu2>,
    }
    unsafe extern "system" fn forward(
        hwnd: HWND,
        msg: u32,
        w: WPARAM,
        l: LPARAM,
        _: usize,
        data: usize,
    ) -> LRESULT {
        if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
            // The scoped subclass is removed before the borrowed context is dropped.
            let state = unsafe { &*(data as *const Forward) };
            let mut result = LRESULT(0);
            if let Some(third) = &state.third {
                if unsafe { third.HandleMenuMsg2(msg, w, l, Some(&mut result)) }.is_ok() {
                    return result;
                }
            } else if let Some(second) = &state.second
                && unsafe { second.HandleMenuMsg(msg, w, l) }.is_ok()
            {
                return LRESULT(0);
            }
        }
        unsafe { DefSubclassProc(hwnd, msg, w, l) }
    }
    struct Subclass(HWND);
    impl Drop for Subclass {
        fn drop(&mut self) {
            unsafe {
                let _ = RemoveWindowSubclass(self.0, Some(forward), 0x4d414e44);
            }
        }
    }

    /// None means dismissed; Some(true) asks the host to show its name editor.
    pub fn popup(frame: &eframe::Frame, paths: &[PathBuf]) -> anyhow::Result<Option<bool>> {
        let _ole = Ole::enter()?;
        let hwnd = owner(frame)?;
        let context = context(paths)?;
        let menu = menu(&context, paths.len() == 1)?;
        let state = Forward { third: context.cast().ok(), second: context.cast().ok() };
        anyhow::ensure!(
            unsafe {
                SetWindowSubclass(hwnd, Some(forward), 0x4d414e44, &state as *const _ as usize)
            }
            .as_bool(),
            "Cannot attach Shell menu handler"
        );
        let subclass = Subclass(hwnd);
        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point)?;
        }
        let command = unsafe {
            TrackPopupMenuEx(
                menu.0,
                TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0,
                point.x,
                point.y,
                hwnd,
                None,
            )
        }
        .0;
        drop(subclass);
        if command == 0 {
            return Ok(None);
        }
        let mut verb = [0u8; 128];
        let _ = unsafe {
            context.GetCommandString(
                (command - 1) as usize,
                GCS_VERBA,
                None,
                PSTR(verb.as_mut_ptr()),
                verb.len() as u32,
            )
        };
        if verb.starts_with(b"rename\0") {
            return Ok(Some(true));
        }
        invoke(&context, hwnd, PCSTR((command - 1) as usize as *const u8))?;
        // Materialize Shell clipboard formats so copy/cut outlive this COM scope.
        if verb.starts_with(b"copy\0") || verb.starts_with(b"cut\0") {
            unsafe {
                OleFlushClipboard()?;
            }
        }
        Ok(Some(false))
    }

    pub fn action(
        frame: &eframe::Frame,
        action: Action,
        paths: &[PathBuf],
        directory: &Path,
    ) -> anyhow::Result<()> {
        let _ole = Ole::enter()?;
        let hwnd = owner(frame)?;
        let context: IContextMenu = if matches!(action, Action::Paste) {
            let folder: IShellFolder =
                unsafe { item(directory)?.BindToHandler(None, &BHID_SFObject)? };
            unsafe { folder.CreateViewObject(hwnd)? }
        } else {
            context(paths)?
        };
        let _menu = menu(&context, false)?;
        let verb = match action {
            Action::Copy => windows::core::s!("copy"),
            Action::Cut => windows::core::s!("cut"),
            Action::Paste => windows::core::s!("paste"),
        };
        invoke(&context, hwnd, verb)?;
        if !matches!(action, Action::Paste) {
            unsafe {
                OleFlushClipboard()?;
            }
        }
        Ok(())
    }

    pub fn rename(frame: &eframe::Frame, path: &Path, name: &str) -> anyhow::Result<()> {
        rename_owned(owner(frame)?, path, name)
    }

    fn rename_owned(hwnd: HWND, path: &Path, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(crate::selection::valid_name(name), "Invalid file name");
        let _ole = Ole::enter()?;
        let operation: IFileOperation =
            unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)? };
        unsafe {
            operation.SetOwnerWindow(hwnd)?;
            operation.SetOperationFlags(FOF_ALLOWUNDO | FOFX_ADDUNDORECORD)?;
            operation.RenameItem(&item(path)?, PCWSTR(wide(Path::new(name)).as_ptr()), None)?;
            operation.PerformOperations()?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn shell_exposes_multi_file_and_background_menus() {
            let _ole = Ole::enter().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let paths: Vec<_> =
                ["猫.txt", "second.txt"].iter().map(|name| dir.path().join(name)).collect();
            for path in &paths {
                std::fs::write(path, b"test").unwrap();
            }
            let context = context(&paths).unwrap();
            let _menu = menu(&context, false).unwrap();
            let folder: IShellFolder =
                unsafe { item(dir.path()).unwrap().BindToHandler(None, &BHID_SFObject).unwrap() };
            let background: IContextMenu =
                unsafe { folder.CreateViewObject(HWND::default()).unwrap() };
            let _background_menu = menu(&background, false).unwrap();
        }

        #[test]
        fn shell_renames_a_unicode_file_without_changing_its_contents() {
            let dir = tempfile::tempdir().unwrap();
            let before = dir.path().join("before.txt");
            std::fs::write(&before, b"unchanged").unwrap();
            rename_owned(HWND::default(), &before, "変更後.txt").unwrap();
            assert!(!before.exists());
            assert_eq!(std::fs::read(dir.path().join("変更後.txt")).unwrap(), b"unchanged");
        }
    }
}

#[cfg(windows)]
pub use native::{ClipboardKeys, action, popup, rename};

#[cfg(not(windows))]
pub struct ClipboardKeys;
#[cfg(not(windows))]
impl ClipboardKeys {
    pub fn new(_: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        Ok(Self)
    }
    pub fn take_paste(&self) -> bool {
        false
    }
}

#[cfg(not(windows))]
pub fn popup(_: &eframe::Frame, _: &[PathBuf]) -> anyhow::Result<Option<bool>> {
    anyhow::bail!("Windows Shell is unavailable")
}
#[cfg(not(windows))]
pub fn action(_: &eframe::Frame, _: Action, _: &[PathBuf], _: &Path) -> anyhow::Result<()> {
    anyhow::bail!("Windows Shell is unavailable")
}
#[cfg(not(windows))]
pub fn rename(_: &eframe::Frame, _: &Path, _: &str) -> anyhow::Result<()> {
    anyhow::bail!("Windows Shell is unavailable")
}

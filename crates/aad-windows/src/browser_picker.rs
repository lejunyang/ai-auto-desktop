//! Bind an already visible Common Item Dialog to the browser frame selected by CDP.
use aad_platform::picker::{Kind, NewFile};
use std::path::Path;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows::core::{BSTR, VARIANT};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::*;

mod open_file;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "file dialog deadline elapsed".into())
}

fn pid(hwnd: HWND) -> u32 {
    let mut id = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut id)) };
    id
}

fn class(hwnd: HWND) -> String {
    let mut buf = [0; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len as usize])
}

fn windows() -> Result<Vec<HWND>> {
    unsafe extern "system" fn collect(hwnd: HWND, arg: LPARAM) -> BOOL {
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            // EnumWindows invokes the callback synchronously on this thread.
            unsafe { &mut *(arg.0 as *mut Vec<HWND>) }.push(hwnd);
        }
        BOOL(1)
    }
    let mut list = Vec::new();
    unsafe { EnumWindows(Some(collect), LPARAM(&mut list as *mut Vec<HWND> as isize)) }?;
    Ok(list)
}

fn owner_of(hwnd: HWND) -> Option<HWND> {
    unsafe { GetWindow(hwnd, GW_OWNER) }.ok()
}

fn frame_matches(hwnd: HWND, bounds: [i32; 4]) -> Result<bool> {
    let mut rect = RECT::default();
    let dpi = unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if previous.0.is_null() {
            return Err("cannot measure the browser frame in physical pixels".into());
        }
        let result = GetWindowRect(hwnd, &mut rect);
        let dpi = GetDpiForWindow(hwnd);
        SetThreadDpiAwarenessContext(previous);
        result?;
        dpi
    };
    if dpi == 0 || (dpi != 96 && unsafe { GetSystemMetrics(SM_CMONITORS) } != 1) {
        return Err("cannot map browser bounds on this DPI/monitor configuration".into());
    }
    let physical = [
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ];
    Ok(physical.iter().zip(bounds).all(|(&px, dip)| {
        // CDP reports DIPs. Allow at most one rounding pixel at this scale.
        (i64::from(px) * 96 - i64::from(dip) * i64::from(dpi)).abs() <= i64::from(dpi)
    }))
}

// Current Chromium can run IFileDialog::Show(browser_hwnd) in chrome.mojom.UtilWin.
fn dialog_process(browser: u32, dialog: u32) -> Result<()> {
    if dialog == browser {
        return Ok(());
    }
    let browser = Pid::from_u32(browser);
    let dialog = Pid::from_u32(dialog);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[browser, dialog]),
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    let parent = sys.process(browser).ok_or("browser process disappeared")?;
    let child = sys
        .process(dialog)
        .ok_or("file dialog process disappeared")?;
    if child.parent() != Some(browser)
        || parent.exe().is_none()
        || child.exe() != parent.exe()
        || !child.cmd().iter().any(|a| a == "--type=utility")
        || !child
            .cmd()
            .iter()
            .any(|a| a == "--utility-sub-type=chrome.mojom.UtilWin")
    {
        return Err("dialog is outside the browser and its Windows file dialog service".into());
    }
    Ok(())
}

fn modal(browser: u32, bounds: [i32; 4], expected: Option<u64>) -> Result<(HWND, HWND)> {
    let list = windows()?;
    let mut frames = Vec::new();
    for &window in &list {
        if pid(window) == browser
            && class(window) == "Chrome_WidgetWin_1"
            && owner_of(window).is_none()
            && !unsafe { IsIconic(window) }.as_bool()
            && frame_matches(window, bounds)?
        {
            frames.push(window);
        }
    }
    let &[frame] = frames.as_slice() else {
        return Err("browser frame bounds are absent or ambiguous".into());
    };
    let owned: Vec<_> = list
        .iter()
        .copied()
        .filter(|&w| owner_of(w) == Some(frame))
        .collect();
    let &[dialog] = owned.as_slice() else {
        return Err("expected exactly one visible window owned by the browser frame".into());
    };
    if class(dialog) != "#32770"
        || expected.is_some_and(|id| id != dialog.0 as usize as u64)
        || unsafe { GetForegroundWindow() } != dialog
        || unsafe { IsWindowEnabled(frame) }.as_bool()
        || !unsafe { IsWindowEnabled(dialog) }.as_bool()
        || list.iter().any(|&w| owner_of(w) == Some(dialog))
    {
        return Err(
            "expected an active, unblocked modal dialog owned by the selected browser window"
                .into(),
        );
    }
    dialog_process(browser, pid(dialog))?;
    Ok((frame, dialog))
}

fn unique(
    parent: &IUIAutomationElement,
    condition: &IUIAutomationCondition,
) -> Result<IUIAutomationElement> {
    unsafe {
        let found = parent.FindAll(TreeScope_Descendants, condition)?;
        if found.Length()? != 1 {
            return Err("missing or ambiguous Common Item Dialog control".into());
        }
        Ok(found.GetElement(0)?)
    }
}

struct Controls {
    frame: IUIAutomationElement,
    dialog: IUIAutomationElement,
    field: IUIAutomationElement,
    accept: IUIAutomationElement,
    cancel: IUIAutomationElement,
}

impl Controls {
    fn observe(ui: &IUIAutomation, frame: HWND, hwnd: HWND, kind: Kind) -> Result<Self> {
        unsafe {
            let dialog = ui.ElementFromHandle(hwnd)?;
            let id = |s: &str| {
                ui.CreatePropertyCondition(
                    UIA_AutomationIdPropertyId,
                    &VARIANT::from(BSTR::from(s)),
                )
            };
            let role = |t: UIA_CONTROLTYPE_ID| {
                ui.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(t.0))
            };
            let button_role = ui.CreateOrCondition(
                &role(UIA_ButtonControlTypeId)?,
                &role(UIA_SplitButtonControlTypeId)?,
            )?;
            let button = |s: &str| -> Result<_> {
                unique(&dialog, &ui.CreateAndCondition(&id(s)?, &button_role)?)
            };
            let accept = button("1")?;
            let cancel = button("2")?;
            // The filename edit is nested under the shell's DirectUI filename host.
            // Search and address edits elsewhere in the dialog also support Value.
            let host = unique(&dialog, &id("FileNameControlHost")?)?;
            let field = unique(&host, &role(UIA_EditControlTypeId)?)?;
            if kind == Kind::Open {
                // Folder mode can reuse the same shell edit control. Require
                // positive evidence of the filename field and file-type selector.
                let field_label = field
                    .CurrentName()?
                    .to_string()
                    .replace('&', "")
                    .to_lowercase();
                if !matches!(
                    field_label.trim_end_matches([':', '：', ' ']),
                    "file name"
                        | "文件名"
                        | "文件名(n)"
                        | "檔案名稱"
                        | "檔案名稱(n)"
                        | "ファイル名"
                        | "ファイル名(n)"
                ) {
                    return Err("expected a file name field, not a folder selector".into());
                }
                let types = unique(
                    &dialog,
                    &ui.CreateAndCondition(
                        &id("FileTypeControlHost")?,
                        &role(UIA_ComboBoxControlTypeId)?,
                    )?,
                )?;
                if types.CurrentIsOffscreen()?.as_bool()
                    || types.CurrentProcessId()? as u32 != pid(hwnd)
                {
                    return Err("file-type selector is unavailable".into());
                }
            }
            for element in [&field, &accept, &cancel] {
                if element.CurrentProcessId()? as u32 != pid(hwnd)
                    || element.CurrentIsOffscreen()?.as_bool()
                {
                    return Err("file dialog control is hidden or from another process".into());
                }
            }
            if !field.CurrentIsEnabled()?.as_bool()
                || field.CurrentIsPassword()?.as_bool()
                || !cancel.CurrentIsEnabled()?.as_bool()
            {
                return Err("file dialog controls are unavailable".into());
            }
            let label = accept
                .CurrentName()?
                .to_string()
                .replace('&', "")
                .to_lowercase();
            let title = dialog.CurrentName()?.to_string().to_lowercase();
            if !matches_kind(kind, &title, &label) {
                return Err("the observed dialog title and primary button do not identify the requested Open/Save operation".into());
            }
            Ok(Self {
                frame: ui.ElementFromHandle(frame)?,
                dialog,
                field,
                accept,
                cancel,
            })
        }
    }
}

// A Save As dialog can change its primary button to Open when a folder is selected.
fn matches_kind(kind: Kind, title: &str, button: &str) -> bool {
    match kind {
        Kind::Open => {
            matches!(
                title,
                "open"
                    | "choose file to upload"
                    | "select a file this site can read"
                    | "select files this site can read"
                    | "打开"
                    | "開啟"
                    | "開く"
            ) && matches!(
                button,
                "open" | "打开" | "打开(o)" | "開啟" | "開啟(o)" | "開く" | "開く(o)"
            )
        }
        Kind::Save => {
            matches!(
                title,
                "save as"
                    | "另存为"
                    | "另存新檔"
                    | "名前を付けて保存"
                    | "warning: this site can see edits you make"
            ) && matches!(
                button,
                "save" | "保存" | "保存(s)" | "存檔" | "存檔(s)" | "儲存" | "儲存(s)"
            )
        }
    }
}

fn ordinary_path<'a>(requested: &Path, canonical: &'a Path) -> Result<&'a str> {
    let text = canonical.to_str().ok_or("invalid file path text")?;
    let ordinary = text
        .strip_prefix(r"\\?\")
        .ok_or("expected a canonical local drive path")?;
    let input = requested.to_str().ok_or("invalid file path text")?;
    let bytes = ordinary.as_bytes();
    if bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || &bytes[1..3] != b":\\"
        || input.strip_prefix(r"\\?\").unwrap_or(input) != ordinary
        || ordinary.chars().any(char::is_control)
        || ordinary[3..].contains(['%', '<', '>', ':', '"', '/', '|', '?', '*'])
        || ordinary[3..]
            .split('\\')
            .any(|part| part.is_empty() || part.ends_with([' ', '.']))
    {
        return Err("use an exact ordinary local drive path without shell expansion".into());
    }
    Ok(ordinary)
}

pub struct FilePicker {
    browser: u32,
    bounds: [i32; 4],
    kind: Kind,
    frame: HWND,
    dialog: HWND,
    ui: IUIAutomation2,
    controls: Controls,
}

fn timeout(ui: &IUIAutomation2, deadline: Instant) -> Result<()> {
    let ms = remaining(deadline)?.as_millis().clamp(1, 2000) as u32;
    unsafe {
        ui.SetConnectionTimeout(ms)?;
        ui.SetTransactionTimeout(ms)?;
    }
    Ok(())
}

impl FilePicker {
    pub fn observe(
        pid: u32,
        expected: Option<u64>,
        bounds: [i32; 4],
        kind: Kind,
        deadline: Instant,
    ) -> Result<Self> {
        remaining(deadline)?;
        crate::com::ensure();
        let (frame, dialog) = modal(pid, bounds, expected)?;
        let ui: IUIAutomation2 =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;
        timeout(&ui, deadline)?;
        let controls = Controls::observe(&ui, frame, dialog, kind)?;
        let picker = Self {
            browser: pid,
            bounds,
            kind,
            frame,
            dialog,
            ui,
            controls,
        };
        picker.revalidate(deadline)?;
        Ok(picker)
    }

    pub fn id(&self) -> u64 {
        self.dialog.0 as usize as u64
    }

    /// The Win32 owner HWND binds this dialog to the selected browser frame.
    pub fn process_only(&self) -> bool {
        false
    }

    pub fn revalidate(&self, deadline: Instant) -> Result<()> {
        timeout(&self.ui, deadline)?;
        let (frame, dialog) = modal(self.browser, self.bounds, Some(self.id()))?;
        if frame != self.frame {
            return Err("file dialog owner changed".into());
        }
        let fresh = Controls::observe(&self.ui, frame, dialog, self.kind)?;
        for (before, now) in [
            (&self.controls.frame, &fresh.frame),
            (&self.controls.dialog, &fresh.dialog),
            (&self.controls.field, &fresh.field),
            (&self.controls.accept, &fresh.accept),
            (&self.controls.cancel, &fresh.cancel),
        ] {
            if !unsafe { self.ui.CompareElements(before, now) }?.as_bool() {
                return Err("observed file dialog or control was replaced".into());
            }
        }
        remaining(deadline)?;
        Ok(())
    }

    fn invoke_and_wait(
        &self,
        button: &IUIAutomationElement,
        deadline: Instant,
        before_invoke: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.revalidate(deadline)?;
        let invoke = unsafe {
            button.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        }?;
        if !unsafe { button.CurrentIsEnabled() }?.as_bool() {
            return Err("dialog button is disabled".into());
        }
        before_invoke()?;
        if unsafe { GetForegroundWindow() } != self.dialog
            || !unsafe { IsWindowEnabled(self.dialog) }.as_bool()
        {
            return Err("file dialog lost its active modal state before confirmation".into());
        }
        remaining(deadline)?;
        unsafe { invoke.Invoke() }?;
        loop {
            let left = remaining(deadline)?;
            let list = windows()?;
            if list.iter().any(|&w| {
                owner_of(w) == Some(self.dialog)
                    || (w != self.dialog && owner_of(w) == Some(self.frame))
            }) {
                return Err("a secondary dialog or confirmation appeared".into());
            }
            if pid(self.frame) != self.browser {
                return Err("browser frame disappeared during confirmation".into());
            }
            if !unsafe { IsWindow(self.dialog) }.as_bool()
                && unsafe { IsWindowEnabled(self.frame) }.as_bool()
            {
                let live = unsafe { self.ui.ElementFromHandle(self.frame) }?;
                if !unsafe { self.ui.CompareElements(&self.controls.frame, &live) }?.as_bool() {
                    return Err("browser frame was replaced during confirmation".into());
                }
                remaining(deadline)?;
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(25)));
        }
    }

    pub fn dismiss(&self, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Open {
            return Err("dismiss requires an observed Open dialog".into());
        }
        self.invoke_and_wait(&self.controls.cancel, deadline, || Ok(()))
    }

    pub fn save_new(&self, path: &Path, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Save {
            return Err("save requires an observed Save dialog".into());
        }
        remaining(deadline)?;
        let dest = NewFile::under(path.parent().ok_or("missing destination parent")?, path)?;
        let ordinary = ordinary_path(path, dest.path())?;
        if path.extension().is_none() {
            return Err("use an explicit extension for the save destination".into());
        }
        self.choose_path(ordinary, deadline, || Ok(dest.revalidate()?))
    }

    fn choose_path(
        &self,
        ordinary: &str,
        deadline: Instant,
        recheck: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.revalidate(deadline)?;
        let value = unsafe {
            self.controls
                .field
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        }?;
        if unsafe { value.CurrentIsReadOnly() }?.as_bool() {
            return Err("filename is read-only".into());
        }
        remaining(deadline)?;
        unsafe { value.SetValue(&BSTR::from(ordinary)) }?;
        self.invoke_and_wait(&self.controls.accept, deadline, || {
            recheck()?;
            if !unsafe { value.CurrentValue() }?
                .as_wide()
                .iter()
                .copied()
                .eq(ordinary.encode_utf16())
            {
                return Err("file dialog did not retain the exact destination".into());
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_path_only_removes_the_verbatim_local_drive_prefix() {
        let canonical = Path::new(r"\\?\C:\收据\one file.txt");
        let ordinary = r"C:\收据\one file.txt";
        assert_eq!(ordinary_path(canonical, canonical).unwrap(), ordinary);
        assert_eq!(
            ordinary_path(Path::new(ordinary), canonical).unwrap(),
            ordinary
        );
        for path in [
            r"\\?\UNC\server\share\one",
            r"\\?\C:\%TEMP%\one",
            r"\\?\C:\dir.\one",
            r"\\?\C:\one.txt:stream",
            r"\\?\C:\a*.txt",
        ] {
            assert!(ordinary_path(Path::new(path), Path::new(path)).is_err());
        }
        assert!(ordinary_path(Path::new(r"C:\different.txt"), canonical).is_err());
    }

    #[test]
    fn a_folder_or_a_save_dialog_browsing_folders_is_not_an_open_file_dialog() {
        assert!(matches_kind(
            Kind::Open,
            "select a file this site can read",
            "open"
        ));
        assert!(!matches_kind(Kind::Open, "select folder", "open"));
        assert!(!matches_kind(Kind::Open, "save as", "open"));
        assert!(!matches_kind(Kind::Save, "save as", "open"));
    }
}

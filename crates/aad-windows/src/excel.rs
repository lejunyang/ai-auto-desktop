//! Read-only Office NativeOM binding to one existing Excel document window.
use std::{
    ptr,
    sync::mpsc,
    time::{Duration, Instant},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows::core::{IUnknown, Interface, BSTR, GUID, HSTRING, PCWSTR, VARIANT};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, IDispatch, COINIT_APARTMENTTHREADED, DISPATCH_PROPERTYGET,
    DISPPARAMS, SAFEARRAY,
};
use windows::Win32::System::Ole::*;
use windows::Win32::System::Variant::*;
use windows::Win32::UI::Accessibility::AccessibleObjectFromWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const MAX_CELLS: usize = 320_000;
const MAX_BYTES: usize = 16 << 20;
// Excel text cells can contain 32,767 UTF-16 code units. Bound each inbound COM allocation.
const STRIP: usize = 128;

/// Zero-based top, left, height, width. Returns the complete rectangle in row-major order.
pub fn read_range(
    hwnd: u64,
    book: &str,
    sheet: &str,
    area: [usize; 4],
) -> std::result::Result<Vec<String>, String> {
    let [row, col, height, width] = area;
    if hwnd == 0
        || hwnd > u64::from(u32::MAX)
        || [book, sheet]
            .iter()
            .any(|s| s.is_empty() || s.len() > 1024 || s.contains('\0'))
        || height == 0
        || width == 0
        || height.checked_mul(width).is_none_or(|n| n > MAX_CELLS)
        || row.checked_add(height).is_none_or(|n| n > 1_048_576)
        || col.checked_add(width).is_none_or(|n| n > 16_384)
    {
        return Err("invalid Excel window, workbook, worksheet or range bounds".into());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let (book, sheet) = (book.to_owned(), sheet.to_owned());
    let limit = Duration::from_secs(30);
    let deadline = Instant::now() + limit;
    // Excel can block a COM call while its user is editing. The worker has no
    // writes or file output and checks the deadline between bounded reads.
    std::thread::Builder::new()
        .name("excel-nativeom".into())
        .spawn(move || {
            if let Err(err) = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
                let _ = tx.send(Err(err.to_string()));
                return;
            }
            let result = read(HWND(hwnd as usize as *mut _), &book, &sheet, area, deadline);
            unsafe { CoUninitialize() };
            let _ = tx.send(result.map_err(|e| e.to_string()));
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(limit)
        .map_err(|_| "Excel did not finish the scoped range read".to_string())?
}

fn class(hwnd: HWND) -> String {
    let mut buf = [0u16; 32];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n as usize])
}

fn pid(hwnd: HWND) -> u32 {
    let mut result = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut result)) };
    result
}

fn document(hwnd: HWND) -> Result<(u32, HWND)> {
    let process = pid(hwnd);
    if process == 0 || class(hwnd) != "XLMAIN" || unsafe { GetAncestor(hwnd, GA_ROOT) } != hwnd {
        return Err("expected the exact top-level XLMAIN workbook HWND".into());
    }
    let id = Pid::from_u32(process);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[id]),
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    if !sys
        .process(id)
        .and_then(|p| p.exe()?.file_name()?.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("excel.exe"))
    {
        return Err("window does not belong to desktop Excel".into());
    }
    unsafe extern "system" fn collect(child: HWND, param: LPARAM) -> BOOL {
        if unsafe { IsWindowVisible(child) }.as_bool() && class(child) == "EXCEL7" {
            unsafe { &mut *(param.0 as *mut Vec<HWND>) }.push(child);
        }
        BOOL(1)
    }
    let mut children = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            hwnd,
            Some(collect),
            LPARAM(&mut children as *mut Vec<HWND> as isize),
        );
    }
    let &[child] = children.as_slice() else {
        return Err("missing or ambiguous visible EXCEL7 document pane".into());
    };
    if pid(child) != process || unsafe { GetAncestor(child, GA_ROOT) } != hwnd {
        return Err("Excel document pane changed owner".into());
    }
    Ok((process, child))
}

// All calls are property gets. Arguments are either an exact worksheet name or
// an A1 address constructed from validated integers, never an Excel expression.
fn get(object: &IDispatch, name: &str, arg: Option<&str>) -> Result<VARIANT> {
    let wide = HSTRING::from(name);
    let mut id = 0;
    let mut argument = arg.map(VARIANT::from);
    let params = DISPPARAMS {
        cArgs: u32::from(argument.is_some()),
        rgvarg: argument.as_mut().map_or(ptr::null_mut(), ptr::from_mut),
        ..Default::default()
    };
    let mut result = VARIANT::new();
    unsafe {
        object.GetIDsOfNames(&GUID::zeroed(), &PCWSTR(wide.as_ptr()), 1, 0x409, &mut id)?;
        object
            .Invoke(
                id,
                &GUID::zeroed(),
                0x409,
                DISPATCH_PROPERTYGET,
                &params,
                Some(&mut result),
                None,
                None,
            )
            .map_err(|e| format!("Excel property {name} failed: {e}"))?;
    }
    Ok(result)
}

fn tag(v: &VARIANT) -> VARENUM {
    VARENUM(unsafe { v.as_raw().Anonymous.Anonymous.vt })
}

fn object(o: &IDispatch, name: &str, arg: Option<&str>) -> Result<IDispatch> {
    let v = get(o, name, arg)?;
    if tag(&v) != VT_DISPATCH {
        return Err(format!("Excel {name} did not return an object").into());
    }
    // Borrow the interface from the owned VARIANT and AddRef before it drops.
    unsafe { IDispatch::from_raw_borrowed(&v.as_raw().Anonymous.Anonymous.Anonymous.pdispVal) }
        .cloned()
        .ok_or_else(|| "empty Excel object".into())
}

fn same(a: &IDispatch, b: &IDispatch) -> Result<bool> {
    Ok(a.cast::<IUnknown>()? == b.cast::<IUnknown>()?)
}

fn check_name(obj: &IDispatch, expected: &str) -> Result<()> {
    let name = get(obj, "Name", None)?;
    if tag(&name) != VT_BSTR
        || !BSTR::try_from(&name)?
            .as_wide()
            .iter()
            .copied()
            .eq(expected.encode_utf16())
    {
        return Err("Excel workbook or worksheet name does not match exactly".into());
    }
    Ok(())
}

struct Binding {
    process: u32,
    pane: HWND,
    window: IDispatch,
    book: IDispatch,
    sheet: IDispatch,
}

impl Binding {
    fn resolve(hwnd: HWND, book: &str, sheet: &str) -> Result<Self> {
        let (process, pane) = document(hwnd)?;
        let mut raw = ptr::null_mut();
        unsafe {
            AccessibleObjectFromWindow(pane, OBJID_NATIVEOM.0 as u32, &IDispatch::IID, &mut raw)
        }?;
        if raw.is_null() {
            return Err("Excel NativeOM returned no Window".into());
        }
        let window = unsafe { IDispatch::from_raw(raw) };
        if i32::try_from(&get(&window, "Hwnd", None)?)? as u32 as usize != hwnd.0 as usize {
            return Err(
                "NativeOM Window.Hwnd does not identify the requested workbook window".into(),
            );
        }
        // ActiveSheet belongs to this specific Window, including when another
        // workbook has the foreground. Its Parent is the workbook to traverse.
        let workbook = object(&object(&window, "ActiveSheet", None)?, "Parent", None)?;
        check_name(&workbook, book)?;
        let worksheet = object(&object(&workbook, "Worksheets", None)?, "Item", Some(sheet))?;
        check_name(&worksheet, sheet)?;
        if !same(&object(&worksheet, "Parent", None)?, &workbook)? {
            return Err("worksheet changed workbook".into());
        }
        Ok(Self {
            process,
            pane,
            window,
            book: workbook,
            sheet: worksheet,
        })
    }
}

fn a1(row: usize, col: usize) -> String {
    let mut n = col + 1;
    let mut letters = Vec::new();
    while n > 0 {
        n -= 1;
        letters.push((b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    format!("{}{}", letters.iter().rev().collect::<String>(), row + 1)
}

fn cell(v: &VARIANT) -> Result<String> {
    Ok(match tag(v) {
        VT_EMPTY => String::new(),
        VT_BSTR => String::from_utf16(BSTR::try_from(v)?.as_wide())?,
        VT_BOOL => bool::try_from(v)?.to_string(),
        VT_R8 => f64::try_from(v)?.to_string(),
        VT_I4 => i32::try_from(v)?.to_string(),
        VT_ERROR => return Err("selected Excel range contains an error value".into()),
        _ => return Err("unsupported Excel Value2 cell type".into()),
    })
}

fn strip(value: &VARIANT, rows: usize, cols: usize) -> Result<Vec<String>> {
    if rows == 1 && cols == 1 {
        return Ok(vec![cell(value)?]);
    }
    if tag(value).0 != (VT_ARRAY.0 | VT_VARIANT.0) {
        return Err("Excel returned an incomplete Value2 array".into());
    }
    unsafe {
        let array: *const SAFEARRAY = value.as_raw().Anonymous.Anonymous.Anonymous.parray.cast();
        if array.is_null()
            || SafeArrayGetDim(array) != 2
            || SafeArrayGetVartype(array)? != VT_VARIANT
            || SafeArrayGetElemsize(array) as usize != std::mem::size_of::<VARIANT>()
        {
            return Err("invalid Excel Value2 array".into());
        }
        for (dim, count) in [(1, rows), (2, cols)] {
            if SafeArrayGetLBound(array, dim)? != 1
                || SafeArrayGetUBound(array, dim)? != count as i32
            {
                return Err("Excel Value2 dimensions do not match the requested range".into());
            }
        }
        let mut data = ptr::null_mut();
        SafeArrayAccessData(array, &mut data)?;
        // Contiguous one-row or one-column strips have the same order in both layouts.
        let cells = if data.is_null() {
            Err("empty Excel array data".into())
        } else {
            std::slice::from_raw_parts(data.cast::<VARIANT>(), rows * cols)
                .iter()
                .map(cell)
                .collect()
        };
        SafeArrayUnaccessData(array)?;
        cells
    }
}

fn read(
    hwnd: HWND,
    book: &str,
    sheet: &str,
    [top, left, height, width]: [usize; 4],
    deadline: Instant,
) -> Result<Vec<String>> {
    let bind = Binding::resolve(hwnd, book, sheet)?;
    let vertical = height >= width;
    let (outside, inside) = if vertical {
        (width, height)
    } else {
        (height, width)
    };
    let mut cells = vec![String::new(); height * width];
    let mut size = 0;
    for outer in 0..outside {
        for start in (0..inside).step_by(STRIP) {
            if Instant::now() >= deadline {
                return Err("Excel range read timed out".into());
            }
            let len = STRIP.min(inside - start);
            let (r, c, h, w) = if vertical {
                (start, outer, len, 1)
            } else {
                (outer, start, 1, len)
            };
            let address = format!(
                "{}:{}",
                a1(top + r, left + c),
                a1(top + r + h - 1, left + c + w - 1)
            );
            let range = object(&bind.sheet, "Range", Some(&address))?;
            if i32::try_from(&get(&range, "Row", None)?)? != (top + r + 1) as i32
                || i32::try_from(&get(&range, "Column", None)?)? != (left + c + 1) as i32
            {
                return Err("Excel returned a range with a different origin".into());
            }
            let merged = get(&range, "MergeCells", None)?;
            if !same(&object(&range, "Parent", None)?, &bind.sheet)?
                || tag(&merged) != VT_BOOL
                || bool::try_from(&merged)?
            {
                return Err("range parent changed or range includes merged cells".into());
            }
            for (i, value) in strip(&get(&range, "Value2", None)?, h, w)?
                .into_iter()
                .enumerate()
            {
                size += value.len();
                if size > MAX_BYTES {
                    return Err("selected Excel text exceeds 16 MiB".into());
                }
                let at = r * width + c + if vertical { i * width } else { i };
                cells[at] = value;
            }
        }
    }
    let fresh = Binding::resolve(hwnd, book, sheet)?;
    if bind.process != fresh.process
        || bind.pane != fresh.pane
        || !same(&bind.window, &fresh.window)?
        || !same(&bind.book, &fresh.book)?
        || !same(&bind.sheet, &fresh.sheet)?
    {
        return Err("Excel window, workbook or worksheet changed during the read".into());
    }
    Ok(cells)
}

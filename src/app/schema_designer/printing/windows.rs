use std::{io, mem::size_of, ptr};

use image::RgbImage;
use windows_sys::Win32::{
    Foundation::{ERROR_CANCELLED, GlobalFree, SetLastError},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteDC, GetDeviceCaps, HALFTONE,
        HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, SRCCOPY, SetBrushOrgEx, SetStretchBltMode,
        StretchDIBits, VERTRES,
    },
    Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage},
    UI::{
        Controls::Dialogs::{
            CommDlgExtendedError, PD_DISABLEPRINTTOFILE, PD_HIDEPRINTTOFILE, PD_NOPAGENUMS,
            PD_NOSELECTION, PD_PAGENUMS, PD_RETURNDC, PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGW,
            PrintDlgW,
        },
        Input::KeyboardAndMouse::GetActiveWindow,
    },
};

struct PrintDialog(PRINTDLGW);

impl Drop for PrintDialog {
    fn drop(&mut self) {
        // SAFETY: Each non-null resource was allocated by PrintDlgW for this dialog.
        unsafe {
            if !self.0.hDC.is_null() {
                DeleteDC(self.0.hDC);
            }
            if !self.0.hDevMode.is_null() {
                GlobalFree(self.0.hDevMode);
            }
            if !self.0.hDevNames.is_null() {
                GlobalFree(self.0.hDevNames);
            }
        }
    }
}

pub(super) fn print(
    title: &str,
    page_count: usize,
    mut render: impl FnMut(usize) -> Result<RgbImage, String>,
) -> Result<bool, String> {
    let last_page = u16::try_from(page_count).map_err(|_| "打印页数超出系统支持范围。")?;
    let mut flags = PD_RETURNDC
        | PD_NOSELECTION
        | PD_USEDEVMODECOPIESANDCOLLATE
        | PD_DISABLEPRINTTOFILE
        | PD_HIDEPRINTTOFILE;
    if last_page == 1 {
        flags |= PD_NOPAGENUMS;
    }
    let mut dialog = PrintDialog(PRINTDLGW {
        lStructSize: size_of::<PRINTDLGW>() as u32,
        // SAFETY: This runs on the app's UI thread. A null HWND is also allowed.
        hwndOwner: unsafe { GetActiveWindow() },
        Flags: flags,
        nFromPage: 1,
        nToPage: last_page,
        nMinPage: 1,
        nMaxPage: last_page,
        nCopies: 1,
        ..Default::default()
    });
    // SAFETY: The complete PRINTDLGW is initialized and valid for the modal call.
    if unsafe { PrintDlgW(&mut dialog.0) } == 0 {
        // SAFETY: This reads the common dialog result for the current thread.
        return match unsafe { CommDlgExtendedError() } {
            0 => Ok(false),
            code => Err(format!("无法打开系统打印对话框（错误 0x{code:04X}）。")),
        };
    }
    let (first, last) = if dialog.0.Flags & PD_PAGENUMS != 0 {
        (dialog.0.nFromPage.max(1), dialog.0.nToPage.min(last_page))
    } else {
        (1, last_page)
    };
    if first > last {
        return Err("系统打印对话框返回的页码范围无效。".into());
    }
    let dc = dialog.0.hDC;
    if dc.is_null() {
        return Err("系统打印对话框没有返回打印机。".into());
    }
    let paper = PrintableArea::new(dc)?;
    let title = title.encode_utf16().chain([0]).collect::<Vec<_>>();
    let doc = DOCINFOW {
        cbSize: size_of::<DOCINFOW>() as i32,
        lpszDocName: title.as_ptr(),
        ..Default::default()
    };
    // SAFETY: The dialog owns the live DC, and doc and its UTF-16 title are live for the call.
    let job = unsafe {
        SetLastError(0);
        StartDocW(dc, &doc)
    };
    if job <= 0 {
        return spool_error("打印机无法开始作业。");
    }
    let result = (|| {
        for page in first..=last {
            let image = render(usize::from(page - 1))?;
            // SAFETY: The DC has an open print job. Each started page is finished below.
            if unsafe { StartPage(dc) } <= 0 {
                return spool_error(&format!("打印机无法开始第 {page} 页。"));
            }
            draw_page(dc, &paper, image)?;
            // SAFETY: A page is active on the DC.
            if unsafe { EndPage(dc) } <= 0 {
                return spool_error(&format!("打印机无法完成第 {page} 页。"));
            }
        }
        // SAFETY: The DC has an open print job with all selected pages finished.
        if unsafe { EndDoc(dc) } <= 0 {
            return spool_error("打印机未能接收作业。");
        }
        Ok(true)
    })();
    if !matches!(result, Ok(true)) {
        // SAFETY: The dialog still owns the DC. Discard any pages still held by this job.
        unsafe { AbortDoc(dc) };
    }
    result
}

fn spool_error(message: &str) -> Result<bool, String> {
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == ERROR_CANCELLED as i32 => Ok(false),
        Some(0) | None => Err(message.into()),
        Some(_) => Err(format!("{message}{error}")),
    }
}

struct PrintableArea {
    width: i32,
    height: i32,
    dpi_x: f64,
    dpi_y: f64,
}

impl PrintableArea {
    fn new(dc: HDC) -> Result<Self, String> {
        // SAFETY: dc is the live printer DC selected by the user.
        let value = |capability| unsafe { GetDeviceCaps(dc, capability as i32) };
        let area = Self {
            width: value(HORZRES),
            height: value(VERTRES),
            dpi_x: f64::from(value(LOGPIXELSX)),
            dpi_y: f64::from(value(LOGPIXELSY)),
        };
        if area.width <= 0 || area.height <= 0 || area.dpi_x <= 0.0 || area.dpi_y <= 0.0 {
            return Err("未能获取打印机的可打印区域。".into());
        }
        Ok(area)
    }
}

fn draw_page(dc: HDC, paper: &PrintableArea, image: RgbImage) -> Result<(), String> {
    let (width, height) = (image.width() as i32, image.height() as i32);
    // Use physical sizes for fitting so printer dots with different X/Y DPI do not distort the page.
    let scale = (f64::from(paper.width) / paper.dpi_x / f64::from(width))
        .min(f64::from(paper.height) / paper.dpi_y / f64::from(height));
    let dest_width = (f64::from(width) * scale * paper.dpi_x).round().max(1.0) as i32;
    let dest_height = (f64::from(height) * scale * paper.dpi_y).round().max(1.0) as i32;
    let x = (paper.width - dest_width) / 2;
    let y = (paper.height - dest_height) / 2;

    let mut dib = Vec::new();
    dib.try_reserve_exact(image.as_raw().len() / 3 * 4)
        .map_err(|_| "没有足够内存准备打印页面。")?;
    for pixel in image.as_raw().chunks_exact(3) {
        dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0]);
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: dc is drawing an active page. info describes the live, top-down BGRX buffer.
    let scan_lines = unsafe {
        SetStretchBltMode(dc, HALFTONE);
        SetBrushOrgEx(dc, 0, 0, ptr::null_mut());
        StretchDIBits(
            dc,
            x,
            y,
            dest_width,
            dest_height,
            0,
            0,
            width,
            height,
            dib.as_ptr().cast(),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    };
    if scan_lines <= 0 {
        return Err("打印机无法绘制架构图页面。".into());
    }
    Ok(())
}

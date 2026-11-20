use image::{ExtendedColorType, ImageEncoder, RgbImage, codecs::png::PngEncoder};
use objc2::{
    AnyThread, MainThreadMarker, MainThreadOnly, extern_class, extern_methods,
    rc::{Allocated, Retained, autoreleasepool},
};
use objc2_app_kit::{NSImage, NSPrintCancelJob, NSPrintInfo, NSPrintOperation};
use objc2_foundation::{NSCopying, NSData, NSInteger, NSObject, NSSize, NSString, NSUInteger};

#[link(name = "PDFKit", kind = "framework")]
unsafe extern "C" {}

extern_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct PDFDocument;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct PDFPage;
);

impl PDFDocument {
    extern_methods!(
        #[unsafe(method(init))]
        #[unsafe(method_family = init)]
        fn init(this: Allocated<Self>) -> Retained<Self>;

        #[unsafe(method(insertPage:atIndex:))]
        #[unsafe(method_family = none)]
        unsafe fn insert_page(&self, page: &PDFPage, index: NSUInteger);

        #[unsafe(method(printOperationForPrintInfo:scalingMode:autoRotate:))]
        #[unsafe(method_family = none)]
        fn print_operation(
            &self,
            info: &NSPrintInfo,
            scaling_mode: NSInteger,
            auto_rotate: bool,
        ) -> Option<Retained<NSPrintOperation>>;
    );
}

impl PDFPage {
    extern_methods!(
        #[unsafe(method(initWithImage:))]
        #[unsafe(method_family = init)]
        fn with_image(this: Allocated<Self>, image: &NSImage) -> Option<Retained<Self>>;
    );
}

const PDF_SCALE_TO_FIT: NSInteger = 1;
const POINTS_PER_MM: f64 = 72.0 / 25.4;
const MARGIN_MM: f64 = 6.0;

pub(super) fn print(
    title: &str,
    page_count: usize,
    mut render: impl FnMut(usize) -> Result<RgbImage, String>,
) -> Result<bool, String> {
    let mtm = MainThreadMarker::new().ok_or("系统打印需要在应用主线程打开。")?;
    autoreleasepool(|_| {
        if NSPrintOperation::currentOperation(mtm).is_some() {
            return Err("已有系统打印操作正在进行。".into());
        }
        let document = PDFDocument::init(PDFDocument::alloc(mtm));
        for index in 0..page_count {
            autoreleasepool(|_| {
                let page = pdf_page(mtm, render(index)?)?;
                // SAFETY: The first insertion is at 0 and subsequent insertions append one page.
                unsafe { document.insert_page(&page, index) };
                Ok::<_, String>(())
            })?;
        }
        let info = NSPrintInfo::sharedPrintInfo().copy();
        let margin = MARGIN_MM * POINTS_PER_MM;
        info.setLeftMargin(margin);
        info.setRightMargin(margin);
        info.setTopMargin(margin);
        info.setBottomMargin(margin);
        let operation = document
            .print_operation(&info, PDF_SCALE_TO_FIT, false)
            .ok_or("无法创建系统打印作业。")?;
        operation.setJobTitle(Some(&NSString::from_str(title)));
        operation.setShowsPrintPanel(true);
        operation.setShowsProgressPanel(true);
        if operation.runOperation() {
            return Ok(true);
        }
        let disposition = operation.printInfo().jobDisposition();
        // SAFETY: NSPrintCancelJob is an immutable AppKit NSString constant.
        if disposition.isEqualToString(unsafe { NSPrintCancelJob }) {
            return Ok(false);
        }
        Err("系统打印操作未完成。".into())
    })
}

fn pdf_page(mtm: MainThreadMarker, raster: RgbImage) -> Result<Retained<PDFPage>, String> {
    let (width, height) = raster.dimensions();
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(raster.as_raw(), width, height, ExtendedColorType::Rgb8)
        .map_err(|error| format!("无法编码打印页面。{error}"))?;
    let data = NSData::with_bytes(&png);
    let image = NSImage::initWithData(NSImage::alloc(), &data).ok_or("无法载入打印页面。")?;
    let width = f64::from(width);
    let height = f64::from(height);
    // Give the image an A4-sized logical extent. PDFKit fits this page to the selected paper.
    let scale =
        ((210.0 - 2.0 * MARGIN_MM) / width).min((297.0 - 2.0 * MARGIN_MM) / height) * POINTS_PER_MM;
    image.setSize(NSSize::new(width * scale, height * scale));
    PDFPage::with_image(PDFPage::alloc(mtm), &image).ok_or_else(|| "无法创建 PDF 打印页面。".into())
}

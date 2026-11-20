#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
#[cfg(debug_assertions)]
mod smoke;

use harbor_sql::{APP_NAME, database, maintenance, odbc, sql_tools};
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

static DATA_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static APPLICATION_ICON: OnceLock<Arc<eframe::egui::IconData>> = OnceLock::new();

fn application_icon() -> &'static Arc<eframe::egui::IconData> {
    APPLICATION_ICON.get_or_init(|| {
        let rgba = image::load_from_memory_with_format(
            include_bytes!("../assets/dolphin.png"),
            image::ImageFormat::Png,
        )
        .expect("the bundled application icon must be a PNG")
        .resize_exact(128, 128, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
        Arc::new(eframe::egui::IconData {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        })
    })
}

fn main() -> eframe::Result {
    #[cfg(debug_assertions)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--smoke-db")
    {
        let args: Vec<String> = std::env::args().collect();
        let result = match args.as_slice() {
            [_, _, host, port, user] => {
                port.parse()
                    .map_err(|_| "端口无效。".into())
                    .and_then(|port| {
                        let password = std::env::var("HARBOR_SMOKE_PASSWORD").unwrap_or_default();
                        smoke::run(host, port, user, &password)
                    })
            }
            _ => Err("用法：harbor-sql --smoke-db <本机回环地址> <端口> <用户名>".into()),
        };
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        println!("数据库冒烟测试通过。");
        return Ok(());
    }
    configure_data_directory().map_err(|error| eframe::Error::AppCreation(Box::new(error)))?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1220.0, 790.0])
            .with_min_inner_size([850.0, 560.0])
            .with_icon(Arc::clone(application_icon()))
            .with_app_id("HarborSQL"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| {
            let mut workbench = app::Workbench::new(&cc.egui_ctx);
            workbench.open_startup_connection();
            Ok(Box::new(workbench))
        }),
    )
}

fn configure_data_directory() -> io::Result<()> {
    if let Some(path) = requested_data_directory(std::env::args_os().skip(1))? {
        let path = if path.is_absolute() {
            path
        } else {
            resolve_data_directory(path, &std::env::current_exe()?)?
        };
        std::fs::create_dir_all(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("无法创建{APP_NAME}的数据目录。{error}"),
            )
        })?;
        DATA_DIRECTORY
            .set(path)
            .map_err(|_| io::Error::other("应用数据目录已初始化。"))?;
    }
    Ok(())
}

fn requested_data_directory(
    mut args: impl Iterator<Item = OsString>,
) -> io::Result<Option<PathBuf>> {
    let mut path = None;
    while let Some(flag) = args.next() {
        if flag != "-dir" {
            continue;
        }
        if path.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "只能指定一个数据目录。",
            ));
        }
        let directory = args
            .next()
            .filter(|value| !value.is_empty() && value != "-dir")
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "-dir 后需要指定数据目录。")
            })?;
        path = Some(PathBuf::from(directory));
    }
    Ok(path)
}

fn resolve_data_directory(path: PathBuf, executable: &Path) -> io::Result<PathBuf> {
    if path.has_root()
        || matches!(
            path.components().next(),
            Some(std::path::Component::Prefix(_))
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "数据目录须使用绝对路径或相对于程序文件夹的路径。",
        ));
    }
    executable
        .parent()
        .map(|directory| directory.join(path))
        .ok_or_else(|| io::Error::other(format!("无法找到{APP_NAME}所在的目录。")))
}

#[cfg(test)]
#[test]
fn accepts_a_portable_directory_relative_to_the_executable() {
    let executable = std::env::current_exe().unwrap();
    let directory = PathBuf::from("便携 数据");
    let args = [OsString::from("-dir"), directory.clone().into_os_string()];
    let parsed = requested_data_directory(args.into_iter()).unwrap().unwrap();
    assert_eq!(
        resolve_data_directory(parsed, &executable).unwrap(),
        executable.parent().unwrap().join(directory),
    );
    assert!(
        requested_data_directory(std::iter::empty())
            .unwrap()
            .is_none()
    );
}

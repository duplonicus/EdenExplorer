use crate::core::portable;
use crossbeam_channel::{Sender, unbounded};
use eframe::egui;
use egui_phosphor::regular;
use std::os::windows::ffi::OsStrExt;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::{
    Win32::{
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        UI::{
            Controls::IImageList,
            Shell::{
                SHFILEINFOW, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGSI_ICON,
                SHGSI_LARGEICON, SHGetFileInfoW, SHGetImageList, SHGetStockIconInfo,
                SHIL_EXTRALARGE, SHSTOCKICONINFO, SIID_DRIVEUNKNOWN,
            },
            WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO},
        },
    },
    core::PCWSTR,
};

struct IconRequest {
    path: PathBuf,
    is_dir: bool,
}

type IconKey = String;

pub struct IconCache {
    textures: Arc<Mutex<HashMap<IconKey, egui::TextureHandle>>>,
    #[allow(dead_code)]
    icon_indices: Arc<Mutex<HashMap<IconKey, i32>>>,
    sender: Sender<IconRequest>,
    // Looked up once: the home check runs for every folder on every frame.
    home_dir: Option<PathBuf>,
}

impl IconCache {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded::<IconRequest>();
        let textures = Arc::new(Mutex::new(HashMap::new()));
        let icon_indices = Arc::new(Mutex::new(HashMap::new()));

        let textures_bg = textures.clone();
        let icon_indices_bg = icon_indices.clone();
        let ctx_bg = ctx.clone();

        thread::spawn(move || {
            let image_list: IImageList = match unsafe { SHGetImageList(SHIL_EXTRALARGE as i32) } {
                Ok(list) => list,
                Err(_) => return,
            };

            while let Ok(req) = rx.recv() {
                let key = icon_key(&req.path, req.is_dir);

                if key.starts_with("portable_device") {
                    if textures_bg.lock().unwrap().contains_key(&key) {
                        continue;
                    }
                    if let Some((pixels, w, h)) = get_portable_device_icon_rgba() {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &pixels,
                        );
                        let texture =
                            ctx_bg.load_texture(format!("icon_{}", key), image, Default::default());
                        textures_bg.lock().unwrap().insert(key.clone(), texture);
                        ctx_bg.request_repaint();
                        continue;
                    }
                }

                // 1️⃣ Get icon index (cached)
                let icon_index = {
                    let mut idx_cache = icon_indices_bg.lock().unwrap();
                    if let Some(&idx) = idx_cache.get(&key) {
                        idx
                    } else {
                        let idx = get_icon_index_for_key(&key, &req.path, req.is_dir).unwrap_or(0);
                        idx_cache.insert(key.clone(), idx);
                        idx
                    }
                };

                // 2️⃣ Skip if texture already exists
                if textures_bg.lock().unwrap().contains_key(&key) {
                    continue;
                }

                // 3️⃣ Fetch icon from system image list
                if let Some(icon) = get_icon_from_list(&image_list, icon_index) {
                    if let Some((pixels, w, h)) = icon_to_rgba(icon) {
                        let _ = unsafe { DestroyIcon(icon) };

                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &pixels,
                        );
                        let texture =
                            ctx_bg.load_texture(format!("icon_{}", key), image, Default::default());

                        textures_bg.lock().unwrap().insert(key.clone(), texture);
                        ctx_bg.request_repaint();
                    }
                }
            }
        });

        Self {
            textures,
            icon_indices,
            sender: tx,
            home_dir: dirs::home_dir(),
        }
    }

    pub fn get(&self, path: &Path, is_dir: bool) -> Option<egui::TextureHandle> {
        let key = icon_key(path, is_dir);

        // Return cached if ready
        if let Some(tex) = self.textures.lock().unwrap().get(&key) {
            return Some(tex.clone());
        }

        // Send request for background thread
        let _ = self.sender.send(IconRequest {
            path: path.to_path_buf(),
            is_dir,
        });

        None
    }

    /// Returns the Phosphor icon for a folder: its own icon for a recognized
    /// name, otherwise the plain folder, so no folder gets Windows' shell icon.
    pub fn get_custom_folder_icon(&self, path: &Path, is_dir: bool) -> Option<&'static str> {
        if !is_dir {
            return None;
        }
        folder_glyph(path, self.home_dir.as_deref())
    }
}

/// `None` leaves drives, shares, This PC and phones on their shell icons.
fn folder_glyph(path: &Path, home_dir: Option<&Path>) -> Option<&'static str> {
    if home_dir == Some(path) || is_linux_home(path) {
        return Some(regular::HOUSE);
    }

    // Drive and share roots have no file name.
    let name = path.file_name()?;
    if path.to_string_lossy().starts_with("::")
        || portable::is_portable_device_path(&path.to_path_buf())
    {
        return None;
    }

    Some(
        name.to_str()
            .and_then(custom_folder_icon)
            .unwrap_or(regular::FOLDER),
    )
}

/// "/home/<user>" in WSL, reached as "W:\home\dup" or
/// "\\wsl.localhost\Ubuntu\home\dup". "home" must sit at the root, so a
/// project's "src\pages\home\tests" keeps its own icon.
fn is_linux_home(path: &Path) -> bool {
    let Some(home) = path.parent() else {
        return false;
    };
    home.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("home"))
        && home.parent().is_some_and(|root| root.parent().is_none())
}

// ---------------- helpers ----------------

fn icon_key(path: &Path, is_dir: bool) -> String {
    if portable::is_portable_device_path(&path.to_path_buf()) {
        if let Some((device_id, _)) = portable::parse_portable_path(&path.to_path_buf()) {
            return format!("portable_device:stock:{}", device_id);
        }
        return "portable_device".to_string();
    }
    if is_dir {
        if is_drive_root(path) {
            format!("drive:{}", path.to_string_lossy().to_lowercase())
        } else {
            "folder".to_string()
        }
    } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        format!("ext:{}", ext.to_lowercase())
    } else {
        "file".to_string()
    }
}

fn is_drive_root(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.len() >= 3 && s.ends_with(":\\") && path.parent().is_none()
}

fn get_icon_index_for_key(key: &str, path: &Path, is_dir: bool) -> Option<i32> {
    let wide: Vec<u16>;
    let mut flags = SHGFI_SYSICONINDEX;
    let file_attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };

    if key.starts_with("drive:") {
        wide = path.as_os_str().encode_wide().chain(Some(0)).collect();
    } else if key.starts_with("portable_device") {
        let fake = PathBuf::from("C:\\");
        wide = fake.as_os_str().encode_wide().chain(Some(0)).collect();
    } else if is_dir {
        wide = "folder".encode_utf16().chain(Some(0)).collect();
        flags |= SHGFI_USEFILEATTRIBUTES;
    } else {
        let ext = key.strip_prefix("ext:").unwrap_or("");
        let fake = if ext.is_empty() {
            "file".to_string()
        } else {
            format!("file.{}", ext)
        };
        wide = fake.encode_utf16().chain(Some(0)).collect();
        flags |= SHGFI_USEFILEATTRIBUTES;
    }

    unsafe {
        let mut info = std::mem::zeroed::<SHFILEINFOW>();
        let res = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            file_attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if res == 0 { None } else { Some(info.iIcon) }
    }
}

fn get_icon_from_list(list: &IImageList, index: i32) -> Option<HICON> {
    unsafe { list.GetIcon(index, 0).ok() }
}

fn get_portable_device_icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let mut info = SHSTOCKICONINFO::default();
        info.cbSize = std::mem::size_of::<SHSTOCKICONINFO>() as u32;
        SHGetStockIconInfo(SIID_DRIVEUNKNOWN, SHGSI_ICON | SHGSI_LARGEICON, &mut info).ok()?;

        if info.hIcon.0.is_null() {
            return None;
        }

        let rgba = icon_to_rgba(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        rgba
    }
}

fn icon_to_rgba(icon: HICON) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let mut icon_info = ICONINFO::default();
        GetIconInfo(icon, &mut icon_info).ok()?;

        let mut bmp = BITMAP::default();
        if GetObjectW(
            icon_info.hbmColor.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as _),
        ) == 0
        {
            return None;
        }

        let width = bmp.bmWidth as u32;
        let height = bmp.bmHeight as u32;

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let hdc = GetDC(None);

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = width as i32;
        bmi.bmiHeader.biHeight = -(height as i32);
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let res = GetDIBits(
            hdc,
            icon_info.hbmColor,
            0,
            height,
            Some(pixels.as_mut_ptr() as _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);

        let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));

        if res == 0 {
            return None;
        }

        // Convert BGRA -> RGBA
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        Some((pixels, width, height))
    }
}

fn custom_folder_icon(name: &str) -> Option<&'static str> {
    let lower = name.trim().to_ascii_lowercase();
    // ".config" matches "config", ".ollama" matches "ollama", and so on.
    let name = lower.trim_start_matches('.');
    // "bin.usr-is-merged" and friends are leftovers from Ubuntu's /usr merge.
    let name = name.strip_suffix(".usr-is-merged").unwrap_or(name);

    let icon = match name {
        // Media
        "music" | "my music" | "songs" => regular::MUSIC_NOTES,
        "pictures" | "my pictures" | "photos" | "images" => regular::IMAGE,
        "wallpapers" => regular::IMAGE_SQUARE,
        "screenshots" | "steam screenshots" | "dcim" | "ansel" => regular::CAMERA,
        "videos" | "my videos" | "captures" => regular::VIDEO_CAMERA,
        "movies" => regular::FILM_STRIP,
        "recordings" | "recording" | "sound recordings" => regular::MICROPHONE,
        "podcasts" => regular::MICROPHONE_STAGE,
        "audiobooks" => regular::HEADPHONES,
        "ringtones" => regular::BELL,
        "books" | "ebooks" => regular::BOOKS,
        "media" => regular::PLAY_CIRCLE,
        "guitar" => regular::GUITAR,
        // Personal folders
        "downloads" | "download" => regular::DOWNLOAD_SIMPLE,
        "documents" | "my documents" | "docs" => regular::FILE_TEXT,
        "desktop" => regular::MONITOR,
        "phone" => regular::DEVICE_MOBILE,
        "crossdevice" => regular::DEVICES,
        "notes" | "notebooks" | "jupyter" => regular::NOTEBOOK,
        "favorites" => regular::STAR,
        "links" => regular::LINK,
        "contacts" => regular::ADDRESS_BOOK,
        "searches" | "saved searches" => regular::MAGNIFYING_GLASS,
        "3d objects" => regular::CUBE,
        "work" | "jobs" => regular::BRIEFCASE,
        "school" => regular::GRADUATION_CAP,
        "archive" | "archives" => regular::ARCHIVE,
        "torrents" => regular::MAGNET,
        "portable apps" => regular::SUITCASE,
        "powertoys" => regular::LEGO,
        "sharex" => regular::SELECTION,
        "obsidian" => regular::DIAMOND,
        "exocharts" | "ta-lib" | "charts" | "trading" => regular::CHART_LINE_UP,
        "matplotlib" | "cufflinks" => regular::CHART_LINE,
        "credentials" | "keys" | "ssh" | "gnupg" | "mcp-auth" => regular::KEY,
        "secrets" => regular::LOCK_KEY,
        "pki" => regular::CERTIFICATE,
        "1password" => regular::KEYHOLE,
        "dropbox" | "google drive" | "icloud drive" | "azure" => regular::CLOUD,
        "aws" => regular::AMAZON_LOGO,
        "oci" => regular::CLOUD,
        "google-cloud-sdk" => regular::GOOGLE_LOGO,
        "astropy" | "openplanetnext" => regular::PLANET,
        "emissary" => regular::DETECTIVE,
        "paperclip" => regular::PAPERCLIP,
        "twitter-cli" => regular::X_LOGO,
        "landscape" => regular::MOUNTAINS,
        // Games
        "games" | "saved games" | "my games" | "xboxgames" | "insomniac" => regular::JOYSTICK,
        "steam" | "steamapps" | "steamlibrary" => regular::STEAM_LOGO,
        "saves" => regular::FLOPPY_DISK,
        "mods" | "plugins" | "extensions" => regular::PUZZLE_PIECE,
        "zomboid" => regular::SKULL,
        // Development
        "dev" | "development" => regular::TERMINAL_WINDOW,
        "projects" | "repos" | "source" | "code" => regular::CODE,
        "scripts" => regular::FILE_CODE,
        "windowspowershell" | "powershell" => regular::TERMINAL,
        "crash_dumps" | "crashdumps" | "crash dumps" => regular::BUG,
        "git" | "gk" => regular::GIT_BRANCH,
        "test" | "tests" => regular::FLASK,
        "data" | "db" | "databases" => regular::DATABASE,
        "installers" | "packages" | "node_modules" | "cargo" | "chocolatey" | "nuget"
        | "pnpm-store" | "npm" | "opt" => regular::PACKAGE,
        "bun" | "nvm" => regular::FILE_JS,
        "docker" => regular::SHIPPING_CONTAINER,
        "dotnet" => regular::FILE_C_SHARP,
        "ipython" | "pyenv" => regular::FILE_PY,
        "openjfx" => regular::COFFEE,
        "m2" => regular::FEATHER,
        "gradle" => regular::HAMMER,
        "rustup" => regular::WRENCH,
        "streamlit" => regular::CROWN,
        "wdm" => regular::BROWSER,
        "apk" | "apks" | "android" => regular::ANDROID_LOGO,
        "app" | "applications" => regular::APP_WINDOW,
        "toolkits" | "toolkit" => regular::TOOLBOX,
        "vm" | "vms" | "virtualbox" => regular::DESKTOP_TOWER,
        "ghidra" => regular::BINARY,
        "antigravity" => regular::ROCKET,
        "kiro" => regular::GHOST,
        // AI tools
        "ai" | "claude" | "anthropic" | "cowork" | "perplexity" | "continue" | "copilot"
        | "lmstudio" => regular::ROBOT,
        "openai" | "chatgpt" | "gpt stuff" => regular::OPEN_AI_LOGO,
        "gemini" => regular::SPARKLE,
        "ollama" => regular::HORSE,
        "comfyui" => regular::GRAPH,
        "huggingface" => regular::SMILEY,
        "models" => regular::BRAIN,
        "triton" | "quadrants_cache" | "nv" => regular::GRAPHICS_CARD,
        // System
        "users" => regular::USERS,
        "public" => regular::GLOBE,
        "windows" => regular::WINDOWS_LOGO,
        "apps" | "programs" | "program files" | "program files (x86)" => regular::APP_WINDOW,
        "appdata" | "programdata" | "config" => regular::GEAR_SIX,
        "local" => regular::HARD_DRIVE,
        "cache" => regular::LIGHTNING,
        "drivers" => regular::CPU,
        "fonts" => regular::TEXT_AA,
        "logs" | "perflogs" => regular::SCROLL,
        "temp" | "tmp" => regular::HOURGLASS,
        "recovery" => regular::LIFEBUOY,
        "startup" => regular::POWER,
        "system volume information" | "inetpub" => regular::LOCK,
        // Windows Installer's rollback files for an install in progress
        "config.msi" => regular::CLOCK_COUNTER_CLOCKWISE,
        "apcpowerchuteconfig" => regular::BATTERY_CHARGING,
        "$recycle.bin" => regular::TRASH,
        // Linux, as seen through WSL ("\\wsl.localhost\Ubuntu" or a mapped drive)
        "home" => regular::USERS,
        "bin" | "sbin" => regular::BINARY,
        "boot" => regular::POWER,
        "etc" => regular::GEAR_SIX,
        "lib" | "lib32" | "lib64" | "libx32" => regular::STACK,
        "lost+found" => regular::QUESTION,
        "mnt" => regular::HARD_DRIVES,
        "snap" => regular::PACKAGE,
        "proc" => regular::PULSE,
        "root" => regular::USER_GEAR,
        "run" => regular::PLAY,
        "srv" => regular::GLOBE,
        "sys" => regular::CPU,
        "usr" => regular::APP_WINDOW,
        "var" => regular::DATABASE,
        // Hidden links Windows keeps for old programs
        "nethood" => regular::NETWORK,
        "recent" => regular::CLOCK,
        "start menu" => regular::SQUARES_FOUR,
        "templates" | "templateengine" => regular::FILE_DASHED,
        "cookies" => regular::COOKIE,
        "sendto" => regular::PAPER_PLANE_TILT,
        // "dev.win", "dev stuff"; "OneDrive - Company" for work accounts
        _ if name.starts_with("dev.") || name.starts_with("dev ") => regular::TERMINAL_WINDOW,
        _ if name.starts_with("onedrive") => regular::CLOUD,
        _ if name.starts_with("vscode") => regular::TERMINAL_WINDOW,
        _ if name.starts_with("cursor") => regular::CURSOR,
        _ if name.starts_with("searx") => regular::MAGNIFYING_GLASS,
        // "trash", and Linux/macOS ".Trash", ".Trash-1000"
        _ if name.starts_with("trash") => regular::TRASH,
        _ if name.contains("backup") || name.ends_with(".bak") => regular::CLOCK_COUNTER_CLOCKWISE,
        _ if name.contains("settings") => regular::GEAR_SIX,
        _ => return None,
    };

    Some(icon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_home_folder_at_the_root_gives_the_house() {
        assert!(is_linux_home(Path::new(r"W:\home\dup")));
        assert!(is_linux_home(Path::new(r"\\wsl.localhost\Ubuntu\home\dup")));
        assert!(!is_linux_home(Path::new(r"C:\site\pages\home\tests")));
        assert!(!is_linux_home(Path::new(r"W:\home")));
    }

    #[test]
    fn unrecognized_folders_get_the_plain_folder_icon() {
        let home = Path::new(r"C:\Users\dup");
        assert_eq!(
            folder_glyph(Path::new(r"C:\Users\dup\dev.win\EdenExplorer"), Some(home)),
            Some(regular::FOLDER)
        );
        assert_eq!(folder_glyph(home, Some(home)), Some(regular::HOUSE));
        assert_eq!(
            folder_glyph(Path::new(r"C:\Users\dup\Downloads"), Some(home)),
            Some(regular::DOWNLOAD_SIMPLE)
        );
    }

    #[test]
    fn drives_shares_and_virtual_folders_keep_their_shell_icons() {
        for path in [
            r"C:\",
            r"\\server\share",
            "::MY_PC::",
            "::RECYCLE_BIN::",
            &portable::make_portable_path("phone", "DEVICE").to_string_lossy(),
        ] {
            assert_eq!(folder_glyph(Path::new(path), None), None, "{path}");
        }
    }

    #[test]
    fn linux_folders_match() {
        assert_eq!(custom_folder_icon("etc"), Some(regular::GEAR_SIX));
        assert_eq!(
            custom_folder_icon("bin.usr-is-merged"),
            Some(regular::BINARY)
        );
    }

    #[test]
    fn dot_folders_and_installer_leftovers_match() {
        assert_eq!(custom_folder_icon(".gnupg"), Some(regular::KEY));
        assert_eq!(
            custom_folder_icon(".claude.bak"),
            Some(regular::CLOCK_COUNTER_CLOCKWISE)
        );
        assert_eq!(
            custom_folder_icon("Config.Msi"),
            Some(regular::CLOCK_COUNTER_CLOCKWISE)
        );
        assert_eq!(custom_folder_icon("dev"), Some(regular::TERMINAL_WINDOW));
    }
}

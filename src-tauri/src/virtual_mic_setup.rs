//! Установка виртуального аудио-кабеля (Soundpad-режим «из коробки»).
//!
//! Windows не даёт обычному приложению зарегистрировать себя системным
//! микрофоном без виртуального аудио-драйвера — сам Soundpad решает это
//! точно так же: во время СВОЕЙ установки тихо ставит собственный
//! виртуальный кабель. Мы делаем то же самое, но не переупаковываем чужой
//! бинарник (у VB-CABLE лицензия «all rights reserved» на редистрибуцию) —
//! скачиваем официальный инсталлятор с vb-audio.com по явному клику
//! пользователя и запускаем его с повышением прав (тот же UAC-диалог,
//! что видел бы пользователь, ставя VB-CABLE вручную; кликнуть «Да» за
//! пользователя программно невозможно и не нужно — UAC для этого и создан).
//!
//! Это единственное место в приложении, где выполняется сетевой запрос.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, SHELLEXECUTEINFOW_0,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use com_policy_config::{IPolicyConfig, PolicyConfigClient};
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};

const DRIVER_PACK_URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip";
/// `WaitForSingleObject` INFINITE — ждём завершения инсталлятора без таймаута.
const INFINITE: u32 = 0xFFFF_FFFF;

/// Скачать официальный driver-пак VB-CABLE, распаковать во временную папку
/// и запустить установщик с UAC-повышением и тихими флагами `-i -h`.
/// Блокирующая функция: вызывающий Tauri command уже исполняется в пуле
/// потоков, не в UI-потоке, так что здесь можно спокойно ждать сеть/установку.
pub fn download_and_install_vb_cable() -> Result<(), String> {
    // Запоминаем текущее устройство вывода ДО установки: Windows может сама
    // переключить дефолтное устройство на новый аудио-эндпоинт в момент
    // установки драйвера (общее поведение ОС, не специфичное для VB-CABLE —
    // настоящий Soundpad восстанавливает исходное устройство точно так же).
    let previous_default = current_default_render_endpoint_id();

    let zip_bytes = download(DRIVER_PACK_URL)?;

    let extract_dir = std::env::temp_dir().join("renarrator_vbcable_setup");
    let _ = std::fs::remove_dir_all(&extract_dir); // старые остатки — не критично, если не удалились
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("не удалось создать временную папку: {e}"))?;

    let setup_exe = extract_installer(&zip_bytes, &extract_dir)?;
    run_elevated(&setup_exe, "-i -h")?;

    if let Some(id) = previous_default {
        restore_default_render_endpoint(&id);
    }
    Ok(())
}

/// Endpoint ID (не отображаемое имя) текущего устройства вывода по умолчанию.
/// Отказоустойчиво: любая ошибка COM → `None`, установка кабеля всё равно
/// продолжится — просто не будет автоматического восстановления после нее.
fn current_default_render_endpoint_id() -> Option<String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let id = device.GetId().ok()?;
        id.to_string().ok()
    }
}

/// Возвращает устройство вывода по умолчанию (для обычных программ,
/// мультимедиа и звонков) к тому, что было ДО установки VB-CABLE.
/// Отказоустойчиво: ошибка здесь не должна выглядеть как провал установки
/// кабеля — она лишь логируется, `setup_virtual_mic` в любом случае Ok.
fn restore_default_render_endpoint(id: &str) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy_config: IPolicyConfig =
            match CoCreateInstance(&PolicyConfigClient, None, CLSCTX_ALL) {
                Ok(pc) => pc,
                Err(e) => {
                    eprintln!("[vmic] cannot create IPolicyConfig to restore default device: {e}");
                    return;
                }
            };
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        let pcwstr = PCWSTR(wide.as_ptr());
        for role in [eConsole, eMultimedia, eCommunications] {
            if let Err(e) = policy_config.SetDefaultEndpoint(pcwstr, role) {
                eprintln!("[vmic] cannot restore default output device (role {role:?}): {e}");
            }
        }
    }
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("не удалось скачать драйвер ({url}): {e}"))?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("ошибка чтения загруженного файла: {e}"))?;
    Ok(bytes)
}

/// Распаковывает весь driver-пак (инсталлятору нужны соседние .inf/.cat/.sys
/// из архива) и возвращает путь к `VBCABLE_Setup_x64.exe`.
fn extract_installer(zip_bytes: &[u8], extract_dir: &Path) -> Result<PathBuf, String> {
    let reader = std::io::Cursor::new(zip_bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("повреждённый архив драйвера: {e}"))?;

    let mut setup_path: Option<PathBuf> = None;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("ошибка чтения архива: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let Some(file_name) = name.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let out_path = extract_dir.join(file_name);
        let mut out_file = File::create(&out_path)
            .map_err(|e| format!("не удалось записать '{file_name}': {e}"))?;
        std::io::copy(&mut entry, &mut out_file)
            .map_err(|e| format!("не удалось распаковать '{file_name}': {e}"))?;

        if file_name.eq_ignore_ascii_case("VBCABLE_Setup_x64.exe") {
            setup_path = Some(out_path);
        }
    }

    setup_path.ok_or_else(|| {
        "в архиве драйвера не найден VBCABLE_Setup_x64.exe (изменился формат пака у VB-Audio?)"
            .to_string()
    })
}

/// Запустить установщик с UAC-повышением (verb "runas") и дождаться его
/// завершения.
fn run_elevated(exe_path: &Path, parameters: &str) -> Result<(), String> {
    let verb = to_wide("runas");
    let file = to_wide(&exe_path.to_string_lossy());
    let params = to_wide(parameters);
    let dir = exe_path.parent().map(|p| to_wide(&p.to_string_lossy()));

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: std::ptr::null_mut(),
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: params.as_ptr(),
        lpDirectory: dir.as_ref().map(|d| d.as_ptr()).unwrap_or(std::ptr::null()),
        nShow: SW_HIDE as i32,
        hInstApp: std::ptr::null_mut(),
        lpIDList: std::ptr::null_mut(),
        lpClass: std::ptr::null(),
        hkeyClass: std::ptr::null_mut(),
        dwHotKey: 0,
        Anonymous: SHELLEXECUTEINFOW_0 {
            hIcon: std::ptr::null_mut(),
        },
        hProcess: std::ptr::null_mut(),
    };

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        return Err("не удалось запустить установщик (запрос UAC отклонён?)".to_string());
    }
    if info.hProcess.is_null() {
        // Успех без хэндла процесса — редко, но не ошибка.
        return Ok(());
    }

    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut exit_code: u32 = 0;
        GetExitCodeProcess(info.hProcess, &mut exit_code);
        CloseHandle(info.hProcess);
        if exit_code != 0 {
            return Err(format!("установщик завершился с кодом {exit_code}"));
        }
    }
    Ok(())
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

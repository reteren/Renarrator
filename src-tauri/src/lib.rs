//! Renarrator — фоновое приложение: триггер-звуки по последовательности клавиш.
//!
//! Сборка всех фаз:
//! ```text
//! Keyboard Hook (Phase 2) → Buffer Manager (Phase 3) → Audio Engine (Phase 4)
//! Config (Phase 5) ↔ Tauri Commands (Phase 6) ↔ Web UI
//! System Tray (Phase 1) + Autostart (Phase 6)
//! ```

mod audio_engine;
mod autostart;
mod buffer_manager;
mod config;
mod keyboard_hook;
mod layout_map;
mod virtual_mic_setup;
mod win_glass;

use audio_engine::{AudioCommand, AudioEngineHandle};
use buffer_manager::{BufferManager, DEFAULT_TIMEOUT};
use config::{AppConfig, TriggerRule};
use keyboard_hook::EngineMessage;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, State, WebviewUrl, WebviewWindowBuilder,
    WindowEvent,
};

/// Общее состояние приложения (Tauri-managed).
pub struct AppState {
    config: Mutex<AppConfig>,
    config_path: PathBuf,
    /// Флаг паузы разделяется с потоком хука клавиатуры.
    paused: Arc<AtomicBool>,
    engine_tx: std_mpsc::Sender<EngineMessage>,
    audio: AudioEngineHandle,
}

// ---------------------------------------------------------------------------
// Tauri Commands (Phase 6)
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_config(state: State<'_, Arc<AppState>>) -> AppConfig {
    state.config.lock().expect("config mutex poisoned").clone()
}

#[tauri::command]
fn get_state(state: State<'_, Arc<AppState>>) -> serde_json::Value {
    serde_json::json!({
        "paused": state.paused.load(Ordering::Relaxed),
        "autostart_enabled": autostart::is_enabled(),
    })
}

/// Остановить всё текущее воспроизведение.
#[tauri::command]
fn stop_all_sounds(state: State<'_, Arc<AppState>>) {
    state.audio.send(AudioCommand::StopAll);
}

/// Список имён доступных устройств вывода — фронт использует это, чтобы
/// самостоятельно определить, установлен ли уже виртуальный кабель
/// (никакого ручного выбора устройства в UI больше нет).
#[tauri::command]
fn list_output_devices() -> Vec<String> {
    audio_engine::list_output_device_names()
}

/// Скачать и установить виртуальный аудио-кабель (Soundpad-режим «из коробки»).
/// Единственный сетевой запрос во всём приложении — выполняется автоматически,
/// но только в момент, когда пользователь впервые включает «Play into
/// microphone» на каком-то триггере (см. main.js), не при обычном запуске.
/// Требует подтверждения UAC (повышение прав для установки драйвера) — этот
/// диалог показывает сама Windows, отклонить/принять его может только
/// пользователь за экраном; программно нажать «Да» невозможно и не нужно.
#[tauri::command]
fn setup_virtual_mic() -> Result<(), String> {
    virtual_mic_setup::download_and_install_vb_cable()
}

/// Показать главное окно настроек (из меню трея / по клику на иконку).
#[tauri::command]
fn show_settings(app: AppHandle) {
    show_settings_window(&app);
}

/// Скрыть главное окно (кастомная кнопка закрытия в титлбаре).
#[tauri::command]
fn hide_main_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

/// Скрыть окно меню трея (после клика по пункту, Esc или потери фокуса).
#[tauri::command]
fn hide_tray_menu(app: AppHandle) {
    if let Some(w) = app.get_webview_window("tray-menu") {
        let _ = w.hide();
    }
}

/// Полный выход из приложения.
#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Сохранить конфиг: валидация → запись на диск → автозагрузка → hot-reload.
/// Возвращает список мягких предупреждений (missing files и т.п.).
#[tauri::command]
fn save_config(state: State<'_, Arc<AppState>>, config: AppConfig) -> Result<Vec<String>, String> {
    let mut warnings = config::validate(&config).map_err(|e| e.to_string())?;
    config::save_to(&config, &state.config_path).map_err(|e| e.to_string())?;

    if cfg!(debug_assertions) {
        // В dev-режиме реестр не трогаем: иначе в Run попадёт путь к debug-exe.
    } else if let Err(e) = autostart::set_enabled(config.auto_start) {
        warnings.push(format!("автозагрузка: {e}"));
    }

    *state.config.lock().expect("config mutex poisoned") = config.clone();
    // Mic-маршрут мог измениться — сообщаем аудио-движку (no-op, если имя то же).
    state.audio.send(AudioCommand::SetMicRouting {
        output_device: config.mic_output_device.clone(),
    });
    let _ = state.engine_tx.send(EngineMessage::Reload {
        triggers: config.triggers.clone(),
        master_volume: config.master_volume,
        allow_overlap: config.allow_overlap,
    });
    Ok(warnings)
}

/// Предпрослушка звука из UI (итоговая громкость = volume * master_volume).
#[tauri::command]
fn test_sound(state: State<'_, Arc<AppState>>, path: String, volume: f32) -> Result<(), String> {
    let master = state
        .config
        .lock()
        .expect("config mutex poisoned")
        .master_volume;
    state.audio.send(AudioCommand::TestSound {
        path,
        volume,
        master_volume: master,
    });
    Ok(())
}

/// Пауза из любого окна: флаг для хука + broadcast события всем окнам.
#[tauri::command]
fn toggle_pause(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    paused: bool,
) -> Result<(), String> {
    state.paused.store(paused, Ordering::Relaxed);
    let _ = app.emit("pause-changed", paused);
    Ok(())
}

// ---------------------------------------------------------------------------
// Поток-движок: Key → BufferManager → TriggerMatched → AudioCommand
// ---------------------------------------------------------------------------

fn trigger_words(triggers: &[TriggerRule]) -> Vec<(String, Vec<String>)> {
    triggers
        .iter()
        .map(|t| (t.id.clone(), t.words.clone()))
        .collect()
}

fn spawn_engine_thread(
    rx: std_mpsc::Receiver<EngineMessage>,
    audio: AudioEngineHandle,
    initial: AppConfig,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut matcher = BufferManager::new(DEFAULT_TIMEOUT);
        let mut triggers: Vec<TriggerRule> = initial.triggers.clone();
        let mut master_volume = initial.master_volume;
        let mut allow_overlap = initial.allow_overlap;
        matcher.set_triggers(&trigger_words(&triggers));
        eprintln!("[engine] processor started ({} triggers)", triggers.len());

        while let Ok(msg) = rx.recv() {
            match msg {
                EngineMessage::Key(mapped) => {
                    let hit = matcher.handle_key(&mapped, std::time::Instant::now());
                    if let Some(trigger_id) = hit {
                        if let Some(trg) = triggers.iter().find(|t| t.id == trigger_id) {
                            eprintln!("[engine] matched '{}' ({})", trg.name, trg.id);
                            audio.send(AudioCommand::PlayTrigger {
                                sounds: trg.sounds.clone(),
                                master_volume,
                                allow_overlap,
                                play_to_mic: trg.play_to_mic,
                                play_for_self: trg.play_for_self,
                            });
                        }
                    }
                }
                EngineMessage::Reload {
                    triggers: t,
                    master_volume: mv,
                    allow_overlap: ov,
                } => {
                    eprintln!("[engine] config reloaded ({} triggers)", t.len());
                    matcher.set_triggers(&trigger_words(&t));
                    triggers = t;
                    master_volume = mv;
                    allow_overlap = ov;
                }
            }
        }
    })
}

/// Показать (и сфокусировать) окно настроек.
fn show_settings_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Показать кастомное меню трея у курсора (с клампом к рабочей области монитора).
fn show_tray_menu_at(app: &AppHandle, cursor: PhysicalPosition<f64>) {
    const MENU_W: f64 = 232.0;
    const MENU_H: f64 = 164.0;
    let Some(menu) = app.get_webview_window("tray-menu") else {
        return;
    };
    // По умолчанию — выше курсора (таскбар обычно снизу).
    let mut x = cursor.x - 12.0;
    let mut y = cursor.y - MENU_H - 12.0;
    if let Ok(Some(mon)) = app.monitor_from_point(cursor.x, cursor.y) {
        let area = mon.work_area();
        let (ax, ay) = (area.position.x as f64, area.position.y as f64);
        let (aw, ah) = (area.size.width as f64, area.size.height as f64);
        x = x.clamp(ax + 4.0, ax + aw - MENU_W - 4.0);
        if y < ay + 4.0 {
            y = cursor.y + 12.0; // курсор у верхней кромки — открыть под ним
        }
        y = y.clamp(ay + 4.0, ay + ah - MENU_H - 4.0);
    }
    let _ = menu.set_position(PhysicalPosition::new(x, y));
    let _ = menu.show();
    let _ = menu.set_focus();
}

pub fn run() {
    // ---------- Состояние готовим ДО создания приложения ----------
    // Builder::manage регистрирует state до создания окон — так webview
    // не может обогнать setup() и получить "state not managed" (гонка).
    let (cfg, warnings, cfg_path) = config::load_or_create();
    for w in &warnings {
        eprintln!("[config] {w}");
    }

    // ---------- Потоки: хук → движок → аудио (Phases 2-4) ----------
    let (engine_tx, engine_rx) = std_mpsc::channel::<EngineMessage>();
    let (audio, _audio_thread) = audio_engine::start_audio_engine();
    // Начальная настройка mic-микшера из конфига (до переноса `audio` в AppState).
    audio.send(AudioCommand::SetMicRouting {
        output_device: cfg.mic_output_device.clone(),
    });
    let paused = Arc::new(AtomicBool::new(false));
    let _engine_thread = spawn_engine_thread(engine_rx, audio.clone(), cfg.clone());
    let _hook_thread = keyboard_hook::start_keyboard_hook(engine_tx.clone(), Arc::clone(&paused));

    // ---------- Автозагрузка: синхронизация реестра с конфигом ----------
    if !cfg!(debug_assertions) {
        if let Err(e) = autostart::set_enabled(cfg.auto_start) {
            eprintln!("[autostart] {e}");
        }
    }

    let state = Arc::new(AppState {
        config: Mutex::new(cfg),
        config_path: cfg_path,
        paused,
        engine_tx,
        audio,
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .setup(|app| {

            // ---------- Окна: создаём в коде с жёстким decorations(false) ----------
            // Декларативные окна из tauri.conf.json иногда «теряют» decorations
            // и над ними прорисовывается нативный титлбар Windows. Создание через
            // builder + явное снятие WS_CAPTION делает кастомный титлбар надёжным.
            let main_win = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Renarrator — Settings")
                .inner_size(940.0, 680.0)
                .visible(false)
                .resizable(false)
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .drag_and_drop(true)
                .build()?;
            win_glass::strip_native_chrome(&main_win);

            let menu_win = WebviewWindowBuilder::new(app, "tray-menu", WebviewUrl::App("tray-menu.html".into()))
                .title("Renarrator Menu")
                .inner_size(232.0, 164.0)
                .visible(false)
                .resizable(false)
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .focused(false)
                .build()?;
            win_glass::strip_native_chrome(&menu_win);

            // ---------- Системный трей: ЛКМ → настройки, ПКМ → кастомное меню ----------
            TrayIconBuilder::with_id("tray")
                .icon(
                    app.default_window_icon()
                        .expect("default window icon is missing")
                        .clone(),
                )
                .tooltip("Renarrator")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button,
                        button_state: MouseButtonState::Up,
                        position,
                        ..
                    } = event
                    {
                        match button {
                            MouseButton::Left => show_settings_window(tray.app_handle()),
                            MouseButton::Right => {
                                show_tray_menu_at(tray.app_handle(), position)
                            }
                            _ => {}
                        }
                    }
                })
                .build(app)?;

            // ---------- «Крестик» сворачивает окно в трей ----------
            if let Some(window) = app.get_webview_window("main") {
                let w = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            // ---------- Меню трея исчезает при потере фокуса ----------
            if let Some(menu_win) = app.get_webview_window("tray-menu") {
                let mw = menu_win.clone();
                menu_win.on_window_event(move |event| {
                    if let WindowEvent::Focused(false) = event {
                        let _ = mw.hide();
                    }
                });
            }

            // ---------- Liquid Glass: акриловый блюр + стойкий скруглённый регион ----------
            // Регион пере-применяем на Moved/Resized/ScaleFactorChanged — DWM может
            // его сбросить, и тогда по углам снова проступают острые квадратные уголки.
            if let Some(main_win) = app.get_webview_window("main") {
                win_glass::apply_rounded_blur(&main_win, 22.0);
                let w = main_win.clone();
                main_win.on_window_event(move |event| {
                    if matches!(
                        event,
                        WindowEvent::Moved(_)
                            | WindowEvent::Resized(_)
                            | WindowEvent::ScaleFactorChanged { .. }
                    ) {
                        let _ = win_glass::apply_region(&w, 22.0);
                    }
                });
            }
            if let Some(menu_win) = app.get_webview_window("tray-menu") {
                win_glass::apply_rounded_blur(&menu_win, 16.0);
                let w = menu_win.clone();
                menu_win.on_window_event(move |event| {
                    if matches!(
                        event,
                        WindowEvent::Moved(_)
                            | WindowEvent::Resized(_)
                            | WindowEvent::ScaleFactorChanged { .. }
                    ) {
                        let _ = win_glass::apply_region(&w, 16.0);
                    }
                });
            }

            // Отладочный флаг: `renarrator.exe --show` — показать окно сразу
            // (для визуальной проверки/скриншотов без клика по трею).
            if std::env::args().any(|a| a == "--show") {
                show_settings_window(app.handle());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_state,
            save_config,
            test_sound,
            toggle_pause,
            stop_all_sounds,
            list_output_devices,
            setup_virtual_mic,
            show_settings,
            hide_main_window,
            hide_tray_menu,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running Renarrator");
}

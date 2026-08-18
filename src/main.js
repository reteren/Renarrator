// Renarrator — окно настроек (vanilla JS через window.__TAURI__, без сборщика).
"use strict";

const { invoke } = window.__TAURI__.core;
const { open: openDialog } = window.__TAURI__.dialog;
const { listen } = window.__TAURI__.event;

const $ = (sel) => document.querySelector(sel);

// Страховка от гонки запуска: если state ещё не зарегистрирован
// (ранний invoke из свежего webview) — повторяем с паузой.
async function invokeRetry(cmd, args, attempts = 8, delayMs = 250) {
  let lastErr;
  for (let i = 0; i < attempts; i++) {
    try {
      return await invoke(cmd, args);
    } catch (e) {
      lastErr = e;
      if (!String(e).includes("not managed")) throw e;
      await new Promise((r) => setTimeout(r, delayMs));
    }
  }
  throw lastErr;
}

let config = { master_volume: 0.8, allow_overlap: true, auto_start: false, mic_output_device: null, triggers: [] };
let paused = false;

function uid() {
  return "t_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
}

function clamp(v, lo, hi) {
  return Math.min(hi, Math.max(lo, v));
}

// Из полного пути берём только имя файла (C:\a\b\c.mp3 → c.mp3).
function basename(p) {
  if (!p) return "";
  const norm = String(p).replace(/\//g, "\\");
  return norm.split("\\").pop() || norm;
}

// ---------- Рендер общих настроек ----------

function renderGlobal() {
  const mv = $("#master-volume");
  mv.value = Math.round(config.master_volume * 100);
  $("#master-volume-label").textContent = mv.value + "%";
  $("#allow-overlap").checked = config.allow_overlap;
  $("#auto-start").checked = config.auto_start;
}

// ---------- Микрофон-микширование (Soundpad-style, полностью автоматически) ----------
//
// Никакого ручного выбора устройств в UI: микрофон всегда системный по
// умолчанию (решает бэкенд), а виртуальный кабель для вывода либо находится
// среди уже установленных устройств, либо ставится сам по требованию — в тот
// момент, когда пользователь впервые включает "Play into microphone" на
// каком-то триггере (см. wiring в buildTriggerCard ниже).

// Общеизвестные имена виртуальных аудио-кабелей (VB-Audio, VoiceMeeter, ...),
// в порядке ОТ САМОГО ТОЧНОГО к самому общему. Важно проверять паттерны по
// очереди ПО ВСЕМ устройствам, а не устройство за устройством: у некоторых
// пользователей VB-CABLE ставит сразу два варианта кабеля — классическую
// пару "CABLE Input"/"CABLE Output" (у которой обязательно есть парная сторона
// для записи) и дополнительный "CABLE In 16ch" (который в некоторых сборках
// драйвера НЕ имеет парного устройства записи — Discord никогда не увидит его
// как микрофон). Общий паттерн "vb-audio" совпадает с обоими, поэтому если
// проверять устройство за устройством, можно случайно выбрать нерабочий
// вариант просто из-за порядка перечисления. Проверка паттерн-за-паттерном
// гарантирует, что точный "cable input" выигрывает у общего "vb-audio".
const VIRTUAL_CABLE_PATTERNS = ["cable input", "voicemeeter", "virtual audio cable", "vb-audio"];

function findVirtualCableName(names) {
  for (const pattern of VIRTUAL_CABLE_PATTERNS) {
    const match = names.find((n) => n.toLowerCase().includes(pattern));
    if (match) return match;
  }
  return undefined;
}

let micSetupInProgress = false;

// Гарантирует, что config.mic_output_device указывает на реальный
// установленный виртуальный кабель — при необходимости ставит его сам
// (сетевой запрос + UAC-повышение, см. src-tauri/src/virtual_mic_setup.rs).
// Ничего не делает, если уже настроено — безопасно звать многократно.
async function ensureMicRoutingReady() {
  if (config.mic_output_device || micSetupInProgress) return;
  micSetupInProgress = true;
  try {
    let names = [];
    try {
      names = await invoke("list_output_devices");
    } catch (e) {
      setStatus("Could not check for a virtual microphone: " + e, "err");
      return;
    }
    const existing = findVirtualCableName(names);
    if (existing) {
      config.mic_output_device = existing;
      return;
    }
    setStatus("Setting up virtual microphone — approve the Windows permission prompt…", "");
    try {
      await invoke("setup_virtual_mic");
    } catch (e) {
      setStatus("Virtual microphone setup failed: " + e, "err");
      return;
    }
    const namesAfter = await invoke("list_output_devices").catch(() => []);
    const installed = findVirtualCableName(namesAfter);
    if (installed) {
      config.mic_output_device = installed;
      setStatus("Virtual microphone ready — pick it as your mic in Discord/your game", "ok");
    } else {
      setStatus(
        "Installed, but couldn't detect the new device yet — try the checkbox again",
        "err"
      );
    }
  } finally {
    micSetupInProgress = false;
  }
}

// ---------- Рендер триггеров ----------

function renderTriggers() {
  const host = $("#triggers");
  host.innerHTML = "";
  for (const trg of config.triggers) {
    host.appendChild(buildTriggerCard(trg));
  }
}

function buildTriggerCard(trg) {
  const node = $("#trigger-template").content.firstElementChild.cloneNode(true);

  const nameInput = node.querySelector(".t-name");
  nameInput.value = trg.name;
  nameInput.addEventListener("input", () => (trg.name = nameInput.value));

  const wordsInput = node.querySelector(".t-words");
  wordsInput.value = trg.words.join(", ");
  wordsInput.addEventListener("input", () => {
    trg.words = wordsInput.value
      .split(",")
      .map((w) => w.trim())
      .filter(Boolean);
  });

  const micInput = node.querySelector(".t-mic");
  micInput.checked = Boolean(trg.play_to_mic);
  micInput.addEventListener("change", () => {
    trg.play_to_mic = micInput.checked;
    // Включили впервые — сам ставит/находит виртуальный кабель, без выбора вручную.
    if (micInput.checked) ensureMicRoutingReady();
  });

  // По умолчанию включено (undefined/отсутствует в старом конфиге тоже считается "включено").
  const playForSelfInput = node.querySelector(".t-play-for-self");
  playForSelfInput.checked = trg.play_for_self !== false;
  playForSelfInput.addEventListener(
    "change",
    () => (trg.play_for_self = playForSelfInput.checked)
  );

  node.querySelector(".t-delete").addEventListener("click", () => {
    config.triggers = config.triggers.filter((t) => t !== trg);
    renderTriggers();
  });

  const tbody = node.querySelector(".t-sounds");
  const renderSounds = () => {
    tbody.innerHTML = "";
    // Колонка Weight имеет смысл только когда звуков больше одного.
    const showWeight = trg.sounds.length > 1;
    node.classList.toggle("single-sound", !showWeight);
    for (const snd of trg.sounds) {
      tbody.appendChild(buildSoundRow(snd, trg, renderSounds));
    }
  };

  node.querySelector(".t-add-sound").addEventListener("click", () => {
    trg.sounds.push({ path: "", volume: 1.0, weight: 50 });
    renderSounds();
  });

  renderSounds();
  return node;
}

function buildSoundRow(snd, trg, rerender) {
  const row = $("#sound-template").content.firstElementChild.cloneNode(true);

  // Показываем только имя файла; полный путь — в подсказке (title).
  const pathInput = row.querySelector(".s-path");
  const syncPathView = () => {
    pathInput.value = basename(snd.path);
    pathInput.title = snd.path || "";
    row.classList.toggle("has-file", Boolean(snd.path));
  };
  syncPathView();

  const setPath = (p) => {
    if (!p) return;
    snd.path = p;
    syncPathView();
  };

  const volInput = row.querySelector(".s-volume");
  volInput.value = Math.round(snd.volume * 100);
  volInput.addEventListener("input", () => {
    snd.volume = clamp((Number(volInput.value) || 0) / 100, 0, 1);
  });

  const weightInput = row.querySelector(".s-weight");
  weightInput.value = snd.weight;
  weightInput.addEventListener("input", () => {
    snd.weight = Math.max(0, Math.round(Number(weightInput.value) || 0));
  });

  row.querySelector(".s-browse").addEventListener("click", async () => {
    const picked = await openDialog({
      multiple: false,
      filters: [{ name: "Audio", extensions: ["mp3", "wav", "ogg"] }],
    });
    if (picked) setPath(picked);
  });

  // Drag-and-drop: визуальный отклик при наведении файла на строку.
  // Само событие дропа ловится глобально (tauri://drag-drop) в init() —
  // через dataset.rowId находим целевую строку и вызываем её setPath.
  row.dataset.dropSound = "1";
  row._setPath = setPath; // ссылка для глобального обработчика дропа
  row.addEventListener("dragover", (e) => e.preventDefault());
  row.addEventListener("dragenter", () => row.classList.add("drop-target"));
  row.addEventListener("dragleave", () => row.classList.remove("drop-target"));

  row.querySelector(".s-test").addEventListener("click", () => {
    if (!snd.path) {
      setStatus("Select a sound file first", "err");
      return;
    }
    invoke("test_sound", { path: snd.path, volume: snd.volume }).catch((e) =>
      setStatus("Playback error: " + e, "err")
    );
  });

  row.querySelector(".s-delete").addEventListener("click", () => {
    trg.sounds = trg.sounds.filter((s) => s !== snd);
    rerender();
  });

  return row;
}

// ---------- Статусная строка ----------

let statusTimer = null;
function setStatus(text, kind = "") {
  const el = $("#status");
  el.textContent = text;
  el.className = "status " + kind;
  clearTimeout(statusTimer);
  if (kind !== "err") {
    statusTimer = setTimeout(() => (el.textContent = ""), 6000);
  }
}

// ---------- Пауза ----------

function renderPause() {
  const btn = $("#pause-btn");
  btn.classList.toggle("active", paused);
  btn.textContent = paused ? "Resume Detection" : "Pause Detection";
}

// ---------- Инициализация ----------

async function init() {
  config = await invokeRetry("get_config");
  const st = await invokeRetry("get_state");
  paused = st.paused;

  renderGlobal();
  renderTriggers();
  renderPause();

  // Самовосстановление: если какой-то триггер уже хочет play_to_mic (например,
  // из старого конфига), но маршрут не настроен (mic_output_device пуст) —
  // не ждём, пока пользователь заново дёрнет чекбокс, чиним сразу при открытии.
  // И сразу сохраняем — это фоновый self-heal, а не ручное действие
  // пользователя, ждать отдельного клика по Save тут не нужно.
  if (!config.mic_output_device && config.triggers.some((t) => t.play_to_mic)) {
    ensureMicRoutingReady().then(() => {
      if (config.mic_output_device) {
        invoke("save_config", { config }).catch(() => {});
      }
    });
  }

  $("#master-volume").addEventListener("input", (e) => {
    config.master_volume = clamp(Number(e.target.value) / 100, 0, 1);
    $("#master-volume-label").textContent = e.target.value + "%";
  });
  $("#allow-overlap").addEventListener(
    "change",
    (e) => (config.allow_overlap = e.target.checked)
  );
  $("#auto-start").addEventListener(
    "change",
    (e) => (config.auto_start = e.target.checked)
  );

  $("#add-trigger").addEventListener("click", () => {
    config.triggers.push({ id: uid(), name: "New Trigger", words: [], sounds: [], play_to_mic: false, play_for_self: true });
    renderTriggers();
  });

  $("#save").addEventListener("click", async () => {
    try {
      const warnings = await invoke("save_config", { config });
      if (warnings && warnings.length) {
        setStatus("Saved with warnings:\n• " + warnings.join("\n• "), "err");
      } else {
        setStatus("Saved", "ok");
      }
    } catch (e) {
      setStatus("Save failed: " + e, "err");
    }
  });

  $("#win-close").addEventListener("click", () => {
    invoke("hide_main_window").catch(() => {});
  });

  $("#pause-btn").addEventListener("click", async () => {
    paused = !paused;
    await invoke("toggle_pause", { paused });
    renderPause();
  });

  // Пауза, переключённая из трея.
  await listen("pause-changed", (e) => {
    paused = e.payload;
    renderPause();
  });

  // ---------- Drag-and-drop аудиофайлов ----------
  // Tauri эмитит tauri://drag-drop с реальными путями ОС (нужны для rodio).
  // Кладём файл в строку звука под курсором; если ни одна строка не под
  // курсором — добавляем новый звук в первый триггер (удобный fallback).
  await listen("tauri://drag-drop", (e) => {
    const paths = (e.payload && e.payload.paths) || [];
    if (!paths.length) return;
    const audio = paths.filter((p) => /\.(mp3|wav|ogg)$/i.test(p));
    if (!audio.length) {
      setStatus("Only .mp3 / .wav / .ogg are supported", "err");
      return;
    }
    // Снимаем подсветку со всех строк.
    document
      .querySelectorAll(".sound.drop-target")
      .forEach((r) => r.classList.remove("drop-target"));
    // Цель — строка под курсором, иначе новая строка в первом триггере.
    const hovered = document.querySelector(".sound:hover");
    let target = hovered && hovered._setPath ? hovered : null;
    if (!target) {
      if (!config.triggers.length) {
        config.triggers.push({ id: uid(), name: "New Trigger", words: [], sounds: [], play_to_mic: false, play_for_self: true });
        renderTriggers();
      }
      const trg = config.triggers[0];
      for (const p of audio) trg.sounds.push({ path: p, volume: 1.0, weight: 50 });
      renderTriggers();
      setStatus("Added " + audio.length + " sound(s)", "ok");
      return;
    }
    target._setPath(audio[0]);
    if (audio.length > 1) {
      setStatus("Dropped 1 file (one file per sound row)", "");
    }
  });
}

init().catch((e) => setStatus("Initialization error: " + e, "err"));


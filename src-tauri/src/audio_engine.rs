//! Phase 4: аудио-движок на rodio 0.22.
//!
//! Архитектура: выделенный поток владеет устройством вывода (`MixerDeviceSink`)
//! и списком активных `Player`ов; команды приходят по `std::sync::mpsc`.
//! Так обходим Send-ограничения аудио-дескрипторов и получаем
//! предсказуемую точку синхронизации всех воспроизведений.
//!
//! * Полифония: `allow_overlap = true` → новые звуки накладываются на текущие;
//!   `false` → всё играющее останавливается перед новым звуком.
//! * Громкость итоговая = `master_volume * sound_volume` (каждая 0.0..=1.0).
//! * Взвешенный случайный выбор: P(i) = weight_i / Σweights (нормализация).
//! * Mic-микшер (Soundpad-режим): на устройстве вывода `mic_output_device`
//!   (обычно "CABLE Input" виртуального кабеля VB-Audio) живёт отдельный
//!   `MixerDeviceSink`, к микшеру которого подключены ДВА вида источников:
//!   1) непрерывный `MicPassthroughSource` — бесконечный поток сэмплов
//!      с реального микрофона (`mic_input_device`), захватываемых через
//!      cpal-входной поток в кольцевой буфер (тишина, если буфер пуст);
//!   2) одноразовые `Player`ы звуков триггеров с `play_to_mic`.
//!   rodio-микшер суммирует их в реальном времени, и приложение, выбравшее
//!   "CABLE Output" как свой микрофон, слышит голос + триггер-звуки вместе.

use crate::config::SoundOption;
use rand::Rng;
use rodio::cpal;
use rodio::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rodio::{ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source};
use std::collections::VecDeque;
use std::fs::File;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// Команды аудио-движку.
#[derive(Debug)]
pub enum AudioCommand {
    /// Сработал триггер: выбрать один звук по весам и проиграть.
    PlayTrigger {
        sounds: Vec<SoundOption>,
        master_volume: f32,
        allow_overlap: bool,
        /// Дополнительно микшировать выбранный звук в живой mic-поток
        /// (слышат люди в звонке).
        play_to_mic: bool,
        /// Проигрывать локально (слышит сам пользователь). Выключено —
        /// звук уходит только в mic-поток, если play_to_mic тоже включён.
        play_for_self: bool,
    },
    /// Кнопка «Тест» в UI: проиграть конкретный файл с заданной громкостью.
    TestSound {
        path: String,
        volume: f32,
        master_volume: f32,
    },
    /// Остановить всё воспроизведение.
    StopAll,
    /// (Пере)настроить mic-микшер: реальный микрофон для захвата и устройство
    /// вывода (виртуальный кабель), куда рендерится смешанный поток.
    SetMicRouting {
        input_device: Option<String>,
        output_device: Option<String>,
    },
}

/// Публичная ручка движка: клонируемый Sender + поток.
#[derive(Clone)]
pub struct AudioEngineHandle {
    tx: Sender<AudioCommand>,
}

impl AudioEngineHandle {
    pub fn send(&self, cmd: AudioCommand) {
        // Если поток умер (нет аудиоустройства) — просто логируем, не падаем.
        if self.tx.send(cmd).is_err() {
            eprintln!("[audio] engine thread is not running");
        }
    }
}

/// Запускает аудио-поток и возвращает ручку для команд.
pub fn start_audio_engine() -> (AudioEngineHandle, JoinHandle<()>) {
    let (tx, rx) = std::sync::mpsc::channel::<AudioCommand>();
    let thread = thread::spawn(move || engine_loop(rx));
    (AudioEngineHandle { tx }, thread)
}

/// Человекочитаемое имя устройства для UI и для поиска по имени.
///
/// `DeviceDescription::name()` в WASAPI-бэкенде cpal — это общее системное
/// имя РОЛИ устройства ("Microphone", "Speakers", "Headphones"), а не то,
/// что пользователь видит в настройках звука Windows: несколько разных
/// физических микрофонов/колонок могут все называться просто "Microphone".
/// Настоящее пользовательское имя (например, "Microphone (HyperX QuadCast)")
/// cpal кладёт в `extended()`, только когда оно отличается от родовой роли —
/// поэтому предпочитаем первую строку `extended()`, а на родовое имя
/// откатываемся, только если расширенной информации нет.
fn device_display_name(desc: &cpal::DeviceDescription) -> String {
    desc.extended()
        .first()
        .cloned()
        .unwrap_or_else(|| desc.name().to_string())
}

/// Список имён доступных устройств вывода — для UI (Tauri command `list_output_devices`).
pub fn list_output_device_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => devices
            .filter_map(|d| d.description().ok())
            .map(|desc| device_display_name(&desc))
            .collect(),
        Err(e) => {
            eprintln!("[audio] cannot enumerate output devices: {e}");
            Vec::new()
        }
    }
}

/// Список имён доступных устройств ввода (микрофонов) — для UI (Tauri command `list_input_devices`).
pub fn list_input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices
            .filter_map(|d| d.description().ok())
            .map(|desc| device_display_name(&desc))
            .collect(),
        Err(e) => {
            eprintln!("[audio] cannot enumerate input devices: {e}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Mic-микшер: кольцевой буфер + бесконечный Source реального микрофона
// ---------------------------------------------------------------------------

/// Кольцевой буфер сэмплов микрофона. Пишет колбэк cpal-потока (поток WASAPI),
/// читает плеер rodio (поток вывода). Ограничен сверху, чтобы не расти
/// бесконечно, если вывод временно отстал.
struct MicRingBuffer {
    queue: Mutex<VecDeque<f32>>,
}

impl MicRingBuffer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
        })
    }

    fn push_samples(&self, samples: &[f32]) {
        const MAX_BUFFERED_SAMPLES: usize = 96_000; // ~1с @ 48kHz стерео — щедрый запас
        let mut q = self.queue.lock().unwrap();
        q.extend(samples.iter().copied());
        while q.len() > MAX_BUFFERED_SAMPLES {
            q.pop_front();
        }
    }

    fn pop_sample(&self) -> f32 {
        // Пустой буфер → тишина (входной поток ещё не выдал данные).
        self.queue.lock().unwrap().pop_front().unwrap_or(0.0)
    }
}

/// Бесконечный `rodio::Source` поверх кольцевого буфера микрофона.
/// `next()` НИКОГДА не возвращает `None` — так микшер не выкидывает этот
/// источник как «доигравший», и mic-канал живёт всё время работы движка.
struct MicPassthroughSource {
    buffer: Arc<MicRingBuffer>,
    channels: ChannelCount,
    sample_rate: SampleRate,
}

impl Iterator for MicPassthroughSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        Some(self.buffer.pop_sample())
    }
}

impl Source for MicPassthroughSource {
    fn current_span_len(&self) -> Option<usize> {
        None // бесконечный источник
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Открыть поток захвата реального микрофона по точному имени и запустить его.
/// Колбэк пишет сэмплы в `buffer`. Отказоустойчиво: при любой ошибке — лог
/// и `None` (микрофон мог быть отключён; приложение продолжает работать).
///
/// ВАЖНО: `cpal::Stream` должен жить только на потоке движка — возвращаем его
/// вызывающему коду в `engine_loop`, не передавая между потоками.
fn open_mic_capture(
    name: &str,
    buffer: Arc<MicRingBuffer>,
) -> Option<(cpal::Stream, ChannelCount, SampleRate)> {
    let host = cpal::default_host();
    let devices = match host.input_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[audio] cannot enumerate input devices: {e}");
            return None;
        }
    };
    let device = devices.into_iter().find(|d| {
        d.description()
            .map(|desc| device_display_name(&desc) == name)
            .unwrap_or(false)
    });
    let device = device?;
    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[audio] cannot get default input config for '{name}': {e}");
            return None;
        }
    };
    // Формат захвата не обязан совпадать с форматом вывода кабеля —
    // rodio-микшер сам сконвертирует каналы/частоту при добавлении источника.
    let channels = ChannelCount::new(config.channels()).unwrap_or(ChannelCount::MIN);
    let sample_rate = SampleRate::new(config.sample_rate()).unwrap_or(SampleRate::MIN);
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();
    let err_fn = |e| eprintln!("[audio] mic capture stream error: {e}");
    let stream_result = match sample_format {
        cpal::SampleFormat::F32 => {
            let b = Arc::clone(&buffer);
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| b.push_samples(data),
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let b = Arc::clone(&buffer);
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let converted: Vec<f32> = data
                        .iter()
                        .map(|s| *s as f32 / i16::MAX as f32)
                        .collect();
                    b.push_samples(&converted);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let b = Arc::clone(&buffer);
            device.build_input_stream(
                &stream_config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let converted: Vec<f32> = data
                        .iter()
                        .map(|s| (*s as f32 - 32768.0) / 32768.0)
                        .collect();
                    b.push_samples(&converted);
                },
                err_fn,
                None,
            )
        }
        other => {
            eprintln!("[audio] unsupported mic capture sample format: {other:?}");
            return None;
        }
    };
    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[audio] cannot open mic capture device '{name}': {e}");
            return None;
        }
    };
    if let Err(e) = stream.play() {
        eprintln!("[audio] cannot start mic capture stream '{name}': {e}");
        return None;
    }
    Some((stream, channels, sample_rate))
}

/// Открыть `MixerDeviceSink` на устройстве с точным совпадением имени.
/// Отказоустойчиво: при любой ошибке — лог и `None`, приложение продолжает работать
/// (виртуальный кабель может быть не установлен или отключён).
fn open_named_output_device(name: &str) -> Option<MixerDeviceSink> {
    let host = cpal::default_host();
    let devices = match host.output_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[audio] cannot enumerate output devices: {e}");
            return None;
        }
    };
    let device = devices.into_iter().find(|d| {
        d.description()
            .map(|desc| device_display_name(&desc) == name)
            .unwrap_or(false)
    });
    match device {
        Some(dev) => match DeviceSinkBuilder::from_device(dev).and_then(|b| b.open_stream()) {
            Ok(sink) => Some(sink),
            Err(e) => {
                eprintln!("[audio] cannot open mic-routing device '{name}': {e}");
                None
            }
        },
        None => {
            eprintln!("[audio] mic-routing device not found: '{name}'");
            None
        }
    }
}

fn engine_loop(rx: Receiver<AudioCommand>) {
    // Устройство вывода открываем один раз на всю жизнь потока.
    let sink_handle = match DeviceSinkBuilder::open_default_sink() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[audio] cannot open default audio device: {e}");
            // Дренируем канал, чтобы отправители не видели обрыв.
            while rx.recv().is_ok() {}
            return;
        }
    };
    eprintln!("[audio] engine started");

    let mut active: Vec<Player> = Vec::new();
    // Mic-микшер (Soundpad-режим): sink на виртуальном кабеле + непрерывный
    // passthrough реального микрофона + одноразовые звуки триггеров —
    // всё в одном rodio-микшере, который суммирует их в реальном времени.
    let mut mic_active: Vec<Player> = Vec::new();
    let mut mic_sink: Option<MixerDeviceSink> = None;
    let mut mic_output_device_name: Option<String> = None;
    let mut mic_input_stream: Option<cpal::Stream> = None;
    let mut mic_input_device_name: Option<String> = None;
    let mut mic_passthrough_player: Option<Player> = None;
    let mut rng = rand::rng();

    while let Ok(cmd) = rx.recv() {
        // Вычищаем доигравшие плееры (Drop у Player останавливает звук).
        // Passthrough-плеер сюда НЕ входит: его Source бесконечен, он живёт
        // в отдельном `Option`, чтобы не быть удалённым при чистке.
        active.retain(|p| !p.empty());
        mic_active.retain(|p| !p.empty());
        match cmd {
            AudioCommand::PlayTrigger {
                sounds,
                master_volume,
                allow_overlap,
                play_to_mic,
                play_for_self,
            } => {
                if let Some(sound) = pick_weighted(&sounds, &mut rng) {
                    if play_for_self {
                        if !allow_overlap {
                            stop_all(&mut active);
                        }
                        play_one(&sink_handle, &mut active, &sound.path, sound.volume * master_volume);
                    }
                    if play_to_mic {
                        if let Some(mic) = &mic_sink {
                            if !allow_overlap {
                                stop_all(&mut mic_active);
                            }
                            play_one(
                                mic,
                                &mut mic_active,
                                &sound.path,
                                sound.volume * master_volume,
                            );
                        } else {
                            eprintln!(
                                "[audio] mic routing requested but no mic device configured/open"
                            );
                        }
                    }
                } else {
                    eprintln!("[audio] trigger fired but no playable sounds configured");
                }
            }
            AudioCommand::TestSound {
                path,
                volume,
                master_volume,
            } => {
                // Тест не прерывает текущее воспроизведение — это предпрослушка.
                play_one(&sink_handle, &mut active, &path, volume * master_volume);
            }
            AudioCommand::StopAll => {
                stop_all(&mut active);
                stop_all(&mut mic_active);
            }
            AudioCommand::SetMicRouting {
                input_device,
                output_device,
            } => {
                if input_device != mic_input_device_name || output_device != mic_output_device_name {
                    // Разбираем в порядке зависимостей: passthrough зависит от mic_sink,
                    // поток захвата независим. Drop старого sink останавливает его вывод.
                    drop(mic_passthrough_player.take());
                    drop(mic_input_stream.take());
                    mic_active.clear();
                    mic_sink = output_device.as_deref().and_then(open_named_output_device);

                    if let Some(sink) = &mic_sink {
                        if let Some(in_name) = input_device.as_deref() {
                            let ring = MicRingBuffer::new();
                            if let Some((stream, channels, sample_rate)) =
                                open_mic_capture(in_name, Arc::clone(&ring))
                            {
                                let source = MicPassthroughSource {
                                    buffer: ring,
                                    channels,
                                    sample_rate,
                                };
                                let player = Player::connect_new(sink.mixer());
                                player.append(source);
                                mic_passthrough_player = Some(player);
                                mic_input_stream = Some(stream);
                            }
                            // иначе open_mic_capture уже залогировала причину;
                            // passthrough выключен, но mic_sink пригоден для
                            // маршрутизации одних только звуков триггеров.
                        }
                    }
                    mic_input_device_name = input_device;
                    mic_output_device_name = output_device;
                }
            }
        }
    }
}

fn stop_all(active: &mut Vec<Player>) {
    for p in active.iter() {
        p.stop();
    }
    active.clear();
}

fn play_one(handle: &MixerDeviceSink, active: &mut Vec<Player>, path: &str, volume: f32) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[audio] cannot open '{path}': {e}");
            return;
        }
    };
    match Decoder::try_from(file) {
        Ok(source) => {
            let player = Player::connect_new(handle.mixer());
            player.set_volume(volume.clamp(0.0, 1.0));
            player.append(source);
            active.push(player);
        }
        Err(e) => eprintln!("[audio] cannot decode '{path}': {e}"),
    }
}

/// Взвешенный случайный выбор одного звука (нормализация весов в 100%).
/// Чистая функция с инжектируемым RNG — полностью тестируема.
///
/// Краевые случаи: пустой список → `None`; все веса нулевые → первый звук
/// (детерминированный fallback вместо «ничего не играть»).
pub fn pick_weighted<'a, R: Rng + ?Sized>(
    sounds: &'a [SoundOption],
    rng: &mut R,
) -> Option<&'a SoundOption> {
    if sounds.is_empty() {
        return None;
    }
    let total: u64 = sounds.iter().map(|s| u64::from(s.weight)).sum();
    if total == 0 {
        return sounds.first();
    }
    let mut roll = rng.random_range(0..total);
    for s in sounds {
        let w = u64::from(s.weight);
        if roll < w {
            return Some(s);
        }
        roll -= w;
    }
    sounds.last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    fn snd(path: &str, weight: u32) -> SoundOption {
        SoundOption {
            path: path.to_string(),
            volume: 1.0,
            weight,
        }
    }

    #[test]
    fn empty_list_returns_none() {
        let mut rng = SmallRng::seed_from_u64(1);
        assert!(pick_weighted(&[], &mut rng).is_none());
    }

    #[test]
    fn single_sound_always_picked() {
        let mut rng = SmallRng::seed_from_u64(2);
        let sounds = vec![snd("a.mp3", 5)];
        for _ in 0..50 {
            assert_eq!(pick_weighted(&sounds, &mut rng).unwrap().path, "a.mp3");
        }
    }

    #[test]
    fn zero_weight_never_picked() {
        let mut rng = SmallRng::seed_from_u64(3);
        let sounds = vec![snd("never.mp3", 0), snd("always.mp3", 10)];
        for _ in 0..200 {
            assert_eq!(pick_weighted(&sounds, &mut rng).unwrap().path, "always.mp3");
        }
    }

    #[test]
    fn all_zero_weights_falls_back_to_first() {
        let mut rng = SmallRng::seed_from_u64(4);
        let sounds = vec![snd("a.mp3", 0), snd("b.mp3", 0)];
        assert_eq!(pick_weighted(&sounds, &mut rng).unwrap().path, "a.mp3");
    }

    #[test]
    fn distribution_matches_normalized_weights() {
        // 60 / 30 / 10 → нормализация в 60% / 30% / 10%.
        let mut rng = SmallRng::seed_from_u64(42);
        let sounds = vec![snd("a.mp3", 60), snd("b.mp3", 30), snd("c.mp3", 10)];
        let n = 20_000;
        let mut counts = [0usize; 3];
        for _ in 0..n {
            let picked = pick_weighted(&sounds, &mut rng).unwrap();
            let idx = sounds
                .iter()
                .position(|s| s.path == picked.path)
                .expect("picked sound must come from the list");
            counts[idx] += 1;
        }
        let p = |i: usize| counts[i] as f64 / n as f64;
        assert!((p(0) - 0.60).abs() < 0.03, "p(0)={}", p(0));
        assert!((p(1) - 0.30).abs() < 0.03, "p(1)={}", p(1));
        assert!((p(2) - 0.10).abs() < 0.02, "p(2)={}", p(2));
    }

    #[test]
    fn mic_ring_buffer_returns_silence_when_empty() {
        let ring = MicRingBuffer::new();
        assert_eq!(ring.pop_sample(), 0.0);
    }

    #[test]
    fn mic_ring_buffer_is_fifo_and_capped() {
        let ring = MicRingBuffer::new();
        // 100_001 > MAX_BUFFERED_SAMPLES (96_000) — начало должно быть отброшено.
        let samples: Vec<f32> = (0..=100_000).map(|i| i as f32).collect();
        ring.push_samples(&samples);
        assert_eq!(ring.pop_sample(), 4_001.0);
        assert_eq!(ring.pop_sample(), 4_002.0);
        // После исчерпания буфера — тишина.
        for _ in 0..95_997 {
            ring.pop_sample();
        }
        assert_eq!(ring.pop_sample(), 100_000.0);
        assert_eq!(ring.pop_sample(), 0.0);
    }

    #[test]
    fn mic_passthrough_source_never_ends() {
        let ring = MicRingBuffer::new();
        ring.push_samples(&[0.25, -0.5]);
        let mut source = MicPassthroughSource {
            buffer: Arc::clone(&ring),
            channels: ChannelCount::MIN,
            sample_rate: SampleRate::MIN,
        };
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(source.next(), Some(-0.5));
        for _ in 0..100 {
            assert_eq!(source.next(), Some(0.0)); // тишина, но НЕ None
        }
        assert_eq!(source.channels().get(), 1);
        assert_eq!(source.sample_rate().get(), 1);
        assert_eq!(source.current_span_len(), None);
        assert_eq!(source.total_duration(), None);
    }
}

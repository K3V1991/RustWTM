//! The last 24 hours of each container, a minute at a time: CPU and memory
//! averages and peaks, kept by Docker context and container name so a
//! restarted or re-created container goes on with its line.
//!
//! Only readings RustWTM already makes are recorded: while a page shows
//! Docker, or while container alerts are on. Minutes without a reading stay
//! missing; nothing is filled in. The file is written by the Docker reader,
//! never the window, and only once Docker has been read.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use wtm_core::docker::Report;

/// Minutes kept per container.
pub const KEEP_MINUTES: i64 = 24 * 60;
/// Containers kept; the one seen longest ago goes first.
pub const NAMES_KEPT: usize = 64;
/// Time between saves while something changed.
pub const SAVE_EVERY: Duration = Duration::from_secs(5 * 60);
/// A larger file is not read.
const FILE_BYTES: u64 = 8 * 1024 * 1024;
const VERSION: u32 = 1;

/// `%LOCALAPPDATA%\RustWTM\container-history.json`; none in tests.
pub fn path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    std::env::var_os("LOCALAPPDATA").map(|d| {
        PathBuf::from(d)
            .join("RustWTM")
            .join("container-history.json")
    })
}

/// One minute of one container.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Minute {
    /// Minutes since 1970, UTC.
    #[serde(rename = "t")]
    pub minute: i64,
    #[serde(rename = "cs", default)]
    cpu_sum: f64,
    #[serde(rename = "cn", default)]
    cpu_count: u32,
    /// Docker's CPU figure (100% per core for Linux containers).
    #[serde(rename = "cp", default)]
    pub cpu_peak: f32,
    #[serde(rename = "ms", default)]
    memory_sum: f64,
    #[serde(rename = "mn", default)]
    memory_count: u32,
    #[serde(rename = "mp", default)]
    pub memory_peak: u64,
}

impl Minute {
    fn new(minute: i64) -> Self {
        Self {
            minute,
            cpu_sum: 0.0,
            cpu_count: 0,
            cpu_peak: 0.0,
            memory_sum: 0.0,
            memory_count: 0,
            memory_peak: 0,
        }
    }
    pub fn cpu(&self) -> Option<(f32, f32)> {
        (self.cpu_count > 0).then(|| {
            (
                (self.cpu_sum / f64::from(self.cpu_count)) as f32,
                self.cpu_peak,
            )
        })
    }
    pub fn memory(&self) -> Option<(f64, u64)> {
        (self.memory_count > 0).then(|| {
            (
                self.memory_sum / f64::from(self.memory_count),
                self.memory_peak,
            )
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Series {
    /// Oldest first, one entry per minute with a reading.
    pub minutes: VecDeque<Minute>,
    /// Docker counts this container's CPU per core.
    pub per_core: bool,
    /// The last measurement recorded, so a repeated one is not counted again.
    last: Option<Instant>,
    /// The last minute it was seen, to drop the oldest containers first.
    seen: i64,
}

#[derive(Debug, Clone, Default)]
pub struct History {
    series: BTreeMap<(String, String), Series>,
    /// Changed since the last save.
    pub dirty: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct File {
    version: u32,
    containers: Vec<Entry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    context: String,
    name: String,
    #[serde(default)]
    per_core: bool,
    minutes: Vec<Minute>,
}

fn unix_minute(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() / 60) as i64,
        Err(_) => 0,
    }
}

impl History {
    pub fn series(&self, context: &str, name: &str) -> Option<&Series> {
        self.series.get(&(context.to_owned(), name.to_owned()))
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    pub fn clear(&mut self) {
        self.series.clear();
        self.dirty = true;
    }

    /// Adds the fresh measurements of running containers; `now` and `wall`
    /// are the same moment on the two clocks.
    pub fn record(&mut self, now: Instant, wall: SystemTime, report: &Report) {
        let context = &report.endpoint.context;
        for c in report.containers.iter().filter(|c| c.state == "running") {
            let m = &c.metrics;
            let Some(measured) = m.measured.filter(|_| m.fresh(now)) else {
                continue;
            };
            if m.cpu.is_none() && m.memory.is_none() {
                continue;
            }
            let key = (context.clone(), c.name.clone());
            if self.series.get(&key).and_then(|s| s.last) == Some(measured) {
                // Docker had no newer sample of it.
                continue;
            }
            let at = wall
                .checked_sub(now.saturating_duration_since(measured))
                .unwrap_or(wall);
            let minute = unix_minute(at);
            let series = self.series.entry(key).or_default();
            series.last = Some(measured);
            series.seen = unix_minute(wall);
            series.per_core = m.cpu_cores.is_some();
            if series.minutes.back().is_none_or(|b| b.minute < minute) {
                series.minutes.push_back(Minute::new(minute));
            }
            // A measurement older than the newest minute (a clock change)
            // is left out rather than reordering the line.
            let Some(slot) = series.minutes.back_mut().filter(|b| b.minute == minute) else {
                continue;
            };
            if let Some(cpu) = m.cpu.filter(|v| v.is_finite() && *v >= 0.0) {
                slot.cpu_sum += f64::from(cpu);
                slot.cpu_count += 1;
                slot.cpu_peak = slot.cpu_peak.max(cpu);
            }
            if let Some(memory) = m.memory {
                slot.memory_sum += memory as f64;
                slot.memory_count += 1;
                slot.memory_peak = slot.memory_peak.max(memory);
            }
            self.dirty = true;
        }
        self.prune(unix_minute(wall));
    }

    /// Keeps the last 24 hours (nothing dated after the next minute) and at
    /// most [`NAMES_KEPT`] containers.
    fn prune(&mut self, now: i64) {
        let oldest = now - KEEP_MINUTES;
        for series in self.series.values_mut() {
            let before = series.minutes.len();
            series
                .minutes
                .retain(|m| m.minute > oldest && m.minute <= now + 1);
            self.dirty |= series.minutes.len() != before;
        }
        let before = self.series.len();
        self.series.retain(|_, s| !s.minutes.is_empty());
        while self.series.len() > NAMES_KEPT {
            let Some(key) = self
                .series
                .iter()
                .min_by_key(|(_, s)| s.seen)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.series.remove(&key);
        }
        self.dirty |= self.series.len() != before;
    }

    fn to_file(&self) -> File {
        File {
            version: VERSION,
            containers: self
                .series
                .iter()
                .map(|((context, name), s)| Entry {
                    context: context.clone(),
                    name: name.clone(),
                    per_core: s.per_core,
                    minutes: s.minutes.iter().copied().collect(),
                })
                .collect(),
        }
    }

    /// The history in `text`, or none when it is not one this version
    /// reads; minutes outside the last 24 hours are left out.
    fn from_text(text: &str, wall: SystemTime) -> Self {
        let Ok(file) = serde_json::from_str::<File>(text) else {
            return Self::default();
        };
        if file.version != VERSION {
            return Self::default();
        }
        let mut history = Self::default();
        let mut seen = HashMap::new();
        for entry in file.containers {
            let mut minutes: Vec<Minute> = entry.minutes;
            minutes.sort_by_key(|m| m.minute);
            minutes.dedup_by_key(|m| m.minute);
            let last = minutes.last().map_or(0, |m| m.minute);
            let key = (entry.context, entry.name);
            seen.insert(key.clone(), last);
            history.series.insert(
                key,
                Series {
                    minutes: minutes.into(),
                    per_core: entry.per_core,
                    last: None,
                    seen: last,
                },
            );
        }
        history.prune(unix_minute(wall));
        history.dirty = false;
        history
    }

    /// Reads the saved history; a missing, unreadable, oversized or newer
    /// file gives an empty one.
    pub fn load(path: &Path, wall: SystemTime) -> Self {
        let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= FILE_BYTES);
        if !small {
            return Self::default();
        }
        std::fs::read_to_string(path)
            .map(|text| Self::from_text(&text, wall))
            .unwrap_or_default()
    }

    /// Writes beside the file, then moves it over, so a crash never leaves
    /// half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string(&self.to_file()).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// The container's last 24 hours in its details: CPU and memory, the
/// average as a line and the peak behind it, with gaps where nothing was
/// read. Returns `true` when the user asked to clear all history.
pub fn show(ui: &mut egui::Ui, series: Option<&Series>, wall: SystemTime) -> bool {
    let now = unix_minute(wall);
    let recorded = series.map_or(0, |s| s.minutes.len());
    let mut clear = false;
    ui.horizontal_wrapped(|ui| {
        ui.strong(crate::i18n::tr("Last 24 hours"));
        ui.weak(crate::i18n::format(
            "Recorded {0} of the last 24 hours.",
            &[wtm_core::format_duration(Duration::from_secs(
                recorded as u64 * 60,
            ))],
        ));
        clear = ui
            .small_button(crate::i18n::tr("Clear history"))
            .on_hover_text(crate::i18n::tr(
                "Forgets the recorded history of every container",
            ))
            .clicked();
    });
    let Some(series) = series.filter(|s| !s.minutes.is_empty()) else {
        ui.weak(crate::i18n::tr(
            "Nothing recorded yet. History fills while Docker is read: while this page or the Overview is open, or while container alerts are on.",
        ));
        return clear;
    };
    let cpu: Vec<(i64, f32, f32)> = series
        .minutes
        .iter()
        .filter_map(|m| m.cpu().map(|(avg, peak)| (m.minute, avg, peak)))
        .collect();
    let memory: Vec<(i64, f32, f32)> = series
        .minutes
        .iter()
        .filter_map(|m| {
            m.memory()
                .map(|(avg, peak)| (m.minute, avg as f32, peak as f32))
        })
        .collect();
    ui.columns(2, |columns| {
        let title = if series.per_core {
            crate::i18n::tr("CPU (100% per core)")
        } else {
            "CPU"
        };
        chart(&mut columns[0], title, &cpu, now, crate::theme::CPU, &|v| {
            crate::i18n::percent(format!("{v:.1}"))
        });
        chart(
            &mut columns[1],
            crate::i18n::tr("Memory"),
            &memory,
            now,
            crate::theme::MEMORY,
            &|v| crate::i18n::bytes(f64::from(v)),
        );
    });
    clear
}

/// One resource over the last 24 hours: `points` are (minute, average,
/// peak), oldest first.
fn chart(
    ui: &mut egui::Ui,
    title: &str,
    points: &[(i64, f32, f32)],
    now: i64,
    color: egui::Color32,
    format: &dyn Fn(f32) -> String,
) {
    ui.label(title);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 60.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    let top = points.iter().map(|p| p.2).fold(0.0, f32::max);
    if points.is_empty() || top <= 0.0 {
        return;
    }
    let start = now - KEEP_MINUTES;
    let x =
        |minute: i64| rect.left() + (minute - start) as f32 / KEEP_MINUTES as f32 * rect.width();
    let y = |v: f32| rect.bottom() - 2.0 - v / top * (rect.height() - 4.0);
    // Runs of consecutive minutes; a missing minute breaks the line.
    let mut runs: Vec<&[(i64, f32, f32)]> = Vec::new();
    let mut from = 0;
    for i in 1..=points.len() {
        if i == points.len() || points[i].0 != points[i - 1].0 + 1 {
            runs.push(&points[from..i]);
            from = i;
        }
    }
    for run in runs {
        for (value, stroke) in [
            (2usize, egui::Stroke::new(1.0, color.gamma_multiply(0.35))),
            (1usize, egui::Stroke::new(1.5, color)),
        ] {
            let line: Vec<egui::Pos2> = run
                .iter()
                .map(|p| {
                    let v = if value == 1 { p.1 } else { p.2 };
                    egui::pos2(x(p.0), y(v))
                })
                .collect();
            if line.len() == 1 {
                painter.circle_filled(line[0], 1.5, stroke.color);
            } else {
                painter.add(egui::Shape::line(line, stroke));
            }
        }
    }
    if let Some(pointer) = response.hover_pos() {
        let minute =
            start + ((pointer.x - rect.left()) / rect.width() * KEEP_MINUTES as f32) as i64;
        let near = points
            .iter()
            .min_by_key(|p| (p.0 - minute).abs())
            .filter(|p| (p.0 - minute).abs() <= 2);
        let text = match near {
            Some(&(m, avg, peak)) => crate::i18n::format(
                "{0}: average {1}, peak {2}",
                &[clock(m), format(avg), format(peak)],
            ),
            None => crate::i18n::format("{0}: not recorded", &[clock(minute)]),
        };
        response.on_hover_text(text);
    }
}

/// `HH:MM` in local time.
fn clock(minute: i64) -> String {
    chrono::DateTime::from_timestamp(minute * 60, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// A fictional day of `web-0` for screenshots: a daily rhythm with a peak
/// every quarter hour and three hours not read.
#[cfg(all(test, feature = "visual-tests"))]
pub fn fixture(wall: SystemTime) -> History {
    let now = unix_minute(wall);
    let minutes = ((now - KEEP_MINUTES + 1)..=now)
        .filter(|m| !(now - 9 * 60..now - 6 * 60).contains(m))
        .map(|minute| {
            let phase = (minute % 1440) as f32 / 1440.0 * std::f32::consts::TAU;
            let cpu = 60.0 + 40.0 * phase.sin();
            let peak = if minute % 15 == 0 {
                cpu * 2.5
            } else {
                cpu * 1.3
            };
            let memory = (300.0 + 80.0 * phase.cos()) * 1024.0 * 1024.0;
            Minute {
                minute,
                cpu_sum: f64::from(cpu),
                cpu_count: 1,
                cpu_peak: peak,
                memory_sum: f64::from(memory),
                memory_count: 1,
                memory_peak: (memory * 1.1) as u64,
            }
        })
        .collect();
    let mut history = History::default();
    history.series.insert(
        ("desktop-linux".into(), "web-0".into()),
        Series {
            minutes,
            per_core: true,
            last: None,
            seen: now,
        },
    );
    history
}

#[cfg(test)]
mod tests {
    use super::*;
    use wtm_core::docker::{Container, Endpoint, Metrics};

    fn report(context: &str, rows: &[(&str, Option<f32>, Instant)]) -> Report {
        Report {
            endpoint: Endpoint {
                context: context.into(),
                pipe: r"\\.\pipe\docker_engine".into(),
            },
            containers: rows
                .iter()
                .map(|(name, cpu, at)| Container {
                    name: (*name).into(),
                    state: "running".into(),
                    metrics: Metrics {
                        cpu: *cpu,
                        cpu_cores: Some(4),
                        memory: Some(1000),
                        measured: Some(*at),
                        ..Metrics::default()
                    },
                    ..Container::default()
                })
                .collect(),
            detail: None,
            detail_id: None,
            sampled: rows.first().map_or_else(Instant::now, |r| r.2),
            truncated: false,
        }
    }

    /// A fixed time on a minute boundary.
    fn noon() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(29_385_360 * 60)
    }

    #[test]
    fn a_repeated_measurement_counts_once_and_a_gap_stays_a_gap() {
        let (now, wall) = (Instant::now(), noon());
        let mut h = History::default();
        let at = |s: u64| (now + Duration::from_secs(s), wall + Duration::from_secs(s));
        let (n, w) = at(0);
        h.record(n, w, &report("desktop", &[("web", Some(100.0), n)]));
        // The same Docker sample again: not counted.
        let (n2, w2) = at(10);
        h.record(n2, w2, &report("desktop", &[("web", Some(100.0), n)]));
        let (n3, w3) = at(20);
        h.record(n3, w3, &report("desktop", &[("web", Some(300.0), n3)]));
        // Nothing read for five minutes, then a reading.
        let (n4, w4) = at(6 * 60);
        h.record(n4, w4, &report("desktop", &[("web", Some(50.0), n4)]));
        let s = h.series("desktop", "web").unwrap();
        assert!(s.per_core);
        let minutes: Vec<i64> = s
            .minutes
            .iter()
            .map(|m| m.minute - unix_minute(wall))
            .collect();
        assert_eq!(minutes, [0, 6], "no minute is invented in the gap");
        assert_eq!(s.minutes[0].cpu(), Some((200.0, 300.0)));
        assert_eq!(s.minutes[0].memory(), Some((1000.0, 1000)));
        assert!(h.dirty);
    }

    #[test]
    fn stale_or_stopped_containers_are_not_recorded() {
        let (now, wall) = (Instant::now(), noon());
        let mut h = History::default();
        let old = now;
        let later = now + wtm_core::docker::STALE + Duration::from_secs(1);
        h.record(later, wall, &report("desktop", &[("web", Some(10.0), old)]));
        let mut stopped = report("desktop", &[("db", Some(10.0), later)]);
        stopped.containers[0].state = "exited".into();
        h.record(later, wall, &stopped);
        assert!(h.is_empty());
        assert!(!h.dirty);
    }

    #[test]
    fn contexts_stay_apart_and_a_recreated_container_goes_on() {
        let (now, wall) = (Instant::now(), noon());
        let mut h = History::default();
        h.record(now, wall, &report("desktop", &[("web", Some(10.0), now)]));
        h.record(now, wall, &report("remote", &[("web", Some(90.0), now)]));
        assert_eq!(
            h.series("desktop", "web").unwrap().minutes[0].cpu_peak,
            10.0
        );
        assert_eq!(h.series("remote", "web").unwrap().minutes[0].cpu_peak, 90.0);
        // Re-created: a new id, the same name, the same line.
        let later = now + Duration::from_secs(60);
        let mut again = report("desktop", &[("web", Some(20.0), later)]);
        again.containers[0].id = "new".into();
        h.record(later, wall + Duration::from_secs(60), &again);
        assert_eq!(h.series("desktop", "web").unwrap().minutes.len(), 2);
    }

    #[test]
    fn only_the_last_day_and_the_last_containers_are_kept() {
        let (now, wall) = (Instant::now(), noon());
        let mut h = History::default();
        h.record(now, wall, &report("desktop", &[("web", Some(10.0), now)]));
        // A day and a minute later the first minute is gone.
        let day = Duration::from_secs(KEEP_MINUTES as u64 * 60);
        let n = now + Duration::from_secs(30);
        h.record(n, wall + day, &report("desktop", &[("db", Some(10.0), n)]));
        assert!(h.series("desktop", "web").is_none());
        // More names than kept: the one seen longest ago goes.
        let mut h = History::default();
        for i in 0..=NAMES_KEPT {
            let (n, w) = (
                now + Duration::from_secs(i as u64),
                wall + Duration::from_secs(i as u64 * 60),
            );
            h.record(
                n,
                w,
                &report("desktop", &[(&format!("c{i}"), Some(1.0), n)]),
            );
        }
        assert_eq!(h.series.len(), NAMES_KEPT);
        assert!(h.series("desktop", "c0").is_none());
        assert!(h.series("desktop", &format!("c{NAMES_KEPT}")).is_some());
    }

    #[test]
    fn the_file_round_trips_and_a_bad_one_reads_as_empty() {
        let (now, wall) = (Instant::now(), noon());
        let dir =
            std::env::temp_dir().join(format!("wtm-container-history-{}", std::process::id()));
        let path = dir.join("container-history.json");
        let mut h = History::default();
        h.record(now, wall, &report("desktop", &[("web", Some(10.0), now)]));
        h.save(&path).unwrap();
        let back = History::load(&path, wall);
        assert_eq!(
            back.series("desktop", "web").unwrap().minutes,
            h.series("desktop", "web").unwrap().minutes
        );
        assert!(!back.dirty);
        // Read a day later, it has nothing left.
        assert!(History::load(&path, wall + Duration::from_secs(25 * 3600)).is_empty());
        // Minutes dated in the future (a clock moved back) are dropped.
        assert!(History::load(&path, wall - Duration::from_secs(3600)).is_empty());
        for bad in ["not json", r#"{"version":2,"containers":[]}"#, "{}"] {
            std::fs::write(&path, bad).unwrap();
            assert!(History::load(&path, wall).is_empty(), "{bad}");
        }
        assert!(History::load(&dir.join("missing.json"), wall).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_chart_renders_with_gaps_in_five_languages() {
        use egui_kittest::{Harness, kittest::Queryable};
        let (now, wall) = (Instant::now(), noon());
        let mut h = History::default();
        for s in [0u64, 60, 120, 600, 660] {
            let n = now + Duration::from_secs(s);
            h.record(
                n,
                wall + Duration::from_secs(s),
                &report("desktop", &[("web", Some(s as f32), n)]),
            );
        }
        let series = h.series("desktop", "web").cloned();
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Turkish,
            crate::i18n::Language::German,
            crate::i18n::Language::French,
            crate::i18n::Language::Spanish,
        ] {
            let _language = crate::i18n::scope(language);
            let mut harness = Harness::builder()
                .allow_missing_glyphs()
                .with_size([700.0, 300.0])
                .build_ui_state(
                    |ui, cleared: &mut bool| {
                        *cleared |= show(ui, series.as_ref(), wall + Duration::from_secs(700));
                    },
                    false,
                );
            harness.run();
            let recorded = crate::i18n::format(
                "Recorded {0} of the last 24 hours.",
                &[wtm_core::format_duration(Duration::from_secs(5 * 60))],
            );
            assert!(harness.query_by_label(&recorded).is_some(), "{language:?}");
            harness
                .get_by_label(crate::i18n::tr("Clear history"))
                .click();
            harness.run();
            assert!(*harness.state(), "{language:?}");
        }
    }
}

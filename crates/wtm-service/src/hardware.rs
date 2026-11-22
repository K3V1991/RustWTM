//! Dedicated sensor/control worker. No hardware call runs on the governor or
//! IPC threads. Profiles are session-only; startup always keeps hardware defaults.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};
use wtm_core::hardware::{HardwareReader, HardwareReport, unix_ms};
use wtm_policy::{CoolingMode, CoolingProfile, smooth_duty};

use crate::events::EventLog;
use wtm_ipc::EventLevel;

/// Readings a change must last before it is logged (about six seconds), so
/// a sensor missing from one reading does not fill the event log.
const SETTLE: u8 = 3;

enum Message {
    Set(
        String,
        CoolingProfile,
        u64,
        Instant,
        Sender<Result<(), String>>,
    ),
    RevokeCooling,
    Stop(Sender<()>),
}

#[derive(Debug, Clone, Copy)]
struct CoolingAccess {
    dry_run: bool,
    revision: u64,
}

impl CoolingAccess {
    fn allows(self, revision: u64) -> bool {
        !self.dry_run && self.revision == revision
    }
}

const COOLING_ACCESS_CHANGED: &str = "Cooling authorization changed. Apply the profile again.";

#[derive(Clone, Debug)]
struct Worker {
    sender: Sender<Message>,
    state: Arc<WorkerState>,
}

#[derive(Debug, Default)]
struct WorkerState {
    report: RwLock<HardwareReport>,
    /// This worker is holding a fan profile, which must keep being renewed.
    controlling: AtomicBool,
}

#[derive(Debug)]
pub struct HardwareHub {
    access: Arc<Mutex<CoolingAccess>>,
    worker: Mutex<Option<Worker>>,
    /// When a client last asked for sensors or a fan setting.
    last_request: Mutex<Instant>,
    /// Where lasting sensor and cooling changes are logged.
    events: OnceLock<EventLog>,
}
impl Default for HardwareHub {
    fn default() -> Self {
        Self {
            access: Arc::new(Mutex::new(CoolingAccess {
                dry_run: true,
                revision: 0,
            })),
            worker: Mutex::new(None),
            last_request: Mutex::new(Instant::now()),
            events: OnceLock::new(),
        }
    }
}

/// How a controlled fan channel is driven, as far as the event log cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cooling {
    Software,
    /// Left to the hardware because the temperature or channel is unavailable.
    Automatic,
    Failed,
}

/// A state that is logged when it changes and the change lasts.
#[derive(Debug)]
struct Track<T> {
    /// The state last settled on; `None` until one has.
    settled: Option<T>,
    /// A different state and the readings it has lasted.
    pending: Option<(T, u8)>,
}

impl<T> Default for Track<T> {
    fn default() -> Self {
        Self {
            settled: None,
            pending: None,
        }
    }
}

impl<T: Copy + PartialEq> Track<T> {
    /// The previous and new state once `now` has lasted `SETTLE` readings.
    fn observe(&mut self, now: T) -> Option<(Option<T>, T)> {
        if self.settled == Some(now) {
            self.pending = None;
            return None;
        }
        let count = match self.pending {
            Some((state, count)) if state == now => count + 1,
            _ => 1,
        };
        if count < SETTLE {
            self.pending = Some((now, count));
            return None;
        }
        self.pending = None;
        let before = self.settled.replace(now);
        Some((before, now))
    }
}

/// Turns the worker's readings into log lines: sensor reading failing and
/// resuming, and a controlled fan falling back to the hardware, failing or
/// returning to software control. Kept by the worker only, so a restarted
/// helper starts from "unknown" and logs nothing for a normal first reading.
#[derive(Debug, Default)]
struct Changes {
    reading: Track<bool>,
    channels: HashMap<String, Track<Cooling>>,
}

impl Changes {
    /// A reading failed with `error`.
    fn failed(&mut self, error: &str) -> Vec<(EventLevel, String)> {
        match self.reading.observe(false) {
            Some((_, false)) => vec![(
                EventLevel::Warn,
                format!("Sensors: reading failed: {error}"),
            )],
            _ => Vec::new(),
        }
    }

    /// A reading succeeded and each controlled channel (`id`, `name`) is in
    /// the given state. Channels no longer controlled are forgotten.
    fn read(
        &mut self,
        channels: &[(String, String, Cooling, String)],
    ) -> Vec<(EventLevel, String)> {
        let mut lines = Vec::new();
        if let Some((Some(false), true)) = self.reading.observe(true) {
            lines.push((EventLevel::Info, "Sensors: readings resumed".to_owned()));
        }
        self.channels
            .retain(|id, _| channels.iter().any(|(c, ..)| c == id));
        for (id, name, state, error) in channels {
            let change = self.channels.entry(id.clone()).or_default().observe(*state);
            match change {
                Some((_, Cooling::Automatic)) => lines.push((
                    EventLevel::Warn,
                    format!(
                        "Cooling {name}: back on hardware automatic, temperature or channel unavailable"
                    ),
                )),
                Some((_, Cooling::Failed)) => lines.push((
                    EventLevel::Warn,
                    format!("Cooling {name}: control failed: {error}"),
                )),
                Some((Some(_), Cooling::Software)) => lines.push((
                    EventLevel::Info,
                    format!("Cooling {name}: under software control again"),
                )),
                _ => {}
            }
        }
        lines
    }
}

fn log(events: Option<&EventLog>, lines: Vec<(EventLevel, String)>) {
    if let Some(events) = events {
        for (level, message) in lines {
            events.push(level, None, message);
        }
    }
}

/// The helper may stop: nobody asked for `after` and no fan is controlled.
fn idle(last_request: Instant, controlling: bool, after: Duration) -> bool {
    !controlling && last_request.elapsed() >= after
}

fn requested_duty(
    snapshot: &HardwareReport,
    id: &str,
    profile: &CoolingProfile,
    now: u64,
) -> Result<f32, String> {
    let channel = snapshot
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .ok_or("Cooling channel is no longer available")?;
    profile.validate(channel)?;
    profile
        .duty(snapshot, now)
        .ok_or_else(|| "Temperature sensor is missing or stale".into())
}

impl HardwareHub {
    pub fn dry_run(&self, dry: bool) {
        let mut access = self.access.lock().unwrap_or_else(PoisonError::into_inner);
        let revoke = dry && !access.dry_run;
        access.dry_run = dry;
        if revoke {
            access.revision = access.revision.wrapping_add(1);
        }
        drop(access);
        if revoke
            && let Some(worker) = self
                .worker
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
        {
            let _ = worker.sender.send(Message::RevokeCooling);
        }
    }
    /// Logs lasting sensor and cooling changes to `events` from the next
    /// start of the helper on.
    pub fn log_to(&self, events: EventLog) {
        let _ = self.events.set(events);
    }
    fn worker(&self) -> Worker {
        let mut current = self.worker.lock().unwrap_or_else(PoisonError::into_inner);
        current.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel();
            // A retiring worker may still finish a reading after a restart.
            // The replacement owns a fresh report and controlling flag.
            let worker = Worker { sender: tx, state: Arc::default() };
            let state = worker.state.clone();
            let access_state = self.access.clone();
            let events = self.events.get().cloned();
            std::thread::spawn(move || {
                let report = &state.report;
                let controlling = &state.controlling;
                let mut reader = HardwareReader::new(true);
                let mut changes = Changes::default();
                let mut profiles: HashMap<String, CoolingProfile> = HashMap::new();
                let mut applied: HashMap<String, (f32, Instant)> = HashMap::new();
                let access = || *access_state.lock().unwrap_or_else(PoisonError::into_inner);
                let mut revision = access().revision;
                let mut revoke_if_needed = |reader: &mut HardwareReader,
                                            profiles: &mut HashMap<String, CoolingProfile>,
                                            applied: &mut HashMap<String, (f32, Instant)>| {
                    let current = access().revision;
                    if revision != current {
                        revision = current;
                        report.write().unwrap_or_else(PoisonError::into_inner).cooling.clear();
                        for (id, _) in profiles.drain() {
                            let _ = reader.set(&id, None);
                        }
                        applied.clear();
                        controlling.store(false, Ordering::Relaxed);
                    }
                    current
                };
                loop {
                    let message = rx.recv_timeout(Duration::from_secs(2));
                    let authorized_revision = revoke_if_needed(&mut reader, &mut profiles, &mut applied);
                    match message {
                        Ok(Message::Stop(reply)) => { for id in profiles.keys() {let _=reader.set(id,None);} drop(reader); let _=reply.send(()); return; }
                        Ok(Message::RevokeCooling) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Ok(Message::Set(id, profile, request_revision, deadline, reply)) => {
                            if Instant::now() >= deadline {let _=reply.send(Err("Cooling request expired before execution".into()));continue;}
                            let automatic = profile.mode == CoolingMode::Automatic;
                            if !automatic && (request_revision != authorized_revision || !access().allows(request_revision)) {
                                let _ = reply.send(Err(COOLING_ACCESS_CHANGED.into()));
                                continue;
                            }
                            let r = if automatic {
                                // Remove the request even when restoring fails; future leases
                                // must not re-apply a setting the user just disabled.
                                profiles.remove(&id); applied.remove(&id);
                                reader.set(&id, None)
                            } else {
                                let snapshot = report.read().unwrap_or_else(PoisonError::into_inner).clone();
                                requested_duty(&snapshot, &id, &profile, unix_ms()).and_then(|duty| {
                                    if !access().allows(request_revision) {
                                        return Err(COOLING_ACCESS_CHANGED.into());
                                    }
                                    let written = reader.set(&id, Some(duty));
                                    if !access().allows(request_revision) {
                                        profiles.remove(&id);
                                        applied.remove(&id);
                                        let _ = reader.set(&id, None);
                                        return Err(COOLING_ACCESS_CHANGED.into());
                                    }
                                    written?;
                                    applied.insert(id.clone(), (duty, Instant::now()));
                                    profiles.insert(id.clone(), profile);
                                    Ok(())
                                })
                            };
                            if reply.send(r).is_err() {
                                // Do not keep renewing an operation whose caller timed out.
                                profiles.remove(&id); applied.remove(&id);
                                let _ = reader.set(&id, None);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    controlling.store(!profiles.is_empty(), Ordering::Relaxed);
                    match reader.sample() {
                        Ok(mut snapshot) => {
                            let authorized_revision = revoke_if_needed(&mut reader, &mut profiles, &mut applied);
                            let now = unix_ms();
                            let mut states = Vec::new();
                            for (id, profile) in &profiles {
                                let requested = if !access().allows(authorized_revision) { None } else { requested_duty(&snapshot, id, profile, now).ok() };
                                let duty = requested.map(|v| applied.get(id).map_or(v, |(old, at)| smooth_duty(*old, v, at.elapsed().as_secs_f32())));
                                let result = reader.set(id, duty);
                                let (state, error) = match &result { Ok(()) if duty.is_some() => (Cooling::Software, String::new()), Ok(()) => (Cooling::Automatic, String::new()), Err(e) => (Cooling::Failed, e.clone()) };
                                let label = match (result, duty) {
                                    (Ok(()), Some(v)) => { applied.insert(id.clone(), (v, Instant::now())); format!("Software · {v:.0}%") },
                                    (Ok(()), None) => { applied.remove(id); "Hardware automatic · temperature or channel unavailable, or monitoring only".into() },
                                    (Err(e), _) => format!("Control failed: {e}"),
                                };
                                snapshot.cooling.push((id.clone(),label));
                                let name = snapshot.channels.iter().find(|c| &c.id == id).map_or_else(|| id.clone(), |c| c.name.clone());
                                states.push((id.clone(), name, state, error));
                            }
                            if access().allows(authorized_revision) {
                                log(events.as_ref(), changes.read(&states));
                            } else {
                                snapshot.cooling.clear();
                                log(events.as_ref(), changes.read(&[]));
                            }
                            controlling.store(!profiles.is_empty(), Ordering::Relaxed);
                            *report.write().unwrap_or_else(PoisonError::into_inner) = snapshot;
                        }
                        Err(e) => {
                            *report.write().unwrap_or_else(PoisonError::into_inner) =
                                HardwareReport::collection_failed(&e);
                            revoke_if_needed(&mut reader, &mut profiles, &mut applied);
                            // Restore immediately on failed collection, and allow the helper's
                            // lease deadline to retry if communication itself is broken.
                            for id in profiles.keys() { let _ = reader.set(id,None); }
                            applied.clear();
                            log(events.as_ref(), changes.failed(&e));
                        }
                    }
                }
                for id in profiles.keys() { let _ = reader.set(id,None); }
            });
            worker
        }).clone()
    }
    fn requested(&self) {
        *self
            .last_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }
    pub fn snapshot(&self) -> HardwareReport {
        self.requested();
        self.worker()
            .state
            .report
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    pub fn set(&self, id: String, profile: CoolingProfile) -> Result<(), String> {
        let access = *self.access.lock().unwrap_or_else(PoisonError::into_inner);
        if profile.mode != CoolingMode::Automatic && access.dry_run {
            return Err(
                "Switch the service from monitoring to enforcing before controlling fans".into(),
            );
        }
        self.requested();
        let (reply, rx) = mpsc::channel();
        self.worker()
            .sender
            .send(Message::Set(
                id,
                profile,
                access.revision,
                Instant::now() + Duration::from_secs(6),
                reply,
            ))
            .map_err(|e| e.to_string())?;
        // Six seconds to start plus the provider's four-second control timeout.
        rx.recv_timeout(Duration::from_secs(12))
            .map_err(|e| format!("Hardware control did not acknowledge: {e}"))?
    }
    /// Stops the helper (about 100 MB and a steady trickle of CPU) once no
    /// client asked for `after` and no fan profile is active. The next
    /// request starts it again. Does not wait for the helper to exit.
    pub fn stop_if_idle(&self, after: Duration) {
        let mut worker = self.worker.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(active) = worker.as_ref() else {
            return;
        };
        let last = *self
            .last_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let controlling = active.state.controlling.load(Ordering::Relaxed);
        if !idle(last, controlling, after) {
            return;
        }
        if let Some(retiring) = worker.take() {
            let (reply, _) = mpsc::channel();
            let _ = retiring.sender.send(Message::Stop(reply));
        }
    }
    pub fn stop(&self) {
        if let Some(retiring) = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let (reply, rx) = mpsc::channel();
            let _ = retiring.sender.send(Message::Stop(reply));
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }
}
impl Drop for HardwareHub {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn monitoring_mode_rejects_writes_without_even_starting_a_provider() {
        let hub = HardwareHub::default();
        let profile = CoolingProfile {
            mode: CoolingMode::Manual(50.0),
            sensor_id: "temp".into(),
            ..Default::default()
        };
        assert!(
            hub.set("fan".into(), profile)
                .unwrap_err()
                .contains("monitoring")
        );
        assert!(hub.worker.lock().unwrap().is_none());
    }

    #[test]
    fn entering_monitoring_revokes_the_previous_permission_and_wakes_the_worker() {
        let hub = HardwareHub::default();
        hub.dry_run(false);
        let previous = *hub.access.lock().unwrap();
        assert!(previous.allows(previous.revision));

        let (sender, messages) = mpsc::channel();
        *hub.worker.lock().unwrap() = Some(Worker {
            sender,
            state: Arc::default(),
        });
        hub.dry_run(true);
        hub.dry_run(true);
        hub.dry_run(false);
        let current = *hub.access.lock().unwrap();
        assert!(!current.allows(previous.revision));
        assert!(current.allows(current.revision));
        assert!(matches!(messages.try_recv(), Ok(Message::RevokeCooling)));
        assert!(matches!(
            messages.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        hub.stop_if_idle(Duration::ZERO);
        assert!(matches!(messages.try_recv(), Ok(Message::Stop(_))));
    }

    #[test]
    fn cooling_requires_a_current_temperature_and_an_eligible_channel() {
        use wtm_core::hardware::{FanChannel, Sensor, SensorKind};

        let profile = CoolingProfile {
            mode: CoolingMode::Manual(42.0),
            sensor_id: "temp".into(),
            ..Default::default()
        };
        let now = 20_000;
        let mut report = HardwareReport {
            sampled_at_ms: now,
            sensors: vec![Sensor {
                id: "temp".into(),
                device: "Fixture".into(),
                name: "Temperature".into(),
                kind: SensorKind::Temperature,
                value: Some(50.0),
            }],
            channels: vec![FanChannel {
                id: "fan".into(),
                device: "Fixture".into(),
                name: "Fan".into(),
                min_percent: 30.0,
                max_percent: 100.0,
                writable: true,
                reason: "Channel unavailable".into(),
            }],
            ..Default::default()
        };

        assert_eq!(requested_duty(&report, "fan", &profile, now), Ok(42.0));
        assert!(requested_duty(&report, "missing", &profile, now).is_err());
        report.channels[0].writable = false;
        assert!(requested_duty(&report, "fan", &profile, now).is_err());
        report.channels[0].writable = true;
        report.channels[0].min_percent = 50.0;
        assert!(requested_duty(&report, "fan", &profile, now).is_err());
        report.channels[0].min_percent = 30.0;
        let unavailable = HardwareReport::collection_failed("injected failure");
        assert!(requested_duty(&unavailable, "fan", &profile, now).is_err());
        let stale = now + wtm_core::hardware::STALE_MS + 1;
        assert!(requested_duty(&report, "fan", &profile, stale).is_err());
        report.sensors[0].value = Some(95.0);
        assert_eq!(requested_duty(&report, "fan", &profile, now), Ok(100.0));
    }

    fn channel(state: Cooling) -> Vec<(String, String, Cooling, String)> {
        vec![(
            "/lpc/0/control/1".into(),
            "CPU Fan".into(),
            state,
            "denied".into(),
        )]
    }

    fn messages(lines: Vec<(EventLevel, String)>) -> Vec<String> {
        lines.into_iter().map(|(_, m)| m).collect()
    }

    #[test]
    fn a_first_normal_reading_logs_nothing() {
        let mut changes = Changes::default();
        for _ in 0..10 {
            assert!(changes.read(&channel(Cooling::Software)).is_empty());
        }
    }

    #[test]
    fn a_sensor_missing_from_a_few_readings_is_not_logged() {
        let mut changes = Changes::default();
        for _ in 0..5 {
            changes.read(&channel(Cooling::Software));
        }
        // Gone for two readings, back, gone for two again: never three.
        for state in [
            Cooling::Automatic,
            Cooling::Automatic,
            Cooling::Software,
            Cooling::Automatic,
            Cooling::Automatic,
            Cooling::Software,
        ] {
            assert!(changes.read(&channel(state)).is_empty(), "{state:?}");
        }
    }

    #[test]
    fn a_lasting_fallback_and_its_recovery_are_logged_once() {
        let mut changes = Changes::default();
        for _ in 0..3 {
            changes.read(&channel(Cooling::Software));
        }
        let mut logged = Vec::new();
        for _ in 0..6 {
            logged.extend(changes.read(&channel(Cooling::Automatic)));
        }
        assert_eq!(
            messages(logged),
            ["Cooling CPU Fan: back on hardware automatic, temperature or channel unavailable"]
        );
        let mut logged = Vec::new();
        for _ in 0..6 {
            logged.extend(changes.read(&channel(Cooling::Software)));
        }
        assert_eq!(
            logged,
            [(
                EventLevel::Info,
                "Cooling CPU Fan: under software control again".to_owned()
            )]
        );
        let mut logged = Vec::new();
        for _ in 0..3 {
            logged.extend(changes.read(&channel(Cooling::Failed)));
        }
        assert_eq!(
            messages(logged),
            ["Cooling CPU Fan: control failed: denied"]
        );
    }

    #[test]
    fn a_fallback_from_the_start_is_logged_and_released_channels_are_forgotten() {
        let mut changes = Changes::default();
        let mut logged = Vec::new();
        for _ in 0..3 {
            logged.extend(changes.read(&channel(Cooling::Automatic)));
        }
        assert_eq!(logged.len(), 1);
        // Released, then controlled again: a fresh start, not a recovery.
        changes.read(&[]);
        for _ in 0..5 {
            assert!(changes.read(&channel(Cooling::Software)).is_empty());
        }
    }

    #[test]
    fn failed_readings_are_logged_once_and_their_end_too() {
        let mut changes = Changes::default();
        // A helper that never read: the failure is still worth a line.
        let mut logged = Vec::new();
        for _ in 0..5 {
            logged.extend(changes.failed("pipe closed"));
        }
        assert_eq!(
            logged,
            [(
                EventLevel::Warn,
                "Sensors: reading failed: pipe closed".to_owned()
            )]
        );
        // One good reading between failures is not a recovery.
        assert!(changes.read(&[]).is_empty());
        assert!(changes.failed("pipe closed").is_empty());
        let mut logged = Vec::new();
        for _ in 0..4 {
            logged.extend(changes.read(&[]));
        }
        assert_eq!(messages(logged), ["Sensors: readings resumed"]);
        // A new helper starts unknown: its first readings are silent.
        let mut changes = Changes::default();
        for _ in 0..4 {
            assert!(changes.read(&[]).is_empty());
        }
    }

    #[test]
    fn idle_needs_quiet_clients_and_no_fan_control() {
        let long_ago = Instant::now() - Duration::from_secs(60);
        let after = Duration::from_secs(30);
        assert!(idle(long_ago, false, after));
        assert!(!idle(long_ago, true, after), "a fan profile keeps it");
        assert!(!idle(Instant::now(), false, after));
    }

    #[test]
    fn a_retired_worker_cannot_replace_current_readings_or_control_state() {
        let hub = HardwareHub::default();
        let install_worker = |provider: &str| {
            let (sender, messages) = mpsc::channel();
            let worker = Worker {
                sender,
                state: Arc::default(),
            };
            worker.state.report.write().unwrap().provider = provider.into();
            *hub.worker.lock().unwrap() = Some(worker.clone());
            (worker, messages)
        };
        let (old, old_messages) = install_worker("old");
        let (finish, wait_to_finish) = mpsc::channel();
        let late_sample = std::thread::spawn(move || {
            wait_to_finish.recv_timeout(Duration::from_secs(5)).unwrap();
            old.state.report.write().unwrap().provider = "late old sample".into();
            old.state.controlling.store(false, Ordering::Relaxed);
        });

        hub.stop_if_idle(Duration::ZERO);
        assert!(matches!(old_messages.try_recv(), Ok(Message::Stop(_))));
        let (new, new_messages) = install_worker("new");
        new.state.controlling.store(true, Ordering::Relaxed);

        finish.send(()).unwrap();
        late_sample.join().unwrap();
        assert_eq!(hub.snapshot().provider, "new");
        hub.stop_if_idle(Duration::ZERO);
        assert!(hub.worker.lock().unwrap().is_some());
        assert!(matches!(
            new_messages.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        new.state.controlling.store(false, Ordering::Relaxed);
        hub.stop_if_idle(Duration::ZERO);
        assert!(matches!(new_messages.try_recv(), Ok(Message::Stop(_))));
    }

    #[test]
    fn the_helper_stops_when_idle_and_restarts_on_request() {
        let hub = HardwareHub::default();
        // Never started: nothing to stop.
        hub.stop_if_idle(Duration::ZERO);
        assert!(hub.worker.lock().unwrap().is_none());

        let _ = hub.snapshot();
        assert!(hub.worker.lock().unwrap().is_some());
        // Asked just now.
        hub.stop_if_idle(Duration::from_secs(30));
        assert!(hub.worker.lock().unwrap().is_some());
        // Driving a fan keeps it running however long nobody asks.
        let first = hub.worker();
        first.state.controlling.store(true, Ordering::Relaxed);
        hub.stop_if_idle(Duration::ZERO);
        assert!(hub.worker.lock().unwrap().is_some());
        first.state.controlling.store(false, Ordering::Relaxed);
        hub.stop_if_idle(Duration::ZERO);
        assert!(hub.worker.lock().unwrap().is_none());
        let _ = hub.snapshot();
        assert!(hub.worker.lock().unwrap().is_some(), "started again");
        hub.stop();
    }
}

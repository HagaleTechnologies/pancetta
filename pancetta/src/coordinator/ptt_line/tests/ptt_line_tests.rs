//! PAN-357: the serial PTT line and the `LineKeyedRig` wrapper, on a fake
//! line (a pty has no modem lines, and CI has no serial adapter).

use super::*;
use pancetta_hamlib::{MockRig, MockRigConfig};
use std::sync::atomic::AtomicUsize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LineEvent {
    Open(String),
    Rts(String, bool),
    Close(String),
}

/// Records opens, RTS writes and closes; can fail the next N opens or writes.
#[derive(Clone, Default)]
pub(super) struct FakeLines {
    pub(super) events: Arc<Mutex<Vec<LineEvent>>>,
    pub(super) fail_opens: Arc<AtomicUsize>,
    pub(super) fail_writes: Arc<AtomicUsize>,
}

struct FakeLine {
    device: String,
    lines: FakeLines,
}

impl RtsLine for FakeLine {
    fn write_rts(&mut self, asserted: bool) -> std::io::Result<()> {
        if take_one(&self.lines.fail_writes) {
            return Err(std::io::Error::other("fake modem-line ioctl failed"));
        }
        self.lines
            .events
            .lock()
            .unwrap()
            .push(LineEvent::Rts(self.device.clone(), asserted));
        Ok(())
    }
}

impl Drop for FakeLine {
    fn drop(&mut self) {
        self.lines
            .events
            .lock()
            .unwrap()
            .push(LineEvent::Close(self.device.clone()));
    }
}

fn take_one(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_ok()
}

impl FakeLines {
    pub(super) fn line(&self) -> Arc<PttLine> {
        let lines = self.clone();
        Arc::new(PttLine::with_opener(Arc::new(move |device: &str| {
            if take_one(&lines.fail_opens) {
                return Err(anyhow!("No such file or directory (os error 2)"));
            }
            lines
                .events
                .lock()
                .unwrap()
                .push(LineEvent::Open(device.to_string()));
            Ok(Box::new(FakeLine {
                device: device.to_string(),
                lines: lines.clone(),
            }) as Box<dyn RtsLine>)
        })))
    }

    pub(super) fn events(&self) -> Vec<LineEvent> {
        self.events.lock().unwrap().clone()
    }

    pub(super) fn rts_levels(&self) -> Vec<bool> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                LineEvent::Rts(_, level) => Some(level),
                _ => None,
            })
            .collect()
    }

    pub(super) fn opens(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, LineEvent::Open(_)))
            .count()
    }
}

const DEV: &str = "/dev/fake-ptt";
const CAT: &str = "/dev/fake-cat";

fn open(device: &str) -> LineEvent {
    LineEvent::Open(device.to_string())
}
fn rts(device: &str, level: bool) -> LineEvent {
    LineEvent::Rts(device.to_string(), level)
}
fn close(device: &str) -> LineEvent {
    LineEvent::Close(device.to_string())
}

async fn connected_mock() -> Arc<MockRig> {
    let rig = Arc::new(MockRig::new(MockRigConfig {
        connection_delay_ms: 0,
        operation_delay_ms: 0,
        ..Default::default()
    }));
    rig.connect().await.unwrap();
    rig
}

#[tokio::test]
async fn open_lowers_rts_first() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    assert_eq!(fake.events(), vec![open(DEV), rts(DEV, false)]);
    assert!(!line.is_asserted());
}

#[tokio::test]
async fn set_ptt_drives_rts_not_cat() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    let inner = connected_mock().await;
    let rig = LineKeyedRig::new(inner.clone(), line.clone());

    rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();
    assert_eq!(fake.rts_levels(), vec![false, true]);
    assert_eq!(inner.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);

    rig.set_ptt(Vfo::Current, PttState::Off).await.unwrap();
    assert_eq!(fake.rts_levels(), vec![false, true, false]);
    assert_eq!(inner.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);
}

#[tokio::test]
async fn get_ptt_reports_the_line_while_asserted() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();

    // An inner rig whose PTT read fails: the line's state still wins.
    let failing = Arc::new(RecordingRig::default());
    let rig = LineKeyedRig::new(failing.clone(), line.clone());
    rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();
    assert_eq!(rig.get_ptt(Vfo::Current).await.unwrap(), PttState::On);
    rig.set_ptt(Vfo::Current, PttState::Off).await.unwrap();
    assert!(rig.get_ptt(Vfo::Current).await.is_err());
    assert!(failing.log().iter().all(|c| !c.starts_with("set_ptt")));

    // Unasserted: the inner rig's own answer.
    let inner = connected_mock().await;
    let rig = LineKeyedRig::new(inner.clone(), line.clone());
    assert_eq!(rig.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);
    inner.set_ptt(Vfo::Current, PttState::On).await.unwrap();
    assert_eq!(rig.get_ptt(Vfo::Current).await.unwrap(), PttState::On);
}

#[tokio::test]
async fn write_failure_is_an_error_and_reopens_next_time() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();

    fake.fail_writes.store(1, Ordering::Release);
    let err = line.set(true).await.unwrap_err();
    assert!(format!("{err:#}").contains(DEV), "{err:#}");
    assert!(line.is_asserted(), "a failed key-down may have asserted");
    assert_eq!(fake.events().last(), Some(&close(DEV)), "handle dropped");

    line.set(true).await.unwrap();
    assert_eq!(fake.opens(), 2, "the next key-down reopens the port");
    assert_eq!(
        fake.events()[fake.events().len() - 3..],
        [open(DEV), rts(DEV, false), rts(DEV, true)]
    );
    assert!(line.is_asserted());
}

#[tokio::test]
async fn unkey_after_a_lost_handle_reopens_to_lower_rts() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.set(true).await.unwrap();

    fake.fail_writes.store(1, Ordering::Release);
    assert!(line.set(false).await.is_err());
    assert!(
        line.is_asserted(),
        "a failed unkey leaves it possibly asserted"
    );

    line.set(false).await.unwrap();
    assert_eq!(fake.opens(), 2);
    assert_eq!(fake.events().last(), Some(&rts(DEV, false)));
    assert!(!line.is_asserted());
}

#[tokio::test]
async fn unkey_without_a_handle_is_ok_when_never_asserted() {
    let fake = FakeLines::default();
    fake.fail_opens.store(1, Ordering::Release);
    let line = fake.line();
    assert!(line.configure(Some(DEV.into()), CAT).await.is_err());

    line.set(false).await.unwrap();
    assert_eq!(fake.opens(), 0, "nothing to lower, nothing opened");
    assert!(fake.events().is_empty());

    // An unconfigured line has nothing to drop either.
    let idle = FakeLines::default();
    idle.line().set(false).await.unwrap();
    assert!(idle.events().is_empty());
}

#[tokio::test]
async fn open_failure_names_the_device() {
    let fake = FakeLines::default();
    fake.fail_opens.store(2, Ordering::Release);
    let line = fake.line();
    let err = line.configure(Some(DEV.into()), CAT).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.starts_with(&format!("cannot open serial PTT port {DEV}: ")),
        "{text}"
    );
    assert!(text.contains("No such file or directory"), "{text}");

    let err = line.set(true).await.unwrap_err();
    assert!(
        format!("{err:#}").contains(&format!("cannot open serial PTT port {DEV}")),
        "{err:#}"
    );
    assert!(!line.is_asserted());
}

#[tokio::test]
async fn shutdown_latch_refuses_a_later_key_down() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.set(true).await.unwrap();

    line.release_for_shutdown().await.unwrap();
    assert_eq!(fake.rts_levels().last(), Some(&false));
    assert!(!line.is_asserted());

    let err = line.set(true).await.unwrap_err();
    assert!(format!("{err:#}").contains("shutdown"), "{err:#}");
    assert_eq!(fake.rts_levels().last(), Some(&false), "never re-keyed");
    line.set(false).await.unwrap();
}

#[test]
fn line_target_needs_rig_control_and_serial_rts() {
    let mut rig = RigConfig::default();
    rig.ptt.method = PttMethod::SerialRts;
    rig.ptt.device = Some(" /dev/ttyUSB1 ".into());
    assert_eq!(line_target(&rig, true), Some("/dev/ttyUSB1".into()));
    assert_eq!(line_target(&rig, false), None, "rig control off or mock");

    rig.ptt.device = Some("  ".into());
    assert_eq!(line_target(&rig, true), None);
    rig.ptt.device = None;
    assert_eq!(line_target(&rig, true), None);

    rig.ptt.device = Some("/dev/ttyUSB1".into());
    for method in [
        PttMethod::None,
        PttMethod::Cat,
        PttMethod::Serial,
        PttMethod::Vox,
        PttMethod::Usb,
    ] {
        rig.ptt.method = method.clone();
        assert_eq!(line_target(&rig, true), None, "{method:?}");
    }
}

#[tokio::test]
async fn configure_reuses_an_open_device() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    assert_eq!(fake.opens(), 1);
    assert_eq!(fake.events(), vec![open(DEV), rts(DEV, false)]);
}

#[tokio::test]
async fn configure_to_none_lowers_and_closes() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.set(true).await.unwrap();
    line.configure(None, CAT).await.unwrap();
    assert_eq!(
        fake.events(),
        vec![
            open(DEV),
            rts(DEV, false),
            rts(DEV, true),
            rts(DEV, false),
            close(DEV)
        ]
    );
    assert!(!line.is_asserted());
    assert!(line.set(true).await.is_err(), "no line, no key-down");
}

#[tokio::test]
async fn configure_to_a_new_device_closes_the_old_one() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.configure(Some("/dev/fake-ptt2".into()), CAT)
        .await
        .unwrap();
    assert_eq!(
        fake.events(),
        vec![
            open(DEV),
            rts(DEV, false),
            rts(DEV, false),
            close(DEV),
            open("/dev/fake-ptt2"),
            rts("/dev/fake-ptt2", false),
        ]
    );
    line.set(true).await.unwrap();
    assert_eq!(fake.events().last(), Some(&rts("/dev/fake-ptt2", true)));
}

#[cfg(unix)]
#[tokio::test]
async fn configure_refuses_a_symlink_to_the_cat_port() {
    let dir = tempfile::tempdir().unwrap();
    let cat = dir.path().join("ttyUSB0");
    std::fs::write(&cat, b"").unwrap();
    let link = dir.path().join("usb-FTDI-if00-port0");
    std::os::unix::fs::symlink(&cat, &link).unwrap();

    let fake = FakeLines::default();
    let line = fake.line();
    let err = line
        .configure(
            Some(link.to_string_lossy().into_owned()),
            &cat.to_string_lossy(),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("CAT port"), "{err:#}");
    assert_eq!(fake.opens(), 0, "never opens the CAT port");
    assert!(line.set(true).await.is_err());
}

/// Implements every `RigControl` method (see `pancetta-hamlib/src/rig.rs`),
/// logs the call with its arguments and returns a distinctive value.
#[derive(Default)]
struct RecordingRig {
    calls: Mutex<Vec<String>>,
}

impl RecordingRig {
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    fn log(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RigControl for RecordingRig {
    async fn receive_gain_capabilities(&self) -> Result<ReceiveGainCapabilities> {
        self.record("receive_gain_capabilities".into());
        Ok(ReceiveGainCapabilities {
            preamp_db: vec![0, 17],
            attenuator_db: vec![0, 23],
        })
    }
    async fn get_receive_gain(&self) -> Result<ReceiveGain> {
        self.record("get_receive_gain".into());
        Ok(ReceiveGain {
            preamp_db: 17,
            attenuator_db: 23,
        })
    }
    async fn set_receive_gain(
        &self,
        gain: ReceiveGain,
        _deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.record(format!("set_receive_gain {gain:?}"));
        Err(anyhow!("set_receive_gain sentinel"))
    }
    async fn connect(&self) -> Result<()> {
        self.record("connect".into());
        Err(anyhow!("connect sentinel"))
    }
    async fn disconnect(&self) -> Result<()> {
        self.record("disconnect".into());
        Err(anyhow!("disconnect sentinel"))
    }
    async fn get_status(&self) -> Result<RigStatus> {
        self.record("get_status".into());
        Ok(RigStatus {
            frequency: Some(7_074_123),
            ..Default::default()
        })
    }
    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
        self.record(format!("set_frequency {vfo:?} {freq}"));
        Err(anyhow!("set_frequency sentinel"))
    }
    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        self.record(format!("get_frequency {vfo:?}"));
        Ok(14_074_321)
    }
    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()> {
        self.record(format!("set_mode {vfo:?} {mode:?} {width:?}"));
        Err(anyhow!("set_mode sentinel"))
    }
    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
        self.record(format!("get_mode {vfo:?}"));
        Ok((Mode::LSB, 2_345))
    }
    async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
        self.record(format!("set_vfo {vfo:?}"));
        Err(anyhow!("set_vfo sentinel"))
    }
    async fn get_vfo(&self) -> Result<Vfo> {
        self.record("get_vfo".into());
        Ok(Vfo::B)
    }
    async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()> {
        self.record(format!("set_ptt {vfo:?} {ptt:?}"));
        Ok(())
    }
    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
        self.record(format!("get_ptt {vfo:?}"));
        Err(anyhow!("get_ptt sentinel"))
    }
    fn connection_generation(&self) -> u64 {
        self.record("connection_generation".into());
        4_242
    }
    async fn get_attenuator(&self) -> Result<i32> {
        self.record("get_attenuator".into());
        Ok(23)
    }
    async fn set_attenuator(&self, db: i32) -> Result<()> {
        self.record(format!("set_attenuator {db}"));
        Err(anyhow!("set_attenuator sentinel"))
    }
    async fn supported_attenuator_steps(&self) -> Result<Vec<i32>> {
        self.record("supported_attenuator_steps".into());
        Ok(vec![0, 6, 23])
    }
    async fn set_power_level(&self, level: f32) -> Result<()> {
        self.record(format!("set_power_level {level}"));
        Err(anyhow!("set_power_level sentinel"))
    }
    async fn get_power_level(&self) -> Result<f32> {
        self.record("get_power_level".into());
        Ok(0.37)
    }
    async fn set_preamp(&self, preamp_db: i32) -> Result<()> {
        self.record(format!("set_preamp {preamp_db}"));
        Err(anyhow!("set_preamp sentinel"))
    }
    async fn get_preamp(&self) -> Result<i32> {
        self.record("get_preamp".into());
        Ok(17)
    }
    async fn get_s_meter(&self) -> Result<i32> {
        self.record("get_s_meter".into());
        Ok(-73)
    }
    async fn get_swr(&self) -> Result<f32> {
        self.record("get_swr".into());
        Ok(1.7)
    }
    async fn reports_swr(&self) -> Result<bool> {
        self.record("reports_swr".into());
        Ok(true)
    }
    async fn get_tx_meter(&self, meter: TxMeter) -> Result<f32> {
        self.record(format!("get_tx_meter {meter:?}"));
        Ok(0.41)
    }
    async fn tx_meter_levels(&self) -> Result<TxMeterLevels> {
        self.record("tx_meter_levels".into());
        Ok(TxMeterLevels {
            alc: true,
            forward_power_watts: false,
            forward_power_fraction: true,
        })
    }
    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
        self.record(format!("set_memory_channel {vfo:?} {channel}"));
        Err(anyhow!("set_memory_channel sentinel"))
    }
    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
        self.record(format!("get_memory_channel {vfo:?}"));
        Ok(61)
    }
    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        self.record(format!("set_scan {vfo:?} {enable}"));
        Err(anyhow!("set_scan sentinel"))
    }
    async fn get_info(&self) -> Result<String> {
        self.record("get_info".into());
        Ok("recording rig".into())
    }
    async fn set_split(&self, enabled: bool, tx_vfo: Vfo) -> Result<()> {
        self.record(format!("set_split {enabled} {tx_vfo:?}"));
        Err(anyhow!("set_split sentinel"))
    }
    async fn get_split(&self) -> Result<(bool, Vfo)> {
        self.record("get_split".into());
        Ok((true, Vfo::B))
    }
    async fn get_split_freq(&self) -> Result<u64> {
        self.record("get_split_freq".into());
        Ok(14_076_555)
    }
    async fn set_split_freq(&self, tx_freq: u64) -> Result<()> {
        self.record(format!("set_split_freq {tx_freq}"));
        Err(anyhow!("set_split_freq sentinel"))
    }
}

fn err_text<T: std::fmt::Debug>(r: Result<T>) -> String {
    format!("{:#}", r.unwrap_err())
}

#[tokio::test]
async fn line_keyed_rig_forwards_every_other_rig_control_method() {
    let inner = Arc::new(RecordingRig::default());
    let line = FakeLines::default().line();
    let rig = LineKeyedRig::new(inner.clone(), line);
    let deadline = tokio::time::Instant::now();
    let gain = ReceiveGain {
        preamp_db: 10,
        attenuator_db: 12,
    };

    // The 16 methods with default bodies.
    assert_eq!(
        rig.receive_gain_capabilities().await.unwrap(),
        ReceiveGainCapabilities {
            preamp_db: vec![0, 17],
            attenuator_db: vec![0, 23],
        }
    );
    assert_eq!(
        rig.get_receive_gain().await.unwrap(),
        ReceiveGain {
            preamp_db: 17,
            attenuator_db: 23,
        }
    );
    assert_eq!(
        err_text(rig.set_receive_gain(gain, deadline).await),
        "set_receive_gain sentinel"
    );
    assert_eq!(rig.connection_generation(), 4_242);
    assert_eq!(rig.get_attenuator().await.unwrap(), 23);
    assert_eq!(
        err_text(rig.set_attenuator(6).await),
        "set_attenuator sentinel"
    );
    assert_eq!(
        rig.supported_attenuator_steps().await.unwrap(),
        vec![0, 6, 23]
    );
    assert_eq!(err_text(rig.set_preamp(20).await), "set_preamp sentinel");
    assert_eq!(rig.get_preamp().await.unwrap(), 17);
    assert!(rig.reports_swr().await.unwrap());
    assert_eq!(rig.get_tx_meter(TxMeter::Alc).await.unwrap(), 0.41);
    assert_eq!(
        rig.tx_meter_levels().await.unwrap(),
        TxMeterLevels {
            alc: true,
            forward_power_watts: false,
            forward_power_fraction: true,
        }
    );
    assert_eq!(
        err_text(rig.set_split(true, Vfo::B).await),
        "set_split sentinel"
    );
    assert_eq!(rig.get_split().await.unwrap(), (true, Vfo::B));
    assert_eq!(rig.get_split_freq().await.unwrap(), 14_076_555);
    assert_eq!(
        err_text(rig.set_split_freq(14_076_000).await),
        "set_split_freq sentinel"
    );

    // The 17 required methods other than set_ptt/get_ptt.
    assert_eq!(err_text(rig.connect().await), "connect sentinel");
    assert_eq!(err_text(rig.disconnect().await), "disconnect sentinel");
    assert_eq!(rig.get_status().await.unwrap().frequency, Some(7_074_123));
    assert_eq!(
        err_text(rig.set_frequency(Vfo::A, 7_074_000).await),
        "set_frequency sentinel"
    );
    assert_eq!(rig.get_frequency(Vfo::A).await.unwrap(), 14_074_321);
    assert_eq!(
        err_text(rig.set_mode(Vfo::A, Mode::USB, Some(3_000)).await),
        "set_mode sentinel"
    );
    assert_eq!(rig.get_mode(Vfo::B).await.unwrap(), (Mode::LSB, 2_345));
    assert_eq!(err_text(rig.set_vfo(Vfo::B).await), "set_vfo sentinel");
    assert_eq!(rig.get_vfo().await.unwrap(), Vfo::B);
    assert_eq!(
        err_text(rig.set_power_level(0.5).await),
        "set_power_level sentinel"
    );
    assert_eq!(rig.get_power_level().await.unwrap(), 0.37);
    assert_eq!(rig.get_s_meter().await.unwrap(), -73);
    assert_eq!(rig.get_swr().await.unwrap(), 1.7);
    assert_eq!(
        err_text(rig.set_memory_channel(Vfo::Memory, 12).await),
        "set_memory_channel sentinel"
    );
    assert_eq!(rig.get_memory_channel(Vfo::Memory).await.unwrap(), 61);
    assert_eq!(
        err_text(rig.set_scan(Vfo::A, true).await),
        "set_scan sentinel"
    );
    assert_eq!(rig.get_info().await.unwrap(), "recording rig");

    assert_eq!(
        inner.log(),
        vec![
            "receive_gain_capabilities".to_string(),
            "get_receive_gain".into(),
            format!("set_receive_gain {gain:?}"),
            "connection_generation".into(),
            "get_attenuator".into(),
            "set_attenuator 6".into(),
            "supported_attenuator_steps".into(),
            "set_preamp 20".into(),
            "get_preamp".into(),
            "reports_swr".into(),
            "get_tx_meter Alc".into(),
            "tx_meter_levels".into(),
            "set_split true B".into(),
            "get_split".into(),
            "get_split_freq".into(),
            "set_split_freq 14076000".into(),
            "connect".into(),
            "disconnect".into(),
            "get_status".into(),
            "set_frequency A 7074000".into(),
            "get_frequency A".into(),
            "set_mode A USB Some(3000)".into(),
            "get_mode B".into(),
            "set_vfo B".into(),
            "get_vfo".into(),
            "set_power_level 0.5".into(),
            "get_power_level".into(),
            "get_s_meter".into(),
            "get_swr".into(),
            "set_memory_channel Memory 12".into(),
            "get_memory_channel Memory".into(),
            "set_scan A true".into(),
            "get_info".into(),
        ],
        "33 forwarded calls, each once, with the caller's arguments"
    );
}

#[tokio::test]
async fn set_ptt_refuses_a_key_down_while_data_mode_is_not_ready() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    let blocked = pancetta_hamlib::DataModeReadiness::new(true);
    let rig = LineKeyedRig::new(connected_mock().await, line.clone()).with_data_mode(blocked);
    let err = rig.set_ptt(Vfo::Current, PttState::On).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("data mode is not ready"),
        "{err:#}"
    );
    assert_eq!(fake.rts_levels(), vec![false], "RTS never raised");
    rig.set_ptt(Vfo::Current, PttState::Off).await.unwrap();

    let disabled = pancetta_hamlib::DataModeReadiness::new(false);
    let rig = LineKeyedRig::new(connected_mock().await, line.clone()).with_data_mode(disabled);
    rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();
    assert_eq!(fake.rts_levels().last(), Some(&true));
}

#[tokio::test]
async fn configure_keeps_an_asserted_line_it_cannot_lower() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.set(true).await.unwrap();

    // The low write and the lowering reopen both fail.
    fake.fail_writes.store(1, Ordering::Release);
    fake.fail_opens.store(1, Ordering::Release);
    let err = line
        .configure(Some("/dev/fake-ptt2".into()), CAT)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("RTS may still be asserted"),
        "{err:#}"
    );
    assert!(line.is_asserted(), "still reported keyed");
    assert!(line.has_device());

    // The next unkey reopens the old port to lower it, then applies the
    // deferred switch to the new port.
    line.set(false).await.unwrap();
    let events = fake.events();
    assert_eq!(
        events[events.len() - 6..],
        [
            open(DEV),
            rts(DEV, false),
            rts(DEV, false),
            close(DEV),
            open("/dev/fake-ptt2"),
            rts("/dev/fake-ptt2", false)
        ],
        "{events:?}"
    );
    assert!(!line.is_asserted());
}

#[tokio::test]
async fn configure_after_shutdown_never_opens_a_port() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.release_for_shutdown().await.unwrap();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    assert_eq!(fake.opens(), 0);
    assert!(line.set(true).await.is_err());
}

/// PAN-357 review: under a managed data mode the inner client never sees the
/// RTS key-down, so the wrapper refuses its mode and VFO mutations while the
/// line may be asserted, and drops the line before a reconnect prepares the
/// mode, as `RigctldClient` does for a CAT key-down.
#[tokio::test]
async fn data_mode_refuses_cat_mutations_while_the_line_is_keyed() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    let inner = connected_mock().await;
    let ready = pancetta_hamlib::DataModeReadiness::new(false);
    let rig = LineKeyedRig::new(inner.clone(), line.clone()).with_data_mode(ready);

    rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();
    for refused in [
        err_text(rig.set_frequency(Vfo::A, 21_074_000).await),
        err_text(rig.set_vfo(Vfo::B).await),
        err_text(rig.set_memory_channel(Vfo::Memory, 3).await),
        err_text(rig.set_split(true, Vfo::B).await),
        err_text(rig.set_split_freq(14_075_000).await),
    ] {
        assert!(refused.contains("while keyed"), "{refused}");
    }
    assert_ne!(inner.get_frequency(Vfo::A).await.unwrap(), 21_074_000);

    // A reconnect drops the line first.
    rig.connect().await.unwrap();
    assert_eq!(fake.rts_levels().last(), Some(&false));
    assert!(!line.is_asserted());

    // Unkeyed, every mutation reaches the inner rig.
    rig.set_frequency(Vfo::A, 21_074_000).await.unwrap();
    assert_eq!(inner.get_frequency(Vfo::A).await.unwrap(), 21_074_000);

    // Without a managed data mode nothing is refused while keyed.
    let plain = LineKeyedRig::new(inner.clone(), line.clone());
    plain.set_ptt(Vfo::Current, PttState::On).await.unwrap();
    plain.set_frequency(Vfo::A, 7_074_000).await.unwrap();
    assert_eq!(inner.get_frequency(Vfo::A).await.unwrap(), 7_074_000);
    plain.set_ptt(Vfo::Current, PttState::Off).await.unwrap();
}

/// PAN-357 hand-landing: a line dropped while keyed (the coordinator unwinding
/// after a panic, or any path that skips `release_for_shutdown`) must lower
/// RTS before its handle closes, never leave the rig keyed.
#[tokio::test]
async fn dropping_the_line_lowers_rts_before_closing() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.set(true).await.unwrap();
    drop(line);
    let events = fake.events();
    assert_eq!(
        events[events.len() - 2..],
        [rts(DEV, false), close(DEV)],
        "RTS low, then close: {events:?}"
    );
}

/// PAN-357 hand-landing: a write that fails mid-transmission (adapter
/// unplugged, ioctl error) must not leave the line keyed once the port is
/// back: the failed unkey is retried and the reopen writes RTS low.
#[tokio::test]
async fn failed_unkey_write_mid_transmission_is_lowered_on_retry() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    let rig = LineKeyedRig::new(connected_mock().await, line.clone());
    rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();

    fake.fail_writes.store(1, Ordering::Release);
    assert!(rig.set_ptt(Vfo::Current, PttState::Off).await.is_err());
    assert_eq!(
        rig.get_ptt(Vfo::Current).await.unwrap(),
        PttState::On,
        "a failed unkey is reported as still keyed, so the watchdog retries"
    );
    rig.set_ptt(Vfo::Current, PttState::Off).await.unwrap();
    assert_eq!(fake.events().last(), Some(&rts(DEV, false)));
    assert_eq!(rig.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);
}

/// PAN-357 hand-landing: the real opener, on a device that does not exist,
/// fails with the device named and asserts nothing.
#[test]
fn real_open_of_a_missing_device_fails_and_names_it() {
    let device = "/nonexistent/pancetta-ptt-test";
    let err = match open_serial_rts_port(device) {
        Ok(_) => panic!("a missing device must not open"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.starts_with(&format!("cannot open serial PTT port {device}: ")),
        "{err}"
    );
}

/// PAN-357 hand-landing: a device the operator may not open (permission
/// denied, e.g. not in the `dialout` group) fails with the cause named.
#[cfg(unix)]
#[test]
fn real_open_of_an_unreadable_device_reports_permission_denied() {
    use std::os::unix::fs::PermissionsExt;
    // Root ignores file modes, so there is nothing to observe.
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("ttyFAKE");
    std::fs::write(&device, b"").unwrap();
    std::fs::set_permissions(&device, std::fs::Permissions::from_mode(0o000)).unwrap();
    let device = device.to_str().unwrap().to_string();
    let err = match open_serial_rts_port(&device) {
        Ok(_) => panic!("an unreadable device must not open"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.starts_with(&format!("cannot open serial PTT port {device}: ")),
        "{err}"
    );
    assert!(err.to_lowercase().contains("permission denied"), "{err}");
}

/// PAN-357 hand-landing: the opened port must carry HUPCL, so the kernel
/// drops RTS (and DTR) on the last close of the fd. That is what unkeys the
/// rig when the process dies without running any Rust code (SIGKILL, abort,
/// OOM kill). Exercised on a pty, which has termios but no modem lines.
#[cfg(unix)]
#[test]
fn hang_up_on_close_is_set_on_the_ptt_fd() {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: both out-pointers are valid `c_int`s; the optional name,
    // termios and winsize arguments may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // Start from a tty with HUPCL clear, as some drivers and udev rules leave it.
    let mut t = tty_attributes(slave).unwrap();
    t.c_cflag &= !libc::HUPCL;
    // SAFETY: `slave` is the open pty from `openpty`, and `t` came from
    // `tcgetattr` on it.
    let rc = unsafe { libc::tcsetattr(slave, libc::TCSANOW, &t) };
    assert_eq!(rc, 0);
    assert_eq!(tty_attributes(slave).unwrap().c_cflag & libc::HUPCL, 0);

    set_hang_up_on_close(slave).unwrap();
    assert_ne!(
        tty_attributes(slave).unwrap().c_cflag & libc::HUPCL,
        0,
        "HUPCL must be set"
    );
    for fd in [slave, master] {
        // SAFETY: each fd came from `openpty` above and is closed once.
        unsafe { libc::close(fd) };
    }
}

/// PR #745 review (Codex P1): a switch away from a line that cannot be
/// lowered must not let a key-down assert the old device; the first
/// confirmed low applies the switch.
#[tokio::test]
async fn stuck_switch_refuses_key_down_and_applies_on_confirmed_low() {
    const DEV2: &str = "/dev/fake-ptt2";
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.set(true).await.unwrap();
    fake.fail_writes.store(1, Ordering::Release);
    fake.fail_opens.store(1, Ordering::Release);
    assert!(line.configure(Some(DEV2.into()), CAT).await.is_err());

    let err = line.set(true).await.unwrap_err();
    assert!(format!("{err:#}").contains("refusing to key"), "{err:#}");
    assert_eq!(fake.opens(), 1, "a refused key-down opens nothing");

    line.set(false).await.unwrap();
    assert!(!line.is_asserted());
    let events = fake.events();
    // Reopen lowers DEV; the switch writes low once more before closing it.
    assert_eq!(
        events[events.len() - 6..],
        [
            open(DEV),
            rts(DEV, false),
            rts(DEV, false),
            close(DEV),
            open(DEV2),
            rts(DEV2, false)
        ],
        "old port lowered, then the deferred switch: {events:?}"
    );
    line.set(true).await.unwrap();
    assert_eq!(fake.events().last(), Some(&rts(DEV2, true)));
}

/// PR #745 review (Codex P1): the same when the new settings key by CAT.
#[tokio::test]
async fn stuck_switch_to_cat_keeps_the_line_until_confirmed_low() {
    let fake = FakeLines::default();
    let line = fake.line();
    line.configure(Some(DEV.into()), CAT).await.unwrap();
    line.set(true).await.unwrap();
    fake.fail_writes.store(1, Ordering::Release);
    fake.fail_opens.store(1, Ordering::Release);
    assert!(line.configure(None, CAT).await.is_err());
    assert!(line.has_device(), "the possibly asserted line is retained");
    assert!(line.set(true).await.is_err());

    line.set(false).await.unwrap();
    assert_eq!(fake.rts_levels().last(), Some(&false));
    assert!(!line.has_device(), "the switch to CAT applied once low");
}

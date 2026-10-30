//! Receive-gain retunes over the real bus, invoking the production apply helper.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use pancetta_config::{rig::ReceiveGainPreset, Config};
use pancetta_core::Band;
use pancetta_hamlib::{MockRig, MockRigConfig, Mode, PttState, RigControl, RigStatus, Vfo};
use pancetta_lib::coordinator::apply_receive_gain_preset;
use pancetta_lib::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};
use tokio::sync::RwLock;

struct RecordingRig {
    mock: MockRig,
    calls: Mutex<Vec<String>>,
    fail_preamp: bool,
}

impl RecordingRig {
    async fn new(fail_preamp: bool) -> Self {
        let mock = MockRig::new(MockRigConfig {
            connection_delay_ms: 0,
            operation_delay_ms: 0,
            ..Default::default()
        });
        mock.connect().await.unwrap();
        Self {
            mock,
            calls: Mutex::new(Vec::new()),
            fail_preamp,
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RigControl for RecordingRig {
    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
        self.calls.lock().unwrap().push(format!("frequency {freq}"));
        self.mock.set_frequency(vfo, freq).await
    }
    async fn set_preamp(&self, db: i32) -> Result<()> {
        self.calls.lock().unwrap().push(format!("preamp {db}"));
        if self.fail_preamp {
            anyhow::bail!("preamp deliberately failed");
        }
        self.mock.set_preamp(db).await
    }
    async fn set_attenuator(&self, db: i32) -> Result<()> {
        self.calls.lock().unwrap().push(format!("attenuator {db}"));
        self.mock.set_attenuator(db).await
    }
    async fn connect(&self) -> Result<()> {
        self.mock.connect().await
    }
    async fn disconnect(&self) -> Result<()> {
        self.mock.disconnect().await
    }
    async fn get_status(&self) -> Result<RigStatus> {
        self.mock.get_status().await
    }
    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        self.mock.get_frequency(vfo).await
    }
    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()> {
        self.mock.set_mode(vfo, mode, width).await
    }
    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
        self.mock.get_mode(vfo).await
    }
    async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
        self.mock.set_vfo(vfo).await
    }
    async fn get_vfo(&self) -> Result<Vfo> {
        self.mock.get_vfo().await
    }
    async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()> {
        self.mock.set_ptt(vfo, ptt).await
    }
    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
        self.mock.get_ptt(vfo).await
    }
    async fn set_power_level(&self, level: f32) -> Result<()> {
        self.mock.set_power_level(level).await
    }
    async fn get_power_level(&self) -> Result<f32> {
        self.mock.get_power_level().await
    }
    async fn get_preamp(&self) -> Result<i32> {
        self.mock.get_preamp().await
    }
    async fn get_attenuator(&self) -> Result<i32> {
        self.mock.get_attenuator().await
    }
    async fn get_s_meter(&self) -> Result<i32> {
        self.mock.get_s_meter().await
    }
    async fn get_swr(&self) -> Result<f32> {
        self.mock.get_swr().await
    }
    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
        self.mock.set_memory_channel(vfo, channel).await
    }
    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
        self.mock.get_memory_channel(vfo).await
    }
    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        self.mock.set_scan(vfo, enable).await
    }
    async fn get_info(&self) -> Result<String> {
        self.mock.get_info().await
    }
}

fn presets() -> Arc<RwLock<Config>> {
    let mut config = Config::default();
    config.rig.receive_gain_presets = Some(HashMap::from([
        (
            "10m".to_owned(),
            ReceiveGainPreset {
                preamp_db: 10,
                attenuator_db: 6,
            },
        ),
        (
            "20m".to_owned(),
            ReceiveGainPreset {
                preamp_db: 0,
                attenuator_db: 0,
            },
        ),
    ]));
    Arc::new(RwLock::new(config))
}

/// Like the production Hamlib command loop, successful SetFrequency is followed
/// immediately by the production helper before the next command is consumed.
async fn retune(
    rig: &RecordingRig,
    config: &Arc<RwLock<Config>>,
    last: &mut Option<Band>,
    hz: u64,
) {
    let bus = MessageBus::new(16).unwrap();
    let (_, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    bus.send_message(ComponentMessage::new(
        ComponentId::Autonomous,
        ComponentId::Hamlib,
        MessageType::RigControl(RigControlMessage::SetFrequency {
            vfo: 0,
            frequency: hz,
        }),
        Instant::now(),
    ))
    .await
    .unwrap();
    let message = rx.try_recv().unwrap();
    let MessageType::RigControl(RigControlMessage::SetFrequency { frequency, .. }) =
        message.message_type
    else {
        panic!("unexpected bus message")
    };
    rig.set_frequency(Vfo::Current, frequency).await.unwrap();
    apply_receive_gain_preset(rig, config, last, frequency).await;
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn configured_crossing_applies_after_frequency_and_logs_band_and_gain() {
    use tracing::instrument::WithSubscriber;
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    async {
        let rig = RecordingRig::new(false).await;
        let mut last = Some(Band::Band20m);
        retune(&rig, &presets(), &mut last, 28_074_000).await;
        assert_eq!(
            rig.calls(),
            ["frequency 28074000", "preamp 10", "attenuator 6"]
        );
        assert_eq!(rig.get_preamp().await.unwrap(), 10);
        assert_eq!(rig.get_attenuator().await.unwrap(), 6);
        assert_eq!(last, Some(Band::Band10m));
    }
    .with_subscriber(subscriber)
    .await;
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains(
            "Band change to 10m: applied receive-gain preset (preamp AMP1/10 dB, attenuator 6 dB)"
        ),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn absent_preset_leaves_current_gain_untouched() {
    let rig = RecordingRig::new(false).await;
    rig.mock.set_preamp(20).await.unwrap();
    rig.mock.set_attenuator(12).await.unwrap();
    let mut last = Some(Band::Band20m);
    retune(&rig, &presets(), &mut last, 7_074_000).await;
    assert_eq!(rig.calls(), ["frequency 7074000"]);
    assert_eq!(rig.get_preamp().await.unwrap(), 20);
    assert_eq!(rig.get_attenuator().await.unwrap(), 12);
}

#[tokio::test(flavor = "current_thread")]
async fn same_band_and_unknown_startup_never_apply_presets() {
    let rig = RecordingRig::new(false).await;
    let config = presets();
    let mut last = Some(Band::Band20m);
    retune(&rig, &config, &mut last, 14_078_000).await;
    last = None;
    retune(&rig, &config, &mut last, 28_074_000).await;
    assert_eq!(rig.calls(), ["frequency 14078000", "frequency 28074000"]);
}

#[tokio::test(flavor = "current_thread")]
async fn apply_failure_keeps_retune_and_advances_tracker_without_retry() {
    use tracing::instrument::WithSubscriber;
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    async {
        let rig = RecordingRig::new(true).await;
        let config = presets();
        let mut last = Some(Band::Band20m);
        retune(&rig, &config, &mut last, 28_074_000).await;
        assert_eq!(last, Some(Band::Band10m));
        assert_eq!(rig.get_frequency(Vfo::Current).await.unwrap(), 28_074_000);
        retune(&rig, &config, &mut last, 28_078_000).await;
        assert_eq!(
            rig.calls()
                .iter()
                .filter(|call| *call == "preamp 10")
                .count(),
            1
        );
    }
    .with_subscriber(subscriber)
    .await;
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("WARN") && text.contains("preamp deliberately failed"),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn saved_live_gain_survives_reload_and_applies_without_restart() {
    let rig = RecordingRig::new(false).await;
    rig.mock.set_preamp(20).await.unwrap();
    rig.mock.set_attenuator(0).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[station]\ncallsign = \"K1ABC\"\ngrid_square = \"FN31\"\n",
    )
    .unwrap();
    let config = presets();
    let preset = ReceiveGainPreset {
        preamp_db: rig.get_preamp().await.unwrap(),
        attenuator_db: rig.get_attenuator().await.unwrap(),
    };
    let mut map = config
        .read()
        .await
        .rig
        .receive_gain_presets
        .clone()
        .unwrap();
    map.insert("15m".into(), preset.clone());
    config
        .read()
        .await
        .set_rig_receive_gain_presets_in_file(&path, &map)
        .unwrap();
    config.write().await.rig.receive_gain_presets = Some(map);
    let reloaded = Config::load_from_file(&path).unwrap();
    assert_eq!(reloaded.rig.receive_gain_presets.unwrap()["15m"], preset);
    rig.mock.set_preamp(0).await.unwrap();
    let mut last = Some(Band::Band20m);
    retune(&rig, &config, &mut last, 21_074_000).await;
    assert_eq!(
        rig.calls(),
        ["frequency 21074000", "preamp 20", "attenuator 0"]
    );
    assert_eq!(rig.get_preamp().await.unwrap(), 20);
}

//! The contest serial follows the process data directory (PAN-376).
//!
//! This binary holds exactly one test because it sets the process-wide data
//! directory (`pancetta_core::data_dir`); a second test in the same binary
//! would see its value.

use pancetta_core::time::UtcTimestamp;
use pancetta_qso::{ContestExchangeConfig, ExchangeFormat, MessageExchange, MessageType, QsoState};

#[test]
fn contest_serial_follows_the_process_data_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("pancetta-40m");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let serial_file = data_dir.join("contest_serial.txt");
    std::fs::write(&serial_file, "42").expect("seed serial");

    pancetta_core::data_dir::set_process_data_dir(data_dir.clone())
        .expect("the process data dir is set once");

    let mut exchange = MessageExchange::new("K5ARH".to_string());
    exchange.set_contest_mode(ContestExchangeConfig {
        contest_name: "PAN-376 TEST".to_string(),
        exchange_format: ExchangeFormat::RstSerial,
        our_exchange: "599 001".to_string(),
    });

    let state = QsoState::RespondingToCq {
        target_callsign: "K5TD".to_string(),
        frequency: 14_074_000.0,
        started_at: UtcTimestamp::now(),
    };
    let incoming = MessageType::ContestExchange {
        to_station: "K5ARH".to_string(),
        from_station: "K5TD".to_string(),
        report: -10,
        serial: 7,
    };

    match exchange
        .generate_response(&state, &incoming, None)
        .expect("a contest response")
    {
        Some(MessageType::ContestExchange { serial, .. }) => {
            assert_eq!(
                serial, 42,
                "the serial is loaded from the data directory's contest_serial.txt"
            );
        }
        other => panic!("expected a contest exchange response, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&serial_file).expect("read serial"),
        "43",
        "the successor is persisted in the data directory"
    );
}

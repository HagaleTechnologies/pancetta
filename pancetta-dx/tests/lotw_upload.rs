#![cfg(unix)]
use pancetta_dx::{LotwUploadClient, QsoUploadOutcome};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn pan300_lotw_executes_expected_argv() {
    let dir = tempfile::tempdir().unwrap();
    let bin_dir = dir.path().join("fake bin with spaces");
    fs::create_dir(&bin_dir).unwrap();
    let bin = bin_dir.join("tqsl");
    fs::write(&bin, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PAN300_DIR/argv\"\nfor arg do last=\"$arg\"; done\ncp \"$last\" \"$PAN300_DIR/input\"\nexit 0\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    for password in [None, Some(""), Some("certificate with spaces")] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pan300_child", "--nocapture"])
            .env(
                "PATH",
                format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap()),
            )
            .env("PAN300_DIR", dir.path())
            .env("PAN300_PASSWORD", password.unwrap_or(""))
            .env(
                "PAN300_USE_BUILDER",
                if password.is_some() { "1" } else { "0" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("PAN300_EXECUTED"));
        let argv = fs::read_to_string(dir.path().join("argv")).unwrap();
        let args: Vec<_> = argv.lines().collect();
        assert_eq!(
            &args[..7],
            &["-a", "compliant", "-d", "-x", "-u", "-l", "Home Station"]
        );
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            assert_eq!(&args[7..9], &["-p", p]);
        } else {
            assert_eq!(args.len(), 8);
        }
        assert!(!std::path::Path::new(args.last().unwrap()).exists());
        let input = fs::read_to_string(dir.path().join("input")).unwrap();
        assert!(input.contains("<ADIF_VER:5>3.1.4"));
        assert!(input.contains("<PROGRAMID:8>Pancetta"));
        assert!(input.contains("<EOH>") && input.contains("<CALL:5>K1ABC<EOR>"));
    }
}

#[tokio::test]
async fn pan300_child() {
    if std::env::var_os("PAN300_DIR").is_none() {
        return;
    }
    let client = LotwUploadClient::new("tqsl", "Home Station");
    let client = if std::env::var("PAN300_USE_BUILDER").unwrap() == "1" {
        client.with_password(std::env::var("PAN300_PASSWORD").unwrap())
    } else {
        client
    };
    assert_eq!(
        client.upload_adif("<CALL:5>K1ABC<EOR>").await.unwrap(),
        QsoUploadOutcome::Logged
    );
    println!("PAN300_EXECUTED");
}

#[tokio::test]
async fn pan300_lotw_duplicate_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    for code in [8, 9] {
        let bin = dir.path().join("fake tqsl");
        fs::write(&bin, format!("#!/bin/sh\nexit {code}\n")).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        let client = LotwUploadClient::new(bin.to_str().unwrap(), "Home Station");
        assert_eq!(
            client.upload_adif("<CALL:5>K1ABC<EOR>").await.unwrap(),
            QsoUploadOutcome::Duplicate
        );
    }
}

#[tokio::test]
async fn pan300_lotw_failure_does_not_echo_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("tqsl");
    let recorded = dir.path().join("input-path");
    fs::write(&bin, format!("#!/bin/sh\nfor arg do last=\"$arg\"; done\nprintf '%s' \"$last\" > '{}'\necho 'secret certificate'\necho 'secret certificate' >&2\nexit 1\n", recorded.display())).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    let client =
        LotwUploadClient::new(bin.to_str().unwrap(), "Home").with_password("secret certificate");
    let error = client.upload_adif("<CALL:5>K1ABC<EOR>").await.unwrap_err();
    assert!(!error.to_string().contains("secret certificate"));
    assert!(error.to_string().contains("exit status: 1"));
    let input_path = fs::read_to_string(recorded).unwrap();
    assert!(!std::path::Path::new(&input_path).exists());
    let missing = LotwUploadClient::new(dir.path().join("missing").to_str().unwrap(), "Home");
    assert!(missing.upload_adif("<CALL:5>K1ABC<EOR>").await.is_err());
}

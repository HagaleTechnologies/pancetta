//! Pre-runtime macOS responsibility selection and a single-threaded thin parent.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug)]
pub(crate) enum Outcome {
    NotMacOs,
    NotNeeded,
    DisabledByEnv,
    MissingUsageDescription,
    SymbolsMissing,
    LookupFailed(String),
    AppLauncher(PathBuf),
    SelfResponsible,
    RelaunchedChild {
        parent: i32,
    },
    RelaunchedButNotResponsible {
        responsible: i32,
        path: Option<PathBuf>,
    },
    SpawnFailed(i32),
}

impl Outcome {
    pub(crate) fn identity_is_own(&self) -> bool {
        matches!(self, Self::SelfResponsible | Self::RelaunchedChild { .. })
    }
}

static OUTCOME: OnceLock<Outcome> = OnceLock::new();
pub(crate) fn prepare(opens_microphone: bool) {
    let _ = OUTCOME.set(compute_outcome(opens_microphone));
}
pub(crate) fn outcome() -> &'static Outcome {
    OUTCOME.get().unwrap_or(&Outcome::NotMacOs)
}

#[cfg(any(target_os = "macos", test))]
struct Probe {
    opens_microphone: bool,
    relaunch_disabled: bool,
    has_usage_description: bool,
    symbols_found: bool,
    own_pid: i32,
    responsible: Option<(i32, Option<PathBuf>)>,
}
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    NotNeeded,
    DisabledByEnv,
    MissingUsageDescription,
    SymbolsMissing,
    LookupFailed,
    SelfResponsible,
    AppLauncher(PathBuf),
    Relaunch,
}
#[cfg(any(target_os = "macos", test))]
fn is_app_bundle_executable(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .map(|v| v.as_os_str().to_string_lossy())
        .collect();
    parts.windows(3).any(|p| {
        p[0].to_ascii_lowercase().ends_with(".app") && p[1] == "Contents" && p[2] == "MacOS"
    })
}
#[cfg(any(target_os = "macos", test))]
fn decide(probe: &Probe) -> Decision {
    if !probe.opens_microphone {
        return Decision::NotNeeded;
    }
    if probe.relaunch_disabled {
        return Decision::DisabledByEnv;
    }
    if !probe.has_usage_description {
        return Decision::MissingUsageDescription;
    }
    if !probe.symbols_found {
        return Decision::SymbolsMissing;
    }
    let Some((pid, path)) = &probe.responsible else {
        return Decision::LookupFailed;
    };
    if *pid <= 0 {
        return Decision::LookupFailed;
    }
    if *pid == probe.own_pid {
        return Decision::SelfResponsible;
    }
    let Some(path) = path else {
        return Decision::LookupFailed;
    };
    if is_app_bundle_executable(path) {
        Decision::AppLauncher(path.clone())
    } else {
        Decision::Relaunch
    }
}

#[cfg(not(target_os = "macos"))]
fn compute_outcome(_: bool) -> Outcome {
    Outcome::NotMacOs
}

#[cfg(any(target_os = "macos", all(test, unix)))]
use supervisor::{spawn_and_supervise, SetDisclaim, SpawnSpec};

#[cfg(target_os = "macos")]
type ResponsibleFor = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;
#[cfg(target_os = "macos")]
fn resolve_symbols() -> Option<(SetDisclaim, ResponsibleFor)> {
    // SAFETY: the named libSystem symbols have these C signatures. Null is checked before conversion.
    unsafe {
        let disclaim = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_spawnattrs_setdisclaim".as_ptr(),
        );
        let responsible = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_get_pid_responsible_for_pid".as_ptr(),
        );
        if disclaim.is_null() || responsible.is_null() {
            return None;
        }
        Some((
            std::mem::transmute::<*mut libc::c_void, SetDisclaim>(disclaim),
            std::mem::transmute::<*mut libc::c_void, ResponsibleFor>(responsible),
        ))
    }
}
#[cfg(target_os = "macos")]
fn responsible_for(pid: i32, query: ResponsibleFor) -> i32 {
    // SAFETY: query is a checked libSystem symbol and pid is passed by value.
    unsafe { query(pid) }
}
#[cfg(target_os = "macos")]
fn pid_path(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buf is writable for the supplied length.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(n as usize);
    buf.truncate(end);
    Some(std::ffi::OsString::from_vec(buf).into())
}
#[cfg(target_os = "macos")]
fn compute_outcome(opens_microphone: bool) -> Outcome {
    let marker = std::env::var("PANCETTA_MACOS_RELAUNCHED").ok();
    // Called before any runtime or worker threads exist.
    std::env::remove_var("PANCETTA_MACOS_RELAUNCHED");
    if !opens_microphone {
        return Outcome::NotNeeded;
    }
    if std::env::var("PANCETTA_MACOS_RELAUNCH").as_deref() == Ok("0") {
        return Outcome::DisabledByEnv;
    }
    if !crate::mic_access::embedded_identity().is_some_and(|v| v.has_usage_description) {
        return Outcome::MissingUsageDescription;
    }
    let symbols = resolve_symbols();
    // SAFETY: getpid has no preconditions.
    let own_pid = unsafe { libc::getpid() };
    let responsible = symbols.map(|(_, query)| responsible_for(own_pid, query));
    if let Some(parent) = marker {
        return if responsible == Some(own_pid) {
            Outcome::RelaunchedChild {
                parent: parent.parse().unwrap_or(0),
            }
        } else {
            let pid = responsible.unwrap_or(0);
            Outcome::RelaunchedButNotResponsible {
                responsible: pid,
                path: pid_path(pid),
            }
        };
    }
    let probe = Probe {
        opens_microphone,
        relaunch_disabled: false,
        has_usage_description: true,
        symbols_found: symbols.is_some(),
        own_pid,
        responsible: responsible.map(|pid| (pid, pid_path(pid))),
    };
    match decide(&probe) {
        Decision::NotNeeded => Outcome::NotNeeded,
        Decision::DisabledByEnv => Outcome::DisabledByEnv,
        Decision::MissingUsageDescription => Outcome::MissingUsageDescription,
        Decision::SymbolsMissing => Outcome::SymbolsMissing,
        Decision::LookupFailed => {
            Outcome::LookupFailed("responsible pid or executable path unavailable".into())
        }
        Decision::SelfResponsible => Outcome::SelfResponsible,
        Decision::AppLauncher(path) => Outcome::AppLauncher(path),
        Decision::Relaunch => {
            let Ok(exe) = std::env::current_exe() else {
                return Outcome::LookupFailed("current executable unavailable".into());
            };
            let mut spec = SpawnSpec::new(exe, std::env::args_os());
            spec.env
                .retain(|v| !v.as_bytes().starts_with(b"PANCETTA_MACOS_RELAUNCHED="));
            spec.env.push(
                std::ffi::CString::new(format!("PANCETTA_MACOS_RELAUNCHED={own_pid}")).unwrap(),
            );
            match spawn_and_supervise(&spec, symbols.map(|s| s.0)) {
                Err(rc) => Outcome::SpawnFailed(rc),
                Ok(never) => match never {},
            }
        }
    }
}

pub(crate) fn log_outcome() {
    #[cfg(target_os = "macos")]
    {
        use tracing::{debug, info, warn};
        match outcome() {
            Outcome::NotMacOs | Outcome::NotNeeded => debug!("macOS microphone identity: not needed"),
            Outcome::SelfResponsible => info!("macOS microphone identity: own responsible process; macOS asks about pancetta (net.hagale.pancetta)"),
            Outcome::RelaunchedChild {parent} => info!("macOS microphone identity: relaunched as its own responsible process (launcher {}); macOS asks about pancetta (net.hagale.pancetta)", resolve_symbols().and_then(|(_, query)| pid_path(responsible_for(*parent, query))).map(|p| p.display().to_string()).unwrap_or_else(|| format!("pid {parent}"))),
            Outcome::AppLauncher(path) => info!("macOS microphone identity: launched by {}; macOS uses that app's microphone approval", path.display()),
            Outcome::DisabledByEnv => info!("macOS microphone identity: relaunch disabled by PANCETTA_MACOS_RELAUNCH=0; macOS uses the launcher's microphone approval"),
            Outcome::MissingUsageDescription => warn!("macOS microphone identity: this binary has no embedded microphone usage description; macOS uses the launcher's microphone approval"),
            Outcome::SymbolsMissing => warn!("macOS microphone identity: private responsibility API not found; macOS uses the launcher's microphone approval"),
            Outcome::LookupFailed(reason) => warn!("macOS microphone identity: could not look up the responsible process ({reason}); macOS uses the launcher's microphone approval"),
            Outcome::SpawnFailed(rc) => warn!("macOS microphone identity: relaunch failed (posix_spawn error {rc}); running in-process with the launcher's microphone approval"),
            Outcome::RelaunchedButNotResponsible {responsible,path} => warn!("macOS microphone identity: relaunched, but macOS still attributes the microphone to pid {responsible} ({path:?}); not waiting for approval"),
        }
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
mod supervisor {
    use std::convert::Infallible;
    use std::ffi::{CString, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicI32, Ordering};
    pub(super) type SetDisclaim =
        unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;
    static CHILD: AtomicI32 = AtomicI32::new(0);
    static PENDING: AtomicI32 = AtomicI32::new(0);
    pub(super) struct SpawnSpec {
        path: CString,
        args: Vec<CString>,
        pub(super) env: Vec<CString>,
    }
    impl SpawnSpec {
        pub(super) fn new(
            path: impl AsRef<OsStr>,
            args: impl IntoIterator<Item = impl AsRef<OsStr>>,
        ) -> Self {
            Self {
                path: CString::new(path.as_ref().as_bytes()).expect("executable contains NUL"),
                args: args
                    .into_iter()
                    .map(|v| CString::new(v.as_ref().as_bytes()).expect("argument contains NUL"))
                    .collect(),
                env: std::env::vars_os()
                    .map(|(k, v)| {
                        let mut bytes = k.as_bytes().to_vec();
                        bytes.push(b'=');
                        bytes.extend_from_slice(v.as_bytes());
                        CString::new(bytes).expect("environment contains NUL")
                    })
                    .collect(),
            }
        }
    }
    extern "C" fn forward(sig: libc::c_int) {
        let pid = CHILD.load(Ordering::Relaxed);
        if pid > 0 {
            // SAFETY: kill is async-signal-safe. The child remains unreaped until waitpid.
            unsafe {
                libc::kill(pid, sig);
            }
        } else {
            PENDING.store(sig, Ordering::Relaxed);
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum ParentExit {
        Code(i32),
        Raise(i32),
    }
    pub(super) fn parent_exit(status: i32) -> ParentExit {
        if libc::WIFEXITED(status) {
            return ParentExit::Code(libc::WEXITSTATUS(status));
        }
        let sig = libc::WTERMSIG(status);
        if [
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGKILL,
            libc::SIGPIPE,
        ]
        .contains(&sig)
        {
            ParentExit::Raise(sig)
        } else {
            ParentExit::Code(128 + sig)
        }
    }
    fn finish(exit: ParentExit) -> ! {
        match exit {
            ParentExit::Code(code) => std::process::exit(code),
            ParentExit::Raise(sig) => {
                // SAFETY: reset and unblock this signal before re-raising it in this single-threaded parent.
                unsafe {
                    libc::signal(sig, libc::SIG_DFL);
                    let mut set = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, sig);
                    libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                    libc::raise(sig);
                }
                std::process::exit(128 + sig)
            }
        }
    }

    /// Called only before threads exist, or in an isolated re-exec test process.
    pub(super) fn spawn_and_supervise(
        spec: &SpawnSpec,
        disclaim: Option<SetDisclaim>,
    ) -> Result<Infallible, i32> {
        // SAFETY: all C objects are initialized before use, pointer arrays are NUL-terminated
        // and backed by CStrings for the entire spawn. Signal changes are confined to this
        // single-threaded process and restored on every failed spawn.
        unsafe {
            let mut blocked = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                libc::sigaddset(&mut blocked, sig);
            }
            let mut oldmask = std::mem::zeroed();
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut oldmask);
            if rc != 0 {
                return Err(rc);
            }
            CHILD.store(0, Ordering::Relaxed);
            PENDING.store(0, Ordering::Relaxed);
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut oldterm = std::mem::zeroed();
            let mut oldhup = std::mem::zeroed();
            if libc::sigaction(libc::SIGTERM, &action, &mut oldterm) != 0 {
                let rc = std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EINVAL);
                libc::pthread_sigmask(libc::SIG_SETMASK, &oldmask, std::ptr::null_mut());
                return Err(rc);
            }
            if libc::sigaction(libc::SIGHUP, &action, &mut oldhup) != 0 {
                let rc = std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EINVAL);
                libc::sigaction(libc::SIGTERM, &oldterm, std::ptr::null_mut());
                libc::pthread_sigmask(libc::SIG_SETMASK, &oldmask, std::ptr::null_mut());
                return Err(rc);
            }
            let mut pid = 0;
            let mut attr = std::mem::zeroed();
            let mut rc = libc::posix_spawnattr_init(&mut attr);
            if rc == 0 {
                rc = (|| {
                    if let Some(disclaim) = disclaim {
                        let rc = disclaim(&mut attr, 1);
                        if rc != 0 {
                            return rc;
                        }
                    }
                    let mut defaults = std::mem::zeroed();
                    libc::sigemptyset(&mut defaults);
                    for sig in [
                        libc::SIGINT,
                        libc::SIGQUIT,
                        libc::SIGTERM,
                        libc::SIGHUP,
                        libc::SIGPIPE,
                    ] {
                        libc::sigaddset(&mut defaults, sig);
                    }
                    let mut empty = std::mem::zeroed();
                    libc::sigemptyset(&mut empty);
                    for rc in [
                        libc::posix_spawnattr_setsigdefault(&mut attr, &defaults),
                        libc::posix_spawnattr_setsigmask(&mut attr, &empty),
                        libc::posix_spawnattr_setflags(
                            &mut attr,
                            (libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_SETSIGMASK)
                                as libc::c_short,
                        ),
                    ] {
                        if rc != 0 {
                            return rc;
                        }
                    }
                    let mut args: Vec<_> =
                        spec.args.iter().map(|v| v.as_ptr().cast_mut()).collect();
                    args.push(std::ptr::null_mut());
                    let mut env: Vec<_> = spec.env.iter().map(|v| v.as_ptr().cast_mut()).collect();
                    env.push(std::ptr::null_mut());
                    libc::posix_spawn(
                        &mut pid,
                        spec.path.as_ptr(),
                        std::ptr::null(),
                        &attr,
                        args.as_ptr(),
                        env.as_ptr(),
                    )
                })();
                libc::posix_spawnattr_destroy(&mut attr);
            }
            if rc != 0 {
                libc::sigaction(libc::SIGTERM, &oldterm, std::ptr::null_mut());
                libc::sigaction(libc::SIGHUP, &oldhup, std::ptr::null_mut());
                libc::pthread_sigmask(libc::SIG_SETMASK, &oldmask, std::ptr::null_mut());
                return Err(rc);
            }
            CHILD.store(pid, Ordering::Relaxed);
            let pending = PENDING.swap(0, Ordering::Relaxed);
            if pending != 0 {
                libc::kill(pid, pending);
            }
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
            libc::pthread_sigmask(libc::SIG_SETMASK, &oldmask, std::ptr::null_mut());
            let mut status = 0;
            loop {
                if libc::waitpid(pid, &mut status, 0) == pid {
                    finish(parent_exit(status));
                }
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_bundle_paths_are_recognized() {
        for path in ["/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal", "/Applications/iTerm.app/Contents/MacOS/iTerm2", "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper", "/Applications/Foo.APP/Contents/MacOS/Foo"] {
            assert!(is_app_bundle_executable(Path::new(path)), "{path}");
        }
        for path in [
            "/usr/libexec/sshd-session",
            "/opt/homebrew/bin/tmux",
            "/usr/sbin/cron",
            "/Users/x/my.app.d/bin/pancetta",
            "/Applications/Foo.app",
        ] {
            assert!(!is_app_bundle_executable(Path::new(path)), "{path}");
        }
    }
    #[test]
    fn decision_table() {
        let base = || Probe {
            opens_microphone: true,
            relaunch_disabled: false,
            has_usage_description: true,
            symbols_found: true,
            own_pid: 42,
            responsible: Some((1, Some("/usr/libexec/sshd-session".into()))),
        };
        assert_eq!(
            decide(&Probe {
                opens_microphone: false,
                ..base()
            }),
            Decision::NotNeeded
        );
        assert_eq!(
            decide(&Probe {
                relaunch_disabled: true,
                ..base()
            }),
            Decision::DisabledByEnv
        );
        assert_eq!(
            decide(&Probe {
                has_usage_description: false,
                ..base()
            }),
            Decision::MissingUsageDescription
        );
        assert_eq!(
            decide(&Probe {
                symbols_found: false,
                ..base()
            }),
            Decision::SymbolsMissing
        );
        for responsible in [None, Some((0, None)), Some((-1, None)), Some((1, None))] {
            assert_eq!(
                decide(&Probe {
                    responsible,
                    ..base()
                }),
                Decision::LookupFailed
            );
        }
        assert_eq!(
            decide(&Probe {
                responsible: Some((42, None)),
                ..base()
            }),
            Decision::SelfResponsible
        );
        assert_eq!(decide(&base()), Decision::Relaunch);
        assert_eq!(
            decide(&Probe {
                responsible: Some((1, Some("/opt/homebrew/bin/tmux".into()))),
                ..base()
            }),
            Decision::Relaunch
        );
        let path = PathBuf::from("/Applications/Terminal.app/Contents/MacOS/Terminal");
        assert_eq!(
            decide(&Probe {
                responsible: Some((1, Some(path.clone()))),
                ..base()
            }),
            Decision::AppLauncher(path)
        );
    }
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn relaunch_is_never_attempted_off_macos() {
        let before = std::env::var_os("PANCETTA_MACOS_RELAUNCHED");
        assert!(matches!(compute_outcome(true), Outcome::NotMacOs));
        assert_eq!(std::env::var_os("PANCETTA_MACOS_RELAUNCHED"), before);
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::super::supervisor::{parent_exit, ParentExit};
        use super::*;
        use std::io::{BufRead, BufReader, Read};
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::process::{Command, Stdio};
        #[test]
        fn thin_parent_child_entry() {
            let Some(script) = std::env::var_os("PANCETTA_TEST_THIN_PARENT_SCRIPT") else {
                return;
            };
            if script == "FAIL" {
                // SAFETY: this re-exec process is isolated from the outer test runner.
                unsafe {
                    let mut before_term: libc::sigaction = std::mem::zeroed();
                    let mut before_hup: libc::sigaction = std::mem::zeroed();
                    libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut before_term);
                    libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut before_hup);
                    assert!(spawn_and_supervise(
                        &SpawnSpec::new("/nonexistent/pan264", ["missing"]),
                        None
                    )
                    .is_err());
                    let mut after: libc::sigaction = std::mem::zeroed();
                    libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut after);
                    assert_eq!(before_term.sa_sigaction, after.sa_sigaction);
                    libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut after);
                    assert_eq!(before_hup.sa_sigaction, after.sa_sigaction);
                    let mut mask = std::mem::zeroed();
                    libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask);
                    for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                        assert_eq!(libc::sigismember(&mask, sig), 0);
                    }
                }
                return;
            }
            spawn_and_supervise(
                &SpawnSpec::new(
                    "/bin/sh",
                    [
                        std::ffi::OsStr::new("sh"),
                        std::ffi::OsStr::new("-c"),
                        &script,
                    ],
                ),
                None,
            )
            .unwrap();
        }
        fn child(script: &str) -> std::process::Child {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "responsibility::tests::unix_tests::thin_parent_child_entry",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("PANCETTA_TEST_THIN_PARENT_SCRIPT", script)
                .process_group(0)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        }
        fn signal_test(sig: i32, group: bool, delay: bool) -> (std::process::ExitStatus, String) {
            let mut c = child("trap 'kill $s 2>/dev/null; echo got TERM; exit 0' TERM; trap 'kill $s 2>/dev/null; echo got HUP; exit 3' HUP; trap 'kill $s 2>/dev/null; echo got INT; exit 0' INT; sleep 30 & s=$!; echo ready; wait $s");
            let mut out = BufReader::new(c.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                assert!(out.read_line(&mut line).unwrap() > 0);
                if line.trim_end().ends_with("ready") {
                    break;
                }
            }
            // SAFETY: the re-exec child owns its process group. No unrelated processes are signalled.
            unsafe {
                libc::kill(
                    if group {
                        -(c.id() as i32)
                    } else {
                        c.id() as i32
                    },
                    sig,
                );
            }
            if delay {
                std::thread::sleep(std::time::Duration::from_millis(500));
                assert!(c.try_wait().unwrap().is_none());
                // SAFETY: child is still alive and owned by this test.
                unsafe {
                    libc::kill(c.id() as i32, libc::SIGTERM);
                }
            }
            let mut rest = String::new();
            out.read_to_string(&mut rest).unwrap();
            (c.wait().unwrap(), rest)
        }
        #[test]
        fn parent_exit_mapping() {
            for (script, expected) in [
                ("exit 7", ParentExit::Code(7)),
                ("kill -INT $$", ParentExit::Raise(libc::SIGINT)),
                ("kill -TERM $$", ParentExit::Raise(libc::SIGTERM)),
                ("kill -ABRT $$", ParentExit::Code(134)),
            ] {
                let status = Command::new("/bin/sh")
                    .args(["-c", script])
                    .status()
                    .unwrap();
                assert_eq!(parent_exit(status.into_raw()), expected);
            }
        }
        #[test]
        fn thin_parent_returns_child_exit_code() {
            assert_eq!(child("exit 7").wait().unwrap().code(), Some(7));
        }
        #[test]
        fn thin_parent_forwards_sigterm() {
            let (s, o) = signal_test(libc::SIGTERM, false, false);
            assert_eq!(s.code(), Some(0));
            assert!(o.contains("got TERM"));
        }
        #[test]
        fn thin_parent_forwards_sighup() {
            let (s, o) = signal_test(libc::SIGHUP, false, false);
            assert_eq!(s.code(), Some(3));
            assert!(o.contains("got HUP"));
        }
        #[test]
        fn thin_parent_ignores_sigint_sent_to_it_alone() {
            assert_eq!(signal_test(libc::SIGINT, false, true).0.code(), Some(0));
        }
        #[test]
        fn thin_parent_lets_group_ctrl_c_reach_child() {
            let (s, o) = signal_test(libc::SIGINT, true, false);
            assert_eq!(s.code(), Some(0));
            assert!(o.contains("got INT"));
        }
        #[test]
        fn thin_parent_reraises_child_termination_signal() {
            assert_eq!(
                child("kill -INT $$").wait().unwrap().signal(),
                Some(libc::SIGINT)
            );
        }
        #[test]
        fn thin_parent_maps_core_signals_to_exit_code() {
            assert_eq!(child("kill -ABRT $$").wait().unwrap().code(), Some(134));
        }
        #[test]
        fn child_starts_with_default_dispositions() {
            for (script, sig) in [
                ("kill -INT $$; exit 9", libc::SIGINT),
                ("kill -PIPE $$; exit 9", libc::SIGPIPE),
            ] {
                assert_eq!(child(script).wait().unwrap().signal(), Some(sig));
            }
        }
        #[cfg(target_os = "linux")]
        #[test]
        fn child_starts_with_an_empty_signal_mask() {
            let output = child("grep -E 'SigBlk|SigIgn' /proc/self/status")
                .wait_with_output()
                .unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("SigBlk:\t0000000000000000"), "{text}");
            let ignored = text
                .lines()
                .find_map(|v| v.strip_prefix("SigIgn:\t"))
                .unwrap();
            assert_eq!(u64::from_str_radix(ignored, 16).unwrap() & 0x5007, 0);
        }
        #[test]
        fn spawn_failure_restores_signal_actions() {
            assert!(child("FAIL").wait().unwrap().success());
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn responsibility_symbols_resolve() {
        assert!(resolve_symbols().is_some());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn disclaimed_child_is_its_own_responsible_process() {
        let (disclaim, query) = resolve_symbols().unwrap();
        // SAFETY: initialized attributes and static C strings live through spawn. Child is killed and reaped.
        unsafe {
            let mut attr = std::mem::zeroed();
            assert_eq!(libc::posix_spawnattr_init(&mut attr), 0);
            assert_eq!(disclaim(&mut attr, 1), 0);
            let args = [
                c"sleep".as_ptr().cast_mut(),
                c"30".as_ptr().cast_mut(),
                std::ptr::null_mut(),
            ];
            let env = [std::ptr::null_mut()];
            let mut pid = 0;
            assert_eq!(
                libc::posix_spawn(
                    &mut pid,
                    c"/bin/sleep".as_ptr(),
                    std::ptr::null(),
                    &attr,
                    args.as_ptr(),
                    env.as_ptr()
                ),
                0
            );
            libc::posix_spawnattr_destroy(&mut attr);
            let responsible = responsible_for(pid, query);
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            assert_eq!(responsible, pid);
        }
    }
}

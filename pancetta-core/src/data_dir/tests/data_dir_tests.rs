use super::*;

#[test]
fn without_a_flag_the_data_dir_is_home_dot_pancetta() {
    let home = Path::new("/home/op");
    assert_eq!(
        default_for_home(Some(home)),
        Some(PathBuf::from("/home/op/.pancetta"))
    );
    assert_eq!(default_for_home(None), None);
}

#[test]
fn an_absolute_flag_is_used_as_given() {
    let dir = std::env::temp_dir().join("pancetta-40m");
    assert_eq!(resolve_flag(&dir, Some(Path::new("/home/op"))), Ok(dir));
}

#[test]
fn a_relative_flag_resolves_against_the_working_directory() {
    let cwd = std::env::current_dir().expect("cwd");
    let resolved = resolve_flag(Path::new("rel"), Some(Path::new("/home/op"))).expect("resolve");
    assert_eq!(resolved, cwd.join("rel"));
    assert!(resolved.is_absolute());
}

#[test]
fn a_leading_tilde_expands_against_home() {
    let home = std::env::temp_dir().join("home-op");
    assert_eq!(resolve_flag(Path::new("~"), Some(&home)), Ok(home.clone()));
    assert_eq!(
        resolve_flag(Path::new("~/x"), Some(&home)),
        Ok(home.join("x"))
    );
    assert_eq!(
        resolve_flag(Path::new("~\\x"), Some(&home)),
        Ok(home.join("x"))
    );
}

#[test]
fn tilde_user_is_not_expanded() {
    let cwd = std::env::current_dir().expect("cwd");
    let home = std::env::temp_dir().join("home-op");
    let resolved = resolve_flag(Path::new("~user/x"), Some(&home)).expect("resolve");
    assert_eq!(resolved, cwd.join("~user/x"));
    assert!(!resolved.starts_with(&home));
}

#[test]
fn a_tilde_without_a_home_is_an_error() {
    for raw in ["~", "~/x", "~\\x"] {
        let err = resolve_flag(Path::new(raw), None).expect_err(raw);
        assert_eq!(err, DataDirError::NoHomeForTilde(PathBuf::from(raw)));
        assert!(err.to_string().contains("no home directory was found"));
    }
}

#[test]
fn an_empty_flag_is_an_error() {
    for raw in ["", "   ", "\t"] {
        let err = resolve_flag(Path::new(raw), Some(Path::new("/home/op"))).expect_err(raw);
        assert_eq!(err, DataDirError::Empty);
        assert_eq!(err.to_string(), "--data-dir must name a directory");
    }
}

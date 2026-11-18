// PAN-376. Formerly a test in the inline module `tests`.
use super::*;

/// PAN-376: a `--data-dir` directory is application-owned too, so a
/// pre-existing lax one is re-hardened like `~/.pancetta`, and
/// `~/.pancetta` is then just another directory.
#[test]
fn a_custom_data_dir_is_hardened_like_the_default() {
    let home = std::path::Path::new("/home/operator");
    let custom = std::path::Path::new("/srv/pancetta-40m");
    assert!(is_app_data_dir(custom, Some(custom)));
    assert!(!is_app_data_dir(&home.join(".pancetta"), Some(custom)));
}

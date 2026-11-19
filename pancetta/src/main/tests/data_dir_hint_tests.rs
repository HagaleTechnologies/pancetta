// PAN-376. Formerly a test in the inline module `tests`.
use super::*;

/// PAN-376: a hint names `--data-dir` only when the operator gave it.
#[test]
fn hints_carry_the_data_dir_flag() {
    assert_eq!(pancetta_invocation(None), "pancetta");
    assert_eq!(
        pancetta_invocation(Some(Path::new("/home/op/.pancetta-40m"))),
        "pancetta --data-dir /home/op/.pancetta-40m"
    );
    assert_eq!(
        pancetta_invocation(Some(Path::new("/home/op/My Stations/40m"))),
        "pancetta --data-dir \"/home/op/My Stations/40m\""
    );
}

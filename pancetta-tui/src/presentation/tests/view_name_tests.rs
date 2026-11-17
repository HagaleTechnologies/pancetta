// PAN-388: the footer's short view name is `view_message` without " view".
use super::*;

#[test]
fn view_name_is_view_message_without_suffix() {
    for view in [
        ActiveView::Operate,
        ActiveView::Hunt,
        ActiveView::Run,
        ActiveView::Monitor,
    ] {
        assert_eq!(view_message(view), format!("{} view", view_name(view)));
    }
}

use pancetta_dx::{DxError, DxErrorInner, UploadFailureClass};

#[test]
fn boxed_error_api() {
    let err: DxError = DxErrorInner::ResponseTooLarge { limit: 65536 }.into();
    assert!(matches!(
        err.kind(),
        DxErrorInner::ResponseTooLarge { limit: 65536 }
    ));
    assert_eq!(err.to_string(), "Response too large: more than 65536 bytes");
    assert_eq!(
        err.upload_failure_class(),
        UploadFailureClass::RecordRejected
    );
    let result: pancetta_dx::Result<()> = Err(err);
    assert!(result.is_err());
}

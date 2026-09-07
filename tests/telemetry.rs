use continuo::telemetry;

#[test]
fn validates_filter_without_installing_global_state() {
    assert!(telemetry::subscriber("continuo=debug,warn").is_ok());
    let error = match telemetry::subscriber("continuo=not-a-level") {
        Ok(_) => panic!("invalid filter accepted"),
        Err(error) => error,
    };
    assert!(std::error::Error::source(&error).is_some());
}

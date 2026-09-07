use continuo::media::capabilities::{
    Continuity as C, MediaCapabilities, ResumeCapability as R, SeekSupport as S,
};

#[test]
fn resume_capability_covers_every_pair() {
    let seeks = [S::Unknown, S::Native, S::RestartAndDiscard, S::Unsupported];
    let rows = [
        (C::Unresolved, [R::Undetermined; 4]),
        (C::Indefinite, [R::Unsupported; 4]),
        (
            C::Finite,
            [R::Undetermined, R::Supported, R::Supported, R::Unsupported],
        ),
    ];
    for (continuity, expected) in rows {
        for (seek, expected) in seeks.into_iter().zip(expected) {
            let capabilities = MediaCapabilities { continuity, seek };
            assert_eq!(
                capabilities.resume_capability(),
                expected,
                "{continuity:?}/{seek:?}"
            );
        }
    }
}

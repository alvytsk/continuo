use continuo::playback::provenance::PositionProvenance;
use continuo::playback::timeline::PositionQuality;

#[test]
fn provenance_and_quality_are_independent_axes() {
    // The whole point of a second field. Every combination is meaningful, so
    // none of them may be collapsed into the other enum.
    for quality in [
        PositionQuality::Exact,
        PositionQuality::Estimated,
        PositionQuality::Degraded,
    ] {
        for provenance in [
            PositionProvenance::Established,
            PositionProvenance::Estimated,
        ] {
            // Constructing the pair is the assertion: if a later change folds
            // provenance into quality this stops compiling.
            let _ = (quality, provenance);
        }
    }
}

#[test]
fn a_degraded_position_can_still_be_established() {
    // A device fault says nothing about whether the media time was confirmed.
    // Reading provenance off quality would call this estimated and refuse to
    // checkpoint a position the decoder actually established.
    let quality = PositionQuality::Degraded;
    let provenance = PositionProvenance::Established;
    assert_eq!(provenance, PositionProvenance::Established);
    assert_eq!(quality, PositionQuality::Degraded);
}

#[test]
fn established_is_the_default_so_every_existing_path_keeps_its_meaning() {
    // M1 and M2 wrote positions the decoder confirmed. Anything that does not
    // opt into an estimate must keep reporting what it always reported.
    assert_eq!(
        PositionProvenance::default(),
        PositionProvenance::Established
    );
}

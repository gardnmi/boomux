use boomux::benchmark_support::{
    EventAppendFixture, EventFixture, RuntimeEventFixture, SessionFixture, TerminalFixture,
    terminal_transcript,
};

#[test]
fn event_fixtures_preserve_bounds_and_deterministic_summaries() {
    let events = EventFixture::retained(8_192);
    let page = events.read_page(256, 256).summary();
    assert_eq!(page.count, 256);
    assert_eq!(page.first_id, 7_937);
    assert_eq!(page.last_id, 8_192);
    assert_ne!(page.checksum, 0);
    assert_eq!(page.checksum, 14_253_501_652_687_160_347);
    assert_eq!(events.read_page(256, 256).summary(), page);

    let resumed = events.projection_cut(256).summary();
    assert!(!resumed.baseline);
    assert_eq!(resumed.count, 256);
    assert_ne!(resumed.checksum, 0);
    assert_eq!(resumed.checksum, 14_253_501_652_687_160_347);
    assert_eq!(events.projection_cut(256).summary(), resumed);
    let reseeded = events.projection_cut(257).summary();
    assert!(reseeded.baseline);
    assert_eq!(reseeded.count, 0);

    let invalidations = RuntimeEventFixture::invalidations(128, 64);
    let coalesced = invalidations.clone().coalesce().summary();
    assert_eq!(coalesced.count, 129);
    assert_eq!(coalesced.checksum, 9_953_927_640_701_513_843);
    assert_eq!(invalidations.coalesce().summary(), coalesced);
    assert_ne!(coalesced.checksum, 0);

    let append = EventAppendFixture::retained_with_batch(8_192, 256);
    let appended = append.clone().append().summary();
    assert_eq!(
        (appended.count, appended.first_id, appended.last_id),
        (8_192, 257, 8_448)
    );
    assert_ne!(appended.checksum, 0);
    assert_eq!(appended.checksum, 8_051_509_862_202_861_121);
    assert_eq!(append.append().summary(), appended);
}

#[test]
fn session_fixtures_preserve_expected_cardinality_and_digest() {
    let durable = SessionFixture::durable(64, 8, 2);
    let summary = durable.project().summary();
    assert_eq!(summary.sessions, 1_024);
    assert_eq!(summary.occurrences, 1_024);
    assert_eq!(summary.current, 1_024);
    assert_ne!(summary.checksum, 0);
    assert_eq!(summary.checksum, 1_011_118_191_847_967_550);
    assert_eq!(durable.project().summary(), summary);

    let unique = SessionFixture::catalog(64, 400, false);
    let unique_summary = unique.project().summary();
    assert_eq!(unique_summary.sessions, 400);
    assert_ne!(unique_summary.checksum, 0);
    assert_eq!(unique_summary.checksum, 8_601_524_830_593_867_508);
    assert_eq!(unique.project().summary(), unique_summary);

    let shared = SessionFixture::catalog(32, 400, true);
    let shared_summary = shared.project().summary();
    assert_eq!(shared_summary.sessions, 12_800);
    assert_ne!(shared_summary.checksum, 0);
    assert_eq!(shared_summary.checksum, 6_725_822_761_224_353_157);
    assert_eq!(shared.project().summary(), shared_summary);
}

#[test]
fn terminal_fixtures_are_synthetic_bounded_and_repeatable() {
    let transcript = terminal_transcript(2_048, 128);
    let terminal = TerminalFixture::from_transcript(24, 80, &transcript);
    let summary = terminal.summary();
    assert_eq!(summary.preview_lines, 16);
    assert_ne!(summary.preview_checksum, 0);
    assert_eq!(summary.preview_checksum, 10_375_648_925_095_483_392);
    assert_ne!(summary.reconstruction_checksum, 0);
    assert_eq!(summary.reconstruction_checksum, 12_069_107_278_816_405_535);
    assert_eq!(terminal.summary(), summary);
    let chunked = TerminalFixture::from_chunked_transcript(24, 80, &transcript, 16 * 1024);
    assert_eq!(chunked.summary(), summary);
    let preview = terminal.preview(1024 * 1024, 16, 20_000);
    assert_eq!(preview.lines.len(), 16);
    assert_eq!(terminal.preview(1024 * 1024, 16, 20_000), preview);

    let reconstruction = terminal.reconstruction();
    assert!(!reconstruction.is_empty());
    assert!(reconstruction.len() <= 1024 * 1024);
    assert_eq!(terminal.reconstruction(), reconstruction);
}

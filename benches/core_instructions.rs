extern crate gungraun;

use std::hint::black_box;

use boomux::benchmark_support::{
    EventFixture, SessionFixture, SessionProjectionResult, TerminalFixture, TransitionResult,
    terminal_transcript,
};
use boomux::protocol::TerminalPreview;
use gungraun::{
    Callgrind, EventKind, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};

fn setup_events(count: usize) -> EventFixture {
    let fixture = EventFixture::retained(count);
    assert_eq!(
        fixture.projection_cut(256).summary().checksum,
        14_253_501_652_687_160_347
    );
    let baseline = fixture.projection_cut(257).summary();
    assert!(baseline.baseline);
    assert_eq!((baseline.count, baseline.checksum), (0, 0));
    fixture
}

#[library_benchmark]
#[bench::cut_256(args = (8_192), setup = setup_events)]
fn event_projection_cut_256(fixture: EventFixture) -> TransitionResult {
    black_box(fixture.projection_cut(256))
}

#[library_benchmark]
#[bench::cut_257(args = (8_192), setup = setup_events)]
fn event_projection_cut_257(fixture: EventFixture) -> TransitionResult {
    black_box(fixture.projection_cut(257))
}

fn setup_shared_catalog(workspaces: usize, records: usize) -> SessionFixture {
    let fixture = SessionFixture::catalog(workspaces, records, true);
    let summary = fixture.project().summary();
    assert_eq!(summary.sessions, workspaces * records);
    assert_eq!(summary.checksum, 6_725_822_761_224_353_157);
    fixture
}

#[library_benchmark]
#[bench::shared_32w_400(args = (32, 400), setup = setup_shared_catalog)]
fn shared_catalog_projection(fixture: SessionFixture) -> SessionProjectionResult {
    black_box(fixture.project())
}

fn setup_terminal(lines: usize, line_bytes: usize) -> TerminalFixture {
    let transcript = terminal_transcript(lines, line_bytes);
    let fixture = TerminalFixture::from_transcript(24, 80, &transcript);
    let summary = fixture.summary();
    assert_eq!(summary.preview_lines, 16);
    assert_eq!(summary.preview_checksum, 10_375_648_925_095_483_392);
    assert_eq!(summary.reconstruction_checksum, 12_069_107_278_816_405_535);
    fixture
}

#[library_benchmark]
#[bench::styled_2000(args = (2_048, 128), setup = setup_terminal)]
fn terminal_preview(fixture: TerminalFixture) -> TerminalPreview {
    black_box(fixture.preview(1024 * 1024, 16, 20_000))
}

library_benchmark_group!(
    name = core_instructions;
    benchmarks = event_projection_cut_256,
                 event_projection_cut_257,
                 shared_catalog_projection,
                 terminal_preview
);

main!(
    config = LibraryBenchmarkConfig::default().tool(
        Callgrind::default().soft_limits([(EventKind::Ir, 10.0)])
    );
    library_benchmark_groups = core_instructions
);

use std::hint::black_box;

use boomux::benchmark_support::{
    EventAppendFixture, EventFixture, RuntimeEventFixture, SessionFixture, TerminalFixture,
    terminal_transcript,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

fn event_benchmarks(criterion: &mut Criterion) {
    let events = EventFixture::retained(8_192);
    for distance in [8_192, 4_096, 256] {
        for limit in [1, 64, 256] {
            assert_eq!(events.read_page(distance, limit).summary().count, limit);
        }
    }
    assert_eq!(
        events.read_page(256, 256).summary().checksum,
        14_253_501_652_687_160_347
    );
    assert_eq!(
        events.projection_cut(256).summary().checksum,
        14_253_501_652_687_160_347
    );
    assert!(events.projection_cut(257).summary().baseline);
    assert_eq!(events.projection_cut(64).summary().count, 64);
    assert!(events.projection_cut(8_192).summary().baseline);

    let append = EventAppendFixture::retained_with_batch(8_192, 256);
    let appended = append.clone().append().summary();
    assert_eq!(
        (appended.count, appended.first_id, appended.last_id),
        (8_192, 257, 8_448)
    );
    assert_eq!(appended.checksum, 8_051_509_862_202_861_121);
    criterion.bench_function("events/append_256_at_retention_limit", |benchmark| {
        benchmark.iter_batched(
            || append.clone(),
            |fixture| black_box(fixture.append()),
            BatchSize::LargeInput,
        );
    });

    let mut reads = criterion.benchmark_group("events/read_page");
    for (position, distance) in [("head", 8_192), ("middle", 4_096), ("tail", 256)] {
        for limit in [1, 64, 256] {
            reads.throughput(Throughput::Elements(limit));
            reads.bench_with_input(
                BenchmarkId::new(position, limit),
                &(distance, limit as usize),
                |benchmark, &(distance, limit)| {
                    benchmark.iter(|| black_box(events.read_page(distance, limit)));
                },
            );
        }
    }
    reads.finish();

    let mut cuts = criterion.benchmark_group("events/projection_cut");
    for distance in [64, 256, 257, 8_192] {
        cuts.bench_with_input(
            BenchmarkId::from_parameter(distance),
            &distance,
            |benchmark, &distance| {
                benchmark.iter(|| black_box(events.projection_cut(distance)));
            },
        );
    }
    cuts.finish();

    let invalidations = RuntimeEventFixture::invalidations(128, 64);
    let summary = invalidations.clone().coalesce().summary();
    assert_eq!(summary.count, 129);
    assert_eq!(summary.checksum, 9_953_927_640_701_513_843);
    criterion.bench_function("events/coalesce_128_nodes_64_revisions", |benchmark| {
        benchmark.iter_batched(
            || invalidations.clone(),
            |fixture| black_box(fixture.coalesce()),
            BatchSize::LargeInput,
        );
    });
}

fn session_benchmarks(criterion: &mut Criterion) {
    let durable = SessionFixture::durable(64, 8, 2);
    let durable_summary = durable.project().summary();
    assert_eq!(durable_summary.sessions, 1_024);
    assert_eq!(durable_summary.occurrences, 1_024);
    assert_eq!(durable_summary.current, 1_024);
    assert_eq!(durable_summary.checksum, 1_011_118_191_847_967_550);

    let unique_catalog = SessionFixture::catalog(64, 400, false);
    let unique_summary = unique_catalog.project().summary();
    assert_eq!(unique_summary.sessions, 400);
    assert_eq!(unique_summary.checksum, 8_601_524_830_593_867_508);
    let shared_catalog = SessionFixture::catalog(32, 400, true);
    let shared_summary = shared_catalog.project().summary();
    assert_eq!(shared_summary.sessions, 12_800);
    assert_eq!(shared_summary.checksum, 6_725_822_761_224_353_157);

    let mut sessions = criterion.benchmark_group("sessions");
    sessions.throughput(Throughput::Elements(1_024));
    sessions.bench_function("durable_64w_512s_1024a", |benchmark| {
        benchmark.iter(|| black_box(durable.project()));
    });
    sessions.throughput(Throughput::Elements(400));
    sessions.bench_function("catalog_400_unique", |benchmark| {
        benchmark.iter(|| black_box(unique_catalog.project()));
    });
    sessions.throughput(Throughput::Elements(12_800));
    sessions.bench_function("catalog_400_shared_32w", |benchmark| {
        benchmark.iter(|| black_box(shared_catalog.project()));
    });
    sessions.finish();
}

fn terminal_benchmarks(criterion: &mut Criterion) {
    let transcript = terminal_transcript(2_048, 128);
    let terminal = TerminalFixture::from_transcript(24, 80, &transcript);
    let summary = terminal.summary();
    assert_eq!(summary.preview_checksum, 10_375_648_925_095_483_392);
    assert_eq!(summary.reconstruction_checksum, 12_069_107_278_816_405_535);
    let chunked = TerminalFixture::from_chunked_transcript(24, 80, &transcript, 16 * 1024);
    assert_eq!(chunked.summary(), summary);
    let preview = terminal.preview(1024 * 1024, 16, 20_000);
    assert_eq!(preview.lines.len(), 16);
    let reconstruction = terminal.reconstruction();
    assert!(!reconstruction.is_empty());
    assert!(reconstruction.len() <= 1024 * 1024);

    let mut process = criterion.benchmark_group("terminal/process");
    process.throughput(Throughput::Bytes(transcript.len() as u64));
    process.bench_function("2048_styled_lines", |benchmark| {
        benchmark.iter_batched(
            || transcript.as_slice(),
            |transcript| {
                black_box(TerminalFixture::from_chunked_transcript(
                    24,
                    80,
                    black_box(transcript),
                    16 * 1024,
                ))
            },
            BatchSize::SmallInput,
        );
    });
    process.finish();

    criterion.bench_function("terminal/preview_2000_scrollback_rows", |benchmark| {
        benchmark.iter(|| black_box(terminal.preview(1024 * 1024, 16, 20_000)));
    });
    criterion.bench_function(
        "terminal/reconstruction_2000_scrollback_rows",
        |benchmark| {
            benchmark.iter(|| black_box(terminal.reconstruction()));
        },
    );
}

criterion_group!(
    benches,
    event_benchmarks,
    session_benchmarks,
    terminal_benchmarks
);
criterion_main!(benches);

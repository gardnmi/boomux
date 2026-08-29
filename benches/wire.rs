use std::hint::black_box;
use std::io::Cursor;

use boomux::protocol::{AttachFrame, read_message, write_message};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ControlFixture {
    sequence: u64,
    payload: String,
}

fn wire_benchmarks(criterion: &mut Criterion) {
    let attach = AttachFrame::Output(vec![b'x'; 16 * 1024]);
    let mut encoded_attach = Vec::new();
    attach.write_to(&mut encoded_attach).unwrap();
    assert_eq!(
        AttachFrame::read_from(&mut Cursor::new(&encoded_attach)).unwrap(),
        attach
    );

    let control = ControlFixture {
        sequence: 42,
        payload: "x".repeat(1024 * 1024),
    };
    let mut encoded_control = Vec::new();
    write_message(&mut encoded_control, &control).unwrap();
    let decoded: ControlFixture = read_message(&mut Cursor::new(&encoded_control)).unwrap();
    assert_eq!(decoded, control);
    assert_eq!(encoded_attach.len(), 16_389);
    assert_eq!(encoded_control.len(), 1_048_608);

    let mut attach_group = criterion.benchmark_group("wire/attach");
    attach_group.throughput(Throughput::Bytes(16 * 1024));
    attach_group.bench_function("encode_16k", |benchmark| {
        benchmark.iter_batched(
            || Vec::with_capacity(encoded_attach.len()),
            |mut output| {
                attach.write_to(&mut output).unwrap();
                black_box(output)
            },
            BatchSize::SmallInput,
        );
    });
    attach_group.bench_function("decode_16k", |benchmark| {
        benchmark.iter(|| {
            let mut input = Cursor::new(encoded_attach.as_slice());
            black_box(AttachFrame::read_from(&mut input).unwrap())
        });
    });
    attach_group.finish();

    let mut control_group = criterion.benchmark_group("wire/control");
    control_group.throughput(Throughput::Bytes(control.payload.len() as u64));
    control_group.bench_function("encode_1m", |benchmark| {
        benchmark.iter_batched(
            || Vec::with_capacity(encoded_control.len()),
            |mut output| {
                write_message(&mut output, &control).unwrap();
                black_box(output)
            },
            BatchSize::LargeInput,
        );
    });
    control_group.bench_function("decode_1m", |benchmark| {
        benchmark.iter(|| {
            let mut input = Cursor::new(encoded_control.as_slice());
            black_box(read_message::<ControlFixture>(&mut input).unwrap())
        });
    });
    control_group.finish();
}

criterion_group!(benches, wire_benchmarks);
criterion_main!(benches);

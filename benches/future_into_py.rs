//! Benchmark `future_into_py` end-to-end wrapper overhead.
//!
//! The user-supplied future is a no-op (`async { Ok(()) }`), so the
//! measured wall time is dominated by the wrapper machinery —
//! `R::spawn`, `R::scope`, panic isolation, and the completion path
//! that calls `set_result` on the Python `asyncio.Future`. Each
//! iteration drives the asyncio event loop until the future
//! completes, which is the same path real users pay per call.
//!
//! Reference numbers on aarch64-apple-darwin, Python 3.14.3,
//! tokio multi-thread, n=100 samples (criterion default):
//!
//! | bench           | main      | this branch | per-fut Δ |
//! | --------------- | --------- | ----------- | --------- |
//! | single_noop     |  34.2 µs  |   33.7 µs   |  -0.5 µs  |
//! | gather_10_noop  | 239.4 µs  |  217.9 µs   |  -2.2 µs  |
//! | gather_100_noop |   2.04 ms |    1.31 ms  |  -7.3 µs  |
//!
//! single_noop is dominated by `run_until_complete` event-loop
//! overhead (~30 µs constant), so the wrapper delta is ~noise.
//! gather_N amortises that constant over N futures, exposing the
//! per-call wrapper cost: 20.4 → 13.1 µs/fut (-35%).

use criterion::{criterion_group, criterion_main, Criterion};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3_async_runtimes::TaskLocals;

fn bench_future_into_py(c: &mut Criterion) {
    Python::initialize();

    Python::attach(|py| {
        let asyncio = py.import("asyncio").unwrap();
        let event_loop = asyncio.call_method0("new_event_loop").unwrap();
        asyncio
            .call_method1("set_event_loop", (&event_loop,))
            .unwrap();

        // Build TaskLocals once with the bench event loop. The bench
        // runs outside any running coroutine, so `get_running_loop`-
        // based helpers (`future_into_py`, `get_current_locals`)
        // cannot be used here; we go through the `_with_locals`
        // entry point that the public API ultimately calls.
        let locals = TaskLocals::new(event_loop.clone());

        // Warm the tokio runtime + event loop so iteration 1 doesn't
        // pay one-time initialisation cost.
        let warm =
            pyo3_async_runtimes::tokio::future_into_py_with_locals(py, locals.clone(), async {
                Ok::<(), pyo3::PyErr>(())
            })
            .unwrap();
        event_loop
            .call_method1("run_until_complete", (warm,))
            .unwrap();

        // Single-future round-trip. Dominated by run_until_complete
        // event-loop overhead (~25-30 µs), so the wrapper delta is
        // small relative to noise here. Useful as a sanity ceiling.
        c.bench_function("single_noop", |b| {
            b.iter(|| {
                let py_fut = pyo3_async_runtimes::tokio::future_into_py_with_locals(
                    py,
                    locals.clone(),
                    async { Ok::<(), pyo3::PyErr>(()) },
                )
                .unwrap();
                event_loop
                    .call_method1("run_until_complete", (py_fut,))
                    .unwrap();
            });
        });

        // Gather of N noop futures. Run-loop overhead is paid once
        // per iteration but spawn/scope/completion runs N times, so
        // the per-future wrapper cost dominates. This is the
        // realistic ophyd-style "asyncio.gather(*[pv.get() for pv
        // in PVs])" workload pattern.
        for &n in &[10usize, 100usize] {
            let bench_name = format!("gather_{}_noop", n);
            c.bench_function(&bench_name, |b| {
                b.iter(|| {
                    let mut futs: Vec<Bound<PyAny>> = Vec::with_capacity(n);
                    for _ in 0..n {
                        futs.push(
                            pyo3_async_runtimes::tokio::future_into_py_with_locals(
                                py,
                                locals.clone(),
                                async { Ok::<(), pyo3::PyErr>(()) },
                            )
                            .unwrap(),
                        );
                    }
                    let args = PyTuple::new(py, futs).unwrap();
                    let gathered = asyncio.call_method1("gather", args).unwrap();
                    event_loop
                        .call_method1("run_until_complete", (gathered,))
                        .unwrap();
                });
            });
        }
    });
}

criterion_group!(benches, bench_future_into_py);
criterion_main!(benches);

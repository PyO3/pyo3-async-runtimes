//! No-I/O completion latency benchmark; run with `--release --features tokio-runtime`.
use std::ffi::CString;

use pyo3::prelude::*;
use pyo3_async_runtimes::{tokio, TaskLocals};

#[pyfunction]
fn immediate<'py>(
    py: Python<'py>,
    event_loop: Option<Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    match event_loop {
        Some(event_loop) => tokio::future_into_py_with_locals(
            py,
            TaskLocals::new(event_loop).copy_context(py)?,
            async { Ok(42usize) },
        ),
        None => tokio::future_into_py(py, async { Ok(42usize) }),
    }
}

const BENCHMARK: &str = r#"
import asyncio
import json
import os
import statistics
import time

async def main(module):
    loop = asyncio.get_running_loop()
    rounds = int(os.getenv("COMPLETION_BENCH_ROUNDS", "8"))
    operations = int(os.getenv("COMPLETION_BENCH_OPS", "200"))
    assert rounds >= 2 and operations > 0
    modes = {"fast": None, "explicit_locals": loop, "fallback": loop}
    attribute = "_pyo3_async_runtimes_completion_bridge"
    for _ in range(100):
        assert await module.immediate(None) == 42
    reader = getattr(loop, attribute, None)
    add_reader = loop.add_reader

    def configure(name):
        # Change capabilities between blocks, never inside a measured call.
        # Use the real loop for fallback too, without proxy dispatch overhead.
        if name == "fallback":
            if hasattr(loop, attribute):
                delattr(loop, attribute)
            loop.add_reader = None
        else:
            loop.add_reader = add_reader
            if reader is not None:
                setattr(loop, attribute, reader)

    def order(round_index):
        names = list(modes)
        offset = round_index % len(names)
        names = names[offset:] + names[:offset]
        return names[::-1] if round_index % 2 else names

    for name, mode in modes.items():
        configure(name)
        for _ in range(100):
            assert await module.immediate(mode) == 42
    results = []
    for gap in (0, 0.001, 0.005):
        samples = {name: [] for name in modes}
        cpu_samples = {name: [] for name in modes}
        for round_index in range(rounds):
            # Rotate, then reverse on alternating rounds to reduce order effects.
            for name in order(round_index):
                configure(name)
                elapsed = cpu = 0
                for _ in range(operations):
                    if gap:
                        await asyncio.sleep(gap)
                    start_cpu = time.thread_time_ns()
                    start = time.perf_counter_ns()
                    value = await module.immediate(modes[name])
                    elapsed += time.perf_counter_ns() - start
                    cpu += time.thread_time_ns() - start_cpu
                    assert value == 42
                samples[name].append(elapsed / operations / 1000)
                cpu_samples[name].append(cpu / operations / 1000)
        for name in modes:
            result = {
                "mode": name,
                "idle_ms": gap * 1000,
                "median_us": statistics.median(samples[name]),
                "python_thread_cpu_us": statistics.median(cpu_samples[name]),
                "samples_us": samples[name],
            }
            results.append(result)
            print(json.dumps(result), flush=True)
    batch_size = 2000
    elapsed = {name: [] for name in modes}
    cpu = {name: [] for name in modes}
    for round_index in range(rounds):
        for name in order(round_index):
            configure(name)
            start_cpu = time.thread_time_ns()
            start = time.perf_counter_ns()
            values = await asyncio.gather(
                *(module.immediate(modes[name]) for _ in range(batch_size))
            )
            elapsed[name].append((time.perf_counter_ns() - start) / batch_size / 1000)
            cpu[name].append((time.thread_time_ns() - start_cpu) / batch_size / 1000)
            assert values == [42] * batch_size
    for name in modes:
        print(json.dumps({
            "mode": name,
            "batch_size": batch_size,
            "median_us": statistics.median(elapsed[name]),
            "python_thread_cpu_us": statistics.median(cpu[name]),
            "samples_us": elapsed[name],
        }), flush=True)
    configure("fast")
    print(json.dumps({"bridge_active": reader is not None and reader() is not None}),
          flush=True)

def run(module):
    backend = os.getenv("COMPLETION_BENCH_LOOP", "asyncio")
    if backend == "asyncio":
        asyncio.run(main(module))
    elif backend == "uvloop":
        import uvloop
        uvloop.run(main(module))
    else:
        raise ValueError(f"unknown event loop: {backend}")
"#;

fn main() -> PyResult<()> {
    Python::initialize();
    Python::attach(|py| {
        let module = PyModule::new(py, "completion_benchmark_native")?;
        module.add_function(wrap_pyfunction!(immediate, &module)?)?;
        let benchmark = PyModule::from_code(
            py,
            &CString::new(BENCHMARK).unwrap(),
            &CString::new("completion_benchmark.py").unwrap(),
            &CString::new("completion_benchmark").unwrap(),
        )?;
        benchmark.call_method1("run", (module,))?;
        Ok(())
    })
}

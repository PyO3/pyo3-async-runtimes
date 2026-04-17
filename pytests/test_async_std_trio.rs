use pyo3::prelude::*;

#[pyfunction]
fn rust_sleep(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::async_std::future_into_py(py, async move {
        async_std::task::sleep(std::time::Duration::from_millis(50)).await;
        Ok(42i64)
    })
}

fn main() -> PyResult<()> {
    Python::initialize();
    Python::attach(|py| {
        if py.import("trio").is_err() {
            if std::env::var_os("CI").is_some()
                && std::env::var_os("PYO3_ASYNC_TEST_TRIO_OPTIONAL").is_none()
            {
                eprintln!("error: trio is not installed but CI is set");
                std::process::exit(1);
            }
            println!("test test_async_std_trio ... skipped (trio not available)");
            return Ok(());
        }
        let driver = PyModule::from_code(
            py,
            c"import trio\nasync def main(f):\n    return await f()\ndef drive(f):\n    return trio.run(main, f)\n",
            c"trio_driver.py",
            c"trio_driver",
        )?;
        let f = wrap_pyfunction!(rust_sleep, py)?;
        let r: i64 = driver.getattr("drive")?.call1((f,))?.extract()?;
        assert_eq!(r, 42);
        println!("test test_async_std_trio ... ok");
        Ok(())
    })
}

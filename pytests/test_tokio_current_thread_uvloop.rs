#[cfg(not(target_os = "windows"))]
fn main() -> pyo3::PyResult<()> {
    use pyo3::{prelude::*, types::PyType};

    pyo3::prepare_freethreaded_python();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();

    pyo3_async_runtimes::tokio::init(builder);
    std::thread::spawn(move || {
        pyo3_async_runtimes::tokio::get_runtime().block_on(futures::future::pending::<()>());
    });

    Python::with_gil(|py| {
        // uvloop not supported on the free-threaded build yet
        // https://github.com/MagicStack/uvloop/issues/642
        let sysconfig = py.import("sysconfig")?;
        let is_freethreaded = sysconfig.call_method1("get_config_var", ("Py_GIL_DISABLED",))?;
        if is_freethreaded.is_truthy()? {
            return Ok(());
        }

        let uvloop = py.import("uvloop")?;
        uvloop.call_method0("install")?;

        // store a reference for the assertion
        let uvloop = PyObject::from(uvloop);

        pyo3_async_runtimes::tokio::run(py, async move {
            // verify that we are on a uvloop.Loop
            Python::with_gil(|py| -> PyResult<()> {
                assert!(
                    pyo3_async_runtimes::tokio::get_current_loop(py)?.is_instance(
                        uvloop
                            .bind(py)
                            .getattr("Loop")?
                            .downcast::<PyType>()
                            .unwrap()
                    )?
                );
                Ok(())
            })?;

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            println!("test test_tokio_current_thread_uvloop ... ok");
            Ok(())
        })
    })
}

#[cfg(target_os = "windows")]
fn main() {}

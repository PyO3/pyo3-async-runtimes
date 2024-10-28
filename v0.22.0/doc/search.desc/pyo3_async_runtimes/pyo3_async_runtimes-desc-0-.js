searchState.loadedDescShard("pyo3_async_runtimes", 0, "Rust Bindings to the Python Asyncio Event Loop\nTask-local data to store for Python conversions.\nasync-std-runtime PyO3 Asyncio functions specific to the …\nCreate a clone of the TaskLocals by incrementing the …\nGet a reference to the python context\nCapture the current task’s contextvars\nErrors and exceptions related to PyO3 Asyncio\nGet a reference to the event loop\nReturns the argument unchanged.\nGeneric implementations of PyO3 Asyncio utilities that can …\nGet a reference to the Python Event Loop from Rust\nCalls <code>U::from(self)</code>.\nConvert a Python <code>awaitable</code> into a Rust Future\nRe-exported for #test attributes\nAt a minimum, TaskLocals must store the event loop.\ntesting Utilities for writing PyO3 Asyncio tests\ntokio-runtime PyO3 Asyncio functions specific to the tokio …\nManually provide the contextvars for the current task.\nConstruct TaskLocals with the event loop returned by …\nConvert a Rust Future into a Python awaitable\nConvert a Rust Future into a Python awaitable\nEither copy the task locals from the current task OR get …\nGet the current event loop from either Python or Rust …\nConvert a Python <code>awaitable</code> into a Rust Future\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nConvert a <code>!Send</code> Rust Future into a Python awaitable\nConvert a <code>!Send</code> Rust Future into a Python awaitable\nattributes Provides the boilerplate for the <code>async-std</code> …\nattributes re-exports for macros\nRun the event loop until the given Future completes\nRun the event loop until the given Future completes\nSet the task local event loop for the given future\nSet the task local event loop for the given !Send future\nattributes testing Registers an <code>async-std</code> test with the …\nre-export spawn_blocking for use in <code>#[test]</code> macro without …\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nCreates a new <code>PyErr</code> of this type.\nExposes the utilities necessary for using task-local data …\nGeneric utilities for a JoinError\nThe error returned by a JoinHandle after being awaited\nA future that completes with the result of the spawned task\nAdds the ability to scope task-local data for !Send futures\nGeneric Rust async/await runtime\nExtension trait for async/await runtimes that support …\nConvert a Rust Future into a Python awaitable with a …\nConvert a Rust Future into a Python awaitable with a …\nEither copy the task locals from the current task OR get …\nGet the current event loop from either Python or Rust …\nGet the task locals for the current task\nConvert a Python <code>awaitable</code> into a Rust Future\nGet the panic object associated with the error.  Panics if …\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nCheck if the spawned task exited because of a panic\nConvert a <code>!Send</code> Rust Future into a Python awaitable with a …\nConvert a <code>!Send</code> Rust Future into a Python awaitable with a …\nRun the event loop until the given Future completes\nRun the event loop until the given Future completes\nSet the task locals for the given future\nSet the task locals for the given !Send future\nSpawn a future onto this runtime’s event loop\nSpawn a !Send future onto this runtime’s event loop\nArgs that should be provided to the test program\nThe structure used by the <code>#[test]</code> macros to provide a test …\nReturns the argument unchanged.\nReturns the argument unchanged.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nParses test arguments and passes the tests to the …\nThe fully qualified name of the test\nParse the test args from the command line\nCreate the task that runs the test\nThe function used to create the task that runs the test.\nRun a sequence of tests while applying any necessary …\nConvert a Rust Future into a Python awaitable\nConvert a Rust Future into a Python awaitable\nEither copy the task locals from the current task OR get …\nGet the current event loop from either Python or Rust …\nGet a reference to the current tokio runtime\nInitialize the Tokio runtime with a custom build\nInitialize the Tokio runtime with a custom Tokio runtime\nConvert a Python <code>awaitable</code> into a Rust Future\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nunstable-streams Convert an async generator into a stream\nConvert a <code>!Send</code> Rust Future into a Python awaitable\nConvert a <code>!Send</code> Rust Future into a Python awaitable\nattributes Enables an async main function that uses the …\nattributes re-exports for macros\nRun the event loop until the given Future completes\nRun the event loop until the given Future completes\nSet the task local event loop for the given future\nSet the task local event loop for the given !Send future\nattributes testing Registers a <code>tokio</code> test with the …\nre-export pending to be used in tokio macros without …\nre-export tokio::runtime to build runtimes in tokio macros …\nBuilds Tokio Runtime with custom configuration values.\nThe flavor that executes all tasks on the current thread.\nRuntime context guard.\nHandle to the runtime.\nThe flavor that executes tasks across multiple threads.\nThe Tokio runtime.\nThe flavor of a <code>Runtime</code>.\nHandle to the runtime’s metrics.\nError returned by <code>try_current</code> when no Runtime has been …\nRuns a future to completion on this <code>Handle</code>’s associated …\nRuns a future to completion on the Tokio runtime. This is …\nCreates the configured <code>Runtime</code>.\nReturns a <code>Handle</code> view over the currently running <code>Runtime</code>.\nEnables both I/O and time drivers.\nEnables the time driver.\nEnters the runtime context. This allows you to construct …\nEnters the runtime context.\nSets the number of scheduler ticks after which the …\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the argument unchanged.\nReturns the number of tasks currently scheduled in the …\nSets the number of scheduler ticks after which the …\nReturns a handle to the runtime’s spawner.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nCalls <code>U::from(self)</code>.\nReturns true if the call failed because there is currently …\nReturns true if the call failed because the Tokio context …\nSpecifies the limit for additional threads spawned by the …\nReturns a view that lets you get information about how the …\nReturns a view that lets you get information about how the …\nCreates a new runtime instance with default configuration …\nReturns a new builder with the current thread scheduler …\nReturns a new builder with the multi thread scheduler …\nReturns the current number of alive tasks in the runtime.\nReturns the number of worker threads used by the runtime.\nExecutes function <code>f</code> just before a thread is parked (goes …\nExecutes function <code>f</code> after each thread is started but …\nExecutes function <code>f</code> before each thread stops.\nExecutes function <code>f</code> just after a thread unparks (starts …\nReturns the flavor of the current <code>Runtime</code>.\nShuts down the runtime, without waiting for any spawned …\nShuts down the runtime, waiting for at most <code>duration</code> for …\nSpawns a future onto the Tokio runtime.\nSpawns a future onto the Tokio runtime.\nRuns the provided function on an executor dedicated to …\nRuns the provided function on an executor dedicated to …\nSets a custom timeout for a thread in the blocking pool.\nSets name of threads spawned by the <code>Runtime</code>’s thread …\nSets a function used to generate the name of threads …\nSets the stack size (in bytes) for worker threads.\nReturns a Handle view over the currently running Runtime\nSets the number of worker threads the <code>Runtime</code> will use.")
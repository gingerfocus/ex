use ex::Executor;

fn main() {
    let mut ex = Executor::new();

    // Spawn a task
    ex.spawn(async { println!("Hello World!") }).detach();

    // Run an executor thread.
    let handle = std::thread::spawn(move || ex.exec());

    // ... run some code on the main thread

    handle.join().unwrap();
}

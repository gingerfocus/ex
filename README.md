# Ex
A very very very tiny executor. A feature stripped version of [smol](https://crates.io/crates/smol).

Making an executor is as simple as this:
```rust
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
```

*A Suckmore Software project.*

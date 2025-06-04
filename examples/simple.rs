extern crate ex;

fn main() {
    let mut ex = ex::Executor::new();
    ex.spawn(async {
        println!("Hello 1");
        ex::futures::yield_now().await;
        println!("Hello 2");
        return;
    }).detach();

    ex.spawn(async {
        println!("Hello A");
        ex::futures::yield_now().await;
        println!("Hello B");
        return;
    }).detach();

    while ex.try_tick() {}
}

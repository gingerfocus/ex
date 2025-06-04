extern crate ex;

fn main() {
    let mut ex = ex::Executor::new();
    ex.spawn(async {
        println!("Hello 1");
        ex::futures::yield_now().await;
        println!("Hello 2");
        ex::futures::yield_now().await;
        println!("Hello 3");
        ex::futures::yield_now().await;
        println!("Hello 4");
    })
    .detach();

    let t1 = ex.spawn(async {
        println!("Hello A");
        ex::futures::yield_now().await;
        println!("Hello B");
        ex::futures::yield_now().await;
        return false;
    });

    let t2 = ex.spawn(async {
        println!("return value");
        ex::futures::yield_now().await;
        return 3usize;
    });

    ex::futures::block_on(async { assert_eq!(false, ex.run(t1).await) });
    // while ex.try_tick() {}
    ex::futures::block_on(async { assert_eq!(3, t2.await) });
}

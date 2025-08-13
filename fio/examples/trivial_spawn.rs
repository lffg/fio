fn main() {
    fio::block_on(async |io| {
        let a = io.spawn(async move {
            io.sleep(1000).await.unwrap();
            println!("will resolve: 1");
            1
        });
        let b = io.spawn(async move {
            io.sleep(1000).await.unwrap();
            println!("will resolve: 2");
            2
        });
        let result = a.await + b.await;
        println!("got: {result}");
    });
}

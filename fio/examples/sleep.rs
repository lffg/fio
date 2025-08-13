fn main() {
    fio::block_on(async |io| {
        println!("1");
        io.sleep(500).await;
        println!("2");
        io.sleep(300).await;
        println!("3 (done)");
    });
}

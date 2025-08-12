fn main() {
    fio::block_on(async |io| {
        println!("1");
        io.sleep(300).await.unwrap();
        println!("2");
        io.sleep(300).await.unwrap();
        println!("3 (done)");
    });
}

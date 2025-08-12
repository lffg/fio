use fio::block_on;

fn main() {
    block_on(async |io| {
        let buf = vec![0; 256];

        let f = io.open("Cargo.toml").await.expect("should open file");
        let (buf, res) = io.read(buf, f).await;
        let n = res.expect("should read");
        println!("---");
        println!("read {n} bytes:");
        let content = std::str::from_utf8(&buf[..n]).expect("invalid utf-8");
        println!("{}", content.trim());
        println!("---");
    });
}

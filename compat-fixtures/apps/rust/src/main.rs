use pincer_sdk::Pincer;

fn main() {
    let driver = Pincer::connect(None).expect("create compatibility client");
    let endpoint = driver.socket_path();
    assert!(!endpoint.trim().is_empty(), "default endpoint must be selected");
    println!("{endpoint}");
}

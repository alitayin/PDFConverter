#[path = "../pdf.rs"]
mod pdf;
#[path = "../worker.rs"]
mod worker;

fn main() {
    if let Err(error) = worker::run_stdio() {
        eprintln!("Worker output failed: {error}");
        std::process::exit(1);
    }
}

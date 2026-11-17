use std::time::Instant;
fn main() {
    let dir = r"C:/Users/qi.shao/AppData/Local/Temp/sh_bench_torrents";
    let start = Instant::now();
    let torrents = sh_core::parser::scan_directory(dir);
    let elapsed = start.elapsed();
    println!("Rust rayon 并行: {}/10000 in {:.2}s", torrents.expect("scan").len(), elapsed.as_secs_f64());
}

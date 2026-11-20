//! Prints a transcript as Phodder parses it: `cargo run --example parse_transcript FILE`.
fn main() {
    let path = std::env::args().nth(1).expect("usage: parse_transcript FILE");
    let text = std::fs::read_to_string(&path).expect("readable file");
    match phodder_core::transcript::parse(&text) {
        None => println!("nothing readable"),
        Some(t) => {
            println!("timed: {}, lines: {}", t.timed, t.lines.len());
            for line in t.lines.iter().take(8) {
                println!("[{:>7}-{:>7}] {:?}: {}", line.start_ms, line.end_ms, line.speaker, line.text);
            }
            let words: usize = t.lines.iter().map(|l| l.text.split_whitespace().count()).sum();
            println!("average words per line: {}", words / t.lines.len().max(1));
        }
    }
}

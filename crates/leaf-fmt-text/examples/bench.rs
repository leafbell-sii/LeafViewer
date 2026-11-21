//! 찾기·색인 처리량 측정: cargo run --release -p leaf-fmt-text --example bench -- <파일> <검색어>
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use leaf_fmt_text::{LineIndex, TextDoc, encoding};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("파일");
    let query = args.next().unwrap_or_else(|| "needle".into());
    let bytes = std::fs::read(&path).unwrap();
    let enc = encoding::detect(&bytes, 949);
    let doc = TextDoc::new(Arc::new(bytes), enc);
    let t = Instant::now();
    let mut n = 0usize;
    let mut p = Some(doc.start());
    while let Some(s) = p {
        p = doc.row(s).next;
        n += 1;
    }
    println!("rows {n}: {:.0}ms", t.elapsed().as_secs_f64() * 1e3);
    let t = Instant::now();
    let mut buf = Vec::new();
    let mut p = Some(doc.start());
    while let Some(s) = p {
        let r = doc.row(s);
        encoding::decode_into(&doc.enc, &doc.bytes()[r.start..r.end], &mut buf);
        p = r.next;
    }
    println!("rows+decode: {:.0}ms", t.elapsed().as_secs_f64() * 1e3);
    let t = Instant::now();
    let mut p = Some(doc.start());
    let mut acc = 0u64;
    while let Some(s) = p {
        let r = doc.row(s);
        encoding::decode_into(&doc.enc, &doc.bytes()[r.start..r.end], &mut buf);
        leaf_fmt_text::search::fold_in_place(&mut buf);
        acc += buf.len() as u64;
        p = r.next;
    }
    println!(
        "rows+decode+fold: {:.0}ms ({acc} units)",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    let ix = LineIndex::build(&doc, &AtomicBool::new(false), |_| {}).unwrap();
    println!(
        "index {} lines: {:.0}ms",
        ix.lines,
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    let o =
        leaf_fmt_text::search::search(&doc, &query, 100_000, &AtomicBool::new(false), |_, _| {});
    println!(
        "search {} hits: {:.0}ms",
        o.hits,
        t.elapsed().as_secs_f64() * 1e3
    );
}

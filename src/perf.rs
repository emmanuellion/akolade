//! Performance measurements of the hot paths, on generated fixtures.
//! Run: AKOLADE_PERF_DIR=<dir> cargo test --release perf -- --ignored --nocapture

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{completion::complete, explorer, image_view, languages::for_path};

fn fixtures() -> Option<PathBuf> {
    std::env::var("AKOLADE_PERF_DIR").ok().map(PathBuf::from)
}

/// Median of `runs` executions.
fn time(runs: usize, mut f: impl FnMut()) -> Duration {
    let mut all: Vec<Duration> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    all.sort();
    all[runs / 2]
}

#[test]
#[ignore]
fn perf_completion_per_keystroke() {
    let Some(dir) = fixtures() else { return };
    let src = std::fs::read_to_string(dir.join("big.js")).unwrap();
    let lang = for_path(Path::new("a.js"));
    // Typing at the end and in the middle of a 1.8 MB / 42k lines file.
    for (label, at) in [("fin", src.len()), ("milieu", src.len() / 2)] {
        let mut text = src.clone();
        text.insert_str(at, "\nhand");
        let offset = at + 5;
        let d = time(5, || {
            complete(&text, offset, lang).unwrap();
        });
        println!("PERF complétion big.js ({label}) : {d:?} par frappe");
    }
    let small = &src[..20_000];
    let d = time(20, || {
        complete(&format!("{small}\nhand"), small.len() + 5, lang).unwrap();
    });
    println!("PERF complétion fichier 20 Ko : {d:?} par frappe");
}

#[test]
#[ignore]
fn perf_project_scan() {
    let Some(dir) = fixtures() else { return };
    let root = dir.join("big");
    let d = time(5, || {
        let out = explorer::scan(&root, &explorer::ScanOptions::default());
        assert!(out.len() >= 5000);
    });
    println!("PERF scan projet 6000 fichiers : {d:?}");
}

#[test]
#[ignore]
fn perf_image_decode() {
    let Some(dir) = fixtures() else { return };
    let path = dir.join("photo.png");
    if !path.exists() {
        let img = image::RgbaImage::from_fn(4000, 3000, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]));
        img.save(&path).unwrap();
    }
    let d = time(3, || {
        image_view::decode(&path).unwrap();
    });
    println!("PERF décodage PNG 4000×3000 : {d:?}");
}

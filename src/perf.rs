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

/// CPU of a running Akolade (`AKOLADE_PERF_PID`), sampled every second for
/// `AKOLADE_PERF_SECS` (default 20): its own process, not the `claude` it runs.
/// Run: AKOLADE_PERF_PID=<pid> cargo test perf_cpu -- --ignored --nocapture
#[test]
#[ignore]
fn perf_cpu_of_a_running_akolade() {
    let Some(pid) = std::env::var("AKOLADE_PERF_PID").ok() else { return };
    let secs: u64 = std::env::var("AKOLADE_PERF_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    // Cumulated CPU time ("[[dd-]hh:]mm:ss" from `ps -o time=`): the difference over the window is exact.
    let cpu_time = || -> Option<f64> {
        let out = crate::platform::command("ps").args(["-o", "time=", "-p", &pid]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().replace('-', ":");
        Some(text.split(':').fold(0., |acc, part| acc * 60. + part.parse::<f64>().unwrap_or(0.)))
    };
    let (Some(start), t0) = (cpu_time(), Instant::now()) else { return println!("PERF ps indisponible (Windows : utilisez le Gestionnaire des tâches)") };
    let mut peak: f64 = 0.;
    let mut last = start;
    for _ in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        let now = cpu_time().unwrap_or(last);
        peak = peak.max((now - last) * 100.);
        last = now;
    }
    let avg = (last - start) / t0.elapsed().as_secs_f64() * 100.;
    println!("PERF CPU d'Akolade sur {secs} s : moyenne {avg:.1} %, pic sur une seconde {peak:.0} %");
}

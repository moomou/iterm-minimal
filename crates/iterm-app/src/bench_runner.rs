use iterm_profile::Profile;
use iterm_pty::{PtySize, TerminalSession};
use iterm_render::{CachedShaper, FrameBatches, GlyphAtlas, MockShaper, PaneRenderCache};
use iterm_vt::{bench::run_vt_benchmarks, TerminalScreen};
use std::time::{Duration, Instant};

/// Run comprehensive performance benchmarks comparing against iTerm2's baseline
/// from `docs/minimal-perf-plan.md` (CPU text build, VT throughput, scrollback RSS, PTY throughput).
pub fn run_all_benchmarks() {
    println!("==========================================================================");
    println!("  iTermMinimal (Rust) Performance Benchmark Suite");
    println!("==========================================================================\n");

    // 1. Standardized VT Benchmarks (kitten __benchmark__ + vtebench + iTerm2 + 16 MiB dump)
    println!("1) Standardized VT Parse+Exec Benchmarks (250x80 grid, best of 3)");
    println!("   Targets: Kitty mean 134.6 MB/s (ASCII 121.8, Uni 105.0, CSI 59.8, LongEsc 251.6)");
    println!("            Ghostty 1.4 parser ASCII 725 MB/s | iTerm2 mixed 1 MB = 53.49 ms (18.7 MB/s)");
    println!("   ----------------------------------------------------------------------------");
    let mut kitten_sum = 0.0;
    let mut kitten_log_sum = 0.0f64;
    let mut kitten_cnt = 0;
    for res in run_vt_benchmarks() {
        println!(
            "   [{:<8}] {:<40} {:>8.2} ms   {:>9.1} MB/s",
            res.category, res.name, res.elapsed_ms, res.throughput_mb_s
        );
        if res.category == "kitten" {
            kitten_sum += res.throughput_mb_s;
            kitten_log_sum += res.throughput_mb_s.max(0.01).ln();
            kitten_cnt += 1;
        }
    }
    if kitten_cnt > 0 {
        let arith = kitten_sum / kitten_cnt as f64;
        let geomean = (kitten_log_sum / kitten_cnt as f64).exp();
        println!(
            "   >>> kitten 4-workload GEOMEAN: {:.1} MB/s | ARITH MEAN: {:.1} MB/s (Kitty 0.33 ref: 134.6 MB/s)",
            geomean, arith
        );
    }

    // 2. CPU Frame Build (250x80 mono, truecolor, truecolor+ligatures: cold vs 1-dirty-row!)
    println!("\n2) CPU Frame Prep (250x80 = 20,000 cells) — Cold Full vs Incremental 1-Dirty-Row");
    println!("   Baseline iTerm2 (M4 Max): mono=2.93ms | truecolor=39.58ms | tc+lig=61.99ms");
    println!("   ----------------------------------------------------------------------");

    let mut screen = TerminalScreen::new(250, 80, 100_000);
    for r in 0..80 {
        let mut line = String::with_capacity(320);
        for c in 0..5 {
            let rr = ((r * 17 + c * 41) & 0xFF) as u8;
            let gg = ((r * 31 + c * 67) & 0xFF) as u8;
            let bb = ((r * 59 + c * 23) & 0xFF) as u8;
            line.push_str(&format!(
                "\x1b[38;2;{rr};{gg};{bb}mfn step_{r:02}(x: f32) -> bool {{ x != 0.0 && a => b }} "
            ));
        }
        line.push_str("\x1b[0m\r\n");
        screen.feed(line.as_bytes());
    }

    let atlas = GlyphAtlas::new(2048, 2048);
    let mut shaper = CachedShaper::new(MockShaper::default());
    let mut cache = PaneRenderCache::new();
    let mut batches = FrameBatches::default();

    // Warm one throwaway pass so both modes benchmark pure full-frame CPU build (not one-time Vec/atlas allocs!)
    {
        let prof = Profile::default();
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 3000.0, 1920.0, true, 0.0, true, &mut batches,
        );
    }

    for (label, ligatures) in [
        ("250x80 truecolor, no ligatures (cold full)", false),
        ("250x80 truecolor + ligatures (cold full)", true),
    ] {
        let mut prof = Profile::default();
        prof.ascii_ligatures = ligatures;
        cache.invalidate_all();
        shaper.clear_cache();

        let t0 = Instant::now();
        batches.clear();
        cache.prepare_pane(
            &screen, &prof, &mut shaper, &atlas,
            0.0, 0.0, 3000.0, 1920.0, true, 0.0, true, &mut batches,
        );
        let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Warm 1-dirty-row incremental update (avg over 200 frames)
        let iters = 200;
        let t1 = Instant::now();
        for k in 0..iters {
            screen.feed(format!("\x1b[40;1Hprompt> status={k:04} -> ok != err").as_bytes());
            batches.clear();
            cache.prepare_pane(
                &screen, &prof, &mut shaper, &atlas,
                0.0, 0.0, 3000.0, 1920.0, true, 0.0, true, &mut batches,
            );
        }
        let warm_ms = (t1.elapsed().as_secs_f64() * 1000.0) / iters as f64;
        println!(
            "   {:<44} cold={:>6.2} ms | 1-dirty-row={:>6.3} ms ({:>6.0} fps)",
            label,
            cold_ms,
            warm_ms,
            1000.0 / warm_ms.max(1e-6)
        );
    }

    // 3. Scrollback memory footprint (100,000 lines)
    println!("\n3) Chunked Scrollback Memory Footprint");
    let mut sb_screen = TerminalScreen::new(120, 40, 100_000);
    let t_sb = Instant::now();
    let sample_line = b"2026-11-26T18:00:00Z INFO worker::loop processed batch=1024 latency=0.42ms status=200\r\n";
    let mut chunk = Vec::with_capacity(sample_line.len() * 1000);
    for _ in 0..1000 {
        chunk.extend_from_slice(sample_line);
    }
    for _ in 0..100 {
        sb_screen.feed(&chunk);
    }
    let sb_ms = t_sb.elapsed().as_secs_f64() * 1000.0;
    let mem_mb = sb_screen.scrollback.memory_bytes() as f64 / (1024.0 * 1024.0);
    println!(
        "   100,000 lines archived: {} lines retained, {:.2} MiB RAM, ingested in {:.1} ms",
        sb_screen.scrollback.len(),
        mem_mb,
        sb_ms
    );

    // 4. End-to-end real POSIX PTY throughput (16 MiB base64 dump — ComputingForGeeks 2026 benchmark!)
    // Ref: foot=168ms (95 MB/s), Ghostty 1.4=195ms (82 MB/s), Kitty=212ms (75 MB/s)
    println!("\n4) End-to-End POSIX PTY 16 MiB Dump (ref: foot 168ms, Ghostty 195ms, Kitty 212ms)");
    let mut prof = Profile::default();
    prof.custom_command = Some(
        "/bin/sh -c 'head -c 16777216 /dev/zero | tr \"\\0\" \"A\"'"
            .to_string(),
    );
    let t_pty = Instant::now();
    if let Ok(sess) = TerminalSession::spawn(prof, PtySize { cols: 120, rows: 40, pixel_width: 1200, pixel_height: 800 }, None, None) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut done = false;
        while Instant::now() < deadline {
            if sess.has_exited() {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let dt = t_pty.elapsed().as_secs_f64();
        let mb_s = 16.0 / dt.max(1e-6);
        println!(
            "   16 MiB through kernel PTY -> coalesced reader -> VT screen: {:.1} ms ({:.1} MB/s, done={})",
            dt * 1000.0,
            mb_s,
            done
        );
    }
    println!("\n==========================================================================");
}

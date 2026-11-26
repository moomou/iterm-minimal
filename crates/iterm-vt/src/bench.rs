use crate::screen::TerminalScreen;
use std::time::Instant;

/// Summary metrics for one benchmark workload.
#[derive(Debug, Clone)]
pub struct BenchResult {
    pub category: &'static str,
    pub name: &'static str,
    pub bytes: usize,
    pub elapsed_ms: f64,
    pub throughput_mb_s: f64,
}

/// 1) kitten __benchmark__ #1: ASCII (scrolling plain ASCII)
pub fn generate_kitten_ascii() -> Vec<u8> {
    let line = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ !@#$%^&*()_+-=[]{}|;:',.<>/?\r\n";
    let mut buf = Vec::with_capacity(8 * 1024 * 1024 + 256);
    while buf.len() < 8 * 1024 * 1024 {
        buf.extend_from_slice(line);
    }
    buf
}

/// 2) kitten __benchmark__ #2: Unicode (CJK + symbols + multi-byte UTF-8)
pub fn generate_kitten_unicode() -> Vec<u8> {
    let line = "日本語テスト漢字仮名交じり文 → λ ≠ ≤ ≥ ∑ ∏ √ ∞ αβγδεζ ─━│┃┌┐└┘├┤┬┴┼ こんにちは世界!\r\n".as_bytes();
    let mut buf = Vec::with_capacity(4 * 1024 * 1024 + 256);
    while buf.len() < 4 * 1024 * 1024 {
        buf.extend_from_slice(line);
    }
    buf
}

/// 3) kitten __benchmark__ #3: CSI (dense SGR colors + cursor motion + erase)
pub fn generate_kitten_csi() -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 * 1024 * 1024 + 256);
    let mut n = 0u32;
    while buf.len() < 4 * 1024 * 1024 {
        let row = (n % 78) + 1;
        let r = (n * 37) & 0xFF;
        let g = (n * 73) & 0xFF;
        let b = (n * 131) & 0xFF;
        let s = format!(
            "\x1b[{row};1H\x1b[K\x1b[1;38;2;{r};{g};{b};48;5;236m[{n:06}] fn render(a: i32) -> bool {{ a != 0 }}\x1b[0m"
        );
        buf.extend_from_slice(s.as_bytes());
        n += 1;
    }
    buf
}

/// 4) kitten __benchmark__ #4: Long escape codes (long OSC/title/hyperlink/DCS payloads)
pub fn generate_kitten_long_escape() -> Vec<u8> {
    let payload = "A".repeat(4096);
    let seq = format!("\x1b]2;{payload}\x07");
    let mut buf = Vec::with_capacity(16 * 1024 * 1024 + 8192);
    while buf.len() < 16 * 1024 * 1024 {
        buf.extend_from_slice(seq.as_bytes());
    }
    buf
}

/// 5) vtebench: scrolling_bottom_region (DECSTBM 1..79 — exact tmux status-bar scrolling workload!)
pub fn generate_vtebench_tmux_region() -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 * 1024 * 1024 + 256);
    buf.extend_from_slice(b"\x1b[?1049h\x1b[1;79r\x1b[79;1H");
    let line = b"2026-11-26T19:40:00Z INFO tmux::pane scroll line 0123456789abcdef payload=ok latency=0.12ms\r\n";
    while buf.len() < 4 * 1024 * 1024 {
        buf.extend_from_slice(line);
    }
    buf.extend_from_slice(b"\x1b[?1049l");
    buf
}

/// 6) vtebench: dense_cells (per-cell SGR color changes across full grid)
pub fn generate_vtebench_dense_cells() -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 * 1024 * 1024 + 256);
    let mut n = 0u32;
    while buf.len() < 4 * 1024 * 1024 {
        let c1 = (n % 216) + 16;
        let c2 = ((n + 53) % 216) + 16;
        let s = format!("\x1b[38;5;{c1};48;5;{c2}mX\x1b[38;5;{c2};48;5;{c1}mY");
        buf.extend_from_slice(s.as_bytes());
        n += 1;
        if n % 120 == 0 {
            buf.extend_from_slice(b"\r\n");
        }
    }
    buf
}

/// 7) vtebench: alt_screen_random_write (random cursor positioning + overwrite on alt screen)
pub fn generate_vtebench_alt_random() -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 * 1024 * 1024 + 256);
    buf.extend_from_slice(b"\x1b[?1049h");
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    while buf.len() < 4 * 1024 * 1024 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let row = (x % 80) + 1;
        let col = ((x >> 16) % 230) + 1;
        let s = format!("\x1b[{row};{col}Habcdef0123456789");
        buf.extend_from_slice(s.as_bytes());
    }
    buf.extend_from_slice(b"\x1b[?1049l");
    buf
}

/// 8) iTerm2 baseline mixed 1 MB (truecolor + CSI + code)
pub fn generate_mixed_1mb() -> Vec<u8> {
    let mut buf = Vec::with_capacity(1_100_000);
    let mut line = 0u32;
    while buf.len() < 1_048_576 {
        let r = (line * 37) & 0xFF;
        let g = (line * 73) & 0xFF;
        let b = (line * 131) & 0xFF;
        let s = format!(
            "\x1b[38;2;{r};{g};{b}m[{line:06}] fn render_frame(row: &Row) -> Result<(), Error> {{ // fast_path == true && a != b }}\x1b[0m\r\n"
        );
        buf.extend_from_slice(s.as_bytes());
        line += 1;
    }
    buf
}

/// 9) ComputingForGeeks 2026: 16 MiB base64 96-col dump (in-memory VT parse+exec)
pub fn generate_16mb_base64() -> Vec<u8> {
    let line = b"QWERTYUIOPASDFGHJKLZXCVBNMqwertyuiopasdfghjklzxcvbnm0123456789+/QWERTYUIOPASDFGHJKLZXCVBNM012345\r\n";
    let mut buf = Vec::with_capacity(16 * 1024 * 1024 + 256);
    while buf.len() < 16 * 1024 * 1024 {
        buf.extend_from_slice(line);
    }
    buf
}

pub fn run_vt_benchmarks() -> Vec<BenchResult> {
    let workloads: [(&'static str, &'static str, Vec<u8>); 9] = [
        ("kitten", "ASCII (8 MiB)", generate_kitten_ascii()),
        ("kitten", "Unicode (4 MiB)", generate_kitten_unicode()),
        ("kitten", "CSI truecolor+cursor (4 MiB)", generate_kitten_csi()),
        ("kitten", "Long escape OSC (16 MiB)", generate_kitten_long_escape()),
        ("vtebench", "scrolling_bottom_region / tmux (4 MiB)", generate_vtebench_tmux_region()),
        ("vtebench", "dense_cells SGR/cell (4 MiB)", generate_vtebench_dense_cells()),
        ("vtebench", "alt_screen_random_write (4 MiB)", generate_vtebench_alt_random()),
        ("iterm2", "mixed truecolor+CSI (1 MiB)", generate_mixed_1mb()),
        ("dump16m", "base64 96-col scrolling (16 MiB)", generate_16mb_base64()),
    ];

    let mut results = Vec::with_capacity(workloads.len());
    for (category, name, data) in &workloads {
        let mut screen = TerminalScreen::new(250, 80, 100_000);
        // Warm up
        screen.feed(&data[..data.len().min(65536)]);
        screen.clear_scrollback_and_screen();

        // Take best of 3 runs to reduce OS jitter
        let mut best_dt = f64::MAX;
        for _ in 0..3 {
            screen.clear_scrollback_and_screen();
            let t0 = Instant::now();
            screen.feed(data);
            let dt = t0.elapsed().as_secs_f64();
            if dt < best_dt {
                best_dt = dt;
            }
        }
        let ms = best_dt * 1000.0;
        let mb_s = (data.len() as f64 / (1024.0 * 1024.0)) / best_dt.max(1e-9);
        results.push(BenchResult {
            category,
            name,
            bytes: data.len(),
            elapsed_ms: ms,
            throughput_mb_s: mb_s,
        });
    }

    // Honest realistic chunked streaming workload: 8 MiB ASCII fed in 64 KiB PTY-sized chunks!
    {
        let data = &workloads[0].2;
        let mut screen = TerminalScreen::new(250, 80, 100_000);
        let mut best_dt = f64::MAX;
        for _ in 0..3 {
            screen.clear_scrollback_and_screen();
            let t0 = Instant::now();
            for chunk in data.chunks(65_536) {
                screen.feed(chunk);
            }
            let dt = t0.elapsed().as_secs_f64();
            if dt < best_dt {
                best_dt = dt;
            }
        }
        results.push(BenchResult {
            category: "stream64",
            name: "ASCII 64 KiB chunks (8 MiB)",
            bytes: data.len(),
            elapsed_ms: best_dt * 1000.0,
            throughput_mb_s: (data.len() as f64 / (1024.0 * 1024.0)) / best_dt.max(1e-9),
        });
    }

    results
}

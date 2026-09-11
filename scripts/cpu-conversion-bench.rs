// Staged with baseline/candidate source copies by benchmark-cpu-conversion.ps1.
mod baseline;
mod baseline_layout;
mod candidate;
mod candidate_layout;

use std::{hint::black_box, time::Instant};

fn random(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn verify(cases: usize) {
    let mut state = 0x72ab91;
    let mut accepted = 0;
    for case in 0..cases {
        let sw = 1 + random(&mut state) % 37;
        let sh = 1 + random(&mut state) % 37;
        let ow = 2 * (1 + random(&mut state) % 20);
        let oh = 2 * (1 + random(&mut state) % 20);
        let crop = if case % 2 == 0 {
            let x = random(&mut state) % sw;
            let y = random(&mut state) % sh;
            Some((
                x,
                y,
                1 + random(&mut state) % (sw - x),
                1 + random(&mut state) % (sh - y),
            ))
        } else {
            None
        };
        let padding = (case % 3) * 7;
        let stride = sw as usize * 4 + padding;
        // No padding after the last row: exercise the minimum accepted buffer.
        let pixels: Vec<_> = (0..stride * (sh as usize - 1) + sw as usize * 4)
            .map(|_| random(&mut state) as u8)
            .collect();
        let a = baseline::CpuVideoConverter::new(
            sw,
            sh,
            crop.map(|(x, y, width, height)| baseline::CpuCropRect {
                x,
                y,
                width,
                height,
            }),
            ow,
            oh,
        );
        let b = candidate::CpuVideoConverter::new(
            sw,
            sh,
            crop.map(|(x, y, width, height)| candidate::CpuCropRect {
                x,
                y,
                width,
                height,
            }),
            ow,
            oh,
        );
        match (a, b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(
                    a.convert(&pixels, stride).unwrap(),
                    b.convert(&pixels, stride).unwrap(),
                    "case {case}: {sw}x{sh}->{ow}x{oh}, crop {crop:?}, padding {padding}"
                );
                accepted += 1;
            }
            (Err(a), Err(b)) => assert_eq!(format!("{a:?}"), format!("{b:?}")),
            _ => panic!("constructor mismatch at case {case}"),
        }
    }
    eprintln!(
        "Verified {cases} cases: {accepted} byte-equal outputs, {} matching rejections",
        cases - accepted
    );
}

#[inline(never)]
fn baseline_run(
    converter: &baseline::CpuVideoConverter,
    pixels: &[u8],
    stride: usize,
    frames: usize,
) -> f64 {
    let start = Instant::now();
    for _ in 0..frames {
        black_box(
            black_box(converter)
                .convert(black_box(pixels), black_box(stride))
                .unwrap(),
        );
    }
    start.elapsed().as_secs_f64() * 1000.0 / frames as f64
}

#[inline(never)]
fn candidate_run(
    converter: &candidate::CpuVideoConverter,
    pixels: &[u8],
    stride: usize,
    frames: usize,
) -> f64 {
    let start = Instant::now();
    for _ in 0..frames {
        black_box(
            black_box(converter)
                .convert(black_box(pixels), black_box(stride))
                .unwrap(),
        );
    }
    start.elapsed().as_secs_f64() * 1000.0 / frames as f64
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let cases: usize = args[2].parse().unwrap();
    let rounds: usize = args[3].parse().unwrap();
    let frames: usize = args[4].parse().unwrap();
    verify(cases);
    if args[1] == "verify" {
        return;
    }
    println!("sw,sh,ow,oh,round,baseline_ms,candidate_ms,ratio");
    for (sw, sh, ow, oh) in [
        (1920, 1080, 1920, 1080),
        (5120, 1440, 1920, 540),
        (5120, 1440, 2560, 720),
        (1920, 1080, 1920, 1200),
        (1080, 1920, 1920, 1080),
    ] {
        let a = baseline::CpuVideoConverter::new(sw, sh, None, ow, oh).unwrap();
        let b = candidate::CpuVideoConverter::new(sw, sh, None, ow, oh).unwrap();
        let stride = sw as usize * 4;
        let mut state = 1234567;
        let pixels: Vec<_> = (0..stride * sh as usize)
            .map(|_| random(&mut state) as u8)
            .collect();
        assert_eq!(
            a.convert(&pixels, stride).unwrap(),
            b.convert(&pixels, stride).unwrap()
        );
        baseline_run(&a, &pixels, stride, 8);
        candidate_run(&b, &pixels, stride, 8);
        for round in 0..rounds {
            let (old, new) = if round % 2 == 0 {
                let old = baseline_run(&a, &pixels, stride, frames);
                (old, candidate_run(&b, &pixels, stride, frames))
            } else {
                let new = candidate_run(&b, &pixels, stride, frames);
                (baseline_run(&a, &pixels, stride, frames), new)
            };
            println!(
                "{sw},{sh},{ow},{oh},{round},{old:.6},{new:.6},{:.6}",
                new / old
            );
        }
    }
}

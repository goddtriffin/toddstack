//! Demos and the parity check, each on a GPU backend and on the CPU, writing PNGs.
//!
//! `cargo run --release --example demo -- <board|chart|grid|images|paths|texture|quantize|parity|all> [--backend B] [--frames N] [--out DIR]`
//! `B` is vulkan, dx12, metal, gl or cpu; `WGPU_BACKEND` does the same. Default: the fallback chain.

use std::path::{Path, PathBuf};
use std::time::Instant;
use toddstack_renderer::demo;
use toddstack_renderer::{
    Backend, Budget, Diff, Kernel, KernelPath, Options, Renderer, Scene, Target,
};

const FONT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/Arimo-Bold.ttf"
));

struct Args {
    demo: String,
    backend: Option<Backend>,
    frames: usize,
    out: PathBuf,
}

fn args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        argv.iter()
            .position(|a| a == k)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let backend: Option<Backend> = get("--backend")
        .or_else(|| std::env::var("WGPU_BACKEND").ok())
        .and_then(|s| Backend::parse(&s));
    Args {
        demo: argv
            .first()
            .filter(|a| !a.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "all".to_owned()),
        backend,
        frames: get("--frames").and_then(|s| s.parse().ok()).unwrap_or(30),
        out: PathBuf::from(get("--out").unwrap_or_else(|| "out".to_owned())),
    }
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .expect("create the output directory");
    let file: std::io::BufWriter<std::fs::File> =
        std::io::BufWriter::new(std::fs::File::create(path).expect("create the PNG"));
    let mut enc: png::Encoder<_> = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("png header")
        .write_image_data(rgba)
        .expect("png data");
    println!("  wrote {}", path.display());
}

fn p50_max(mut ms: Vec<f64>) -> (f64, f64) {
    ms.sort_by(f64::total_cmp);
    (ms[ms.len() / 2], ms[ms.len() - 1])
}

async fn renderer(a: &Args, t: Target, force: Option<Backend>) -> Renderer {
    let mut opts: Options = Options::from_env();
    opts.force = force.or(a.backend);
    let r: Renderer = Renderer::with_options(t, opts).await.expect("a renderer");
    let c = r.capabilities();
    println!(
        "  backend {} | adapter {} | compute {} | storage buffers {}",
        c.backend.name(),
        c.adapter,
        c.compute,
        c.storage_buffers
    );
    r
}

/// Renders `build`'s scene `frames` times on the chosen backend and once on the CPU; prints frame
/// time (render + readback) p50 and max, writes both PNGs, prints the diff.
async fn scene_demo<F>(a: &Args, name: &str, t: Target, build: F)
where
    F: Fn(&mut Renderer) -> Scene,
{
    println!("{name} ({}x{})", t.width, t.height);
    let mut gpu: Renderer = renderer(a, t, None).await;
    let scene: Scene = build(&mut gpu);
    let mut ms: Vec<f64> = Vec::new();
    let mut px: Vec<u8> = Vec::new();
    for _ in 0..a.frames.max(1) {
        let s: Instant = Instant::now();
        px = gpu.render(&scene).await.expect("render");
        ms.push(s.elapsed().as_secs_f64() * 1e3);
    }
    let (p50, max): (f64, f64) = p50_max(ms);
    println!(
        "  frame ms over {}: p50 {p50:.3}  max {max:.3}",
        a.frames.max(1)
    );
    write_png(
        &a.out.join(format!("{name}-{}.png", gpu.backend().name())),
        t.width,
        t.height,
        &px,
    );
    let mut cpu: Renderer = renderer(a, t, Some(Backend::Cpu)).await;
    let scene: Scene = build(&mut cpu);
    let s: Instant = Instant::now();
    let reference: Vec<u8> = cpu.render(&scene).await.expect("cpu render");
    println!("  cpu frame ms: {:.3}", s.elapsed().as_secs_f64() * 1e3);
    write_png(
        &a.out.join(format!("{name}-cpu.png")),
        t.width,
        t.height,
        &reference,
    );
    println!("  gpu vs cpu: {}", Diff::between(&reference, &px));
}

async fn quantize(a: &Args) {
    let (width, height): (u32, u32) = (640, 480);
    println!("quantize ({width}x{height})");
    let rend: Renderer = renderer(a, Target::offscreen(width, height), None).await;
    let kernel: Kernel = demo::quantize_kernel(width, height);
    let cpu_out = rend
        .run_kernel_with(&kernel, KernelPath::Cpu)
        .await
        .expect("cpu kernel");
    write_png(
        &a.out.join("quantize-cpu.png"),
        width,
        height,
        &cpu_out.rgba8,
    );
    for path in [KernelPath::Compute, KernelPath::Fragment] {
        let mut ms: Vec<f64> = Vec::new();
        let mut last = None;
        for _ in 0..a.frames.max(1) {
            let s: Instant = Instant::now();
            match rend.run_kernel_with(&kernel, path).await {
                Ok(o) => last = Some(o),
                Err(e) => {
                    println!("  {path:?}: not available here ({e})");
                    break;
                }
            }
            ms.push(s.elapsed().as_secs_f64() * 1e3);
        }
        if let Some(o) = last {
            let (p50, slowest): (f64, f64) = p50_max(ms);
            let differing: usize = o
                .rgba8
                .as_chunks::<4>()
                .0
                .iter()
                .zip(cpu_out.rgba8.as_chunks::<4>().0.iter())
                .filter(|(l, rt)| l[..3] != rt[..3])
                .count();
            println!(
                "  {path:?}: frame ms p50 {p50:.3}  max {slowest:.3}  pixels differing from cpu: {differing} of {}",
                width * height
            );
            write_png(
                &a.out.join(format!(
                    "quantize-{}-{}.png",
                    rend.backend().name(),
                    format!("{path:?}").to_lowercase()
                )),
                width,
                height,
                &o.rgba8,
            );
        }
    }
    println!(
        "  best form on this backend: {:?}",
        rend.run_kernel(&kernel).await.expect("kernel").path
    );
}

async fn parity(a: &Args) -> bool {
    println!(
        "parity (board + chart scene, 800x520, vs CPU; budget {:?})",
        Budget::ARENA
    );
    let backends: Vec<Backend> = a.backend.map_or_else(
        || {
            Backend::chain()
                .iter()
                .copied()
                .filter(|b| *b != Backend::Cpu)
                .collect()
        },
        |b| vec![b],
    );
    let mut ok: bool = true;
    for b in backends {
        match demo::parity(b, FONT).await.expect("parity run") {
            None => println!("  {}: SKIP (no adapter on this machine)", b.name()),
            Some((caps, diff)) => {
                let pass: bool = diff.within(&Budget::ARENA);
                ok &= pass;
                println!(
                    "  {}: {} | {} | adapter {}",
                    b.name(),
                    if pass { "PASS" } else { "FAIL" },
                    diff,
                    caps.adapter
                );
            }
        }
    }
    ok
}

fn main() {
    let a: Args = args();
    pollster::block_on(async {
        let all: bool = a.demo == "all";
        let mut ok: bool = true;
        if all || a.demo == "board" {
            scene_demo(&a, "board", Target::offscreen(520, 520), |r| {
                demo::board_scene(r.add_font(FONT).expect("font"))
            })
            .await;
        }
        if all || a.demo == "chart" {
            scene_demo(&a, "chart", Target::offscreen(640, 400), |r| {
                demo::chart_scene(r.add_font(FONT).expect("font"))
            })
            .await;
        }
        if all || a.demo == "grid" {
            scene_demo(&a, "grid", Target::offscreen(1000, 1000), |r| {
                let id = r.add_batch(demo::grid_batch(1000, 1000)).expect("batch");
                r.set_batch_params(id, [1000.0, 1.0, 0.0, 0.0])
                    .expect("params");
                let mut s: Scene = Scene::new([255, 255, 255, 255]);
                s.batch(id);
                s
            })
            .await;
        }
        if all || a.demo == "images" {
            scene_demo(&a, "images", Target::offscreen(640, 260), |r| {
                let font = r.add_font(FONT).expect("font");
                demo::images_scene(r, font).expect("images scene")
            })
            .await;
        }
        if all || a.demo == "paths" {
            scene_demo(&a, "paths", Target::offscreen(640, 400), |r| {
                demo::paths_scene(r.add_font(FONT).expect("font"))
            })
            .await;
        }
        if all || a.demo == "texture" {
            scene_demo(&a, "texture", Target::offscreen(640, 270), |r| {
                let font = r.add_font(FONT).expect("font");
                demo::texture_scene(r, font).expect("texture scene")
            })
            .await;
        }
        if all || a.demo == "quantize" {
            quantize(&a).await;
        }
        if all || a.demo == "parity" {
            ok = parity(&a).await;
        }
        if !ok {
            std::process::exit(1);
        }
    });
}

//! Each GPU backend available here against the CPU reference, on the board scene.

use toddstack_renderer::{
    Backend, Budget, Diff, Kernel, KernelPath, Options, Renderer, Target, demo,
};

const FONT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/Arimo-Bold.ttf"
));

#[test]
fn every_available_gpu_backend_matches_the_cpu_within_the_arena_budget() {
    let mut ran: usize = 0;
    for b in Backend::chain()
        .iter()
        .copied()
        .filter(|b| *b != Backend::Cpu)
    {
        let Some((caps, diff)) = pollster::block_on(demo::parity(b, FONT)).expect("parity run")
        else {
            println!("{}: skipped, no adapter", b.name());
            continue;
        };
        println!("{}: {diff} ({})", b.name(), caps.adapter);
        assert!(
            diff.within(&Budget::ARENA),
            "{} is outside the budget: {diff}",
            b.name()
        );
        ran += 1;
    }
    println!("{ran} GPU backend(s) compared");
}

#[test]
fn the_diff_of_an_image_with_itself_is_zero_and_a_flipped_image_fails_the_budget() {
    let a: Vec<u8> = vec![10; 16 * 4];
    let b: Vec<u8> = vec![200; 16 * 4];
    assert_eq!(
        Diff {
            over_1: 0.0,
            over_8: 0.0,
            over_32: 0.0,
            max: 0
        },
        Diff::between(&a, &a)
    );
    assert!(!Diff::between(&a, &b).within(&Budget::ARENA));
}

#[test]
fn the_cpu_backend_is_always_selected_when_forced_and_reports_no_compute() {
    let r: Renderer = pollster::block_on(Renderer::with_options(
        Target::offscreen(8, 8),
        Options {
            force: Some(Backend::Cpu),
            data_texture: None,
        },
    ))
    .expect("cpu renderer");
    assert_eq!(Backend::Cpu, r.backend());
    assert!(!r.capabilities().compute);
}

#[test]
fn a_kernel_runs_on_the_cpu_form_and_every_available_gpu_form_agrees_with_it() {
    let (width, height): (u32, u32) = (96, 64);
    let kernel: Kernel = demo::quantize_kernel(width, height);
    let rend: Renderer =
        pollster::block_on(Renderer::new(Target::offscreen(width, height))).expect("renderer");
    let cpu = pollster::block_on(rend.run_kernel_with(&kernel, KernelPath::Cpu)).expect("cpu form");
    assert_eq!((width * height * 4) as usize, cpu.rgba8.len());
    for path in [KernelPath::Compute, KernelPath::Fragment] {
        let Ok(o) = pollster::block_on(rend.run_kernel_with(&kernel, path)) else {
            println!("{path:?}: not available on {}", rend.backend().name());
            continue;
        };
        let differing: usize = o
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .zip(cpu.rgba8.as_chunks::<4>().0)
            .filter(|(l, rt)| l[..3] != rt[..3])
            .count();
        println!("{path:?}: {differing} differing pixels");
        assert!(
            differing * 1000 <= (width * height) as usize,
            "{path:?} differs from the CPU on {differing} pixels"
        );
    }
}

#[test]
fn a_grid_batch_renders_the_same_on_gpu_storage_gpu_data_texture_and_cpu() {
    let (cols, rows): (u32, u32) = (64, 48);
    let render = |force: Backend, data_texture: Option<bool>| -> Option<Vec<u8>> {
        pollster::block_on(async {
            let mut r: Renderer = Renderer::with_options(
                Target::offscreen(cols, rows),
                Options {
                    force: Some(force),
                    data_texture,
                },
            )
            .await
            .ok()?;
            let id = r.add_batch(demo::grid_batch(cols, rows)).expect("batch");
            r.set_batch_params(id, [64.0, 1.0, 0.0, 0.0])
                .expect("params");
            let mut s = toddstack_renderer::Scene::new([255, 255, 255, 255]);
            s.batch(id);
            Some(r.render(&s).await.expect("render"))
        })
    };
    let cpu: Vec<u8> = render(Backend::Cpu, None).expect("cpu");
    for b in Backend::chain()
        .iter()
        .copied()
        .filter(|b| *b != Backend::Cpu)
    {
        for dt in [Some(false), Some(true)] {
            let Some(px) = render(b, dt) else { continue };
            let diff: Diff = Diff::between(&cpu, &px);
            println!("{} data_texture={dt:?}: {diff}", b.name());
            assert_eq!(0.0, diff.over_1, "{} {dt:?} differs from the CPU", b.name());
        }
    }
}

#[test]
fn a_partial_batch_update_changes_only_those_tiles_and_matches_the_cpu() {
    let (cols, rows): (u32, u32) = (64, 48);
    let render = |force: Backend, data_texture: Option<bool>| -> Option<(Vec<u8>, Vec<u8>)> {
        pollster::block_on(async {
            let mut r: Renderer = Renderer::with_options(
                Target::offscreen(cols, rows),
                Options {
                    force: Some(force),
                    data_texture,
                },
            )
            .await
            .ok()?;
            let id = r.add_batch(demo::grid_batch(cols, rows)).expect("batch");
            r.set_batch_params(id, [64.0, 1.0, 0.0, 0.0])
                .expect("params");
            let mut s = toddstack_renderer::Scene::new([255, 255, 255, 255]);
            s.batch(id);
            let before: Vec<u8> = r.render(&s).await.expect("render");
            r.update_batch(id, 3000, &[0x100 | 5; 40]).expect("update");
            Some((before, r.render(&s).await.expect("render")))
        })
    };
    let (cpu_before, cpu_after): (Vec<u8>, Vec<u8>) = render(Backend::Cpu, None).expect("cpu");
    assert_ne!(cpu_before, cpu_after);
    for b in Backend::chain()
        .iter()
        .copied()
        .filter(|b| *b != Backend::Cpu)
    {
        for dt in [Some(false), Some(true)] {
            let Some((before, after)) = render(b, dt) else {
                continue;
            };
            assert_eq!(cpu_before, before, "{} {dt:?} before the update", b.name());
            assert_eq!(cpu_after, after, "{} {dt:?} after the update", b.name());
        }
    }
}

//! Each GPU backend available here against the CPU reference, on the board scene.

use toddstack_renderer::{
    Backend, Budget, Diff, Kernel, KernelPath, Options, Renderer, Sampling, Scene, Target, demo,
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
        Options::new().with_force(Backend::Cpu),
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
                Options::new()
                    .with_force(force)
                    .with_data_texture(data_texture),
            )
            .await
            .ok()?;
            let id = r
                .add_batch(demo::grid_batch(cols, rows))
                .await
                .expect("batch");
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
                Options::new()
                    .with_force(force)
                    .with_data_texture(data_texture),
            )
            .await
            .ok()?;
            let id = r
                .add_batch(demo::grid_batch(cols, rows))
                .await
                .expect("batch");
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

/// Renders `build`'s scene on `force`; `None` if that backend is not here.
fn render_on<F>(force: Backend, target: Target, build: F) -> Option<Vec<u8>>
where
    F: Fn(&mut Renderer) -> Scene,
{
    pollster::block_on(async {
        let mut r: Renderer = Renderer::with_options(target, Options::new().with_force(force))
            .await
            .ok()?;
        let scene: Scene = build(&mut r);
        Some(r.render(&scene).await.expect("render"))
    })
}

fn gpu_backends() -> impl Iterator<Item = Backend> {
    Backend::chain()
        .iter()
        .copied()
        .filter(|b| *b != Backend::Cpu)
}

fn scene_parity<F>(name: &str, target: &Target, budget: &Budget, build: F)
where
    F: AsyncFn(&mut Renderer) -> toddstack_renderer::Result<Scene> + Copy,
{
    for b in gpu_backends() {
        let Some((caps, diff)) =
            pollster::block_on(demo::parity_scene(b, target.clone(), build)).expect("parity run")
        else {
            println!("{name} {}: skipped, no adapter", b.name());
            continue;
        };
        println!("{name} {}: {diff} ({})", b.name(), caps.adapter);
        assert!(
            diff.within(budget),
            "{name} on {} is outside the budget: {diff}",
            b.name()
        );
    }
}

#[test]
fn image_quads_scaled_nearest_and_linear_with_opacity_match_the_cpu_within_the_arena_budget() {
    scene_parity(
        "images",
        &Target::offscreen(640, 260),
        &Budget::ARENA,
        async |r| {
            let font = r.add_font(FONT)?;
            demo::images_scene(r, font)
        },
    );
}

#[test]
fn paths_with_curves_dashes_joins_holes_and_text_between_runs_match_the_cpu() {
    scene_parity(
        "paths",
        &Target::offscreen(640, 400),
        &Budget::PATHS,
        async |r| Ok(demo::paths_scene(r.add_font(FONT)?)),
    );
}

#[test]
fn a_kernel_output_drawn_as_a_scene_image_matches_the_cpu_form() {
    scene_parity(
        "texture",
        &Target::offscreen(640, 270),
        &Budget::KERNEL,
        async |r| {
            let font = r.add_font(FONT)?;
            demo::texture_scene(r, font).await
        },
    );
}

#[test]
fn a_kernel_image_drawn_one_to_one_equals_the_kernel_readback_on_every_gpu_form() {
    let (width, height): (u32, u32) = (96, 64);
    let kernel: Kernel = demo::quantize_kernel(width, height);
    for b in gpu_backends() {
        let Ok(mut r) = pollster::block_on(Renderer::with_options(
            Target::offscreen(width, height),
            Options::new().with_force(b),
        )) else {
            continue;
        };
        for path in [KernelPath::Compute, KernelPath::Fragment] {
            let Ok((id, ran)) =
                pollster::block_on(r.kernel_image_with(&kernel, path, Sampling::Nearest))
            else {
                println!("{} {path:?}: not available", b.name());
                continue;
            };
            assert_eq!(path, ran);
            let mut s: Scene = Scene::new([0, 0, 0, 255]);
            s.image(id, 0.0, 0.0, 96.0, 64.0, 1.0);
            let drawn: Vec<u8> = pollster::block_on(r.render(&s)).expect("render");
            let read: Vec<u8> = pollster::block_on(r.run_kernel_with(&kernel, path))
                .expect("readback")
                .rgba8;
            println!("{} {path:?}: {}", b.name(), Diff::between(&read, &drawn));
            assert_eq!(
                read,
                drawn,
                "{} {path:?}: drawn image != readback",
                b.name()
            );
        }
    }
}

#[test]
fn a_nearest_upscale_is_exact_and_a_removed_or_unknown_image_is_refused() {
    let tiny: Vec<u8> = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [9, 9, 9, 0],
    ]
    .concat();
    let build = |r: &mut Renderer| {
        let id = r.add_image(2, 2, &tiny, Sampling::Nearest).expect("image");
        let mut s: Scene = Scene::new([255, 255, 255, 255]);
        s.image(id, 0.0, 0.0, 8.0, 8.0, 1.0);
        s
    };
    let cpu: Vec<u8> = render_on(Backend::Cpu, Target::offscreen(8, 8), build).expect("cpu");
    assert_eq!([255, 0, 0, 255], cpu[..4], "top-left texel");
    assert_eq!([0, 255, 0, 255], cpu[7 * 4..8 * 4], "top-right texel");
    assert_eq!(
        [255, 255, 255, 255],
        cpu[(7 * 8 + 7) * 4..],
        "transparent texel"
    );
    for b in gpu_backends() {
        let Some(px) = render_on(b, Target::offscreen(8, 8), build) else {
            continue;
        };
        assert_eq!(cpu, px, "{} nearest upscale", b.name());
    }
    let mut r: Renderer = pollster::block_on(Renderer::with_options(
        Target::offscreen(8, 8),
        Options::new().with_force(Backend::Cpu),
    ))
    .expect("cpu renderer");
    assert!(r.add_image(2, 2, &[0; 3], Sampling::Linear).is_err());
    let id = r.add_image(2, 2, &tiny, Sampling::Linear).expect("image");
    r.remove_image(id).expect("remove");
    assert!(r.remove_image(id).is_err());
}

#[test]
fn paths_fill_non_zero_winding_keep_holes_and_respect_paint_order_on_every_backend() {
    use toddstack_renderer::tiny_skia::PathBuilder;
    let build = |_: &mut Renderer| {
        let mut s: Scene = Scene::new([255, 255, 255, 255]);
        // A pentagram: its centre is wound twice and is filled.
        let mut star: PathBuilder = PathBuilder::new();
        for k in 0..5_u8 {
            let a: f32 =
                f32::from(k * 2) * std::f32::consts::TAU / 5.0 - std::f32::consts::FRAC_PI_2;
            let (x, y): (f32, f32) = (40.0 + 36.0 * a.cos(), 40.0 + 36.0 * a.sin());
            if k == 0 {
                star.move_to(x, y);
            } else {
                star.line_to(x, y);
            }
        }
        star.close();
        s.fill_path(&star.finish().expect("star"), [200, 0, 0, 255]);
        // A frame whose inner square is wound the other way: a hole.
        let mut frame: PathBuilder = PathBuilder::new();
        frame.move_to(80.0, 4.0);
        frame.line_to(156.0, 4.0);
        frame.line_to(156.0, 76.0);
        frame.line_to(80.0, 76.0);
        frame.close();
        for (i, (x, y)) in [(96.0, 20.0), (96.0, 60.0), (140.0, 60.0), (140.0, 20.0)]
            .iter()
            .enumerate()
        {
            if i == 0 {
                frame.move_to(*x, *y);
            } else {
                frame.line_to(*x, *y);
            }
        }
        frame.close();
        s.fill_path(&frame.finish().expect("frame"), [0, 0, 200, 255]);
        // A plain rect between two path runs, then a path over it.
        s.rect(0.0, 84.0, 160.0, 12.0, [0, 160, 0, 255]);
        let tile = toddstack_renderer::rounded_rect(60.0, 80.0, 40.0, 20.0, 6.0).expect("tile");
        s.fill_path(&tile, [255, 255, 0, 255]);
        s
    };
    let target: Target = Target::offscreen(160, 100);
    let at = |px: &[u8], x: usize, y: usize| -> [u8; 3] {
        let o: usize = (y * 160 + x) * 4;
        [px[o], px[o + 1], px[o + 2]]
    };
    let mut all: Vec<Backend> = vec![Backend::Cpu];
    all.extend(gpu_backends());
    for b in all {
        let Some(px) = render_on(b, target.clone(), build) else {
            continue;
        };
        assert_eq!(
            [200, 0, 0],
            at(&px, 40, 40),
            "{} star centre is filled",
            b.name()
        );
        assert_eq!([255, 255, 255], at(&px, 4, 4), "{} outside", b.name());
        assert_eq!([0, 0, 200], at(&px, 88, 40), "{} frame ring", b.name());
        assert_eq!([255, 255, 255], at(&px, 118, 40), "{} frame hole", b.name());
        assert_eq!([0, 160, 0], at(&px, 10, 90), "{} rect", b.name());
        assert_eq!(
            [255, 255, 0],
            at(&px, 80, 90),
            "{} path over the rect",
            b.name()
        );
    }
}

#[test]
fn batch_updates_and_instance_counts_are_range_checked_on_the_cpu_backend() {
    pollster::block_on(async {
        let mut r: Renderer = Renderer::with_options(
            Target::offscreen(8, 8),
            Options::new().with_force(Backend::Cpu),
        )
        .await
        .expect("cpu renderer");
        let id = r.add_batch(demo::grid_batch(4, 4)).await.expect("batch");
        // An offset that overflows usize is an error, not a panic.
        assert!(r.update_batch(id, usize::MAX, &[1, 2]).is_err());
        assert!(r.update_batch(id, 15, &[1, 2]).is_err());
        r.update_batch(id, 14, &[1, 2]).expect("in range");
        // The count is not checked against the data: a batch may draw from instance_index and
        // params alone.
        r.set_batch_instances(id, 17).expect("not checked");
        r.set_batch_instances(id, 16).expect("16 fits");
    });
}

#[test]
fn bad_wgsl_in_a_batch_or_a_kernel_is_an_error_not_a_panic_on_every_gpu_backend() {
    for b in gpu_backends() {
        pollster::block_on(async {
            let Ok(mut r) =
                Renderer::with_options(Target::offscreen(16, 16), Options::new().with_force(b))
                    .await
            else {
                println!("bad wgsl {}: skipped, no adapter", b.name());
                return;
            };
            let mut batch = demo::grid_batch(4, 4);
            batch.wgsl = "this is not wgsl".to_owned();
            let err = r.add_batch(batch).await.expect_err("bad batch wgsl");
            assert!(
                matches!(err, toddstack_renderer::Error::Shader(_)),
                "{}: {err}",
                b.name()
            );
            let mut kernel: Kernel = demo::quantize_kernel(16, 16);
            kernel.fragment = Some("this is not wgsl".to_owned());
            kernel.compute = Some("this is not wgsl".to_owned());
            for path in [KernelPath::Compute, KernelPath::Fragment] {
                if let Err(e) = r.run_kernel_with(&kernel, path).await {
                    println!("{} {path:?}: {e}", b.name());
                } else {
                    panic!("{} {path:?}: bad WGSL ran", b.name());
                }
            }
            // The renderer still works after the rejected shaders.
            let good = r.add_batch(demo::grid_batch(4, 4)).await;
            assert!(good.is_ok(), "{}: {good:?}", b.name());
        });
    }
}

#[test]
fn a_half_transparent_fill_over_a_clear_of_zero_matches_across_backends() {
    // Premultiplied, all four channels: the GPU's blend and the CPU's pixmap must agree.
    scene_parity_transparent(
        "transparent",
        &Target::offscreen(320, 200),
        &Budget::ARENA,
        async |r| {
            let font = r.add_font(FONT)?;
            demo::transparent_scene(r, font)
        },
    );
    let build = |_: &mut Renderer| {
        let mut s: Scene = Scene::new([0, 0, 0, 0]);
        s.rect(0.0, 0.0, 8.0, 8.0, [255, 0, 0, 128]);
        s
    };
    let mut all: Vec<Backend> = vec![Backend::Cpu];
    all.extend(gpu_backends());
    for b in all {
        let Some(px) = render_transparent_on(b, Target::offscreen(16, 16), build) else {
            continue;
        };
        let inside: [u8; 4] = [px[4 * 4], px[4 * 4 + 1], px[4 * 4 + 2], px[4 * 4 + 3]];
        let outside: &[u8] = &px[(12 * 16 + 12) * 4..(12 * 16 + 12) * 4 + 4];
        assert_eq!([0, 0, 0, 0], outside, "{} outside is clear", b.name());
        for (got, want) in inside.iter().zip([128_u8, 0, 0, 128]) {
            assert!(
                got.abs_diff(want) <= 1,
                "{} premultiplied pixel {inside:?}",
                b.name()
            );
        }
        let straight: Vec<u8> = toddstack_renderer::unpremultiply_rgba8(&px);
        assert!(
            straight[4 * 4].abs_diff(255) <= 1 && straight[4 * 4 + 3].abs_diff(128) <= 1,
            "{} unpremultiplied {:?}",
            b.name(),
            &straight[4 * 4..4 * 4 + 4]
        );
    }
}

fn render_transparent_on<F>(force: Backend, target: Target, build: F) -> Option<Vec<u8>>
where
    F: Fn(&mut Renderer) -> Scene,
{
    pollster::block_on(async {
        let mut r: Renderer = Renderer::with_options(
            target,
            Options::new().with_force(force).with_transparent(true),
        )
        .await
        .ok()?;
        let scene: Scene = build(&mut r);
        Some(r.render(&scene).await.expect("render"))
    })
}

fn scene_parity_transparent<F>(name: &str, target: &Target, budget: &Budget, build: F)
where
    F: AsyncFn(&mut Renderer) -> toddstack_renderer::Result<Scene> + Copy,
{
    for b in gpu_backends() {
        let Some((caps, diff)) =
            pollster::block_on(demo::parity_scene_transparent(b, target.clone(), build))
                .expect("parity run")
        else {
            println!("{name} {}: skipped, no adapter", b.name());
            continue;
        };
        println!("{name} {}: {diff} ({})", b.name(), caps.adapter);
        assert!(
            diff.within(budget),
            "{name} on {} is outside the budget: {diff}",
            b.name()
        );
    }
}

#[test]
fn sprites_rotated_tinted_and_cut_from_a_sheet_match_the_cpu() {
    scene_parity(
        "sprites",
        &Target::offscreen(640, 400),
        &Budget::SPRITES,
        async |r| {
            let font = r.add_font(FONT)?;
            demo::sprites_scene(r, font)
        },
    );
}

#[test]
fn a_quarter_turn_moves_the_source_corner_and_a_tint_multiplies_on_every_backend() {
    let texels: Vec<u8> = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
    ]
    .concat();
    // Clockwise a quarter turn about the centre: the top-left texel (red) goes to the top-right.
    let turned = |r: &mut Renderer| {
        let id = r
            .add_image(2, 2, &texels, Sampling::Nearest)
            .expect("image");
        let mut s: Scene = Scene::new([255, 255, 255, 255]);
        s.image_rect(
            id,
            [0, 0, 2, 2],
            [0.0, 0.0, 8.0, 8.0],
            std::f32::consts::FRAC_PI_2,
            [255, 255, 255, 255],
        );
        s
    };
    // Only the bottom-right texel (the source rect [1, 1, 1, 1]), stretched over the whole target,
    // tinted half-transparent red over white.
    let tinted = |r: &mut Renderer| {
        let id = r
            .add_image(2, 2, &texels, Sampling::Nearest)
            .expect("image");
        let mut s: Scene = Scene::new([255, 255, 255, 255]);
        s.image_rect(
            id,
            [1, 1, 1, 1],
            [0.0, 0.0, 8.0, 8.0],
            0.0,
            [255, 0, 0, 128],
        );
        s
    };
    let mut all: Vec<Backend> = vec![Backend::Cpu];
    all.extend(gpu_backends());
    for b in all {
        let Some(px) = render_on(b, Target::offscreen(8, 8), turned) else {
            continue;
        };
        let at = |x: usize, y: usize| -> [u8; 3] {
            let o: usize = (y * 8 + x) * 4;
            [px[o], px[o + 1], px[o + 2]]
        };
        assert_eq!([255, 0, 0], at(6, 1), "{} red went top-right", b.name());
        assert_eq!(
            [0, 255, 0],
            at(6, 6),
            "{} green went bottom-right",
            b.name()
        );
        assert_eq!([0, 0, 255], at(1, 1), "{} blue went top-left", b.name());
        assert_eq!(
            [255, 255, 0],
            at(1, 6),
            "{} yellow went bottom-left",
            b.name()
        );
        let Some(px) = render_on(b, Target::offscreen(8, 8), tinted) else {
            continue;
        };
        // Yellow (255,255,0) times a straight (255,0,0) tint is (255,0,0), at half alpha over white.
        for (got, want) in px[3 * 4..3 * 4 + 3].iter().zip([255_u8, 127, 127]) {
            assert!(
                got.abs_diff(want) <= 2,
                "{} tinted {:?}",
                b.name(),
                &px[12..15]
            );
        }
    }
}

#[test]
fn surfaces_need_a_canvas_and_a_device_only_renderer_has_no_target_of_its_own() {
    pollster::block_on(async {
        let mut r: Renderer = Renderer::with_options(
            Target::device_only(),
            Options::new().with_force(Backend::Cpu),
        )
        .await
        .expect("device-only renderer");
        assert_eq!((0, 0), (r.width(), r.height()));
        let scene: Scene = Scene::new([0, 0, 0, 0]);
        assert!(r.render(&scene).await.is_err());
        assert!(r.present(&scene).is_err());
        assert!(r.resize(8, 8).is_err());
        // Off the web an offscreen target is not a canvas.
        assert!(r.attach(Target::offscreen(8, 8)).is_err());
        let mut other: Renderer = Renderer::with_options(
            Target::offscreen(8, 8),
            Options::new().with_force(Backend::Cpu),
        )
        .await
        .expect("offscreen renderer");
        assert!(other.attach(Target::offscreen(8, 8)).is_err());
    });
}

#[test]
fn a_batch_instance_count_below_the_data_length_draws_that_many_on_every_backend() {
    let render = |force: Backend| -> Option<Vec<u8>> {
        pollster::block_on(async {
            let mut r: Renderer =
                Renderer::with_options(Target::offscreen(8, 8), Options::new().with_force(force))
                    .await
                    .ok()?;
            let id = r.add_batch(demo::grid_batch(8, 8)).await.expect("batch");
            r.set_batch_params(id, [8.0, 1.0, 0.0, 0.0])
                .expect("params");
            r.set_batch_instances(id, 10).expect("count");
            let mut s: Scene = Scene::new([255, 255, 255, 255]);
            s.batch(id);
            Some(r.render(&s).await.expect("render"))
        })
    };
    let cpu: Vec<u8> = render(Backend::Cpu).expect("cpu");
    // Rows 2.. are untouched white.
    assert_eq!(
        [255; 4],
        cpu[(2 * 8) * 4..(2 * 8) * 4 + 4],
        "cpu draws only 10 tiles"
    );
    for b in gpu_backends() {
        let Some(px) = render(b) else { continue };
        assert_eq!(
            0.0,
            Diff::between(&cpu, &px).over_1,
            "{} differs from the CPU",
            b.name()
        );
    }
}

#[test]
fn a_clear_with_partial_alpha_and_colour_is_premultiplied_the_same_on_every_backend() {
    for clear in [[255_u8, 0, 0, 128], [255, 255, 255, 0], [10, 200, 90, 77]] {
        let build = |_: &mut Renderer| Scene::new(clear);
        let cpu: Vec<u8> =
            render_transparent_on(Backend::Cpu, Target::offscreen(8, 8), build).expect("cpu");
        for b in gpu_backends() {
            let Some(px) = render_transparent_on(b, Target::offscreen(8, 8), build) else {
                continue;
            };
            for (g, c) in px.iter().zip(&cpu) {
                assert!(
                    g.abs_diff(*c) <= 1,
                    "{} clear {clear:?}: gpu {g} cpu {c}",
                    b.name()
                );
            }
        }
    }
}

#[test]
fn a_sprite_with_a_zero_or_negative_destination_draws_nothing_on_every_backend() {
    let build = |r: &mut Renderer| {
        let id = r
            .add_image(2, 2, &[255; 16], Sampling::Nearest)
            .expect("image");
        let mut s: Scene = Scene::new([0, 0, 0, 255]);
        s.image_rect(id, [0, 0, 2, 2], [6.0, 6.0, -4.0, -4.0], 0.0, [255; 4]);
        s.image_rect(id, [0, 0, 2, 2], [2.0, 2.0, 0.0, 4.0], 0.0, [255; 4]);
        s
    };
    let mut all: Vec<Backend> = vec![Backend::Cpu];
    all.extend(gpu_backends());
    for b in all {
        let Some(px) = render_on(b, Target::offscreen(8, 8), build) else {
            continue;
        };
        assert!(
            px.chunks(4).all(|p| p == [0, 0, 0, 255]),
            "{} drew something",
            b.name()
        );
    }
}

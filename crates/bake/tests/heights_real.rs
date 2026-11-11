//! Bakes the `main_world` height grid from the real client pak and checks low-resolution sector
//! points against the original 1.2 client's terrain heights.
//!
//! ```text
//! AA_PAK=<client>/game_pak cargo test --release -p aa-bake --test heights_real -- --ignored --nocapture
//! ```
//!
//! The output goes to a temporary directory that is deleted afterwards.

use std::path::PathBuf;

use aa_bake::{bake_steps, BakeOptions, BakeSteps};
use aa_world::terrain::Terrain;

#[test]
#[ignore = "needs AA_PAK (the client game_pak)"]
fn low_resolution_node_heights_match_the_original_client() {
    let Some(pak) = std::env::var_os("AA_PAK").map(PathBuf::from) else {
        eprintln!("AA_PAK not set; skipping");
        return;
    };
    let out = std::env::temp_dir().join(format!("aa-bake-heights-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    let mut steps = BakeSteps::none();
    steps.heights = true;
    let report = bake_steps(&BakeOptions::new(&pak, &out, "main_world"), &steps).unwrap();
    println!("{report}");
    assert!(
        report.checks.iter().all(|c| c.ok),
        "failed checks: {:?}",
        report.checks.iter().filter(|c| !c.ok).collect::<Vec<_>>()
    );

    let terrain = Terrain::open(&out.join("heights_main_world.bin")).unwrap();
    // The twelve points whose 2 m quad touches a low-resolution sector node (n_size 5, 9 or 17),
    // with the original 1.2 client's terrain height there. The old C# upscale rule was off by up
    // to ~25 m at these points; the client's resampling rule, also in AAEmu/AAEmu-1.2-Rust PR #56
    // (`NodeCell::get_height`), lands within 0.1 m, the rest being the 4 m grid and the 1/32 m
    // sample rounding.
    let points: [(f32, f32, f32); 12] = [
        (23731.848, 28045.19, 186.21),
        (22061.431, 32225.733, 297.32),
        (24468.402, 25835.133, 121.1429),
        (22951.33, 27138.477, 143.5613),
        (10559.033, 14422.889, 590.6133),
        (22276.248, 32309.476, 252.6515),
        (9623.759, 15653.809, 518.2814),
        (25080.578, 26982.832, 231.5654),
        (24168.77, 25735.047, 121.442),
        (21159.459, 28314.062, 128.1005),
        (21697.676, 32621.94, 297.7408),
        (21090.287, 28251.78, 104.0008),
    ];
    for (x, y, original) in points {
        let z = terrain.height(x, y).unwrap();
        let diff = (z - original).abs();
        println!("({x}, {y}): bake {z}, original {original}, diff {diff}");
        assert!(
            diff <= 0.15,
            "({x}, {y}): bake {z}, original {original}, diff {diff}"
        );
    }

    std::fs::remove_dir_all(&out).unwrap();
}

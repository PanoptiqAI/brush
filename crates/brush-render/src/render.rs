use super::shaders;

use std::mem::{offset_of, size_of};

use crate::{
    BBase, INTERSECTS_UPPER_BOUND, RenderAuxPrimitive,
    camera::Camera,
    dim_check::DimCheck,
    kernels::{MapGaussiansToIntersect, ProjectSplats, ProjectVisible, Rasterize},
};

use brush_kernel::create_dispatch_buffer;
use brush_kernel::create_tensor;
use brush_kernel::create_uniform_buffer;
use brush_kernel::{CubeCount, calc_cube_count};
use brush_prefix_sum::prefix_sum;
use brush_sort::radix_argsort;
use burn::tensor::DType;
use burn::tensor::ops::IntTensorOps;
use burn_cubecl::{BoolElement, FloatElement, IntElement};
use burn_wgpu::CubeTensor;
use burn_wgpu::WgpuRuntime;

use burn::tensor::ops::FloatTensorOps;
use glam::{Vec3, ivec2, uvec2};

const SH_C0: f32 = shaders::project_visible::SH_C0;

pub const fn sh_coeffs_for_degree(degree: u32) -> u32 {
    (degree + 1).pow(2)
}

pub fn sh_degree_from_coeffs(coeffs_per_channel: u32) -> u32 {
    match coeffs_per_channel {
        1 => 0,
        4 => 1,
        9 => 2,
        16 => 3,
        25 => 4,
        _ => panic!("Invalid nr. of sh bases {coeffs_per_channel}"),
    }
}

pub fn channel_to_sh(rgb: f32) -> f32 {
    (rgb - 0.5) / SH_C0
}

pub fn rgb_to_sh(rgb: Vec3) -> Vec3 {
    glam::vec3(
        channel_to_sh(rgb.x),
        channel_to_sh(rgb.y),
        channel_to_sh(rgb.z),
    )
}

pub(crate) fn calc_tile_bounds(img_size: glam::UVec2) -> glam::UVec2 {
    uvec2(
        img_size.x.div_ceil(shaders::helpers::TILE_WIDTH),
        img_size.y.div_ceil(shaders::helpers::TILE_WIDTH),
    )
}

pub(crate) fn max_intersections(img_size: glam::UVec2, num_splats: u32) -> u32 {
    // Divide screen into tiles.
    let tile_bounds = calc_tile_bounds(img_size);
    let num_tiles = tile_bounds[0] * tile_bounds[1];

    // On wasm, we cannot do a sync readback at all.
    // Instead, can just estimate a max number of intersects. All the kernels only handle the actual
    // number of intersects, and spin up empty threads for the rest atm. In the future, could use indirect
    // dispatch to avoid this.
    // Estimating the max number of intersects can be a bad hack though... The worst case sceneario is so massive
    // that it's easy to run out of memory... How do we actually properly deal with this :/
    let max = num_splats.saturating_mul(num_tiles);

    // clamp to max nr. of dispatches.
    max.min(INTERSECTS_UPPER_BOUND)
}

pub(crate) fn render_forward<F: FloatElement, I: IntElement, BT: BoolElement>(
    camera: &Camera,
    img_size: glam::UVec2,
    means: CubeTensor<WgpuRuntime>,
    log_scales: CubeTensor<WgpuRuntime>,
    quats: CubeTensor<WgpuRuntime>,
    sh_coeffs: CubeTensor<WgpuRuntime>,
    raw_opacities: CubeTensor<WgpuRuntime>,
    raster_u32: bool,
) -> (CubeTensor<WgpuRuntime>, RenderAuxPrimitive<BBase<F, I, BT>>) {
    assert!(
        img_size[0] > 0 && img_size[1] > 0,
        "Can't render 0 sized images"
    );

    let device = &means.device.clone();
    let client = means.client.clone();

    // Check whether any work needs to be flushed.
    tracing::trace_span!("pre setup", sync_burn = true).in_scope(|| {});

    let _span = tracing::trace_span!("render_forward", sync_burn = true).entered();

    // Check whether dimensions are valid.
    DimCheck::new()
        .check_dims(&means, &["D".into(), 3.into()])
        .check_dims(&log_scales, &["D".into(), 3.into()])
        .check_dims(&quats, &["D".into(), 4.into()])
        .check_dims(&sh_coeffs, &["D".into(), "C".into(), 3.into()])
        .check_dims(&raw_opacities, &["D".into()]);

    // Divide screen into tiles.
    let tile_bounds = ivec2(
        img_size.x.div_ceil(shaders::helpers::TILE_WIDTH) as i32,
        img_size.y.div_ceil(shaders::helpers::TILE_WIDTH) as i32,
    );

    // A note on some confusing naming that'll be used throughout this function:
    // Gaussians are stored in various states of buffers, eg. at the start they're all in one big buffer,
    // then we sparsely store some results, then sort gaussian based on depths, etc.
    // Overall this means there's lots of indices flying all over the place, and it's hard to keep track
    // what is indexing what. So, for some sanity, try to match a few "gaussian ids" (gid) variable names.
    // - Global Gaussin ID - global_gid
    // - Compacted Gaussian ID - compact_gid
    // - Per tile intersection depth sorted ID - tiled_gid
    // - Sorted by tile per tile intersection depth sorted ID - sorted_tiled_gid
    // Then, various buffers map between these, which are named x_from_y_gid, eg.
    //  global_from_compact_gid.

    // Tile rendering setup.
    let sh_degree = sh_degree_from_coeffs(sh_coeffs.shape.dims[1] as u32);
    let total_splats = means.shape.dims[0] as u32;

    let uniforms_buffer = create_uniform_buffer(
        shaders::helpers::RenderUniforms {
            viewmat: glam::Mat4::from(camera.world_to_local()).to_cols_array_2d(),
            camera_position: [camera.position.x, camera.position.y, camera.position.z, 0.0],
            focal: camera.focal(img_size).into(),
            pixel_center: camera.center(img_size).into(),
            img_size: ivec2(img_size.x as i32, img_size.y as i32).into(),
            tile_bounds: tile_bounds.into(),
            num_visible: 0,
            num_intersections: 0,
            sh_degree,
            total_splats,
        },
        device,
        &client,
    );

    let device = &means.device.clone();

    let num_points = means.shape.dims[0];
    let client = &means.client.clone();

    let radii = BBase::<F, I, BT>::float_zeros([num_points].into(), device);

    let (global_from_compact_gid, num_visible) = {
        let global_from_presort_gid = BBase::<F, I, BT>::int_zeros([num_points].into(), device);
        let depths = create_tensor([num_points], device, client, DType::F32);

        tracing::trace_span!("ProjectSplats", sync_burn = true).in_scope(||
            // SAFETY: wgsl FFI, kernel checked to have no OOB.
            unsafe {
            client.execute_unchecked(
                ProjectSplats::task(),
                calc_cube_count([num_points as u32], ProjectSplats::WORKGROUP_SIZE),
                vec![
                    uniforms_buffer.clone().handle.binding(),
                    means.clone().handle.binding(),
                    quats.clone().handle.binding(),
                    log_scales.clone().handle.binding(),
                    raw_opacities.clone().handle.binding(),
                    global_from_presort_gid.clone().handle.binding(),
                    depths.clone().handle.binding(),
                    radii.clone().handle.binding(),
                ],
            );
        });

        // Get just the number of visible splats from the uniforms buffer.
        let num_vis_field_offset = offset_of!(shaders::helpers::RenderUniforms, num_visible) / 4;
        let num_visible = BBase::<F, I, BT>::int_slice(
            uniforms_buffer.clone(),
            &[num_vis_field_offset..num_vis_field_offset + 1],
        );

        let (_, global_from_compact_gid) = tracing::trace_span!("DepthSort", sync_burn = true)
            .in_scope(|| {
                // Interpret the depth as a u32. This is fine for a radix sort, as long as the depth > 0.0,
                // which we know to be the case given how we cull splats.
                radix_argsort(depths, global_from_presort_gid, &num_visible, 32)
            });

        (global_from_compact_gid, num_visible)
    };

    let projected_size = size_of::<shaders::helpers::ProjectedSplat>() / size_of::<f32>();
    let projected_splats =
        create_tensor::<2, _>([num_points, projected_size], device, client, DType::F32);

    let num_vis_wg = create_dispatch_buffer(num_visible.clone(), [shaders::helpers::MAIN_WG, 1, 1]);

    let max_intersects = max_intersections(img_size, num_points as u32);
    // 1 extra length to make this an exclusive sum.
    let tiles_hit_per_splat = BBase::<F, I, BT>::int_zeros([num_points + 1].into(), device);
    let isect_info =
        create_tensor::<2, WgpuRuntime>([max_intersects as usize, 2], device, client, DType::I32);

    tracing::trace_span!("ProjectVisible", sync_burn = true).in_scope(||
        // SAFETY: Kernel has to contain no OOB indexing.
        unsafe {
        client.execute_unchecked(
            ProjectVisible::task(),
            CubeCount::Dynamic(num_vis_wg.clone().handle.binding()),
            vec![
                uniforms_buffer.clone().handle.binding(),
                means.handle.binding(),
                log_scales.handle.binding(),
                quats.handle.binding(),
                sh_coeffs.handle.binding(),
                raw_opacities.handle.binding(),
                global_from_compact_gid.handle.clone().binding(),
                projected_splats.handle.clone().binding(),
                tiles_hit_per_splat.handle.clone().binding(),
                isect_info.handle.clone().binding(),
            ],
        );
    });

    let num_intersections_offset =
        offset_of!(shaders::helpers::RenderUniforms, num_intersections) / 4;
    let num_intersections = BBase::<F, I, BT>::int_slice(
        uniforms_buffer.clone(),
        &[num_intersections_offset..num_intersections_offset + 1],
    );

    let intersect_wg_buf = create_dispatch_buffer(
        num_intersections.clone(),
        MapGaussiansToIntersect::WORKGROUP_SIZE,
    );

    // Each intersection maps to a gaussian.
    let (tile_offsets, compact_gid_from_isect) = {
        let num_tiles = tile_bounds.x * tile_bounds.y;

        let tile_id_from_isect =
            create_tensor::<1, _>([max_intersects as usize], device, client, DType::I32);
        let compact_gid_from_isect =
            create_tensor::<1, _>([max_intersects as usize], device, client, DType::I32);

        let tile_counts = BBase::<F, I, BT>::int_zeros(
            [(tile_bounds.y * tile_bounds.x) as usize + 1].into(),
            device,
        );

        let cum_tiles_hit = tracing::trace_span!("PrefixSum", sync_burn = true).in_scope(|| {
            // TODO: Only need to do this up to num_visible gaussians really.
            prefix_sum(tiles_hit_per_splat)
        });

        tracing::trace_span!("MapGaussiansToIntersect", sync_burn = true).in_scope(||
        // SAFETY: Kernel has to contain no OOB indexing.
        unsafe {
            client.execute_unchecked(
                MapGaussiansToIntersect::task(),
                CubeCount::Dynamic(intersect_wg_buf.handle.clone().binding()),
                vec![
                    num_intersections.clone().handle.binding(),
                    isect_info.handle.clone().binding(),
                    cum_tiles_hit.handle.binding(),
                    tile_counts.handle.clone().binding(),
                    tile_id_from_isect.handle.clone().binding(),
                    compact_gid_from_isect.handle.clone().binding(),
                ],
            );
        });

        // We're sorting by tile ID, but we know beforehand what the maximum value
        // can be. We don't need to sort all the leading 0 bits!
        let bits = u32::BITS - num_tiles.leading_zeros();

        let (_, compact_gid_from_isect) = tracing::trace_span!("Tile sort", sync_burn = true)
            .in_scope(|| {
                radix_argsort(
                    tile_id_from_isect,
                    compact_gid_from_isect,
                    &num_intersections,
                    bits,
                )
            });

        let _span = tracing::trace_span!("PrefixSumTileCounts", sync_burn = true).entered();
        let tile_offsets = prefix_sum(tile_counts);

        (tile_offsets, compact_gid_from_isect)
    };

    let _span = tracing::trace_span!("Rasterize", sync_burn = true).entered();

    let out_dim = if raster_u32 {
        // Channels are packed into 4 bytes aka one float.
        1
    } else {
        4
    };

    let out_img = create_tensor(
        [img_size.y as usize, img_size.x as usize, out_dim],
        device,
        client,
        // We always pretend this image is a float to simplify other code.
        // In reality it might be a packed u32 aka 4xu8.
        DType::F32,
    );

    // Only record the final visible splat per tile if we're not rendering a u32 buffer.
    // If we're renering a u32 buffer, we can't autodiff anyway, and final index is only needed for
    // the backward pass.

    // Record the final visible splat per tile.
    let final_index = create_tensor::<2, _>(
        [img_size.y as usize, img_size.x as usize],
        device,
        client,
        DType::I32,
    );

    // SAFETY: Kernel has to contain no OOB indexing.
    unsafe {
        client.execute_unchecked(
            Rasterize::task(raster_u32),
            calc_cube_count([img_size.x, img_size.y], Rasterize::WORKGROUP_SIZE),
            vec![
                uniforms_buffer.clone().handle.binding(),
                compact_gid_from_isect.handle.clone().binding(),
                tile_offsets.handle.clone().binding(),
                projected_splats.handle.clone().binding(),
                out_img.handle.clone().binding(),
                final_index.handle.clone().binding(),
            ],
        );
    }

    (
        out_img,
        RenderAuxPrimitive {
            uniforms_buffer,
            num_visible,
            num_intersections,
            tile_offsets,
            projected_splats,
            final_index,
            compact_gid_from_isect,
            global_from_compact_gid,
            radii,
        },
    )
}

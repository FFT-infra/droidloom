//! Crop-local texture coordinates, shared with the host regression harness.

/// Return TL, TR, BL, BR texture coordinates for Android transform flags.
/// `crop` is normalized against the source buffer before entering this function.
pub(super) fn transformed_uv(crop: [f32; 4], transform: i32) -> [[f32; 2]; 4] {
    let [left, top, right, bottom] = crop;
    // Select crop edges before packing vertices. This preserves exact corner
    // values and avoids repeated swaps of the assembled coordinate array.
    let (left, right) = if transform & 1 == 0 {
        (left, right)
    } else {
        (right, left)
    };
    let (top, bottom) = if transform & 2 == 0 {
        (top, bottom)
    } else {
        (bottom, top)
    };
    if transform & 4 == 0 {
        [[left, top], [right, top], [left, bottom], [right, bottom]]
    } else {
        [[left, bottom], [left, top], [right, bottom], [right, top]]
    }
}

#[cfg(test)]
mod tests {
    use super::transformed_uv;

    #[test]
    fn asymmetric_crop_uses_the_same_corners_for_every_transform() {
        let crop = [0.1, 0.2, 0.4, 0.7];
        let corners = [[0.1, 0.2], [0.4, 0.2], [0.1, 0.7], [0.4, 0.7]];
        // Identity, H, V, 180, 90, H+90, V+90, 270 degrees.
        let expected = [
            [0, 1, 2, 3],
            [1, 0, 3, 2],
            [2, 3, 0, 1],
            [3, 2, 1, 0],
            [2, 0, 3, 1],
            [3, 1, 2, 0],
            [0, 2, 1, 3],
            [1, 3, 0, 2],
        ];
        for (flags, order) in expected.into_iter().enumerate() {
            assert_eq!(
                transformed_uv(crop, i32::try_from(flags).unwrap()),
                order.map(|index| corners[index]),
                "transform {flags} sampled outside the source crop"
            );
        }
    }

    #[test]
    fn vertical_flip_does_not_mirror_crop_about_the_full_buffer() {
        assert_eq!(
            transformed_uv([0.05, 0.1, 0.8, 0.4], 2),
            [[0.05, 0.4], [0.8, 0.4], [0.05, 0.1], [0.8, 0.1]]
        );
    }

    #[test]
    fn full_buffer_rotation_and_flip_remain_unchanged() {
        assert_eq!(
            transformed_uv([0.0, 0.0, 1.0, 1.0], 7),
            [[1.0, 0.0], [1.0, 1.0], [0.0, 0.0], [0.0, 1.0]]
        );
    }

    #[test]
    #[ignore = "manual CPU microbenchmark; does not measure GPU or application FPS"]
    fn uv_transform_microbenchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        let mut samples = Vec::new();
        for _ in 0..7 {
            let start = Instant::now();
            for index in 0_i32..2_000_000 {
                black_box(transformed_uv(
                    black_box([0.1, 0.2, 0.4, 0.7]),
                    black_box(index & 7),
                ));
            }
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        println!(
            "uv_transform: median={} ns / 2000000 calls; samples_ns={samples:?}",
            samples[samples.len() / 2]
        );
    }
}

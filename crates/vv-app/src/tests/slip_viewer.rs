use super::*;

fn frame() -> Arc<FrameYuv420> {
    Arc::new(FrameYuv420 {
        width: 1,
        height: 1,
        y: vec![0],
        chroma: vv_media::Chroma::Planar {
            u: vec![0],
            v: vec![0],
        },
        chroma_width: 1,
        chroma_height: 1,
        matrix: vv_media::ColorMatrix::Bt601,
        full_range: false,
        alpha: None,
    })
}

#[test]
fn split_layers_put_the_first_frame_left_and_the_last_right_at_half_size() {
    let layers = split_layers([frame(), frame()], (1920, 1080), (1920, 1080));
    let placed: Vec<([f32; 2], [f32; 2])> = layers
        .iter()
        .map(|l| (l.transform.zoom, l.transform.position))
        .collect();
    assert_eq!(
        placed,
        vec![([0.5, 0.5], [-480.0, 0.0]), ([0.5, 0.5], [480.0, 0.0])]
    );
}

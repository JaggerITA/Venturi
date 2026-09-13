//! Clip "generatore", cioè senza un frame sorgente decodificato: per ora
//! solo `SolidColor` (milestone 6). Un riempimento uniforme non ha nulla
//! da campionare, quindi qui si genera il buffer RGBA direttamente su CPU
//! invece di passare dal compositor GPU — più semplice e altrettanto
//! corretto per un colore piatto (il crop/zoom di un colore piatto è un
//! no-op visivo).

use vv_core::Rgba;

pub fn solid_color_frame(color: Rgba, width: u32, height: u32) -> Vec<u8> {
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let pixel = [
        to_u8(color.r),
        to_u8(color.g),
        to_u8(color.b),
        to_u8(color.a),
    ];

    let mut data = Vec::with_capacity((width * height) as usize * 4);
    for _ in 0..(width * height) {
        data.extend_from_slice(&pixel);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_color_frame_fills_every_pixel_with_the_given_color() {
        let color = Rgba {
            r: 1.0,
            g: 0.5,
            b: 0.0,
            a: 1.0,
        };
        let frame = solid_color_frame(color, 3, 2);
        assert_eq!(frame.len(), 3 * 2 * 4);
        for px in frame.as_chunks::<4>().0 {
            assert_eq!(px, &[255, 128, 0, 255]);
        }
    }

    #[test]
    fn solid_color_frame_clamps_out_of_range_values() {
        let color = Rgba {
            r: 2.0,
            g: -1.0,
            b: 0.5,
            a: 1.0,
        };
        let frame = solid_color_frame(color, 1, 1);
        assert_eq!(&frame[..], &[255, 0, 128, 255]);
    }
}

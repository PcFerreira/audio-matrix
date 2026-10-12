//! RBJ cookbook biquads, shared by the DSP and response graph.
use crate::{EqBand, FilterType};

pub fn coefficients(band: EqBand, rate: u32) -> ([f64; 3], [f64; 2]) {
    if !band.enabled {
        return ([1.0, 0.0, 0.0], [0.0, 0.0]);
    }
    let a = 10.0_f64.powf(band.gain_db as f64 / 40.0);
    let w = std::f64::consts::TAU * band.frequency as f64 / rate as f64;
    let c = w.cos();
    let alpha = w.sin() / (2.0 * band.q as f64);
    let beta = 2.0 * a.sqrt() * alpha;
    let (b, denominator) = match band.kind {
        FilterType::Bell => (
            [1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a],
        ),
        FilterType::LowPass => (
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            [1.0 + alpha, -2.0 * c, 1.0 - alpha],
        ),
        FilterType::HighPass => (
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
            [1.0 + alpha, -2.0 * c, 1.0 - alpha],
        ),
        FilterType::LowShelf => (
            [
                a * ((a + 1.0) - (a - 1.0) * c + beta),
                2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                a * ((a + 1.0) - (a - 1.0) * c - beta),
            ],
            [
                (a + 1.0) + (a - 1.0) * c + beta,
                -2.0 * ((a - 1.0) + (a + 1.0) * c),
                (a + 1.0) + (a - 1.0) * c - beta,
            ],
        ),
        FilterType::HighShelf => (
            [
                a * ((a + 1.0) + (a - 1.0) * c + beta),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                a * ((a + 1.0) + (a - 1.0) * c - beta),
            ],
            [
                (a + 1.0) - (a - 1.0) * c + beta,
                2.0 * ((a - 1.0) - (a + 1.0) * c),
                (a + 1.0) - (a - 1.0) * c - beta,
            ],
        ),
    };
    (
        b.map(|v| v / denominator[0]),
        [
            denominator[1] / denominator[0],
            denominator[2] / denominator[0],
        ],
    )
}
pub fn response_db(band: EqBand, frequency: f32, rate: u32) -> f64 {
    let (b, a) = coefficients(band, rate);
    let w = std::f64::consts::TAU * frequency as f64 / rate as f64;
    let magnitude = |v: [f64; 3]| {
        let re = v[0] + v[1] * w.cos() + v[2] * (2.0 * w).cos();
        let im = -v[1] * w.sin() - v[2] * (2.0 * w).sin();
        re * re + im * im
    };
    10.0 * (magnitude(b) / magnitude([1.0, a[0], a[1]]).max(1e-30))
        .max(1e-30)
        .log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filter_shapes_and_bypass_match_expected_response() {
        let mut band = crate::flat_eq()[2];
        band.gain_db = 6.0;
        assert!((response_db(band, 1000.0, 48000) - 6.0).abs() < 0.001);
        band.kind = FilterType::LowShelf;
        assert!((response_db(band, 20.0, 48000) - 6.0).abs() < 0.02);
        assert!(response_db(band, 18000.0, 48000).abs() < 0.02);
        band.kind = FilterType::HighShelf;
        assert!(response_db(band, 20.0, 48000).abs() < 0.02);
        assert!((response_db(band, 18000.0, 48000) - 6.0).abs() < 0.02);
        band.kind = FilterType::HighPass;
        assert!(response_db(band, 20.0, 48000) < -60.0);
        band.kind = FilterType::LowPass;
        assert!(response_db(band, 18000.0, 48000) < -45.0);
        band.enabled = false;
        assert_eq!(coefficients(band, 48000), ([1.0, 0.0, 0.0], [0.0, 0.0]));
    }
}

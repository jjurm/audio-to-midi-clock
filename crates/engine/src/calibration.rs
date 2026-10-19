//! Hit-versus-noise classifier learned from calibration examples.
//!
//! Each example is the feature vector of one detected peak (per channel: rise
//! per band, level above the noise floor, low and high band balance). A small
//! logistic regression separates "hits that count" from "noise to ignore".

use crate::settings::Example;

/// Most examples kept per class; older ones are dropped first.
pub const MAX_PER_CLASS: usize = 100;
/// Examples needed per class before the classifier is used.
pub const MIN_PER_CLASS: usize = 3;

#[derive(Clone, Debug)]
pub struct Classifier {
    mean: Vec<f32>,
    scale: Vec<f32>,
    weights: Vec<f32>,
    bias: f32,
}

impl Classifier {
    pub fn train(examples: &[Example], dims: usize) -> Option<Self> {
        let usable: Vec<&Example> = examples.iter().filter(|e| e.features.len() == dims).collect();
        let hits = usable.iter().filter(|e| e.hit).count();
        let noise = usable.len() - hits;
        if hits < MIN_PER_CLASS || noise < MIN_PER_CLASS || dims == 0 {
            return None;
        }
        let n = usable.len() as f32;
        let mut mean = vec![0.0f32; dims];
        for e in &usable {
            for (m, x) in mean.iter_mut().zip(&e.features) {
                *m += x / n;
            }
        }
        let mut scale = vec![0.0f32; dims];
        for e in &usable {
            for ((s, x), m) in scale.iter_mut().zip(&e.features).zip(&mean) {
                *s += (x - m).powi(2) / n;
            }
        }
        for s in &mut scale {
            *s = 1.0 / s.sqrt().max(1.0);
        }
        let xs: Vec<Vec<f32>> = usable
            .iter()
            .map(|e| e.features.iter().zip(&mean).zip(&scale).map(|((x, m), s)| (x - m) * s).collect())
            .collect();
        // Balance the classes so many noise examples don't drown a few hits.
        let w_hit = 0.5 / hits as f32;
        let w_noise = 0.5 / noise as f32;
        let mut weights = vec![0.0f32; dims];
        let mut bias = 0.0f32;
        let l2 = 0.05f32;
        let lr = 0.5f32;
        for _ in 0..500 {
            let mut gw = vec![0.0f32; dims];
            let mut gb = 0.0f32;
            for (x, e) in xs.iter().zip(&usable) {
                let z = bias + x.iter().zip(&weights).map(|(a, b)| a * b).sum::<f32>();
                let p = sigmoid(z);
                let y = if e.hit { 1.0 } else { 0.0 };
                let cw = if e.hit { w_hit } else { w_noise };
                let err = (p - y) * cw;
                for (g, xi) in gw.iter_mut().zip(x) {
                    *g += err * xi;
                }
                gb += err;
            }
            for (w, g) in weights.iter_mut().zip(&gw) {
                *w -= lr * (g + l2 * *w);
            }
            bias -= lr * gb;
        }
        Some(Self { mean, scale, weights, bias })
    }

    /// Probability that the features belong to a hit.
    pub fn prob(&self, features: &[f32]) -> f32 {
        if features.len() != self.weights.len() {
            return 1.0;
        }
        let z = self.bias
            + features
                .iter()
                .zip(&self.mean)
                .zip(&self.scale)
                .zip(&self.weights)
                .map(|(((x, m), s), w)| (x - m) * s * w)
                .sum::<f32>();
        sigmoid(z)
    }
}

fn sigmoid(z: f32) -> f32 {
    1.0 / (1.0 + (-z.clamp(-40.0, 40.0)).exp())
}

/// Keep at most `MAX_PER_CLASS` examples per class, dropping the oldest.
pub fn trim(examples: &mut Vec<Example>) {
    for class in [true, false] {
        let count = examples.iter().filter(|e| e.hit == class).count();
        if count > MAX_PER_CLASS {
            let mut drop = count - MAX_PER_CLASS;
            examples.retain(|e| {
                if drop > 0 && e.hit == class {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_bright_from_dull() {
        let mut ex = Vec::new();
        for i in 0..10 {
            let j = i as f32 * 0.3;
            ex.push(Example { hit: true, strength: 10.0, features: vec![20.0 + j, -10.0 - j] });
            ex.push(Example { hit: false, strength: 10.0, features: vec![18.0 - j, 5.0 + j] });
        }
        let c = Classifier::train(&ex, 2).unwrap();
        assert!(c.prob(&[21.0, -12.0]) > 0.8);
        assert!(c.prob(&[18.0, 6.0]) < 0.2);
    }
}

//! Project-wide numeric tolerance used during canonicalization.
//!
//! IFC files from different exporters emit the same geometric values with
//! different trailing-float noise. We quantize to tolerance buckets before
//! hashing so semantically identical models produce identical digests.

use serde::{Deserialize, Serialize};

/// Linear + angular tolerance in SI units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tolerance {
    /// Linear tolerance in meters. Default: 1 µm.
    pub linear: f64,
    /// Angular tolerance in radians. Default: ~5.7 µrad.
    pub angular: f64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            linear: 1.0e-6,
            angular: 1.0e-6,
        }
    }
}

impl Tolerance {
    #[must_use]
    pub const fn new(linear: f64, angular: f64) -> Self {
        Self { linear, angular }
    }

    /// Quantize a linear scalar to this tolerance. NaN and infinities pass
    /// through unchanged so canonicalization can detect them explicitly.
    #[must_use]
    pub fn quantize_linear(&self, x: f64) -> f64 {
        if !x.is_finite() || self.linear <= 0.0 {
            return x;
        }
        (x / self.linear).round() * self.linear
    }

    /// Quantize an angle to this tolerance, first normalizing to (-π, π].
    #[must_use]
    pub fn quantize_angular(&self, a: f64) -> f64 {
        if !a.is_finite() || self.angular <= 0.0 {
            return a;
        }
        let pi = std::f64::consts::PI;
        let two_pi = 2.0 * pi;
        let mut a = a % two_pi;
        if a <= -pi {
            a += two_pi;
        } else if a > pi {
            a -= two_pi;
        }
        (a / self.angular).round() * self.angular
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_quantization() {
        let t = Tolerance::new(1e-3, 1e-6);
        assert!((t.quantize_linear(1.0004) - 1.000).abs() < 1e-9);
        assert!((t.quantize_linear(1.0006) - 1.001).abs() < 1e-9);
    }

    #[test]
    fn angular_wraps() {
        let t = Tolerance::new(1e-6, 1e-6);
        let pi = std::f64::consts::PI;
        let q = t.quantize_angular(pi + 0.5);
        assert!(q < 0.0 && q > -pi);
    }

    #[test]
    fn nonfinite_passthrough() {
        let t = Tolerance::default();
        assert!(t.quantize_linear(f64::NAN).is_nan());
        assert!(t.quantize_linear(f64::INFINITY).is_infinite());
    }
}

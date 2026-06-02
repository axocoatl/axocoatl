use std::time::Instant;

/// Signal state for pheromone-gated activation.
/// Agents activate only when accumulated signal exceeds threshold.
#[derive(Debug, Clone)]
pub struct SignalState {
    pub intensity: f32,
    pub threshold: f32,
    pub last_updated: Instant,
    pub decay_rate: f32,
}

impl SignalState {
    pub fn new(threshold: f32, decay_rate: f32) -> Self {
        Self {
            intensity: 0.0,
            threshold,
            last_updated: Instant::now(),
            decay_rate,
        }
    }

    /// Apply time-based decay: I(t) = I₀ × e^(-λt)
    pub fn apply_decay(&mut self) {
        let elapsed = self.last_updated.elapsed().as_secs_f32();
        self.intensity *= (-self.decay_rate * elapsed).exp();
        self.last_updated = Instant::now();
    }

    /// Add signal from a new event.
    pub fn add_signal(&mut self, strength: f32) {
        self.apply_decay();
        self.intensity += strength;
    }

    /// Check if threshold crossed — if so, reset intensity and return true.
    pub fn should_activate(&mut self) -> bool {
        self.apply_decay();
        if self.intensity >= self.threshold {
            self.intensity = 0.0;
            true
        } else {
            false
        }
    }

    /// Current intensity after decay.
    pub fn current_intensity(&mut self) -> f32 {
        self.apply_decay();
        self.intensity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_signal_is_zero() {
        let s = SignalState::new(1.0, 0.1);
        assert_eq!(s.intensity, 0.0);
    }

    #[test]
    fn add_signal_accumulates() {
        let mut s = SignalState::new(1.0, 0.0); // No decay for test
        s.add_signal(0.3);
        s.add_signal(0.3);
        s.add_signal(0.3);
        assert!(s.intensity > 0.8);
    }

    #[test]
    fn activation_when_threshold_crossed() {
        let mut s = SignalState::new(1.0, 0.0);
        s.add_signal(0.5);
        assert!(!s.should_activate());
        s.add_signal(0.6);
        assert!(s.should_activate());
        // After activation, intensity resets
        assert_eq!(s.intensity, 0.0);
    }

    #[test]
    fn no_activation_below_threshold() {
        let mut s = SignalState::new(1.0, 0.0);
        s.add_signal(0.5);
        assert!(!s.should_activate());
    }

    #[test]
    fn decay_reduces_intensity() {
        let mut s = SignalState::new(1.0, 100.0); // Very fast decay
        s.intensity = 1.0;
        s.last_updated = Instant::now() - std::time::Duration::from_secs(1);
        s.apply_decay();
        // After 1 second with decay_rate=100, intensity should be near zero
        assert!(s.intensity < 0.001);
    }

    #[test]
    fn zero_decay_rate_preserves_signal() {
        let mut s = SignalState::new(1.0, 0.0);
        s.add_signal(0.5);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let intensity = s.current_intensity();
        assert!((intensity - 0.5).abs() < 0.01);
    }
}

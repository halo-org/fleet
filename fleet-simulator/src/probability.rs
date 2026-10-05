use fastrand::Rng;

const SCALE: u32 = 1_000_000;

/// A probability value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Probability(u32);

impl Probability {
    /// A probability of zero.
    pub const NEVER: Self = Self(0);

    /// A probability of one.
    pub const ALWAYS: Self = Self(SCALE);

    /// Draws equally from never, rare, sometimes, often, and always cases.
    pub fn generate(rng: &mut Rng) -> Self {
        Self(match rng.u8(0..5) {
            0 => Self::NEVER.0,
            1 => rare(rng),
            2 => rng.u32(10_000..=100_000),
            3 => rng.u32(100_000..=900_000),
            _ => Self::ALWAYS.0,
        })
    }

    /// Generates a fault probability no greater than 50%.
    pub fn generate_for_faults(rng: &mut Rng) -> Self {
        Self(match rng.u8(0..100) {
            0..35 => 0,
            35..60 => rare(rng),
            60..80 => rng.u32(10_000..=100_000),
            _ => rng.u32(100_000..=500_000),
        })
    }

    /// Returns `true` with this probability.
    pub fn sample(self, rng: &mut Rng) -> bool {
        rng.u32(0..SCALE) < self.0
    }
}

fn rare(rng: &mut Rng) -> u32 {
    if rng.bool() {
        rng.u32(100..1_000)
    } else {
        rng.u32(1_000..=10_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_never() {
        let mut rng = Rng::with_seed(0);
        let probability = Probability::NEVER;

        for _ in 0..100 {
            assert!(!probability.sample(&mut rng));
        }
    }

    #[test]
    fn sample_always() {
        let mut rng = Rng::with_seed(0);
        let probability = Probability::ALWAYS;

        for _ in 0..100 {
            assert!(probability.sample(&mut rng));
        }
    }

    #[test]
    fn generate_for_faults_does_not_cause_too_many_faults() {
        let mut rng = Rng::with_seed(0);

        for _ in 0..10_000 {
            let probability = Probability::generate_for_faults(&mut rng);
            assert!(probability.0 * 2 <= SCALE);
        }
    }

    #[test]
    fn rare_stays_within_range() {
        let mut rng = Rng::with_seed(0);

        for _ in 0..10_000 {
            assert!((100..=10_000).contains(&rare(&mut rng)));
        }
    }
}

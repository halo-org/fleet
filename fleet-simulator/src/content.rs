use core::range::RangeInclusive;
use fastrand::Rng;
use fleet_core::ContentBlock;

/// The bounds for generated text content.
pub struct ContentConfig {
    /// The range of block counts.
    pub blocks: RangeInclusive<usize>,
    /// The range of base-2 exponents for the maximum number of text fragments.
    pub text_length_log2: RangeInclusive<u32>,
}

impl ContentConfig {
    /// Generates content bounds with text length exponents up to `max_text_length_log2`.
    pub fn generate(rng: &mut Rng, max_text_length_log2: u32) -> Self {
        let max_blocks = rng.usize(0..=5);
        let max_length = rng.u32(0..=max_text_length_log2);

        Self {
            blocks: (rng.usize(0..=max_blocks)..=max_blocks).into(),
            text_length_log2: (rng.u32(0..=max_length)..=max_length).into(),
        }
    }
}

/// Generates text blocks within the given bounds.
pub fn generate(rng: &mut Rng, config: &ContentConfig) -> Vec<ContentBlock> {
    (0..rng.usize(config.blocks))
        .map(|_| ContentBlock::Text(generate_text(rng, config)))
        .collect()
}

/// Generates text within the configured length limit.
pub fn generate_text(rng: &mut Rng, config: &ContentConfig) -> String {
    const FRAGMENTS: &[&str] = &[
        "a",
        "b",
        " ",
        "\n",
        "\t",
        "\"",
        "\\",
        "é",
        "中",
        "🦀",
        "e\u{301}",
        "👩‍💻",
    ];

    let max_length = 1_usize
        .checked_shl(rng.u32(config.text_length_log2))
        .expect("text length fits usize");
    let length = rng.usize(0..=max_length);

    (0..length)
        .map(|_| {
            rng.choice(FRAGMENTS)
                .copied()
                .expect("fragments is not empty")
        })
        .collect()
}

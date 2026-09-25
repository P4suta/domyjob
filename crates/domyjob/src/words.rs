pub const WORDS: [&str; 256] = [
    "acorn", "actor", "adobe", "agent", "album", "alpine", "amber", "anchor", "angle", "ankle",
    "apple", "apron", "arbor", "arena", "armor", "arrow", "aspen", "atlas", "attic", "autumn",
    "avocado", "badge", "bagel", "bakery", "bamboo", "banjo", "barley", "basil", "basket",
    "beacon", "beaver", "berry", "bicycle", "birch", "biscuit", "bison", "blanket", "blossom",
    "boat", "bonsai", "bottle", "boulder", "bracket", "breeze", "brick", "bridge", "bronze",
    "brook", "bubble", "bucket", "buffalo", "bugle", "butter", "button", "cabin", "cactus",
    "camel", "camera", "candle", "canoe", "canyon", "carbon", "carpet", "carrot", "castle",
    "cedar", "cello", "cement", "chalk", "cherry", "chess", "chimney", "cider", "cinder", "circle",
    "citrus", "cliff", "clock", "clover", "cobalt", "coconut", "comet", "compass", "copper",
    "coral", "cotton", "cradle", "crane", "crater", "crayon", "cricket", "crystal", "cucumber",
    "cypress", "daisy", "dancer", "delta", "denim", "desert", "diamond", "dolphin", "donkey",
    "dragon", "drum", "dune", "eagle", "easel", "echo", "eclipse", "ember", "engine", "falcon",
    "feather", "fern", "ferry", "fiddle", "fig", "flag", "flint", "flute", "forest", "fossil",
    "fountain", "fox", "galaxy", "garden", "garnet", "ginger", "glacier", "globe", "granite",
    "grape", "guitar", "hammer", "harbor", "harp", "hazel", "helmet", "heron", "hill", "honey",
    "horizon", "igloo", "island", "ivory", "jacket", "jade", "jasmine", "jasper", "jelly",
    "jungle", "juniper", "kayak", "kelp", "kettle", "kite", "koala", "ladder", "lagoon", "lantern",
    "lava", "lemon", "lens", "lilac", "linen", "lobster", "lotus", "lunar", "magnet", "mango",
    "maple", "marble", "meadow", "melon", "meteor", "mint", "mirror", "mosaic", "moss", "mountain",
    "mural", "nectar", "needle", "nimbus", "noodle", "oak", "oasis", "ocean", "olive", "onion",
    "onyx", "orbit", "orchid", "otter", "owl", "paddle", "palm", "panda", "paper", "parrot",
    "pasta", "peach", "pebble", "pencil", "pepper", "piano", "pillow", "pine", "pixel", "planet",
    "plume", "polar", "pony", "poppy", "prairie", "prism", "pumpkin", "puzzle", "quartz", "quill",
    "rabbit", "radar", "radish", "raven", "reef", "ribbon", "ridge", "river", "robin", "rocket",
    "saddle", "saffron", "sage", "salmon", "sapphire", "satin", "scarf", "sequoia", "shadow",
    "shell", "sierra", "silver", "sketch", "sparrow", "spruce", "squash", "summit", "sunset",
    "swan", "tango", "teapot", "thistle", "thunder", "tiger", "timber", "tomato",
];

#[must_use]
pub fn word(byte: u8) -> &'static str {
    WORDS.get(usize::from(byte)).copied().unwrap_or("oak")
}

#[cfg(test)]
mod tests {
    #[test]
    fn words_are_distinct() {
        let mut sorted = super::WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 256);
    }
}

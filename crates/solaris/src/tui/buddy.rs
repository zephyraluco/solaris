//! The terminal companion ("buddy") shown in the welcome box.
//!
//! Ported from claurst's `claurst-buddy` crate: the *bones* (species, rarity,
//! eyes, hat, shiny flag, stats) are rolled from an FNV-1a hash of the user id,
//! so a companion is stable per user and can never be hand-edited. The *soul*
//! (name, personality, hatch time) is stored in `companion.json` beside the
//! credentials.
//!
//! The sprite table is a curated subset of claurst's eighteen species; adding
//! one is a `Species` variant plus three [`SpriteFrame`]s. Art is 12 cells wide
//! after `{E}` substitution, and the idle animation fidgets between three
//! frames the way the original does.

use serde::{Deserialize, Serialize};

/// Width every sprite row is padded to after `{E}` substitution.
pub const FRAME_WIDTH: usize = 12;

// ---------------------------------------------------------------------------
// Seeded PRNG
// ---------------------------------------------------------------------------

/// Mulberry32 — the same tiny PRNG the reference uses.
struct Mulberry32 {
    state: u32,
}

impl Mulberry32 {
    fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    fn next_f64(&mut self) -> f64 {
        self.next_u32() as f64 / 4_294_967_296.0
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut t = (self.state ^ (self.state >> 15)).wrapping_mul(1 | self.state);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        t ^ (t >> 14)
    }

    /// Pick an index in `0..len`.
    fn pick(&mut self, len: usize) -> usize {
        (self.next_f64() * len as f64) as usize % len.max(1)
    }
}

/// FNV-1a hash of a user id, used as the companion's seed.
pub fn seed_from_user_id(user_id: &str) -> u32 {
    const FNV_OFFSET_BASIS: u32 = 2_166_136_261;
    const FNV_PRIME: u32 = 16_777_619;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in user_id.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

/// The companion's species. The list is a subset of claurst's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Species {
    Duck,
    Goose,
    Blob,
    Cat,
    Dragon,
    Owl,
    Ghost,
    Robot,
}

impl Species {
    /// Every species a companion can be rolled as.
    pub const ALL: [Species; 8] = [
        Species::Duck,
        Species::Goose,
        Species::Blob,
        Species::Cat,
        Species::Dragon,
        Species::Owl,
        Species::Ghost,
        Species::Robot,
    ];

    /// Lower-case name, as shown in the `/buddy` card.
    pub fn as_str(&self) -> &'static str {
        match self {
            Species::Duck => "duck",
            Species::Goose => "goose",
            Species::Blob => "blob",
            Species::Cat => "cat",
            Species::Dragon => "dragon",
            Species::Owl => "owl",
            Species::Ghost => "ghost",
            Species::Robot => "robot",
        }
    }
}

/// Rarity tier, weighted like the reference (common 60 %, legendary 1 %).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rarity {
    Common,
    Uncommon,
    Rare,
    Epic,
    Legendary,
}

impl Rarity {
    /// Weight of each tier; the sum is 100.
    const WEIGHTS: [(Rarity, f64); 5] = [
        (Rarity::Common, 60.0),
        (Rarity::Uncommon, 25.0),
        (Rarity::Rare, 10.0),
        (Rarity::Epic, 4.0),
        (Rarity::Legendary, 1.0),
    ];

    /// Lower-case name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Rarity::Common => "common",
            Rarity::Uncommon => "uncommon",
            Rarity::Rare => "rare",
            Rarity::Epic => "epic",
            Rarity::Legendary => "legendary",
        }
    }

    /// Stars shown next to the species in the card.
    pub fn stars(&self) -> &'static str {
        match self {
            Rarity::Common => "★",
            Rarity::Uncommon => "★★",
            Rarity::Rare => "★★★",
            Rarity::Epic => "★★★★",
            Rarity::Legendary => "★★★★★",
        }
    }

    /// Base value stats are rolled around.
    fn stat_floor(&self) -> u8 {
        match self {
            Rarity::Common => 5,
            Rarity::Uncommon => 15,
            Rarity::Rare => 25,
            Rarity::Epic => 35,
            Rarity::Legendary => 50,
        }
    }
}

/// Eye glyph substituted into the sprite's `{E}` slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Eye {
    Dot,
    Star,
    X,
    Circle,
    At,
    Degree,
}

impl Eye {
    const ALL: [Eye; 6] = [
        Eye::Dot,
        Eye::Star,
        Eye::X,
        Eye::Circle,
        Eye::At,
        Eye::Degree,
    ];

    /// The single character drawn in the eye slot.
    pub fn glyph(&self) -> &'static str {
        match self {
            Eye::Dot => "·",
            Eye::Star => "✦",
            Eye::X => "×",
            Eye::Circle => "◉",
            Eye::At => "@",
            Eye::Degree => "°",
        }
    }
}

/// Decoration drawn on the sprite's spare top row, when the art leaves it free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hat {
    None,
    Crown,
    Tophat,
    Propeller,
    Halo,
    Wizard,
    Beanie,
    TinyDuck,
}

impl Hat {
    const ALL: [Hat; 8] = [
        Hat::None,
        Hat::Crown,
        Hat::Tophat,
        Hat::Propeller,
        Hat::Halo,
        Hat::Wizard,
        Hat::Beanie,
        Hat::TinyDuck,
    ];

    /// Lower-case name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Hat::None => "none",
            Hat::Crown => "crown",
            Hat::Tophat => "top hat",
            Hat::Propeller => "propeller",
            Hat::Halo => "halo",
            Hat::Wizard => "wizard hat",
            Hat::Beanie => "beanie",
            Hat::TinyDuck => "tiny duck",
        }
    }

    /// The decoration row. Lines are padded to [`FRAME_WIDTH`] on render, so
    /// the leading spaces here are what centre the hat over the head.
    fn hat_line(&self) -> &'static str {
        match self {
            Hat::None => "",
            Hat::Crown => "   \\^^^/",
            Hat::Tophat => "   [___]",
            Hat::Propeller => "    -+-",
            Hat::Halo => "   (   )",
            Hat::Wizard => "    /^\\",
            Hat::Beanie => "   (___)",
            Hat::TinyDuck => "    ,>",
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// The companion's five traits, each 1–100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionStats {
    pub debugging: u8,
    pub patience: u8,
    pub chaos: u8,
    pub wisdom: u8,
    pub snark: u8,
}

impl CompanionStats {
    /// Roll one peak trait (+50..79 over the floor), one dump trait and three
    /// scattered values, as the reference does.
    fn roll(rarity: Rarity, rng: &mut Mulberry32) -> Self {
        let floor = f64::from(rarity.stat_floor());
        let peak = rng.pick(5);
        let mut dump = rng.pick(5);
        if dump == peak {
            dump = (dump + 1) % 5;
        }

        let mut values = [0u8; 5];
        for (index, value) in values.iter_mut().enumerate() {
            *value = if index == peak {
                ((floor + 50.0 + rng.next_f64() * 30.0) as u8).min(100)
            } else if index == dump {
                (floor - 10.0 + rng.next_f64() * 15.0).max(1.0) as u8
            } else {
                (floor + rng.next_f64() * 40.0) as u8
            };
        }

        CompanionStats {
            debugging: values[0],
            patience: values[1],
            chaos: values[2],
            wisdom: values[3],
            snark: values[4],
        }
    }

    /// Label/value pairs, in the order the card shows them.
    pub fn rows(&self) -> [(&'static str, u8); 5] {
        [
            ("debugging", self.debugging),
            ("patience", self.patience),
            ("chaos", self.chaos),
            ("wisdom", self.wisdom),
            ("snark", self.snark),
        ]
    }
}

// ---------------------------------------------------------------------------
// Bones, soul and companion
// ---------------------------------------------------------------------------

/// The deterministic part of a companion — always re-derived from the user id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bones {
    pub rarity: Rarity,
    pub species: Species,
    pub eye: Eye,
    pub hat: Hat,
    pub shiny: bool,
    pub stats: CompanionStats,
}

impl Bones {
    /// Roll every trait from a user id.
    pub fn from_user_id(user_id: &str) -> Self {
        Self::roll(&mut Mulberry32::new(seed_from_user_id(user_id)))
    }

    /// Roll every trait from an already-seeded RNG.
    fn roll(rng: &mut Mulberry32) -> Self {
        let rarity = {
            let mut roll = rng.next_f64() * 100.0;
            let mut chosen = Rarity::Common;
            for (rarity, weight) in Rarity::WEIGHTS {
                roll -= weight;
                if roll < 0.0 {
                    chosen = rarity;
                    break;
                }
            }
            chosen
        };

        let species = Species::ALL[rng.pick(Species::ALL.len())];
        let eye = Eye::ALL[rng.pick(Eye::ALL.len())];
        // Common companions never wear anything; rarer ones do, but `None` is
        // in the pool, so some still come out bare-headed.
        let hat = if rarity == Rarity::Common {
            Hat::None
        } else {
            Hat::ALL[rng.pick(Hat::ALL.len())]
        };
        let shiny = rng.next_f64() < 0.01;
        let stats = CompanionStats::roll(rarity, rng);

        Bones {
            rarity,
            species,
            eye,
            hat,
            shiny,
            stats,
        }
    }
}

/// The generated identity of a companion, persisted next to the credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Soul {
    pub name: String,
    pub personality: String,
    pub hatched_at_ms: u64,
}

impl Soul {
    /// A soul for `name`.
    pub fn new(name: impl Into<String>, personality: impl Into<String>, at_ms: u64) -> Self {
        Soul {
            name: name.into(),
            personality: personality.into(),
            hatched_at_ms: at_ms,
        }
    }

    /// Serialize for `companion.json`.
    pub fn to_json(&self) -> Result<String, BuddyError> {
        serde_json::to_string_pretty(self).map_err(|error| BuddyError::Encode(error.to_string()))
    }

    /// Parse a previously saved soul.
    pub fn from_json(text: &str) -> Result<Self, BuddyError> {
        serde_json::from_str(text).map_err(|error| BuddyError::Decode(error.to_string()))
    }
}

/// Bones plus the optional soul.
#[derive(Debug, Clone)]
pub struct Companion {
    pub bones: Bones,
    /// `None` until the companion has been named.
    pub soul: Option<Soul>,
}

impl Companion {
    /// Build the companion for `user_id`.
    pub fn new(user_id: &str, soul: Option<Soul>) -> Self {
        Companion {
            bones: Bones::from_user_id(user_id),
            soul,
        }
    }

    /// The given name, falling back to the species before naming.
    pub fn display_name(&self) -> &str {
        match &self.soul {
            Some(soul) => soul.name.as_str(),
            None => self.bones.species.as_str(),
        }
    }

    /// Label/value rows for the `/buddy` card.
    pub fn card(&self) -> Vec<(&'static str, String)> {
        let stats = self
            .bones
            .stats
            .rows()
            .iter()
            .map(|(label, value)| format!("{label} {value}"))
            .collect::<Vec<_>>()
            .join(" · ");

        vec![
            ("name", self.display_name().to_string()),
            ("species", self.bones.species.as_str().to_string()),
            (
                "rarity",
                format!(
                    "{} {}",
                    self.bones.rarity.stars(),
                    self.bones.rarity.as_str()
                ),
            ),
            ("eyes", self.bones.eye.glyph().to_string()),
            ("hat", self.bones.hat.as_str().to_string()),
            (
                "shiny",
                if self.bones.shiny { "yes" } else { "no" }.to_string(),
            ),
            ("stats", stats),
        ]
    }
}

/// Why a companion file could not be read or written.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BuddyError {
    #[error("could not encode the companion: {0}")]
    Encode(String),
    #[error("could not read the companion: {0}")]
    Decode(String),
}

// ---------------------------------------------------------------------------
// Sprites
// ---------------------------------------------------------------------------

/// One animation frame: five rows of raw art, `{E}` marking the eye slot.
#[derive(Debug, Clone, Copy)]
pub struct SpriteFrame(pub [&'static str; 5]);

/// The three idle frames for `species`.
pub fn sprite_frames(species: Species) -> [SpriteFrame; 3] {
    match species {
        Species::Duck => [
            SpriteFrame(["", "    __", "  <({E} )___", "   (  ._>", "    `--´"]),
            SpriteFrame(["", "    __", "  <({E} )___", "   (  ._>", "    `--´~"]),
            SpriteFrame(["", "    __", "  <({E} )___", "   (  .__>", "    `--´"]),
        ],
        Species::Goose => [
            SpriteFrame(["", "     ({E}>", "     ||", "   _(__)_", "    ^^^^"]),
            SpriteFrame(["", "    ({E}>", "     ||", "   _(__)_", "    ^^^^"]),
            SpriteFrame(["", "     ({E}>>", "     ||", "   _(__)_", "    ^^^^"]),
        ],
        Species::Blob => [
            SpriteFrame(["", "   .----.", "  ( {E}  {E} )", "  (      )", "   `----´"]),
            SpriteFrame([
                "",
                "  .------.",
                " (  {E}  {E}  )",
                " (        )",
                "  `------´",
            ]),
            SpriteFrame(["", "    .--.", "   ({E}  {E})", "   (    )", "    `--´"]),
        ],
        Species::Cat => [
            SpriteFrame([
                "",
                "   /\\_/\\",
                "  ( {E}   {E})",
                "  (  ω  )",
                "  (\")_(\")",
            ]),
            SpriteFrame([
                "",
                "   /\\_/\\",
                "  ( {E}   {E})",
                "  (  ω  )",
                "  (\")_(\")~",
            ]),
            SpriteFrame([
                "",
                "   /\\-/\\",
                "  ( {E}   {E})",
                "  (  ω  )",
                "  (\")_(\")",
            ]),
        ],
        Species::Dragon => [
            SpriteFrame([
                "",
                "  /^\\  /^\\",
                " <  {E}  {E}  >",
                " (   ~~   )",
                "  `-vvvv-´",
            ]),
            SpriteFrame([
                "",
                "  /^\\  /^\\",
                " <  {E}  {E}  >",
                " (        )",
                "  `-vvvv-´",
            ]),
            SpriteFrame([
                "   ~    ~",
                "  /^\\  /^\\",
                " <  {E}  {E}  >",
                " (   ~~   )",
                "  `-vvvv-´",
            ]),
        ],
        Species::Owl => [
            SpriteFrame([
                "",
                "   /\\  /\\",
                "  (({E})({E}))",
                "  (  ><  )",
                "   `----´",
            ]),
            SpriteFrame([
                "",
                "   /\\  /\\",
                "  (({E})({E}))",
                "  (  ><  )",
                "   .----.",
            ]),
            SpriteFrame(["", "   /\\  /\\", "  (({E})(-))", "  (  ><  )", "   `----´"]),
        ],
        Species::Ghost => [
            SpriteFrame([
                "",
                "   .----.",
                "  / {E}  {E} \\",
                "  |      |",
                "  ~`~``~`~",
            ]),
            SpriteFrame([
                "",
                "   .----.",
                "  / {E}  {E} \\",
                "  |      |",
                "  `~`~~`~`",
            ]),
            SpriteFrame([
                "    ~  ~",
                "   .----.",
                "  / {E}  {E} \\",
                "  |      |",
                "  ~~`~~`~~",
            ]),
        ],
        Species::Robot => [
            SpriteFrame([
                "",
                "   .[||].",
                "  [ {E}  {E} ]",
                "  [ ==== ]",
                "  `------´",
            ]),
            SpriteFrame([
                "",
                "   .[||].",
                "  [ {E}  {E} ]",
                "  [ -==- ]",
                "  `------´",
            ]),
            SpriteFrame([
                "     *",
                "   .[||].",
                "  [ {E}  {E} ]",
                "  [ ==== ]",
                "  `------´",
            ]),
        ],
    }
}

/// Idle animation: a fifteen-step sequence that mostly rests on frame 0.
///
/// The companion fidgets twice per cycle, which is what the reference's
/// `[0,0,0,0,1,0,0,0,2,0,0,2,0,0,0]` table reproduces.
pub fn animation_frame(tick: u64) -> usize {
    const SEQUENCE: [usize; 15] = [0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 2, 0, 0, 0];
    SEQUENCE[tick as usize % SEQUENCE.len()]
}

/// Pad `line` to [`FRAME_WIDTH`] so the art keeps its alignment regardless of
/// trailing whitespace.
fn padded(line: &str) -> String {
    let width = line.chars().count();
    if width >= FRAME_WIDTH {
        line.to_string()
    } else {
        format!("{line}{}", " ".repeat(FRAME_WIDTH - width))
    }
}

/// Render the companion at `tick` as sprite rows, eye glyph substituted and the
/// hat drawn when the art leaves its top row free.
pub fn render_lines(bones: &Bones, tick: u64) -> Vec<String> {
    let frames = sprite_frames(bones.species);
    let frame = &frames[animation_frame(tick)];
    let eye = bones.eye.glyph();

    let mut lines: Vec<String> = frame
        .0
        .iter()
        .map(|line| padded(&line.replace("{E}", eye)))
        .collect();

    // Only art that leaves row 0 free can wear a hat; species that put smoke or
    // antennae there keep their own decoration.
    if bones.hat != Hat::None && lines[0].trim().is_empty() {
        lines[0] = padded(bones.hat.hat_line());
    }

    // Drop the spare top row when no frame uses it and there is nothing to wear.
    let top_is_free = frames
        .iter()
        .all(|frame| frame.0[0].replace("{E}", eye).trim().is_empty());
    if top_is_free && lines[0].trim().is_empty() {
        lines.remove(0);
    }

    lines
}

/// A one-line face for the companion, used beside its name.
pub fn render_face(bones: &Bones) -> String {
    let eye = bones.eye.glyph();
    match bones.species {
        Species::Duck | Species::Goose => format!("({eye}>"),
        Species::Blob => format!("({eye}{eye})"),
        Species::Cat => format!("={eye}ω{eye}="),
        Species::Dragon => format!("<{eye}~{eye}>"),
        Species::Owl => format!("({eye})({eye})"),
        Species::Ghost => format!("/{eye}{eye}\\"),
        Species::Robot => format!("[{eye}{eye}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eye glyphs used by the sprite tests.
    const EYES: [&str; 6] = ["·", "✦", "×", "◉", "@", "°"];

    fn bones_of(species: Species, hat: Hat, eye: Eye) -> Bones {
        Bones {
            rarity: Rarity::Uncommon,
            species,
            eye,
            hat,
            shiny: false,
            stats: CompanionStats {
                debugging: 50,
                patience: 50,
                chaos: 50,
                wisdom: 50,
                snark: 50,
            },
        }
    }

    #[test]
    fn bones_are_deterministic_per_user() {
        let first = Bones::from_user_id("zeal");
        let second = Bones::from_user_id("zeal");
        assert_eq!(first, second);

        // A different user rolls a different companion far more often than not.
        let others = (0..40)
            .filter(|index| Bones::from_user_id(&format!("user-{index}")) != first)
            .count();
        assert!(others > 35, "bones barely vary: {others}/40");
    }

    #[test]
    fn common_is_the_most_likely_rarity_and_legendary_the_rarest() {
        let mut counts = [0usize; 5];
        for index in 0..600 {
            let bones = Bones::from_user_id(&format!("seed-{index}"));
            let slot = Rarity::WEIGHTS
                .iter()
                .position(|(rarity, _)| *rarity == bones.rarity)
                .expect("known rarity");
            counts[slot] += 1;
        }

        assert!(counts[0] > 300, "common should dominate: {counts:?}");
        assert!(
            counts[4] < counts[3] && counts[3] < counts[2],
            "weights are not ordered: {counts:?}"
        );
    }

    #[test]
    fn stats_are_rolled_around_the_rarity_floor() {
        for rarity in Rarity::WEIGHTS.map(|(rarity, _)| rarity) {
            for index in 0..50 {
                let mut rng = Mulberry32::new(seed_from_user_id(&format!("{index}")));
                let stats = CompanionStats::roll(rarity, &mut rng);
                let values = stats.rows().map(|(_, value)| value);

                assert!(values.iter().all(|value| *value >= 1), "{values:?}");
                assert!(
                    values
                        .iter()
                        .any(|value| *value >= rarity.stat_floor() + 50),
                    "{rarity:?} rolled no peak: {values:?}"
                );
            }
        }
    }

    #[test]
    fn every_sprite_row_is_frame_width_after_substitution() {
        for species in Species::ALL {
            for frame in sprite_frames(species) {
                for line in frame.0 {
                    assert!(!line.contains('\t'), "{species:?} art uses a tab: {line:?}");
                    for eye in EYES {
                        let substituted = padded(&line.replace("{E}", eye));
                        assert_eq!(
                            substituted.chars().count(),
                            FRAME_WIDTH,
                            "{species:?} row is not {FRAME_WIDTH} cells: {line:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_idle_animation_rests_on_the_first_frame() {
        let frames: Vec<usize> = (0..15).map(animation_frame).collect();
        assert_eq!(frames, vec![0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 2, 0, 0, 0]);
        // The cycle repeats.
        assert_eq!(animation_frame(15), animation_frame(0));
    }

    #[test]
    fn rendering_substitutes_the_eye_and_keeps_the_rows_aligned() {
        let bones = bones_of(Species::Cat, Hat::None, Eye::Star);
        let lines = render_lines(&bones, 0);

        assert_eq!(lines.len(), 4, "the spare top row is dropped: {lines:?}");
        assert!(lines.iter().any(|line| line.contains("✦")), "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains("{E}")), "{lines:?}");
        assert!(lines.iter().all(|line| line.chars().count() == FRAME_WIDTH));
    }

    #[test]
    fn a_hat_only_lands_on_art_with_a_free_top_row() {
        let hatted = render_lines(&bones_of(Species::Cat, Hat::Crown, Eye::Dot), 0);
        assert_eq!(hatted.len(), 5, "a hat needs the top row: {hatted:?}");
        assert!(hatted[0].contains("\\^^^/"), "{hatted:?}");

        // The dragon's third frame uses row 0 for smoke, so no hat is drawn.
        // Frame 2 is reached at ticks 8 and 11 of the idle cycle.
        let dragon = render_lines(&bones_of(Species::Dragon, Hat::Crown, Eye::Dot), 8);
        assert_eq!(dragon.len(), 5);
        assert!(dragon[0].contains('~'), "{dragon:?}");
        assert!(
            !dragon.iter().any(|line| line.contains("^^^")),
            "{dragon:?}"
        );
    }

    #[test]
    fn a_named_companion_introduces_itself_by_name() {
        let unnamed = Companion::new("zeal", None);
        assert_eq!(unnamed.display_name(), unnamed.bones.species.as_str());

        let soul = Soul::new("Pip", "perpetually unimpressed", 1_700_000_000_000);
        let named = Companion::new("zeal", Some(soul));
        assert_eq!(named.display_name(), "Pip");
        assert_eq!(named.bones, unnamed.bones, "a name never changes the bones");
    }

    #[test]
    fn the_card_reports_every_trait() {
        let companion = Companion::new("zeal", None);
        let card = companion.card();
        let labels: Vec<&str> = card.iter().map(|(label, _)| *label).collect();

        assert_eq!(
            labels,
            vec!["name", "species", "rarity", "eyes", "hat", "shiny", "stats"]
        );
        let rarity = &card[2].1;
        assert!(rarity.contains(companion.bones.rarity.as_str()), "{rarity}");
        assert!(rarity.starts_with('★'), "{rarity}");
        let stats = &card[6].1;
        for (label, value) in companion.bones.stats.rows() {
            assert!(stats.contains(&format!("{label} {value}")), "{stats}");
        }
    }

    #[test]
    fn faces_use_the_eye_glyph() {
        for species in Species::ALL {
            let face = render_face(&bones_of(species, Hat::None, Eye::At));
            assert!(face.contains('@'), "{species:?}: {face}");
        }
    }

    #[test]
    fn a_soul_round_trips_through_json() {
        let soul = Soul::new("Pip", "sleepy", 42);
        let json = soul.to_json().expect("encode");
        assert_eq!(Soul::from_json(&json).expect("decode"), soul);

        let error = Soul::from_json("{ not json").expect_err("corrupt file");
        assert!(matches!(error, BuddyError::Decode(_)), "{error:?}");
    }
}

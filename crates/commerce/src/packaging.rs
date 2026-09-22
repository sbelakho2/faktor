//! Packaging kinds.
//!
//! Packaging is a first-class commercial dimension: the same die sold on
//! cut tape, on a full reel, in a tray or in a tube has a different MOQ, a
//! different order multiple and often a different price. Packaging is
//! therefore never folded into "the" price of an offer.

use serde::{Deserialize, Serialize};

/// The packaging of a purchasable item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackagingType {
    /// Cut tape (a length cut from a reel).
    CutTape,
    /// Tape and reel (the manufacturer's reel).
    TapeAndReel,
    /// Digi-Reel (a distributor-cut reel).
    DigiReel,
    /// Tray.
    Tray,
    /// Tube / stick.
    Tube,
    /// Bulk / bag.
    Bulk,
    /// Full factory reel.
    FullReel,
    /// Factory pack / case.
    FactoryPack,
    /// Packaging reported by the source but not recognized. Never silently
    /// mapped onto one of the named kinds.
    Other,
}

impl PackagingType {
    /// The canonical wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CutTape => "cut_tape",
            Self::TapeAndReel => "tape_and_reel",
            Self::DigiReel => "digi_reel",
            Self::Tray => "tray",
            Self::Tube => "tube",
            Self::Bulk => "bulk",
            Self::FullReel => "full_reel",
            Self::FactoryPack => "factory_pack",
            Self::Other => "other",
        }
    }

    /// True for the reel-family packagings.
    pub const fn is_reel(self) -> bool {
        matches!(self, Self::TapeAndReel | Self::DigiReel | Self::FullReel)
    }
}

impl std::fmt::Display for PackagingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaging_wire_labels_are_stable() {
        for (packaging, label) in [
            (PackagingType::CutTape, "\"cut_tape\""),
            (PackagingType::TapeAndReel, "\"tape_and_reel\""),
            (PackagingType::DigiReel, "\"digi_reel\""),
            (PackagingType::Tray, "\"tray\""),
            (PackagingType::Tube, "\"tube\""),
            (PackagingType::Bulk, "\"bulk\""),
            (PackagingType::FullReel, "\"full_reel\""),
            (PackagingType::FactoryPack, "\"factory_pack\""),
            (PackagingType::Other, "\"other\""),
        ] {
            assert_eq!(serde_json::to_string(&packaging).expect("serialize"), label);
            assert_eq!(
                serde_json::from_str::<PackagingType>(label).expect("round trip"),
                packaging
            );
        }
        assert!(PackagingType::TapeAndReel.is_reel());
        assert!(!PackagingType::Tray.is_reel());
        assert!(serde_json::from_str::<PackagingType>("\"reel\"").is_err());
        assert!(serde_json::from_str::<PackagingType>("\"TapeAndReel\"").is_err());
    }
}

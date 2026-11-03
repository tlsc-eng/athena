use serde::{Deserialize, Serialize};

/// A sidebar folder's icon; the sidebar draws each from its own SVG.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FolderIcon {
    Cup,
    Briefcase,
    House,
    Flask,
    Rocket,
    Book,
    Star,
    Leaf,
    Gear,
    Globe,
    Heart,
    Code,
}

impl FolderIcon {
    pub const ALL: [Self; 12] = [
        Self::Cup,
        Self::Briefcase,
        Self::House,
        Self::Flask,
        Self::Rocket,
        Self::Book,
        Self::Star,
        Self::Leaf,
        Self::Gear,
        Self::Globe,
        Self::Heart,
        Self::Code,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Cup => "Cup",
            Self::Briefcase => "Briefcase",
            Self::House => "House",
            Self::Flask => "Flask",
            Self::Rocket => "Rocket",
            Self::Book => "Book",
            Self::Star => "Star",
            Self::Leaf => "Leaf",
            Self::Gear => "Gear",
            Self::Globe => "Globe",
            Self::Heart => "Heart",
            Self::Code => "Code",
        }
    }
}

/// A sidebar folder's colour, named so it follows the theme rather than fixing a shade.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FolderColor {
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Accent,
    Muted,
}

impl FolderColor {
    pub const ALL: [Self; 8] = [
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Blue,
        Self::Magenta,
        Self::Cyan,
        Self::Accent,
        Self::Muted,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Red => "Red",
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Blue => "Blue",
            Self::Magenta => "Magenta",
            Self::Cyan => "Cyan",
            Self::Accent => "Accent",
            Self::Muted => "Grey",
        }
    }
}

/// What the user changed about a folder's looks; a part left `None` keeps the default.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FolderStyle {
    pub path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<FolderIcon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<FolderColor>,
}

/// The icon and colour a folder named `name` gets until the user picks, the same on every run.
pub fn default_look(name: &str) -> (FolderIcon, FolderColor) {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    // The low bits of FNV-1a barely move for short names, so the colour reads higher ones.
    let icon = FolderIcon::ALL[(hash % FolderIcon::ALL.len() as u64) as usize];
    let color = FolderColor::ALL[((hash >> 32) % FolderColor::ALL.len() as u64) as usize];
    (icon, color)
}

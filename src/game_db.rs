//! Small compatibility database for controller-related HuCard quirks.
//!
//! This is intentionally narrower than a full game database: it only captures
//! games where selecting a different physical pad improves controls or avoids
//! Avenue Pad 6 incompatibility.

use crate::io::PadMode;

/// Return the preferred pad mode for a known ROM CRC-32.
#[must_use]
pub fn pad_mode_for_crc(crc: u32) -> Option<PadMode> {
    match crc {
        // 6-button games. Prefer Avenue Pad 6 when a title also has a 3-button
        // fallback, because compatible games explicitly detect the 0000 header.
        0xD15C_B6BB // Street Fighter II' - Champion Edition (J)
        | 0x5CF2_FE36 // Martial Champion (J)
        | 0x70F5_F55B // World Heroes 2 (J)
        | 0xC55F_9963 // World Heroes 2 (J) [Demo]
        => Some(PadMode::AvenuePad6),

        // Avenue Pad 3: third action button maps to Select.
        0x933D_5BCC // Air Zonk (USA)
        | 0xD76C_4169 // Air Zonk (USA) [Wii U]
        | 0x28B7_2810 // Akumajou Dracula X - Chi no Rondo (J)
        | 0xA199_6DD5 // Akumajou Dracula X - Chi no Rondo (J) [Alt]
        | 0x4A3D_F3CA // Barunba (J)
        | 0xE70B_01AF // Battle Royale (USA)
        | 0xB4A1_B0F6 // Blazing Lazers (USA)
        | 0xE52B_E33E // Blazing Lazers (USA) [Wii U]
        | 0xA98D_276A // Cyber Core (J)
        | 0x4CFB_6E3E // Cyber Core (USA)
        | 0x8510_1C20 // Download (J)
        | 0x4E0D_E488 // Download (J) [Alt]
        | 0xAF2D_D2AF // Final Soldier (J)
        | 0x02A5_78C5 // Final Soldier - Special Version (J)
        | 0x0683_5D94 // Final Soldier (J) [Wii U]
        | 0x7FF6_BE90 // Forgotten Worlds (J)
        | 0x8C70_F1C0 // Forgotten Worlds (USA)
        | 0xF175_D704 // Gate of Thunder (J)
        | 0xD58E_6C61 // Golden Axe (J)
        | 0x6DBC_6A0B // Gradius II - Gofer no Yabou (J)
        | 0xA17D_4D7E // Gunhed (J)
        | 0x57F1_83AE // Gunhed - Special Version (J)
        | 0xE187_48B1 // Metamor Jupiter (J)
        | 0x37E3_3F90 // Nekketsu Koukou Dodgeball-bu CD - Soccer-hen (J)
        | 0xDE8A_F1C1 // Ninja Spirit (USA)
        | 0x85D1_E33B // Ninja Spirit (USA) [Wii U]
        | 0x7404_91C2 // PC Denjin - Punkic Cyborgs (J)
        | 0x5198_2CE2 // PC Denjin - Punkic Cyborgs (J) [Wii U]
        | 0x0590_A156 // Saigou no Nindou - Ninja Spirit (J)
        | 0x5ECB_B3AC // Saigou no Nindou - Ninja Spirit (J) [Wii U]
        | 0xBC65_5CF3 // Shinobi (J)
        | 0x8420_B12B // Soldier Blade (J)
        | 0x4BB6_8B13 // Soldier Blade (USA)
        | 0xF39F_38ED // Soldier Blade (J) [Special - Caravan Stage]
        | 0x1D87_01C9 // Soldier Blade (J) [Wii U]
        | 0xD211_3BF1 // Soldier Blade (USA) [Wii U]
        | 0x9DB3_C8C7 // Special Criminal Investigation (J)
        | 0x2316_2307 // Spriggan Mark 2 - Re Terraform Project (J)
        | 0x5D0E_3105 // Super Star Soldier (J)
        | 0xDB29_486F // Super Star Soldier (USA)
        | 0x71A5_A90B // Super Star Soldier (J) [Wii U]
        | 0x0FCA_781A // Super Star Soldier (USA) [Wii U]
        | 0xEB04_5EDF // Turrican (USA)
        => Some(PadMode::AvenuePad3Select),

        // Avenue Pad 3: third action button maps to Run.
        0xCA72_A828 // After Burner II (J)
        | 0xCACC_06FB // Ankoku Densetsu (J)
        | 0xFDDC_5814 // Atlantean [Unl]
        | 0x37BA_F6BC // Bloody Wolf (USA)
        | 0x560D_2305 // Final Match Tennis (J)
        | 0x609E_AB27 // John Madden Duo CD Football (USA)
        | 0x220E_BF91 // Legendary Axe 2 (USA)
        | 0xB01F_70C2 // Narazumono Sento Butai - Bloody Wolf (J)
        | 0x61B5_B8D9 // Ookami-teki Monshou - Crest of Wolf (J)
        | 0xDAE8_F28D // Riot Zone (USA)
        | 0x616E_A179 // Silent Debuggers (J)
        | 0xFA7E_5D66 // Silent Debuggers (USA)
        | 0x30D4_2007 // Valis III (J)
        | 0xD77D_ACCE // Valis III (USA)
        => Some(PadMode::AvenuePad3Run),

        _ => None,
    }
}

/// Best-effort fallback for unknown CRCs or renamed/test ROMs.
#[must_use]
pub fn pad_mode_for_title(title: &str) -> Option<PadMode> {
    let title = normalized_title(title);
    if contains_any(&title, SIX_BUTTON_TITLE_PATTERNS) {
        return Some(PadMode::AvenuePad6);
    }
    if contains_any(&title, AVENUE_PAD_3_SELECT_TITLE_PATTERNS) {
        return Some(PadMode::AvenuePad3Select);
    }
    if contains_any(&title, AVENUE_PAD_3_RUN_TITLE_PATTERNS) {
        return Some(PadMode::AvenuePad3Run);
    }
    None
}

#[must_use]
pub const fn pad_mode_name(mode: PadMode) -> &'static str {
    match mode {
        PadMode::TwoButton => "2-button pad",
        PadMode::AvenuePad3Select => "Avenue Pad 3 (III = Select)",
        PadMode::AvenuePad3Run => "Avenue Pad 3 (III = Run)",
        PadMode::AvenuePad6 => "Avenue Pad 6",
    }
}

fn contains_any(title: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| title.contains(pattern))
}

fn normalized_title(title: &str) -> String {
    title
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const SIX_BUTTON_TITLE_PATTERNS: &[&str] = &[
    "advanced vg",
    "advanced variable geo",
    "art of fighting",
    "battlefield 94",
    "fatal fury 2",
    "fatal fury special",
    "fire pro",
    "flash hiders",
    "garou densetsu 2",
    "garou densetsu special",
    "kabuki itouryodan",
    "kabuki ittouryoudan",
    "kakutou haou densetsu algunos",
    "linda cube",
    "mahjong sword princess quest gaiden",
    "martial champion",
    "princess maker 2",
    "ryuuko no ken",
    "sotsugyou ii",
    "street fighter ii",
    "strip fighter",
    "super real mahjong",
    "world heroes 2",
    "ys iv",
];

const AVENUE_PAD_3_SELECT_TITLE_PATTERNS: &[&str] = &[
    "air zonk",
    "akumajou dracula x",
    "barunba",
    "battle royale",
    "blazing lazers",
    "cyber core",
    "download",
    "final soldier",
    "forgotten worlds",
    "gate of thunder",
    "golden axe",
    "gradius ii",
    "gunhed",
    "metamor jupiter",
    "ninja spirit",
    "pc denjin",
    "shinobi",
    "soldier blade",
    "special criminal investigation",
    "spriggan mark 2",
    "super star soldier",
    "turrican",
];

const AVENUE_PAD_3_RUN_TITLE_PATTERNS: &[&str] = &[
    "after burner ii",
    "ankoku densetsu",
    "atlantean",
    "bloody wolf",
    "final match tennis",
    "john madden",
    "legendary axe 2",
    "legendary axe ii",
    "narazumono sento butai",
    "crest of wolf",
    "riot zone",
    "silent debuggers",
    "valis iii",
    "vallis iii",
];

#[cfg(test)]
mod tests {
    use super::{pad_mode_for_crc, pad_mode_for_title};
    use crate::io::PadMode;

    #[test]
    fn prefers_six_button_for_known_six_button_hucards() {
        assert_eq!(pad_mode_for_crc(0xD15C_B6BB), Some(PadMode::AvenuePad6));
        assert_eq!(
            pad_mode_for_title("Street Fighter II' - Champion Edition (Japan).zip"),
            Some(PadMode::AvenuePad6)
        );
    }

    #[test]
    fn recognizes_avenue_pad_3_select_and_run_titles() {
        assert_eq!(
            pad_mode_for_title("Blazing Lazers.zip"),
            Some(PadMode::AvenuePad3Select)
        );
        assert_eq!(
            pad_mode_for_title("Silent Debuggers.zip"),
            Some(PadMode::AvenuePad3Run)
        );
    }
}

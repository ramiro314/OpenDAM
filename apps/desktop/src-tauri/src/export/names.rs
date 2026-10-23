//! Default game names (plan game-export C5): every `assetName` and material name OpenDAM infers
//! comes from `pascal`, and every name it writes must pass `is_pascal`, the contract's
//! `pascalName`.

/// The type prefixes `pascal` drops, one at most, ignoring case. Each ends in `_`, so a word that
/// only starts like one (`Mattress`, `Small`) is kept.
const TYPE_PREFIXES: [&str; 11] = [
    "SM_", "SK_", "S_", "T_", "M_", "MI_", "MAT_", "A_", "AN_", "P_", "BP_",
];

/// `stem` without its leading type prefix, if it has one and something follows it.
pub fn strip_type_prefix(stem: &str) -> &str {
    TYPE_PREFIXES
        .iter()
        .find_map(|prefix| {
            let head = stem.get(..prefix.len())?;
            (head.eq_ignore_ascii_case(prefix) && stem.len() > prefix.len())
                .then(|| &stem[prefix.len()..])
        })
        .unwrap_or(stem)
}

/// The material the export adds to a model whose kept mesh has none (plan C9 step 2).
pub const ADDED_MATERIAL: &str = "OpenDAMDefault";

/// The name of the material the export adds to a model whose kept slots are `slots`:
/// `ADDED_MATERIAL`, or the lowest `OpenDAMDefault2`, `OpenDAMDefault3`, ... that no slot holds.
/// It depends only on the slots, so renaming the file doesn't change it.
pub fn added_material<'a>(slots: impl IntoIterator<Item = &'a str> + Clone) -> String {
    (1..)
        .map(|n| match n {
            1 => ADDED_MATERIAL.to_string(),
            n => format!("{ADDED_MATERIAL}{n}"),
        })
        .find(|name| !slots.clone().into_iter().any(|slot| slot == name))
        .expect("a free name")
}

/// The default name for `stem`: one type prefix dropped, then words split on `_`, `-`, spaces
/// and each lower-to-upper case change, each word capitalised, digits kept and every other
/// character dropped. A result starting with a digit gets an `X` in front. So `SM_arch01_broken`
/// is `Arch01Broken`, `mattress_01` is `Mattress01` and `01_crate` is `X01Crate`. A stem with no
/// ASCII letter or digit gives `""`, which `is_pascal` refuses.
pub fn pascal(stem: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for c in strip_type_prefix(stem).chars() {
        if matches!(c, '_' | '-') || c.is_whitespace() {
            words.push(std::mem::take(&mut word));
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower {
            words.push(std::mem::take(&mut word));
        }
        previous_lower = c.is_lowercase();
        word.push(c);
    }
    words.push(word);

    let mut name = String::new();
    for word in words {
        let mut kept = word.chars().filter(char::is_ascii_alphanumeric);
        if let Some(first) = kept.next() {
            name.push(first.to_ascii_uppercase());
            name.extend(kept);
        }
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, 'X');
    }
    name
}

/// Whether `name` is a contract `pascalName`: `^[A-Z][A-Za-z0-9]*$`. Domain and root collection
/// names must be too, since they are folders of the bundle path.
pub fn is_pascal(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_gives_the_plans_examples() {
        for (stem, name) in [
            ("SM_arch01_broken", "Arch01Broken"),
            ("Small_chest", "SmallChest"),
            ("mattress_01", "Mattress01"),
            ("Mattress_01", "Mattress01"),
            ("Material_Wood", "MaterialWood"),
            ("MAT_Wood", "Wood"),
            ("mat_wood", "Wood"),
            ("PolygonCity_Texture_01_A", "PolygonCityTexture01A"),
            ("T_Rune", "Rune"),
        ] {
            assert_eq!(pascal(stem), name, "{stem}");
            assert!(is_pascal(&pascal(stem)), "{stem}");
        }
    }

    #[test]
    fn pascal_splits_words_and_drops_other_characters() {
        for (stem, name) in [
            ("old-wooden chest", "OldWoodenChest"),
            ("stoneWall", "StoneWall"),
            ("RGBMask", "RGBMask"),
            ("chest (2)", "Chest2"),
            ("café_table", "CafTable"),
            ("01_crate", "X01Crate"),
            ("SM_01", "X01"),
            ("sm_crate", "Crate"),
            ("BP_", "BP"),
            ("T__Double", "Double"),
        ] {
            assert_eq!(pascal(stem), name, "{stem}");
        }
    }

    #[test]
    fn only_one_prefix_is_dropped() {
        assert_eq!(pascal("SM_T_Rock"), "TRock");
        assert_eq!(pascal("T_SM_Rock"), "SMRock");
    }

    #[test]
    fn a_stem_without_letters_or_digits_gives_no_name() {
        for stem in ["", "___", "日本語"] {
            assert_eq!(pascal(stem), "", "{stem}");
            assert!(!is_pascal(&pascal(stem)));
        }
    }

    #[test]
    fn the_added_material_takes_the_lowest_free_name() {
        assert_eq!(added_material([]), "OpenDAMDefault");
        assert_eq!(added_material(["Stone", "Material"]), "OpenDAMDefault");
        assert_eq!(added_material(["OpenDAMDefault"]), "OpenDAMDefault2");
        assert_eq!(
            added_material(["OpenDAMDefault2", "OpenDAMDefault", "OpenDAMDefault4"]),
            "OpenDAMDefault3"
        );
        // Only an exact name is taken
        assert_eq!(added_material(["opendamdefault"]), "OpenDAMDefault");
    }

    #[test]
    fn is_pascal_is_the_contracts_pattern() {
        for name in ["Warden", "DungeonGame", "A", "Crate01"] {
            assert!(is_pascal(name), "{name}");
        }
        for name in [
            "",
            "warden",
            "Dungeon Game",
            "Dungeon_Game",
            "1Crate",
            "Café",
        ] {
            assert!(!is_pascal(name), "{name}");
        }
    }
}

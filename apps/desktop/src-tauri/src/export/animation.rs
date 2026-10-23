//! Animations and rigs (plan game-export C9 step 4, D20, D21): the model an animation follows by
//! default, whether its bones are that model's, and when a model's bones suggest the humanoid
//! rig, which only you set.

use std::collections::HashSet;

/// Unity's 15 required humanoid bones as each naming calls them: the hips, spine and head, then
/// each side's upper arm, lower arm and hand, then each side's upper leg, lower leg and foot.
const NAMINGS: [(&str, [&str; 15]); 3] = [
    (
        "Mixamo",
        [
            "Hips",
            "Spine",
            "Head",
            "LeftArm",
            "LeftForeArm",
            "LeftHand",
            "RightArm",
            "RightForeArm",
            "RightHand",
            "LeftUpLeg",
            "LeftLeg",
            "LeftFoot",
            "RightUpLeg",
            "RightLeg",
            "RightFoot",
        ],
    ),
    (
        "Unity humanoid",
        [
            "Hips",
            "Spine",
            "Head",
            "LeftUpperArm",
            "LeftLowerArm",
            "LeftHand",
            "RightUpperArm",
            "RightLowerArm",
            "RightHand",
            "LeftUpperLeg",
            "LeftLowerLeg",
            "LeftFoot",
            "RightUpperLeg",
            "RightLowerLeg",
            "RightFoot",
        ],
    ),
    (
        "UE mannequin",
        [
            "pelvis",
            "spine_01",
            "head",
            "upperarm_l",
            "lowerarm_l",
            "hand_l",
            "upperarm_r",
            "lowerarm_r",
            "hand_r",
            "thigh_l",
            "calf_l",
            "foot_l",
            "thigh_r",
            "calf_r",
            "foot_r",
        ],
    ),
];

/// The naming whose humanoid skeleton `bones` hold, which suggests the humanoid rig (D20):
/// "Mixamo" (`mixamorig:Hips`, `mixamorig:LeftForeArm`...), "Unity humanoid" (`Hips`,
/// `LeftLowerArm`...) or "UE mannequin" (`pelvis`, `spine_01`, `lowerarm_l`...). A bone matches
/// ignoring case and any namespace before a `:`, as Mixamo's `mixamorig:` or `mixamorig1:`.
/// `None` when no naming's required bones are all there.
pub fn humanoid_naming(bones: &[String]) -> Option<&'static str> {
    let names: HashSet<String> = bones
        .iter()
        .map(|b| b.rsplit(':').next().unwrap_or(b).to_ascii_lowercase())
        .collect();
    NAMINGS
        .iter()
        .find(|(_, required)| {
            required
                .iter()
                .all(|b| names.contains(&b.to_ascii_lowercase()))
        })
        .map(|(naming, _)| *naming)
}

/// The model an animation with `bones` follows by default (D21): of `models`, each a model's id
/// with its bones, the one sharing the most bone names with it, the first in `models`' order on
/// a tie. `None` when no model shares a bone name with it.
pub fn default_avatar<'a>(
    bones: &[String],
    models: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> Option<&'a str> {
    let own: HashSet<&str> = bones.iter().map(String::as_str).collect();
    let mut best: Option<(&str, usize)> = None;
    for (id, model) in models {
        let shared = shared_bones(&own, model);
        if shared > 0 && best.is_none_or(|(_, most)| shared > most) {
            best = Some((id, shared));
        }
    }
    best.map(|(id, _)| id)
}

/// How many of `model`'s bone names are in `own`.
fn shared_bones(own: &HashSet<&str>, model: &[String]) -> usize {
    let model: HashSet<&str> = model.iter().map(String::as_str).collect();
    model.intersection(own).count()
}

/// How many bone names `animation` and `model` share.
pub fn shared(animation: &[String], model: &[String]) -> usize {
    shared_bones(&animation.iter().map(String::as_str).collect(), model)
}

/// The bones of `animation` that aren't `model`'s, in order. An animation is refused unless
/// there are none (C9 step 4): its clips would drive bones the model doesn't have.
pub fn missing_bones<'a>(animation: &'a [String], model: &[String]) -> Vec<&'a str> {
    let model: HashSet<&str> = model.iter().map(String::as_str).collect();
    animation
        .iter()
        .map(String::as_str)
        .filter(|b| !model.contains(b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bones(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn humanoid_is_suggested_by_a_whole_mixamo_unity_or_unreal_skeleton() {
        for (naming, required) in NAMINGS {
            let mut skeleton = bones(&required);
            skeleton.extend(bones(&["Spine1", "LeftHandIndex1", "Tail"]));
            assert_eq!(humanoid_naming(&skeleton), Some(naming), "{naming}");
            // One required bone short, it is no humanoid
            let short: Vec<String> = skeleton.iter().skip(1).cloned().collect();
            assert_eq!(humanoid_naming(&short), None, "{naming}");
        }
        // Mixamo's namespaces, numbered or not, and another case
        let mixamo: Vec<String> = NAMINGS[0]
            .1
            .iter()
            .map(|b| format!("mixamorig1:{b}"))
            .collect();
        assert_eq!(humanoid_naming(&mixamo), Some("Mixamo"));
        let shouting: Vec<String> = NAMINGS[2].1.iter().map(|b| b.to_uppercase()).collect();
        assert_eq!(humanoid_naming(&shouting), Some("UE mannequin"));
        // The fixture's guard has a hips, spine and head, and no limbs
        assert_eq!(
            humanoid_naming(&bones(&["Hips", "Spine", "Head", "Head_end"])),
            None
        );
        assert_eq!(humanoid_naming(&[]), None);
    }

    #[test]
    fn an_animation_follows_the_model_sharing_the_most_bone_names_by_default() {
        let guard = bones(&["Hips", "Spine", "Head", "Head_end"]);
        let dog = bones(&["Hips", "Tail", "Tail_end"]);
        let none: Vec<String> = Vec::new();
        let models = || {
            [
                ("none", &none[..]),
                ("guard", &guard[..]),
                ("dog", &dog[..]),
            ]
        };

        let idle = bones(&["Hips", "Spine", "Head", "Head_end"]);
        assert_eq!(default_avatar(&idle, models()), Some("guard"));
        let wag = bones(&["Hips", "Tail", "Tail_end"]);
        assert_eq!(default_avatar(&wag, models()), Some("dog"));
        // A tie goes to the first model
        let hips = bones(&["Hips"]);
        assert_eq!(default_avatar(&hips, models()), Some("guard"));
        // No shared bone, no model
        assert_eq!(default_avatar(&bones(&["Root"]), models()), None);
        assert_eq!(default_avatar(&none, models()), None);
        assert_eq!(shared(&wag, &guard), 1);
    }

    #[test]
    fn missing_bones_are_the_animation_s_bones_the_model_lacks_in_order() {
        let guard = bones(&["Hips", "Spine", "Head", "Head_end"]);
        assert_eq!(
            missing_bones(&bones(&["Hips", "Spine"]), &guard),
            Vec::<&str>::new()
        );
        assert_eq!(
            missing_bones(&bones(&["Hips", "Tail", "Tail_end"]), &guard),
            ["Tail", "Tail_end"]
        );
        // Bone names are compared exactly, as Blender binds them
        assert_eq!(missing_bones(&bones(&["hips"]), &guard), ["hips"]);
    }
}

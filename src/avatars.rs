//! The bundled mascot avatars a bot can pick for its widget, served at `/avatars/<name>.svg`.
//! Only these names are accepted, so a bot setting can never point the widget at other files.

macro_rules! avatars {
    ($($name:literal),* $(,)?) => {
        const AVATARS: &[(&str, &str)] = &[
            $(($name, include_str!(concat!("../static/avatars/", $name, ".svg"))),)*
        ];
    };
}

avatars!(
    "default",
    "electrician",
    "plumber",
    "heating",
    "roofer",
    "solar",
    "builder",
    "locksmith",
    "gardener",
    "cleaner",
    "directory",
);

/// How the launcher avatar moves: not at all, one wave after load, or turning to follow the pointer.
pub const MOTIONS: &[&str] = &["still", "wave", "follow"];

/// The SVG for a bundled avatar, by name (without `.svg`).
pub fn svg(name: &str) -> Option<&'static str> {
    AVATARS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// The avatar a bot record asked for, if it is one of the bundled ones.
pub fn preset(name: Option<&str>) -> Option<&'static str> {
    let name = name?.trim();
    AVATARS.iter().find(|(n, _)| *n == name).map(|(n, _)| *n)
}

/// The launcher motion for a bot with an avatar; unknown or missing values mean still.
pub fn motion(value: Option<&str>) -> &'static str {
    let value = value.map(str::trim).unwrap_or("");
    MOTIONS
        .iter()
        .find(|m| **m == value)
        .copied()
        .unwrap_or("still")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bundled_names_are_accepted() {
        assert_eq!(preset(Some("electrician")), Some("electrician"));
        assert_eq!(preset(Some(" directory ")), Some("directory"));
        for bad in [
            "",
            "Electrician",
            "../widget",
            "electrician.svg",
            "https://x.test/a.svg",
        ] {
            assert_eq!(preset(Some(bad)), None, "{bad}");
        }
        assert_eq!(preset(None), None);
    }

    #[test]
    fn motion_defaults_to_still() {
        assert_eq!(motion(Some("follow")), "follow");
        assert_eq!(motion(Some("wave")), "wave");
        assert_eq!(motion(Some("spin")), "still");
        assert_eq!(motion(None), "still");
    }

    #[test]
    fn every_avatar_is_a_small_svg_with_the_moving_parts() {
        for (name, svg) in AVATARS {
            assert!(
                svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""),
                "{name}"
            );
            assert!(svg.len() < 2048, "{name} is {} bytes", svg.len());
            for part in ["orc-head", "orc-face", "orc-hat", "orc-eyes", "orc-hand"] {
                assert!(svg.contains(part), "{name} lacks {part}");
            }
            assert!(!svg.contains("<script") && !svg.contains(" on"), "{name}");
        }
    }
}

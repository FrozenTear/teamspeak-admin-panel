//! WS-4 quality presets (PURA-142). Maps the operator-visible preset
//! strings `"480p"`, `"720p"`, `"1080p"` to the resolution / framerate /
//! bitrate triples that drive `pipeline.rs`'s FFmpeg invocation.
//!
//! Source of truth: spec §23.4 (Quality presets):
//!
//! | preset  | resolution | framerate | bitrate |
//! |---------|------------|-----------|---------|
//! | `480p`  | 854×480    | 24 fps    | 1000k   |
//! | `720p`  | 1280×720   | 30 fps    | 2500k   |
//! | `1080p` | 1920×1080  | 30 fps    | 4500k   |
//!
//! The preset strings are external contract — they appear in REST
//! request bodies (`POST /source`) and in `MusicBot.streamPreset` rows.
//! Parsing is case-insensitive (`"720P"` parses), serialisation always
//! emits lowercase. Unknown values fail Deserialize, which surfaces as a
//! 400 from WS-3's error model.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Operator-selected encoding quality. Immutable for the life of a
/// source — switching requires `POST /source/stop` + `POST /source`.
/// Live transcoding switching is out of scope for v1 (epic note).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityPreset {
    P480,
    P720,
    P1080,
}

impl QualityPreset {
    /// The default preset, applied when a `POST /source` payload omits
    /// `preset` or sends `null`. Spec §23.4: default is `720p`.
    pub const DEFAULT: Self = Self::P720;

    pub const fn width(self) -> u32 {
        match self {
            Self::P480 => 854,
            Self::P720 => 1280,
            Self::P1080 => 1920,
        }
    }

    pub const fn height(self) -> u32 {
        match self {
            Self::P480 => 480,
            Self::P720 => 720,
            Self::P1080 => 1080,
        }
    }

    pub const fn framerate(self) -> u32 {
        match self {
            Self::P480 => 24,
            Self::P720 => 30,
            Self::P1080 => 30,
        }
    }

    /// Bitrate string in FFmpeg's `-b:v` / `-maxrate` format.
    pub const fn video_bitrate(self) -> &'static str {
        match self {
            Self::P480 => "1000k",
            Self::P720 => "2500k",
            Self::P1080 => "4500k",
        }
    }

    /// External-contract identifier — what appears on the wire in
    /// `POST /source` bodies and in `/stats` JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P480 => "480p",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
        }
    }
}

impl Default for QualityPreset {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for QualityPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when an unrecognised preset string is parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown quality preset '{0}' (expected '480p', '720p', or '1080p')")]
pub struct ParseQualityPresetError(pub String);

impl FromStr for QualityPreset {
    type Err = ParseQualityPresetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Case-insensitive: `"720P"`, `"720p"`, and `"720P "` (after trim)
        // all parse. The leading-int trim follows the spec, which always
        // shows the preset as `<int>p` with no surrounding whitespace —
        // we trim so accidental operator whitespace doesn't surface a 400.
        match s.trim().to_ascii_lowercase().as_str() {
            "480p" => Ok(Self::P480),
            "720p" => Ok(Self::P720),
            "1080p" => Ok(Self::P1080),
            _ => Err(ParseQualityPresetError(s.to_string())),
        }
    }
}

impl Serialize for QualityPreset {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for QualityPreset {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = QualityPreset;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a quality preset string: '480p', '720p', or '1080p'")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                QualityPreset::from_str(value).map_err(de::Error::custom)
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                QualityPreset::from_str(&value).map_err(de::Error::custom)
            }
        }
        de.deserialize_str(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lowercase() {
        assert_eq!(
            "480p".parse::<QualityPreset>().unwrap(),
            QualityPreset::P480
        );
        assert_eq!(
            "720p".parse::<QualityPreset>().unwrap(),
            QualityPreset::P720
        );
        assert_eq!(
            "1080p".parse::<QualityPreset>().unwrap(),
            QualityPreset::P1080
        );
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(
            "720P".parse::<QualityPreset>().unwrap(),
            QualityPreset::P720
        );
        assert_eq!(
            "1080P".parse::<QualityPreset>().unwrap(),
            QualityPreset::P1080
        );
    }

    #[test]
    fn parse_trims_whitespace() {
        assert_eq!(
            " 720p ".parse::<QualityPreset>().unwrap(),
            QualityPreset::P720
        );
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!("foo".parse::<QualityPreset>().is_err());
        assert!("".parse::<QualityPreset>().is_err());
        assert!("4k".parse::<QualityPreset>().is_err());
        // Adjacent-but-wrong values must not silently coerce.
        assert!("480".parse::<QualityPreset>().is_err());
        assert!("720".parse::<QualityPreset>().is_err());
    }

    #[test]
    fn default_is_720p() {
        assert_eq!(QualityPreset::default(), QualityPreset::P720);
        assert_eq!(QualityPreset::DEFAULT, QualityPreset::P720);
    }

    #[test]
    fn serde_roundtrip() {
        for preset in [
            QualityPreset::P480,
            QualityPreset::P720,
            QualityPreset::P1080,
        ] {
            let s = serde_json::to_string(&preset).unwrap();
            let back: QualityPreset = serde_json::from_str(&s).unwrap();
            assert_eq!(back, preset);
        }
    }

    #[test]
    fn deserialize_case_insensitive() {
        let p: QualityPreset = serde_json::from_str("\"1080P\"").unwrap();
        assert_eq!(p, QualityPreset::P1080);
    }

    #[test]
    fn deserialize_rejects_unknown() {
        let err = serde_json::from_str::<QualityPreset>("\"4k\"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown quality preset"), "{msg}");
    }

    #[test]
    fn spec_table_values() {
        // Spec §23.4 — verbatim values guarded against accidental edits.
        assert_eq!(
            (
                QualityPreset::P480.width(),
                QualityPreset::P480.height(),
                QualityPreset::P480.framerate(),
                QualityPreset::P480.video_bitrate()
            ),
            (854, 480, 24, "1000k")
        );
        assert_eq!(
            (
                QualityPreset::P720.width(),
                QualityPreset::P720.height(),
                QualityPreset::P720.framerate(),
                QualityPreset::P720.video_bitrate()
            ),
            (1280, 720, 30, "2500k")
        );
        assert_eq!(
            (
                QualityPreset::P1080.width(),
                QualityPreset::P1080.height(),
                QualityPreset::P1080.framerate(),
                QualityPreset::P1080.video_bitrate()
            ),
            (1920, 1080, 30, "4500k")
        );
    }
}

//! Working out which audio tracks a release carries.
//!
//! Two independent signals, because neither is reliable alone:
//!
//! 1. **Flag emoji.** Torrentio appends regional-indicator flags to a stream's
//!    title — `🇬🇧 / 🇮🇹 / 🇮🇳` means English, Italian and Hindi audio. This is
//!    structured and trustworthy, but only Torrentio provides it.
//! 2. **The release name.** `Dual.Audio`, `Hin-Eng`, `[Hindi+English]` and so on.
//!    Noisier, but it is all a raw indexer like apibay gives us.
//!
//! Both feed one `Audio` value so ranking does not care where a candidate came
//! from.

use std::collections::BTreeSet;

/// Audio languages detected on a release.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Audio {
    pub hindi: bool,
    pub english: bool,
    /// An explicit "dual audio" / "multi audio" marker in the name, which says
    /// there is more than one track without naming them.
    pub multi_marker: bool,
    /// Everything else recognised, for display only.
    pub others: BTreeSet<&'static str>,
}

/// How confident we are that a release carries both Hindi and English.
///
/// Ordered worst to best so `Ord` can rank candidates directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dual {
    /// No evidence of two languages.
    No,
    /// Says "Dual Audio" (or similar) and mentions one of the two, but does not
    /// name both. Usually right for Indian releases, occasionally not.
    Likely,
    /// Both languages named or flagged.
    Yes,
}

impl Audio {
    /// Confidence that this carries Hindi *and* English.
    ///
    /// `Likely` requires Hindi specifically, not just "some second language".
    /// An earlier version accepted a multi-audio marker plus English, and
    /// promoted a `Dual? English+Portuguese+Spanish` release of Breaking Bad
    /// to the top of the list — multi-language, but not the two languages
    /// anyone asked for.
    pub fn dual(&self) -> Dual {
        if self.hindi && self.english {
            Dual::Yes
        } else if self.multi_marker && self.hindi {
            Dual::Likely
        } else {
            Dual::No
        }
    }

    /// Short label for the candidate line, empty when nothing was detected.
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.hindi {
            parts.push("Hindi");
        }
        if self.english {
            parts.push("English");
        }
        parts.extend(self.others.iter().copied());
        match (self.dual(), parts.is_empty()) {
            (Dual::Yes, _) => format!("Dual {}", parts.join("+")),
            (Dual::Likely, false) => format!("Dual? {}", parts.join("+")),
            (Dual::Likely, true) => "Dual?".into(),
            (Dual::No, true) => String::new(),
            (Dual::No, false) => parts.join("+"),
        }
    }
}

/// Read every signal out of a stream's name and title text.
pub fn detect(text: &str) -> Audio {
    let mut audio = from_flags(text);
    let named = from_name(text);
    audio.hindi |= named.hindi;
    audio.english |= named.english;
    audio.multi_marker |= named.multi_marker;
    audio.others.extend(named.others);
    audio
}

/// Map a country flag emoji to the audio language it stands for here.
///
/// Torrentio flags the *language*, using a representative country: 🇮🇳 for
/// Hindi, 🇬🇧 for English. It is not a claim about where the release came from.
fn language_of(cc: &str) -> Option<(&'static str, Lang)> {
    Some(match cc {
        "IN" => ("Hindi", Lang::Hindi),
        "GB" | "US" => ("English", Lang::English),
        "IT" => ("Italian", Lang::Other),
        "FR" => ("French", Lang::Other),
        "DE" => ("German", Lang::Other),
        "ES" => ("Spanish", Lang::Other),
        "PT" | "BR" => ("Portuguese", Lang::Other),
        "RU" => ("Russian", Lang::Other),
        "JP" => ("Japanese", Lang::Other),
        "KR" => ("Korean", Lang::Other),
        "CN" | "TW" => ("Chinese", Lang::Other),
        "TR" => ("Turkish", Lang::Other),
        "NL" => ("Dutch", Lang::Other),
        "PL" => ("Polish", Lang::Other),
        "SE" => ("Swedish", Lang::Other),
        "DK" => ("Danish", Lang::Other),
        "NO" => ("Norwegian", Lang::Other),
        "FI" => ("Finnish", Lang::Other),
        "GR" => ("Greek", Lang::Other),
        "HU" => ("Hungarian", Lang::Other),
        "RO" => ("Romanian", Lang::Other),
        "AR" | "SA" => ("Arabic", Lang::Other),
        "TH" => ("Thai", Lang::Other),
        "VN" => ("Vietnamese", Lang::Other),
        "ID" => ("Indonesian", Lang::Other),
        "LT" => ("Lithuanian", Lang::Other),
        "UA" => ("Ukrainian", Lang::Other),
        "CZ" => ("Czech", Lang::Other),
        "BG" => ("Bulgarian", Lang::Other),
        "MX" => ("Spanish", Lang::Other),
        _ => return None,
    })
}

enum Lang {
    Hindi,
    English,
    Other,
}

/// Decode regional-indicator pairs (🇮🇳 → "IN") into languages.
fn from_flags(text: &str) -> Audio {
    let mut audio = Audio::default();
    // Regional indicator symbols are U+1F1E6..=U+1F1FF and always come in pairs.
    let letters: Vec<(usize, char)> = text
        .chars()
        .enumerate()
        .filter(|(_, c)| ('\u{1F1E6}'..='\u{1F1FF}').contains(c))
        .collect();

    for pair in letters.chunks(2) {
        let [(i, a), (j, b)] = pair else { continue };
        // Only a genuinely adjacent pair forms a flag.
        if j != &(i + 1) {
            continue;
        }
        let cc = format!(
            "{}{}",
            (b'A' + (*a as u32 - 0x1F1E6) as u8) as char,
            (b'A' + (*b as u32 - 0x1F1E6) as u8) as char
        );
        match language_of(&cc) {
            Some((_, Lang::Hindi)) => audio.hindi = true,
            Some((_, Lang::English)) => audio.english = true,
            Some((name, Lang::Other)) => {
                audio.others.insert(name);
            }
            None => {}
        }
    }
    audio
}

/// Pull language claims out of the release name.
fn from_name(text: &str) -> Audio {
    // Collapse separators so `Dual.Audio`, `Dual_Audio` and `Dual Audio` all
    // read the same, and token tests cannot match inside a longer word.
    let flat: String = text
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    let padded = format!(
        " {} ",
        flat.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let has = |t: &str| padded.contains(&format!(" {t} "));
    let has_phrase = |p: &str| padded.contains(&format!(" {p} "));

    let mut audio = Audio {
        multi_marker: has_phrase("dual audio")
            || has_phrase("dual audios")
            || has_phrase("multi audio")
            || has_phrase("multiaudio")
            || has("dualaudio")
            || has_phrase("dual lang")
            || has_phrase("multi lang"),
        // `hin` and `eng` are standard release abbreviations. Whole-token
        // matching keeps `eng` off "England" and `hin` off "Hinterland".
        hindi: has("hindi") || has("hin") || has_phrase("hindi dd") || has("hindiorg"),
        english: has("english") || has("eng") || has_phrase("english dd"),
        ..Audio::default()
    };

    // Joined forms that the token pass cannot see, e.g. "Hin-Eng" collapses to
    // "hin eng" and is caught above, but "hindienglish" would not be.
    if flat.contains("hindienglish") || flat.contains("enghindi") || flat.contains("hindieng") {
        audio.hindi = true;
        audio.english = true;
    }

    for (token, name) in [
        ("tamil", "Tamil"),
        ("telugu", "Telugu"),
        ("malayalam", "Malayalam"),
        ("kannada", "Kannada"),
        ("bengali", "Bengali"),
        ("bangla", "Bengali"),
        ("marathi", "Marathi"),
        ("punjabi", "Punjabi"),
        ("urdu", "Urdu"),
    ] {
        if has(token) {
            audio.others.insert(name);
        }
    }

    audio
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_decode_to_languages() {
        let a = detect("Some Release\n👤 15 💾 2.49 GB\n🇬🇧 / 🇮🇹 / 🇮🇳");
        assert!(a.hindi, "IN flag means Hindi audio");
        assert!(a.english, "GB flag means English audio");
        assert!(a.others.contains("Italian"));
        assert_eq!(a.dual(), Dual::Yes);
    }

    #[test]
    fn a_single_hindi_flag_is_not_dual() {
        let a = detect("3 Idiots 2009 1080p BluRay x264 Hindi AAC\n🇮🇳");
        assert!(a.hindi);
        assert!(!a.english);
        assert_eq!(a.dual(), Dual::No);
    }

    #[test]
    fn explicit_dual_audio_marker_with_one_language_is_likely() {
        let a = detect("Movie.2019.1080p.BluRay.Dual.Audio.Hindi.DD5.1-XYZ");
        assert!(a.multi_marker);
        assert!(a.hindi);
        assert_eq!(a.dual(), Dual::Likely);
    }

    #[test]
    fn both_languages_named_is_definite() {
        let a = detect("Movie.2019.1080p.Dual.Audio.Hindi.English.x264");
        assert_eq!(a.dual(), Dual::Yes);
    }

    #[test]
    fn hin_eng_abbreviation_is_recognised() {
        let a = detect("Movie 2019 1080p BluRay Hin-Eng x264");
        assert!(a.hindi);
        assert!(a.english);
        assert_eq!(a.dual(), Dual::Yes);
    }

    #[test]
    fn bracketed_form_is_recognised() {
        let a = detect("Movie (2019) 1080p [Hindi + English] BluRay");
        assert_eq!(a.dual(), Dual::Yes);
    }

    #[test]
    fn abbreviations_do_not_fire_inside_longer_words() {
        // "England" must not read as English audio, nor "Hinterland" as Hindi.
        let a = detect("Hinterland.S01.England.1080p.WEB");
        assert!(!a.english, "England should not imply English audio");
        assert!(!a.hindi, "Hinterland should not imply Hindi audio");
    }

    #[test]
    fn a_european_multi_audio_release_is_not_hindi_dual() {
        // Observed on Breaking Bad: multi-language, but not the two languages
        // that were asked for, so it must not be promoted.
        let a = detect("Breaking.Bad.S02E01.720p.Dual.Audio.English.Portuguese.Spanish");
        assert!(a.multi_marker);
        assert!(!a.hindi);
        assert_eq!(a.dual(), Dual::No);
    }

    #[test]
    fn dual_audio_naming_only_english_is_not_enough() {
        let a = detect("Movie.2019.1080p.Dual.Audio.English.x264");
        assert_eq!(a.dual(), Dual::No, "no evidence of Hindi");
    }

    #[test]
    fn a_plain_english_release_is_not_dual() {
        let a = detect("3.Idiots.2009.1080p.BluRay.x264-PHOBOS");
        assert_eq!(a.dual(), Dual::No);
        assert!(!a.hindi);
    }

    #[test]
    fn dual_ranks_above_likely_ranks_above_none() {
        assert!(Dual::Yes > Dual::Likely);
        assert!(Dual::Likely > Dual::No);
    }

    #[test]
    fn other_south_asian_languages_are_listed() {
        let a = detect("Movie.2019.Tamil.Telugu.Hindi.1080p");
        assert!(a.others.contains("Tamil"));
        assert!(a.others.contains("Telugu"));
        assert!(a.hindi);
    }

    #[test]
    fn labels_read_sensibly() {
        assert_eq!(
            detect("Movie.Dual.Audio.Hindi.English").label(),
            "Dual Hindi+English"
        );
        assert_eq!(detect("Movie.Hindi.1080p").label(), "Hindi");
        assert_eq!(detect("Movie.1080p.x264").label(), "");
    }

    #[test]
    fn unpaired_regional_indicators_are_ignored() {
        // A lone indicator is not a flag and must not panic or half-decode.
        let a = detect("Movie \u{1F1EE} 1080p");
        assert_eq!(a, Audio::default());
    }

    #[test]
    fn us_flag_also_counts_as_english() {
        let a = detect("Movie\n🇺🇸 / 🇮🇳");
        assert!(a.english);
        assert!(a.hindi);
        assert_eq!(a.dual(), Dual::Yes);
    }
}

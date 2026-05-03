use regex::Regex;
use std::sync::LazyLock;

/// Parsed result from Claude's response.
#[derive(Debug, Clone)]
pub struct PointingParseResult {
    pub spoken_text: String,
    pub pointing: Option<ParsedPointingCoordinate>,
}

/// A coordinate extracted from a `[POINT:x,y:label:screenN]` tag.
#[derive(Debug, Clone)]
pub struct ParsedPointingCoordinate {
    pub screenshot_pixel_x: f64,
    pub screenshot_pixel_y: f64,
    #[allow(dead_code)]
    pub element_label: String,
    pub screen_number: Option<u32>,
}

static POINT_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\[POINT:(?:none|(\d+)\s*,\s*(\d+)(?::([^\]:\s][^\]:]*?))?(?::screen(\d+))?)\]\s*$",
    )
    .expect("POINT tag regex must compile")
});

/// Parses Claude's response to extract the spoken portion and any pointing coordinate.
pub fn parse_claude_response(full_response_text: &str) -> PointingParseResult {
    let Some(captures) = POINT_TAG_REGEX.captures(full_response_text) else {
        return PointingParseResult {
            spoken_text: full_response_text.trim().to_string(),
            pointing: None,
        };
    };

    let tag_start = captures.get(0).expect("full match always present").start();
    let spoken_text = full_response_text[..tag_start].trim().to_string();

    let pointing = captures.get(1).map(|x_match| {
        let screenshot_pixel_x = x_match.as_str().parse::<f64>().unwrap_or(0.0);
        let screenshot_pixel_y = captures
            .get(2)
            .map(|m| m.as_str().parse::<f64>().unwrap_or(0.0))
            .unwrap_or(0.0);
        let element_label = captures
            .get(3)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let screen_number = captures.get(4).and_then(|m| m.as_str().parse::<u32>().ok());

        ParsedPointingCoordinate {
            screenshot_pixel_x,
            screenshot_pixel_y,
            element_label,
            screen_number,
        }
    });

    PointingParseResult { spoken_text, pointing }
}

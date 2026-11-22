//! Subtitles in SRT and WebVTT, and which lines are on screen at a given time.

/// The most lines shown at once. YouTube's automatic captions overlap, each cue staying up
/// while the next one starts, which makes two rolling lines; more than three is clutter.
pub const MAX_LINES: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Subtitles {
    /// Sorted by start time.
    pub cues: Vec<Cue>,
}

impl Subtitles {
    /// The lines on screen at `secs`, oldest first: the text of every cue that has started and
    /// not yet ended, repeated lines dropped, at most [`MAX_LINES`] (the newest).
    pub fn lines_at(&self, secs: f64) -> Vec<&str> {
        let started = self.cues.partition_point(|c| c.start <= secs);
        let mut lines: Vec<&str> = Vec::new();
        for cue in self.cues[..started].iter().filter(|c| secs < c.end) {
            for line in cue.text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if !lines.contains(&line) {
                    lines.push(line);
                }
            }
        }
        let extra = lines.len().saturating_sub(MAX_LINES);
        lines.drain(..extra);
        lines
    }
}

/// `01:02:03,456`, `01:02:03.456` or `02:03.456` in seconds.
pub fn parse_timestamp(s: &str) -> Option<f64> {
    let s = s.trim();
    let (clock, millis) = s.rsplit_once([',', '.']).unwrap_or((s, "0"));
    if millis.is_empty() || millis.len() > 3 || !millis.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fraction = millis.parse::<f64>().ok()? / 10f64.powi(millis.len() as i32);
    let parts: Vec<&str> = clock.split(':').collect();
    if !(2..=3).contains(&parts.len()) || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let mut secs = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let value: f64 = part.parse().ok()?;
        if (i > 0 || parts.len() == 2) && value >= 60.0 {
            return None;
        }
        secs = secs * 60.0 + value;
    }
    Some(secs + fraction)
}

/// Removes markup (`<i>`, `<c>`, `<00:00:01.000>`, `{\an8}`) and decodes the common entities.
pub fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' if chars.clone().take_while(|&c| c != '<').any(|c| c == '>') => {
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
            }
            '{' if chars.peek() == Some(&'\\') => {
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Parses SRT or WebVTT. Blocks that cannot be read are skipped; text with no cue at all is an
/// error.
pub fn parse(text: &str) -> Result<Subtitles, String> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    for block in text.split("\n\n") {
        let mut lines = block.lines().skip_while(|l| l.trim().is_empty());
        let Some(timing) = lines.by_ref().take(2).find(|l| l.contains("-->")) else { continue };
        let Some((from, to)) = timing.split_once("-->") else { continue };
        // WebVTT puts cue settings after the end time ("00:01.000 align:start position:0%").
        let to = to.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (parse_timestamp(from), parse_timestamp(to)) else { continue };
        if end <= start {
            continue;
        }
        let body: Vec<String> = lines.map(clean_text).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        if body.is_empty() {
            continue;
        }
        cues.push(Cue { start, end, text: body.join("\n") });
    }
    if cues.is_empty() {
        return Err("no subtitles found in the file".into());
    }
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(Subtitles { cues })
}

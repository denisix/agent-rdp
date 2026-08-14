//! OCR engine wrapper using the ocrs library.

use agent_rdp_protocol::OcrMatch;
use anyhow::{Context, Result};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};
use rten::Model;
use std::path::{Path, PathBuf};
use tracing::{debug, trace};

/// OCR service for text detection and recognition.
pub struct OcrService {
    engine: OcrEngine,
}

impl OcrService {
    /// Create a new OCR service by loading models from the given directory.
    pub fn new(models_dir: &Path) -> Result<Self> {
        let detection_path = models_dir.join("text-detection.rten");
        let recognition_path = models_dir.join("text-recognition.rten");

        debug!("Loading OCR detection model from {:?}", detection_path);
        let detection_model = Model::load_file(&detection_path)
            .with_context(|| format!("Failed to load detection model from {:?}", detection_path))?;

        debug!("Loading OCR recognition model from {:?}", recognition_path);
        let recognition_model = Model::load_file(&recognition_path)
            .with_context(|| format!("Failed to load recognition model from {:?}", recognition_path))?;

        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(detection_model),
            recognition_model: Some(recognition_model),
            ..Default::default()
        })
        .context("Failed to create OCR engine")?;

        debug!("OCR engine initialized successfully");
        Ok(Self { engine })
    }

    /// Find text lines on screen that contain the query.
    ///
    /// # Arguments
    /// * `image_data` - PNG or JPEG image bytes
    /// * `query` - Text to search for (searches within full line text)
    /// * `pattern` - If true, use glob-style pattern matching (* and ?)
    /// * `ignore_case` - If true, match case-insensitively
    ///
    /// # Returns
    /// A tuple of (matching lines, total line count)
    pub fn find_text(
        &self,
        image_data: &[u8],
        query: &str,
        pattern: bool,
        ignore_case: bool,
    ) -> Result<(Vec<OcrMatch>, u32)> {
        let (all_lines, total_lines) = self.get_all_lines(image_data)?;

        // Prepare query for comparison
        let query_cmp = if ignore_case {
            query.to_lowercase()
        } else {
            query.to_string()
        };

        let matches: Vec<OcrMatch> = all_lines
            .into_iter()
            .filter(|line| {
                let text_cmp = if ignore_case {
                    line.text.to_lowercase()
                } else {
                    line.text.clone()
                };

                if pattern {
                    glob_match(&query_cmp, &text_cmp)
                } else {
                    // Contains search for non-pattern mode
                    text_cmp.contains(&query_cmp)
                }
            })
            .collect();

        debug!(
            "Found {} matching lines for '{}' out of {} total lines",
            matches.len(),
            query,
            total_lines
        );

        Ok((matches, total_lines))
    }

    /// Get all text lines on screen with positions.
    ///
    /// # Arguments
    /// * `image_data` - PNG or JPEG image bytes
    ///
    /// # Returns
    /// A tuple of (all lines with positions, total line count)
    pub fn get_all_lines(&self, image_data: &[u8]) -> Result<(Vec<OcrMatch>, u32)> {
        // Load image
        let img = image::load_from_memory(image_data)
            .context("Failed to decode image")?
            .into_rgb8();

        let (width, height) = (img.width(), img.height());
        trace!("Image loaded: {}x{}", width, height);

        // Create ImageSource from RGB data
        let img_source = ImageSource::from_bytes(img.as_raw(), (width, height))
            .context("Failed to create image source")?;

        // Prepare input for OCR
        let ocr_input = self
            .engine
            .prepare_input(img_source)
            .context("Failed to prepare OCR input")?;

        // Detect word regions
        let word_rects = self
            .engine
            .detect_words(&ocr_input)
            .context("Failed to detect words")?;

        trace!("Detected {} word regions", word_rects.len());

        // Group words into lines
        let line_rects = self.engine.find_text_lines(&ocr_input, &word_rects);

        // Recognize text in each line
        let line_texts = self
            .engine
            .recognize_text(&ocr_input, &line_rects)
            .context("Failed to recognize text")?;

        // Collect all lines with their bounding boxes
        let mut lines = Vec::new();

        for line_opt in line_texts.iter() {
            if let Some(line) = line_opt {
                // Get full line text
                let text = line.to_string();
                if text.trim().is_empty() {
                    continue;
                }

                // Compute line bounding box from words
                let words: Vec<_> = line.words().collect();
                if words.is_empty() {
                    continue;
                }

                let mut min_x = i32::MAX;
                let mut min_y = i32::MAX;
                let mut max_x = i32::MIN;
                let mut max_y = i32::MIN;

                for word in &words {
                    let rect = word.bounding_rect();
                    min_x = min_x.min(rect.left() as i32);
                    min_y = min_y.min(rect.top() as i32);
                    max_x = max_x.max((rect.left() + rect.width()) as i32);
                    max_y = max_y.max((rect.top() + rect.height()) as i32);
                }

                let x = min_x;
                let y = min_y;
                let width = max_x - min_x;
                let height = max_y - min_y;

                lines.push(OcrMatch {
                    text,
                    x,
                    y,
                    width,
                    height,
                    center_x: x + width / 2,
                    center_y: y + height / 2,
                });
            }
        }

        let total_lines = lines.len() as u32;
        debug!("Detected {} text lines", total_lines);

        Ok((lines, total_lines))
    }
}

/// Simple glob-style pattern matching supporting * and ? wildcards.
fn glob_match(pattern: &str, text: &str) -> bool {
    let mut pattern_chars = pattern.chars().peekable();
    let mut text_chars = text.chars().peekable();

    while pattern_chars.peek().is_some() || text_chars.peek().is_some() {
        match (pattern_chars.peek(), text_chars.peek()) {
            (Some('*'), _) => {
                pattern_chars.next();
                // * matches zero or more characters
                if pattern_chars.peek().is_none() {
                    return true; // * at end matches everything
                }
                // Try matching rest of pattern at each position
                let remaining_pattern: String = pattern_chars.collect();
                let mut remaining_text: String = text_chars.collect();
                while !remaining_text.is_empty() {
                    if glob_match(&remaining_pattern, &remaining_text) {
                        return true;
                    }
                    remaining_text = remaining_text.chars().skip(1).collect();
                }
                return glob_match(&remaining_pattern, "");
            }
            (Some('?'), Some(_)) => {
                pattern_chars.next();
                text_chars.next();
            }
            (Some(pc), Some(tc)) if *pc == *tc => {
                pattern_chars.next();
                text_chars.next();
            }
            (None, None) => return true,
            _ => return false,
        }
    }

    true
}

/// Find the OCR models directory.
///
/// Searched in order, first match wins:
/// 1. `$AGENT_RDP_MODELS_DIR` - set by the npm wrapper, which ships the models in
///    the main `@denisix/agent-rdp` package (they're architecture-independent, so
///    they aren't duplicated into each platform package).
/// 2. `bin/../models` relative to the executable - standalone layouts that keep
///    the models beside the binary.
/// 3. `bin/../../models` relative to the executable - repo checkout, where
///    `target/release/agent-rdp` sits two levels below the root `models/` dir.
pub fn find_models_dir() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(dir) = std::env::var_os("AGENT_RDP_MODELS_DIR") {
        candidates.push(PathBuf::from(dir));
    }

    let exe_path = std::env::current_exe().context("Failed to get executable path")?;
    if let Some(bin_dir) = exe_path.parent() {
        if let Some(root) = bin_dir.parent() {
            candidates.push(root.join("models"));
            if let Some(parent) = root.parent() {
                candidates.push(parent.join("models"));
            }
        }
    }

    for models_dir in &candidates {
        if models_dir.join("text-detection.rten").exists()
            && models_dir.join("text-recognition.rten").exists()
        {
            debug!("Found models directory at {:?}", models_dir);
            return Ok(models_dir.clone());
        }
    }

    anyhow::bail!(
        "Could not find OCR models (text-detection.rten, text-recognition.rten). \
         Searched: {:?}. Run 'bun run build' to copy models, or set AGENT_RDP_MODELS_DIR.",
        candidates
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn test_glob_match_star() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("hello*", "helloworld"));
        assert!(glob_match("*world", "helloworld"));
        assert!(glob_match("*llo*", "helloworld"));
        assert!(!glob_match("hello*", "world"));
    }

    #[test]
    fn test_glob_match_question() {
        assert!(glob_match("h?llo", "hello"));
        assert!(glob_match("h?llo", "hallo"));
        assert!(!glob_match("h?llo", "hllo"));
    }

    #[test]
    fn test_glob_match_combined() {
        assert!(glob_match("h*o", "hello"));
        assert!(glob_match("h?ll*", "helloworld"));
    }

    /// Both env-var cases live in one test: `AGENT_RDP_MODELS_DIR` is
    /// process-global, so splitting them would race under the parallel runner.
    #[test]
    fn test_find_models_dir_env_var() {
        let complete = tempfile::tempdir().unwrap();
        std::fs::write(complete.path().join("text-detection.rten"), b"x").unwrap();
        std::fs::write(complete.path().join("text-recognition.rten"), b"x").unwrap();

        std::env::set_var("AGENT_RDP_MODELS_DIR", complete.path());
        let found = find_models_dir();
        std::env::remove_var("AGENT_RDP_MODELS_DIR");
        assert_eq!(found.unwrap(), complete.path());

        // A directory missing one of the two models must not be accepted; the
        // search falls through to the exe-relative candidates instead.
        let partial = tempfile::tempdir().unwrap();
        std::fs::write(partial.path().join("text-detection.rten"), b"x").unwrap();

        std::env::set_var("AGENT_RDP_MODELS_DIR", partial.path());
        let found = find_models_dir();
        std::env::remove_var("AGENT_RDP_MODELS_DIR");
        assert!(found.map(|p| p != partial.path()).unwrap_or(true));
    }
}

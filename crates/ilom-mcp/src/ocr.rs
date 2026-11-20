//! OCR of the host screen for the `read_screen` tool.
//!
//! The engine (`ocrs`, pure Rust) is only built with the `ocr` feature. The
//! types and the text formatting below always build, so they are tested in
//! every configuration.

// The model lookup is only used by the engine.
#![cfg_attr(not(feature = "ocr"), allow(dead_code))]

use std::path::{Path, PathBuf};

use ilom_kvm_core::session::DecodedFrame;

const DETECTION_MODEL: &str = "text-detection.onnx";
const RECOGNITION_MODEL: &str = "text-recognition.onnx";
const MODELS_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com/";

/// A line of text and its bounding box in frame pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Text lines sorted top to bottom, then left to right, one per output line:
/// `y=<row> x=<col> w=<w> h=<h>: <text>`. Boxes are clamped to the frame.
pub fn format_lines(lines: &[Line], width: u32, height: u32) -> String {
    let (frame_w, frame_h) = (width as i32, height as i32);
    let mut boxes: Vec<(i32, i32, i32, i32, &str)> = Vec::new();
    if frame_w > 0 && frame_h > 0 {
        for line in lines {
            let x = line.x.clamp(0, frame_w - 1);
            let y = line.y.clamp(0, frame_h - 1);
            let right = (line.x + line.width).clamp(x + 1, frame_w);
            let bottom = (line.y + line.height).clamp(y + 1, frame_h);
            boxes.push((y, x, right - x, bottom - y, line.text.trim()));
        }
    }
    boxes.sort_by_key(|&(y, x, ..)| (y, x));
    let mut out = format!("{width}x{height} pixels");
    for (y, x, w, h, text) in boxes {
        out.push_str(&format!("\ny={y} x={x} w={w} h={h}: {text}"));
    }
    out
}

/// Message for models that cannot be found.
fn missing_models(dir: &Path, missing: &[&str]) -> String {
    format!(
        "OCR models not found in {}: missing {}. Download them with \
         `curl -O {MODELS_URL}{DETECTION_MODEL} -O {MODELS_URL}{RECOGNITION_MODEL}` \
         into that directory, or point --ocr-models (ILOM_OCR_MODELS) to the \
         directory that holds them",
        dir.display(),
        missing.join(" and ")
    )
}

/// Checks that both model files exist in `dir`.
fn model_paths(dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let detection = dir.join(DETECTION_MODEL);
    let recognition = dir.join(RECOGNITION_MODEL);
    let missing: Vec<&str> = [
        (&detection, DETECTION_MODEL),
        (&recognition, RECOGNITION_MODEL),
    ]
    .into_iter()
    .filter(|(path, _)| !path.is_file())
    .map(|(_, name)| name)
    .collect();
    if missing.is_empty() {
        Ok((detection, recognition))
    } else {
        Err(missing_models(dir, &missing))
    }
}

/// Lazily loaded OCR engine, shared by all hosts.
pub struct Ocr {
    dir: PathBuf,
    #[cfg(feature = "ocr")]
    engine: std::sync::OnceLock<Result<engine::Engine, String>>,
}

impl Ocr {
    /// `dir` holds the two model files; they are read on the first `read`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            #[cfg(feature = "ocr")]
            engine: std::sync::OnceLock::new(),
        }
    }

    /// Recognizes the text lines of `frame`.
    #[cfg(feature = "ocr")]
    pub fn read(&self, frame: &DecodedFrame) -> Result<Vec<Line>, String> {
        self.engine
            .get_or_init(|| engine::Engine::load(&self.dir))
            .as_ref()
            .map_err(Clone::clone)?
            .read(frame)
    }

    #[cfg(not(feature = "ocr"))]
    pub fn read(&self, _frame: &DecodedFrame) -> Result<Vec<Line>, String> {
        Err("this build has no OCR; rebuild ilom-mcp with `--features ocr`".into())
    }
}

#[cfg(feature = "ocr")]
mod engine {
    use std::path::Path;

    use ilom_kvm_core::session::DecodedFrame;
    use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};
    use rten::Model;

    use super::{Line, model_paths};
    use crate::console::rgb_bytes;

    pub struct Engine(OcrEngine);

    impl Engine {
        pub fn load(dir: &Path) -> Result<Self, String> {
            let (detection, recognition) = model_paths(dir)?;
            let load = |path: &Path| {
                Model::load_file(path)
                    .map_err(|error| format!("cannot load OCR model {}: {error}", path.display()))
            };
            OcrEngine::new(OcrEngineParams {
                detection_model: Some(load(&detection)?),
                recognition_model: Some(load(&recognition)?),
                ..Default::default()
            })
            .map(Self)
            .map_err(|error| format!("cannot start the OCR engine: {error}"))
        }

        pub fn read(&self, frame: &DecodedFrame) -> Result<Vec<Line>, String> {
            let fail = |error: ocrs::OcrError| format!("OCR failed: {error}");
            let rgb = rgb_bytes(frame);
            let source = ImageSource::from_bytes(&rgb, (frame.width, frame.height))
                .map_err(|error| format!("OCR cannot use this frame: {error}"))?;
            let input = self.0.prepare_input(source).map_err(fail)?;
            let words = self.0.detect_words(&input).map_err(fail)?;
            let lines = self.0.find_text_lines(&input, &words);
            let texts = self.0.recognize_text(&input, &lines).map_err(fail)?;
            Ok(texts
                .into_iter()
                .flatten()
                .filter_map(|line| {
                    let text = line.to_string();
                    let text = text.trim();
                    if text.is_empty() {
                        return None;
                    }
                    let rect = line.bounding_rect();
                    Some(Line {
                        text: text.to_string(),
                        x: rect.left(),
                        y: rect.top(),
                        width: rect.width(),
                        height: rect.height(),
                    })
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, x: i32, y: i32, width: i32, height: i32) -> Line {
        Line {
            text: text.into(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn lines_are_sorted_and_clamped() {
        let lines = [
            line("right", 50, 10, 20, 8),
            line("bottom", 5, 40, 10, 30),
            line("left", 2, 10, 20, 8),
            line("outside", -5, 0, 200, 6),
        ];
        assert_eq!(
            format_lines(&lines, 100, 50),
            "100x50 pixels\n\
             y=0 x=0 w=100 h=6: outside\n\
             y=10 x=2 w=20 h=8: left\n\
             y=10 x=50 w=20 h=8: right\n\
             y=40 x=5 w=10 h=10: bottom"
        );
    }

    #[test]
    fn no_lines_gives_the_header_only() {
        assert_eq!(format_lines(&[], 4, 3), "4x3 pixels");
    }

    #[test]
    fn missing_models_name_the_directory_and_files() {
        let dir = std::env::temp_dir().join(format!("ilom-mcp-no-models-{}", std::process::id()));
        let error = model_paths(&dir).unwrap_err();
        assert!(error.contains(&dir.display().to_string()), "{error}");
        assert!(error.contains(DETECTION_MODEL), "{error}");
        assert!(error.contains(RECOGNITION_MODEL), "{error}");
        assert!(error.contains("ILOM_OCR_MODELS"), "{error}");
    }

    fn blank_frame() -> DecodedFrame {
        DecodedFrame {
            width: 8,
            height: 8,
            sequence: 1,
            rgba: vec![255; 8 * 8 * 4],
        }
    }

    #[cfg(not(feature = "ocr"))]
    #[test]
    fn reading_without_the_feature_says_how_to_enable_it() {
        let error = Ocr::new(PathBuf::from("/nonexistent"))
            .read(&blank_frame())
            .unwrap_err();
        assert!(error.contains("--features ocr"), "{error}");
    }

    #[cfg(feature = "ocr")]
    #[test]
    fn reading_without_models_reports_them_and_keeps_the_error() {
        let ocr = Ocr::new(PathBuf::from("/nonexistent/ocr-models"));
        for _ in 0..2 {
            let error = ocr.read(&blank_frame()).unwrap_err();
            assert!(error.contains("/nonexistent/ocr-models"), "{error}");
        }
    }

    /// Needs the models and a picture of text:
    /// `ILOM_OCR_MODELS=<dir> ILOM_OCR_IMAGE=<png> ILOM_OCR_EXPECT=<word>
    /// cargo test --features ocr -- --ignored`.
    #[cfg(feature = "ocr")]
    #[test]
    #[ignore = "needs ILOM_OCR_MODELS, ILOM_OCR_IMAGE and ILOM_OCR_EXPECT"]
    fn ocr_reads_rendered_text() {
        let var = |name| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let image = image::open(var("ILOM_OCR_IMAGE")).unwrap().into_rgba8();
        let frame = DecodedFrame {
            width: image.width(),
            height: image.height(),
            sequence: 1,
            rgba: image.into_raw(),
        };
        let lines = Ocr::new(var("ILOM_OCR_MODELS").into())
            .read(&frame)
            .unwrap();
        let text: String = lines.iter().map(|line| line.text.as_str()).collect();
        let expect = var("ILOM_OCR_EXPECT").to_lowercase();
        assert!(text.to_lowercase().contains(&expect), "{text}");
    }
}

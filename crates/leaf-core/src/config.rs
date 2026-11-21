//! 설정 파일 (SPEC §8, FR-A07). `%APPDATA%\LeafViewer\config.toml` — 사용자가 편집한다.
//!
//! 빠진 키는 기본값, 범위를 벗어난 값은 가까운 허용값으로 바꾼다. 문법 오류가 있으면 파일 전체를
//! 기본값으로 읽고 오류 문구를 돌려준다(앱은 상태 표시줄에 알린다).

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub text: Text,
    pub markdown: Markdown,
    pub pdf: Pdf,
    pub memory: Memory,
    pub editor: Editor,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    pub theme: Theme,
    pub single_instance: bool,
    pub show_all_files: bool,
    pub show_hidden: bool,
    pub gpu: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            single_instance: true,
            show_all_files: false,
            show_hidden: false,
            gpu: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Text {
    pub font: String,
    pub font_size: f32,
    pub word_wrap: bool,
    pub tab_width: u32,
    pub line_numbers: bool,
    pub mmap_threshold_mb: u64,
}

impl Default for Text {
    fn default() -> Self {
        Self {
            font: "Consolas".into(),
            font_size: 11.0,
            word_wrap: true,
            tab_width: 4,
            line_numbers: false,
            mmap_threshold_mb: 16,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Markdown {
    pub font: String,
    pub font_size: f32,
    pub max_width_em: f32,
    pub render_size_limit_mb: u64,
    pub load_remote_images: bool,
}

impl Default for Markdown {
    fn default() -> Self {
        Self {
            font: "Segoe UI".into(),
            font_size: 11.0,
            max_width_em: 50.0,
            render_size_limit_mb: 8,
            load_remote_images: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pdf {
    /// "fit-width" | "fit-page" | 배율(%) 숫자 또는 문자열 ("100")
    pub default_zoom: ZoomSetting,
    pub bitmap_cache_mb: u64,
    pub pdfium_path: String,
}

impl Default for Pdf {
    fn default() -> Self {
        Self {
            default_zoom: ZoomSetting::FitWidth,
            bitmap_cache_mb: 64,
            pdfium_path: String::new(),
        }
    }
}

/// PDF 기본 배율
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ZoomSetting {
    #[default]
    FitWidth,
    FitPage,
    /// 1.0 = 100%
    Percent(f32),
}

impl<'de> Deserialize<'de> for ZoomSetting {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(f64),
            Str(String),
        }
        let pct = |v: f64| ZoomSetting::Percent((v as f32 / 100.0).clamp(0.1, 8.0));
        Ok(match Raw::deserialize(d)? {
            Raw::Num(v) => pct(v),
            Raw::Str(s) => match s.trim().trim_end_matches('%') {
                "fit-width" => ZoomSetting::FitWidth,
                "fit-page" => ZoomSetting::FitPage,
                n => n
                    .parse::<f64>()
                    .map(pct)
                    .map_err(|_| serde::de::Error::custom(format!("알 수 없는 배율: {s}")))?,
            },
        })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Memory {
    pub keep_recent_documents: u32,
    pub image_cache_mb: u64,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            keep_recent_documents: 2,
            image_cache_mb: 32,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Editor {
    /// 비우면 연결 프로그램. `{path}`는 파일 경로로 바뀐다. 예: `code "{path}"`
    pub command: String,
}

pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("LeafViewer").join("config.toml"))
}

/// 설정 파일이 없을 때 "설정 파일 열기"가 만들어 주는 내용 (§8과 같은 기본값, 주석 포함).
pub const TEMPLATE: &str = r#"# LeafViewer 설정 — 저장하면 바로 반영된다. 지운 키는 기본값을 쓴다.

[general]
theme = "system"            # system | light | dark
single_instance = true      # 두 번째 실행은 기존 창에서 연다 (다음 실행부터)
show_all_files = false      # 트리에 지원하지 않는 파일도 표시
show_hidden = false         # 숨김·시스템 파일 표시
gpu = false                 # true면 Direct2D 하드웨어 렌더 타깃 (메모리 증가)

[text]
font = "Consolas"
font_size = 11.0            # pt
word_wrap = true
tab_width = 4
mmap_threshold_mb = 16      # 이 크기 이상은 메모리 매핑

[markdown]
font = "Segoe UI"           # 한글은 DirectWrite 대체 폰트(맑은 고딕)가 자동 처리
font_size = 11.0
max_width_em = 50
render_size_limit_mb = 8    # 넘으면 원문으로 열고 Ctrl+U로 렌더링

[pdf]
default_zoom = "fit-width"  # fit-width | fit-page | 100 (%)
bitmap_cache_mb = 64
pdfium_path = ""            # 비우면 exe 옆의 pdfium.dll (다음 실행부터)

[memory]
image_cache_mb = 32         # MD 이미지 캐시

[editor]
command = ""                # 비우면 연결 프로그램. 예: 'code "{path}"'  (Ctrl+Shift+E)
"#;

impl Config {
    /// 파일이 없으면 기본값. 문법·값 오류면 기본값과 오류 문구.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (Self::default(), Some(e.to_string())),
        }
    }

    pub fn parse(s: &str) -> (Self, Option<String>) {
        // BOM이 붙은 파일(메모장 등)도 읽는다
        match toml::from_str::<Config>(s.trim_start_matches('\u{feff}')) {
            Ok(c) => (c.sanitized(), None),
            Err(e) => (Self::default(), Some(e.message().to_owned())),
        }
    }

    /// 범위를 벗어난 값을 허용 범위로 고친다.
    pub fn sanitized(mut self) -> Self {
        let t = &mut self.text;
        t.font_size = finite_or(t.font_size, 11.0).clamp(6.0, 48.0);
        t.tab_width = t.tab_width.clamp(1, 16);
        t.mmap_threshold_mb = t.mmap_threshold_mb.clamp(1, 4096);
        if t.font.trim().is_empty() {
            t.font = Text::default().font;
        }
        let m = &mut self.markdown;
        m.font_size = finite_or(m.font_size, 11.0).clamp(6.0, 48.0);
        m.max_width_em = finite_or(m.max_width_em, 50.0).clamp(20.0, 400.0);
        m.render_size_limit_mb = m.render_size_limit_mb.clamp(1, 1024);
        if m.font.trim().is_empty() {
            m.font = Markdown::default().font;
        }
        self.pdf.bitmap_cache_mb = self.pdf.bitmap_cache_mb.clamp(16, 2048);
        self.memory.image_cache_mb = self.memory.image_cache_mb.clamp(4, 1024);
        self
    }

    pub fn md_render_limit(&self) -> u64 {
        self.markdown.render_size_limit_mb << 20
    }

    pub fn mmap_threshold(&self) -> u64 {
        self.text.mmap_threshold_mb << 20
    }
}

fn finite_or(v: f32, d: f32) -> f32 {
    if v.is_finite() { v } else { d }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_equals_defaults() {
        let (c, err) = Config::parse(TEMPLATE);
        assert_eq!(err, None);
        assert_eq!(c, Config::default());
    }

    #[test]
    fn partial_and_clamped() {
        let (c, err) = Config::parse(
            "\u{feff}[general]\ntheme = \"dark\"\n[text]\nfont_size = 200\ntab_width = 0\n[pdf]\ndefault_zoom = 150\n",
        );
        assert_eq!(err, None);
        assert_eq!(c.general.theme, Theme::Dark);
        assert!(c.general.single_instance);
        assert_eq!(c.text.font_size, 48.0);
        assert_eq!(c.text.tab_width, 1);
        assert_eq!(c.text.font, "Consolas");
        assert_eq!(c.pdf.default_zoom, ZoomSetting::Percent(1.5));
        assert_eq!(
            Config::parse("[pdf]\ndefault_zoom = \"fit-page\"")
                .0
                .pdf
                .default_zoom,
            ZoomSetting::FitPage
        );
        assert_eq!(
            Config::parse("[pdf]\ndefault_zoom = \"80%\"")
                .0
                .pdf
                .default_zoom,
            ZoomSetting::Percent(0.8)
        );
    }

    #[test]
    fn errors_fall_back_to_defaults() {
        let (c, err) = Config::parse("[general]\ntheme = \"purple\"");
        assert!(err.is_some());
        assert_eq!(c, Config::default());
        let (c, err) = Config::parse("[text\n");
        assert!(err.is_some());
        assert_eq!(c, Config::default());
        let (c, err) = Config::load(Path::new(r"Z:\no\such\config.toml"));
        assert_eq!((c, err), (Config::default(), None));
    }
}
